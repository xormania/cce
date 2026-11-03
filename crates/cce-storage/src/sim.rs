//! Storage simulator: an [`Io`] adapter that models what survives a crash.
//!
//! Each file has a durable image and a volatile image. Writes change the volatile image and are
//! remembered as pending until `sync` makes the file's volatile image durable. File existence is
//! durable only after `sync_dir`. A crash keeps the durable state plus a policy-chosen subset of
//! pending writes, possibly torn at sector granularity, applied out of order where the policy says
//! so. The engine under test is the production engine; only the I/O adapter differs.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

use cce_core::storage::StorageError;

use crate::io::{FileId, Io};

/// Torn-write granularity: a pending write may persist in whole sectors only.
pub(crate) const SECTOR: usize = 512;

#[derive(Clone, Debug)]
struct Pending {
    offset: u64,
    data: Vec<u8>,
}

#[derive(Clone, Debug, Default)]
struct SimFile {
    durable: Vec<u8>,
    volatile: Vec<u8>,
    pending: Vec<Pending>,
}

fn put(buf: &mut Vec<u8>, offset: u64, data: &[u8]) {
    let end = offset as usize + data.len();
    if buf.len() < end {
        buf.resize(end, 0);
    }
    buf[offset as usize..end].copy_from_slice(data);
}

/// What survives a crash among pending (unsynchronized) effects.
#[derive(Clone, Copy, Debug)]
pub(crate) enum Persist {
    /// Every pending write is lost.
    None,
    /// Every pending write survives.
    All,
    /// Each pending write independently survives, chosen by a seeded generator; surviving writes
    /// may be torn at a sector boundary and are applied in a shuffled order.
    Seeded(u64),
}

#[derive(Debug, Default)]
struct State {
    files: HashMap<FileId, SimFile>,
    /// Files whose directory entry is durable.
    durable_entries: HashSet<FileId>,
    locked: bool,
    /// Remaining mutating operations before the simulated crash (`None` = no crash planned).
    budget: Option<u64>,
    crashed: bool,
    /// Mutating operations performed (writes, syncs, directory syncs).
    ops: u64,
    /// Fail this mutating operation number with an I/O error, without crashing.
    fail_op: Option<u64>,
    /// Fail the next read with an I/O error.
    fail_next_read: bool,
}

/// A simulated store directory. Clones share the same state.
#[derive(Clone, Default)]
pub(crate) struct SimIo {
    st: Arc<Mutex<State>>,
}

fn xorshift(s: &mut u64) -> u64 {
    *s ^= *s << 13;
    *s ^= *s >> 7;
    *s ^= *s << 17;
    *s
}

impl SimIo {
    pub fn new() -> Self {
        Self::default()
    }

    /// Crash after `n` more mutating operations: operation n+1 and later fail and change nothing.
    pub fn crash_after(&self, n: u64) {
        self.st.lock().expect("sim").budget = Some(n);
    }

    /// Make the n-th mutating operation (counting from now) fail with an I/O error.
    pub fn fail_nth_op(&self, n: u64) {
        let mut st = self.st.lock().expect("sim");
        st.fail_op = Some(st.ops + n);
    }

    /// Makes the next read fail with an I/O error (once).
    pub fn fail_next_read(&self) {
        self.st.lock().expect("sim").fail_next_read = true;
    }

    pub fn ops(&self) -> u64 {
        self.st.lock().expect("sim").ops
    }

