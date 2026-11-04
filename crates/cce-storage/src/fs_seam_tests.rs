//! The real filesystem adapter under injected interruptions, short transfers and errors. Faults
//! are injected below the adapter's file handles, on real files, so the adapter's own handling of
//! each positional call is what runs.

use std::io::Read;
use std::path::PathBuf;
use std::sync::Arc;

use cce_core::storage::{
    AttemptId, AttemptOutcome, Change, ChangeSet, CommitError, ContentBinding, ContextId,
    Precondition, ReadView, StorageEngine, StorageError,
};

use crate::NativeStore;
use crate::engine::Inner;
use crate::format::StoreId;
use crate::io::{CONTENT_FILE, Call, Fault, Faults, FileId, FsIo, Injector, Io, SeamCall};

const ENOSPC: i32 = 28;
const EIO: i32 = 5;

/// A fresh directory under the system temporary directory, removed when dropped.
struct Scratch(PathBuf);

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A scratch directory and the store path inside it.
fn scratch(name: &str) -> (Scratch, PathBuf) {
    let d = std::env::temp_dir().join(format!("cce-fs-seam-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    let store = d.join("store");
    (Scratch(d), store)
}

fn pattern(n: usize, seed: u8) -> Vec<u8> {
    (0..n)
        .map(|i| (i as u8).wrapping_mul(29).wrapping_add(seed))
        .collect()
}

/// For every read and write: interrupted first, then a 7-byte transfer, then a whole one, in turn.
fn interrupt_and_shorten() -> Injector {
    let mut n = 0u64;
    Box::new(move |_, call, _| {
        if call == Call::Sync {
            return None;
        }
        n += 1;
        match n % 3 {
            0 => Some(Fault::Interrupted),
            1 => Some(Fault::Short(7)),
            _ => None,
        }
    })
}

fn injected(faults: &Faults, call: Call, fault: Fault) -> usize {
    faults
        .calls
        .lock()
        .unwrap()
        .iter()
        .filter(|(_, c, _, f)| *c == call && *f == Some(fault))
        .count()
}

fn calls(faults: &Faults) -> Vec<SeamCall> {
    faults.calls.lock().unwrap().clone()
}

#[test]
fn interrupted_and_short_transfers_still_write_and_read_every_byte() {
    let (_dir, store) = scratch("retry");
    let faults = Arc::new(Faults::default());
    let io = FsIo::with_faults(&store, faults.clone());
    io.create().unwrap();
    let data = pattern(10_000, 3);
    // Every call is interrupted or moves at most 7 bytes: the adapter must keep going.
    let mut n = 0u64;
    faults.set(Box::new(move |_, _, _| {
        n += 1;
        Some(if n.is_multiple_of(2) {
            Fault::Interrupted
        } else {
            Fault::Short(7)
        })
    }));
    io.write_at(FileId::Content, 4096, &data).unwrap();
    let mut got = vec![0; data.len()];
    io.read_at(FileId::Content, 4096, &mut got).unwrap();
    faults.clear();
    // The faults happened, on both kinds of call.
    for call in [Call::Read, Call::Write] {
        assert!(
            injected(&faults, call, Fault::Interrupted) > 100,
            "{call:?}"
        );
        assert!(injected(&faults, call, Fault::Short(7)) > 100, "{call:?}");
    }
    assert_eq!(got, data, "read through the adapter");
    let on_disk = std::fs::read(store.join(CONTENT_FILE)).unwrap();
    assert_eq!(on_disk.len(), 4096 + data.len());
    assert_eq!(&on_disk[4096..], &data[..], "bytes on disk");
}

#[test]
fn a_permanent_write_error_is_reported_after_a_short_write_and_not_retried() {
    let (_dir, store) = scratch("enospc");
    let faults = Arc::new(Faults::default());
    let io = FsIo::with_faults(&store, faults.clone());
    io.create().unwrap();
    let data = pattern(1000, 5);
    let mut n = 0;
    faults.set(Box::new(move |_, _, _| {
        n += 1;
        match n {
            1 => Some(Fault::Short(100)),
            _ => Some(Fault::Os(ENOSPC)),
        }
    }));
    let err = io.write_at(FileId::Content, 0, &data);
    faults.clear();
    match &err {
        Err(StorageError::Io(m)) => {
            assert!(m.contains(CONTENT_FILE) && m.contains("os error 28"), "{m}")
        }
        other => panic!("a full disk must fail the write: {other:?}"),
    }
    // One short call, one failing call, and no retry of the failure.
    let c = calls(&faults);
    assert_eq!(c.len(), 2, "{c:?}");
    assert_eq!((c[1].1, c[1].2), (Call::Write, 900), "{c:?}");
    // The file holds exactly what was written before the error.
    assert_eq!(
        std::fs::read(store.join(CONTENT_FILE)).unwrap(),
        &data[..100]
    );
}

#[test]
fn a_permanent_read_error_is_reported_and_not_retried() {
    let (_dir, store) = scratch("eio");
    let faults = Arc::new(Faults::default());
    let io = FsIo::with_faults(&store, faults.clone());
    io.create().unwrap();
    io.write_at(FileId::Meta, 0, &pattern(500, 1)).unwrap();
    faults.set(Box::new(|_, _, _| Some(Fault::Os(EIO))));
    let mut buf = vec![0; 500];
    let err = io.read_at(FileId::Meta, 0, &mut buf);
    faults.clear();
    match &err {
        Err(StorageError::Io(m)) => assert!(m.contains("os error 5"), "{m}"),
        other => panic!("an I/O error must fail the read: {other:?}"),
    }
    assert_eq!(calls(&faults).len(), 1, "the failed read was retried");
}

#[test]
fn a_transfer_that_makes_no_progress_fails_instead_of_looping_or_succeeding() {
    let (_dir, store) = scratch("zero");
    let faults = Arc::new(Faults::default());
    let io = FsIo::with_faults(&store, faults.clone());
    io.create().unwrap();
    io.write_at(FileId::Meta, 0, &pattern(500, 1)).unwrap();
    // A write that transfers nothing after a partial one.
    let mut n = 0;
    faults.set(Box::new(move |_, _, _| {
        n += 1;
        Some(if n == 1 {
            Fault::Short(10)
        } else {
            Fault::Zero
        })
    }));
    let err = io.write_at(FileId::Meta, 0, &pattern(500, 2));
    assert!(matches!(&err, Err(StorageError::Io(_))), "{err:?}");
    assert_eq!(calls(&faults).len(), 2);
    // A read that reaches the end early: past the end of the file, as a real short file would.
    faults.calls.lock().unwrap().clear();
    let mut n = 0;
    faults.set(Box::new(move |_, _, _| {
        n += 1;
        Some(if n == 1 {
            Fault::Short(10)
        } else {
            Fault::Zero
        })
    }));
    let mut buf = vec![0; 500];
    let err = io.read_at(FileId::Meta, 0, &mut buf);
    faults.clear();
    match &err {
        Err(StorageError::Integrity(m)) => assert!(m.contains("read past end"), "{m}"),
        other => panic!("{other:?}"),
    }
    assert_eq!(calls(&faults).len(), 2);
}

#[test]
fn an_interrupted_sync_is_reported_not_retried() {
    let (_dir, store) = scratch("sync");
    let faults = Arc::new(Faults::default());
    let io = FsIo::with_faults(&store, faults.clone());
    io.create().unwrap();
    faults.set(Box::new(|_, call, _| {
        (call == Call::Sync).then_some(Fault::Interrupted)
    }));
    let err = io.sync(FileId::Meta);
    faults.clear();
    assert!(matches!(&err, Err(StorageError::Io(_))), "{err:?}");
    assert_eq!(calls(&faults).len(), 1, "the interrupted sync was retried");
}

fn put(id: &str, body: &[u8], content: Option<usize>) -> Change {
    Change::PutContext {
        id: ContextId::new(id.as_bytes().to_vec()).unwrap(),
        expect: Precondition::Any,
        body: body.to_vec(),
        content,
    }
}

/// Body of `id` and, if bound, its whole content.
fn read(s: &NativeStore, id: &str) -> Option<(Vec<u8>, Option<Vec<u8>>)> {
    let mut v = s.begin().unwrap();
    let r = v
        .context(&ContextId::new(id.as_bytes().to_vec()).unwrap())
        .unwrap()?;
    let content = r.content.map(|c| {
        let mut out = Vec::new();
        v.content(&c).unwrap().read_to_end(&mut out).unwrap();
        out
    });
    Some((r.body, content))
}

#[test]
fn a_store_on_the_real_adapter_commits_and_reads_through_interruptions_and_short_transfers() {
    let (_dir, store) = scratch("engine-retry");
    let faults = Arc::new(Faults::default());
    let s = NativeStore {
        inner: Inner::create(
            Box::new(FsIo::with_faults(&store, faults.clone())),
            StoreId(*b"fs-seam-test-sto"),
            Default::default(),
        )
        .unwrap(),
    };
    let blob = pattern(200_000, 9);
    faults.set(interrupt_and_shorten());
    let staged = s.stage_content(&mut &blob[..]).unwrap();
    s.commit(
        AttemptId::new(*b"a").unwrap(),
        s.begin().unwrap(),
        ChangeSet {
            changes: vec![put("A", b"alpha", Some(0)), put("B", b"beta", None)],
            content: vec![ContentBinding::from(&staged)],
            ..ChangeSet::default()
        },
    )
    .unwrap();
    let a = read(&s, "A");
    faults.clear();
    for call in [Call::Read, Call::Write] {
        assert!(injected(&faults, call, Fault::Interrupted) > 0, "{call:?}");
        assert!(injected(&faults, call, Fault::Short(7)) > 0, "{call:?}");
    }
    assert_eq!(a, Some((b"alpha".to_vec(), Some(blob.clone()))));
    drop((staged, s));

    // A fresh adapter without faults reads the same store.
    let s = NativeStore::open(&store).unwrap();
    assert_eq!(read(&s, "A"), Some((b"alpha".to_vec(), Some(blob))));
    assert_eq!(read(&s, "B"), Some((b"beta".to_vec(), None)));
}

#[test]
fn a_full_disk_under_the_real_adapter_fails_the_commit_and_leaves_the_store_usable() {
    let (_dir, store) = scratch("engine-enospc");
    let faults = Arc::new(Faults::default());
    let s = NativeStore {
        inner: Inner::create(
            Box::new(FsIo::with_faults(&store, faults.clone())),
            StoreId(*b"fs-seam-test-sto"),
            Default::default(),
        )
        .unwrap(),
    };
    let commit = |s: &NativeStore, attempt: &[u8], id: &str| {
        s.commit(
            AttemptId::new(attempt.to_vec()).unwrap(),
            s.begin().unwrap(),
            ChangeSet {
                changes: vec![put(id, id.as_bytes(), None)],
                ..ChangeSet::default()
            },
        )
    };
    commit(&s, b"a", "A").unwrap();
    // The first metadata page write of the next commit is cut short, then the disk is full.
    let mut n = 0;
    faults.set(Box::new(move |file, call, _| {
        if file != FileId::Meta || call != Call::Write {
            return None;
        }
        n += 1;
        Some(if n == 1 {
            Fault::Short(1000)
        } else {
            Fault::Os(ENOSPC)
        })
    }));
    let r = commit(&s, b"b", "B");
    faults.clear();
    match &r {
        Err(CommitError::Rejected(StorageError::Io(m))) => {
            assert!(m.contains("os error 28"), "{m}")
        }
        other => panic!("a full disk must reject the commit: {other:?}"),
    }
    assert!(injected(&faults, Call::Write, Fault::Os(ENOSPC)) == 1);
    assert_eq!(
        s.outcome(&AttemptId::new(*b"b").unwrap()).unwrap(),
        AttemptOutcome::Absent
    );
    assert_eq!(read(&s, "B"), None);
    // With space again, the same process commits.
    commit(&s, b"c", "C").unwrap();
    drop(s);
    let s = NativeStore::open(&store).unwrap();
    assert!(read(&s, "A").is_some() && read(&s, "C").is_some());
    assert_eq!(read(&s, "B"), None);
}