    /// The image a fresh process would see after a crash now, under `persist`.
    pub fn after_crash(&self, persist: Persist) -> SimIo {
        let st = self.st.lock().expect("sim");
        let mut files = HashMap::new();
        let mut rng = match persist {
            Persist::Seeded(s) => s.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1,
            _ => 1,
        };
        let mut ids: Vec<FileId> = st.files.keys().copied().collect();
        ids.sort_by_key(|f| f.name());
        for id in ids {
            let f = &st.files[&id];
            if !st.durable_entries.contains(&id) {
                continue; // the directory entry itself was never made durable
            }
            let mut img = f.durable.clone();
            let mut survivors: Vec<Pending> = match persist {
                Persist::None => Vec::new(),
                Persist::All => f.pending.clone(),
                Persist::Seeded(_) => {
                    let mut out = Vec::new();
                    for p in &f.pending {
                        if !xorshift(&mut rng).is_multiple_of(2) {
                            continue;
                        }
                        let sectors = p.data.len().div_ceil(SECTOR) as u64;
                        let keep = if sectors > 1 && xorshift(&mut rng).is_multiple_of(3) {
                            (xorshift(&mut rng) % sectors) as usize * SECTOR
                        } else {
                            p.data.len()
                        };
                        out.push(Pending {
                            offset: p.offset,
                            data: p.data[..keep].to_vec(),
                        });
                    }
                    out
                }
            };
            if let Persist::Seeded(_) = persist {
                for i in (1..survivors.len()).rev() {
                    let j = (xorshift(&mut rng) % (i as u64 + 1)) as usize;
                    survivors.swap(i, j);
                }
            }
            for p in &survivors {
                put(&mut img, p.offset, &p.data);
            }
            files.insert(
                id,
                SimFile {
                    durable: img.clone(),
                    volatile: img,
                    pending: Vec::new(),
                },
            );
        }
        SimIo {
            st: Arc::new(Mutex::new(State {
                durable_entries: files.keys().copied().collect(),
                files,
                ..State::default()
            })),
        }
    }

    /// The state a fresh process sees after the owner exits without a power loss: the operating
    /// system still holds unsynced writes (readable, but still not durable), and the lock is free.
    pub fn after_process_exit(&self) -> SimIo {
        let st = self.st.lock().expect("sim");
        SimIo {
            st: Arc::new(Mutex::new(State {
                files: st.files.clone(),
                durable_entries: st.durable_entries.clone(),
                ..State::default()
            })),
        }
    }

    /// Shortens a file's durable and volatile images (test setup for damaged stores).
    pub fn truncate_for_test(&self, file: FileId, len: usize) {
        let mut st = self.st.lock().expect("sim");
        if let Some(f) = st.files.get_mut(&file) {
            f.durable.truncate(len);
            f.volatile.truncate(len);
        }
    }

    /// Releases the simulated ownership lock (the owning "process" ended).
    pub fn release(&self) {
        self.st.lock().expect("sim").locked = false;
    }

    fn mutate<T>(&self, f: impl FnOnce(&mut State) -> T) -> Result<T, StorageError> {
        let mut st = self.st.lock().expect("sim");
        if st.crashed {
            return Err(StorageError::Io("simulated crash".into()));
        }
        if let Some(b) = st.budget {
            if b == 0 {
                st.crashed = true;
                return Err(StorageError::Io("simulated crash".into()));
            }
            st.budget = Some(b - 1);
        }
        st.ops += 1;
        if st.fail_op == Some(st.ops) {
            return Err(StorageError::Io(format!(
                "simulated I/O error at op {}",
                st.ops
            )));
        }
        Ok(f(&mut st))
    }
}

impl Io for SimIo {
    fn create(&self) -> Result<(), StorageError> {
        let mut st = self.st.lock().expect("sim");
        if !st.files.is_empty() {
            return Err(StorageError::StoreState(
                "simulated directory already contains a store".into(),
            ));
        }
        if st.locked {
            return Err(StorageError::Busy("simulated store is owned".into()));
        }
        st.locked = true;
        st.files.insert(FileId::Meta, SimFile::default());
        st.files.insert(FileId::Content, SimFile::default());
        Ok(())
    }

    fn open(&self) -> Result<(), StorageError> {
        let mut st = self.st.lock().expect("sim");
        if st.locked {
            return Err(StorageError::Busy("simulated store is owned".into()));
        }
        for f in [FileId::Meta, FileId::Content] {
            if !st.files.contains_key(&f) {
                return Err(StorageError::StoreState(format!(
                    "required store file {} is missing",
                    f.name()
                )));
            }
        }
        st.locked = true;
        Ok(())
    }

    fn read_at(&self, file: FileId, offset: u64, buf: &mut [u8]) -> Result<(), StorageError> {
        let mut st = self.st.lock().expect("sim");
        if std::mem::take(&mut st.fail_next_read) {
            buf.fill(0);
            return Err(StorageError::Io("simulated read error".into()));
        }
        let f = st
            .files
            .get(&file)
            .ok_or_else(|| StorageError::Io("no such file".into()))?;
        let end = offset as usize + buf.len();
        if end > f.volatile.len() {
            return Err(StorageError::Integrity(format!(
                "{}: read past end at {offset}+{}",
                file.name(),
                buf.len()
            )));
        }
        buf.copy_from_slice(&f.volatile[offset as usize..end]);
        Ok(())
    }

    fn write_at(&self, file: FileId, offset: u64, data: &[u8]) -> Result<(), StorageError> {
        self.mutate(|st| {
            let f = st.files.entry(file).or_default();
            put(&mut f.volatile, offset, data);
            f.pending.push(Pending {
                offset,
                data: data.to_vec(),
            });
        })
    }

    fn len(&self, file: FileId) -> Result<u64, StorageError> {
        let st = self.st.lock().expect("sim");
        Ok(st.files.get(&file).map_or(0, |f| f.volatile.len() as u64))
    }

    fn sync(&self, file: FileId) -> Result<(), StorageError> {
        self.mutate(|st| {
            if let Some(f) = st.files.get_mut(&file) {
                f.durable = f.volatile.clone();
                f.pending.clear();
            }
        })
    }

    fn sync_dir(&self) -> Result<(), StorageError> {
        self.mutate(|st| {
            let ids: Vec<FileId> = st.files.keys().copied().collect();
            st.durable_entries.extend(ids);
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn read(io: &SimIo, at: u64, n: usize) -> Option<Vec<u8>> {
        let mut b = vec![0; n];
        io.read_at(FileId::Meta, at, &mut b).ok().map(|()| b)
    }

    #[test]
    fn skipping_a_barrier_can_lose_data_and_syncing_preserves_it() {
        let io = SimIo::new();
        io.create().unwrap();
        io.sync_dir().unwrap();
        io.write_at(FileId::Meta, 0, b"synced").unwrap();
        io.sync(FileId::Meta).unwrap();
        io.write_at(FileId::Meta, 6, b"unsynced").unwrap();
        let lost = io.after_crash(Persist::None);
        assert_eq!(read(&lost, 0, 6).as_deref(), Some(&b"synced"[..]));
        assert_eq!(
            read(&lost, 6, 8),
            None,
            "unsynced write survived a crash that should drop it"
        );
        let kept = io.after_crash(Persist::All);
        assert_eq!(read(&kept, 6, 8).as_deref(), Some(&b"unsynced"[..]));
    }

    #[test]
    fn file_existence_needs_a_directory_barrier() {
        let io = SimIo::new();
        io.create().unwrap();
        io.write_at(FileId::Meta, 0, b"x").unwrap();
        io.sync(FileId::Meta).unwrap();
        let img = io.after_crash(Persist::All);
        assert!(
            img.open().is_err(),
            "files without a durable directory entry survived"
        );
    }

    #[test]
    fn seeded_crashes_are_reproducible_and_explore_subsets() {
        let io = SimIo::new();
        io.create().unwrap();
        io.sync_dir().unwrap();
        for i in 0..8u64 {
            io.write_at(FileId::Meta, i * 2048, &[i as u8 + 1; 2048])
                .unwrap();
        }
        let img = |s| {
            let c = io.after_crash(Persist::Seeded(s));
            let mut b = vec![0; c.len(FileId::Meta).unwrap() as usize];
            c.read_at(FileId::Meta, 0, &mut b).unwrap();
            b
        };
        assert_eq!(img(42), img(42), "same seed must reproduce the same image");
        let distinct: HashSet<Vec<u8>> = (1..40).map(img).collect();
        assert!(
            distinct.len() > 5,
            "seeded crashes explored only {} images",
            distinct.len()
        );
    }

    #[test]
    fn a_second_owner_is_refused_until_release() {
        let io = SimIo::new();
        io.create().unwrap();
        assert!(matches!(io.open(), Err(StorageError::Busy(_))));
        io.release();
        io.open().unwrap();
    }
}
