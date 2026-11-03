//! The physical-I/O boundary. Every byte the engine persists goes through [`Io`].
//!
//! [`FsIo`] is the real filesystem adapter. The simulator (`sim`) implements the same trait for
//! crash testing; both run the same engine code.

use std::fs::{self, File, OpenOptions, TryLockError};
use std::io::ErrorKind;
use std::os::unix::fs::{DirBuilderExt, FileExt, MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use cce_core::storage::StorageError;

pub(crate) const META_FILE: &str = "meta.cce";
pub(crate) const CONTENT_FILE: &str = "content.cce";
pub(crate) const LOCK_FILE: &str = "LOCK";

/// A store file.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum FileId {
    Meta,
    Content,
}

impl FileId {
    pub fn name(self) -> &'static str {
        match self {
            FileId::Meta => META_FILE,
            FileId::Content => CONTENT_FILE,
        }
    }
}

#[cfg(test)]
/// One traced physical operation, normalized (no descriptors, timings or syscall counts).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Op {
    Write { file: FileId, offset: u64, len: u64 },
    Sync { file: FileId },
    SyncDir,
}

/// Physical operations the engine needs. Implementations must not reorder or drop the
/// engine's requests; durability is only what `sync`/`sync_dir` established.
pub(crate) trait Io: Send + Sync {
    /// Creates the store directory entries and takes exclusive ownership. Refuses if a store
    /// (any store data file) already exists.
    fn create(&self) -> Result<(), StorageError>;
    /// Opens an existing store and takes exclusive ownership. Missing files are an error.
    fn open(&self) -> Result<(), StorageError>;
    /// Opens an existing store for reading only, under a shared lock: refused while an owner
    /// holds the store, and holding it keeps owners out. The handle then refuses writes and
    /// syncs. Never creates anything. Adapters without a read-only mode refuse.
    fn open_read_only(&self) -> Result<(), StorageError> {
        Err(StorageError::Unsupported(
            "read-only open is not supported by this I/O adapter".into(),
        ))
    }
    /// Reads exactly `buf.len()` bytes at `offset`.
    fn read_at(&self, file: FileId, offset: u64, buf: &mut [u8]) -> Result<(), StorageError>;
    /// Writes all of `data` at `offset` (positioned; never append).
    fn write_at(&self, file: FileId, offset: u64, data: &[u8]) -> Result<(), StorageError>;
    fn len(&self, file: FileId) -> Result<u64, StorageError>;
    /// Makes previously written data of `file` durable.
    fn sync(&self, file: FileId) -> Result<(), StorageError>;
    /// Makes the store directory's entries durable.
    fn sync_dir(&self) -> Result<(), StorageError>;
}

#[cfg(test)]
/// Records the normalized operation sequence of any adapter.
pub(crate) struct Traced<I> {
    pub inner: I,
    pub trace: Arc<Mutex<Vec<Op>>>,
}

#[cfg(test)]
impl<I: Io> Io for Traced<I> {
    fn create(&self) -> Result<(), StorageError> {
        self.inner.create()
    }
    fn open(&self) -> Result<(), StorageError> {
        self.inner.open()
    }
    fn open_read_only(&self) -> Result<(), StorageError> {
        self.inner.open_read_only()
    }
    fn read_at(&self, file: FileId, offset: u64, buf: &mut [u8]) -> Result<(), StorageError> {
        self.inner.read_at(file, offset, buf)
    }
    fn write_at(&self, file: FileId, offset: u64, data: &[u8]) -> Result<(), StorageError> {
        self.trace.lock().expect("trace").push(Op::Write {
            file,
            offset,
            len: data.len() as u64,
        });
        self.inner.write_at(file, offset, data)
    }
    fn len(&self, file: FileId) -> Result<u64, StorageError> {
        self.inner.len(file)
    }
    fn sync(&self, file: FileId) -> Result<(), StorageError> {
        self.trace.lock().expect("trace").push(Op::Sync { file });
        self.inner.sync(file)
    }
    fn sync_dir(&self) -> Result<(), StorageError> {
        self.trace.lock().expect("trace").push(Op::SyncDir);
        self.inner.sync_dir()
    }
}

impl<I: Io + ?Sized> Io for Box<I> {
    fn create(&self) -> Result<(), StorageError> {
        (**self).create()
    }
    fn open(&self) -> Result<(), StorageError> {
        (**self).open()
    }
    fn open_read_only(&self) -> Result<(), StorageError> {
        (**self).open_read_only()
    }
    fn read_at(&self, file: FileId, offset: u64, buf: &mut [u8]) -> Result<(), StorageError> {
        (**self).read_at(file, offset, buf)
    }
    fn write_at(&self, file: FileId, offset: u64, data: &[u8]) -> Result<(), StorageError> {
        (**self).write_at(file, offset, data)
    }
    fn len(&self, file: FileId) -> Result<u64, StorageError> {
        (**self).len(file)
    }
    fn sync(&self, file: FileId) -> Result<(), StorageError> {
        (**self).sync(file)
    }
    fn sync_dir(&self) -> Result<(), StorageError> {
        (**self).sync_dir()
    }
}

fn io_err(what: &str, e: std::io::Error) -> StorageError {
    StorageError::Io(format!("{what}: {e}"))
}

struct Files {
    meta: File,
    content: File,
    /// Held for the life of the adapter; the OS lock is released when it closes.
    _lock: File,
    /// Opened for reading only, under a shared lock: writes and syncs are refused.
    read_only: bool,
}

impl Files {
    fn writable(&self) -> Result<(), StorageError> {
        if self.read_only {
            return Err(StorageError::StoreState(
                "read-only store handle: writes and syncs are refused".into(),
            ));
        }
        Ok(())
    }
}

/// Real filesystem adapter for one store directory.
pub(crate) struct FsIo {
    dir: PathBuf,
    files: Mutex<Option<Arc<Files>>>,
    /// Directories this adapter synced, in order (test observation only).
    #[cfg(test)]
    synced_dirs: Mutex<Vec<PathBuf>>,
}

impl FsIo {
    pub fn new(dir: &Path) -> Self {
        Self {
            dir: dir.to_path_buf(),
            files: Mutex::new(None),
            #[cfg(test)]
            synced_dirs: Mutex::new(Vec::new()),
        }
    }

    fn files(&self) -> Result<Arc<Files>, StorageError> {
        self.files
            .lock()
            .expect("files")
            .clone()
            .ok_or_else(|| StorageError::StoreState("store files not open".into()))
    }

    fn file(&self, f: FileId) -> Result<(Arc<Files>, FileId), StorageError> {
        Ok((self.files()?, f))
    }

    fn take_lock(&self) -> Result<File, StorageError> {
        let path = self.dir.join(LOCK_FILE);
        match refuse_non_regular(&path) {
            // A missing lock file is created below.
            Err(StorageError::StoreState(m)) if m.ends_with("does not exist") => {}
            other => other?,
        }
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .open(&path)
            .map_err(|e| io_err("open lock file", e))?;
        same_regular_file(&path, &lock)?;
        match lock.try_lock() {
            Ok(()) => Ok(lock),
            Err(TryLockError::WouldBlock) => Err(StorageError::Busy(format!(
                "store {} is owned by another process",
                self.dir.display()
            ))),
            Err(TryLockError::Error(e)) => Err(io_err("lock store", e)),
        }
    }

    /// Takes the ownership lock in shared mode for a read-only open. The lock file must already
    /// exist: a read-only open creates nothing, and without the file it cannot keep owners out.
    fn take_shared_lock(&self) -> Result<File, StorageError> {
        let path = self.dir.join(LOCK_FILE);
        match refuse_non_regular(&path) {
            Err(StorageError::StoreState(m)) if m.ends_with("does not exist") => {
                return Err(StorageError::StoreState(format!(
                    "{} has no {LOCK_FILE} file; a read-only open cannot keep owners out without \
                     it (a read-write open creates it)",
                    self.dir.display()
                )));
            }
            other => other?,
        }
        let lock = File::open(&path).map_err(|e| io_err("open lock file", e))?;
        same_regular_file(&path, &lock)?;
        match lock.try_lock_shared() {
            Ok(()) => Ok(lock),
            Err(TryLockError::WouldBlock) => Err(StorageError::Busy(format!(
                "store {} is owned by another process",
                self.dir.display()
            ))),
            Err(TryLockError::Error(e)) => Err(io_err("lock store", e)),
        }
    }

    /// Opens the existing store files with ownership: exclusive for read-write, shared for
    /// read-only (files opened without write access).
    fn open_files(&self, read_only: bool) -> Result<(), StorageError> {
        if !self.dir.is_dir() {
            return Err(StorageError::StoreState(format!(
                "no store directory at {}",
                self.dir.display()
            )));
        }
        let lock = if read_only {
            self.take_shared_lock()?
        } else {
            self.take_lock()?
        };
        let existing = |name: &str| match refuse_non_regular(&self.dir.join(name)).and_then(|()| {
            OpenOptions::new()
                .read(true)
                .write(!read_only)
                .open(self.dir.join(name))
                .map_err(|e| match e.kind() {
                    ErrorKind::NotFound => StorageError::StoreState(format!(
                        "required store file {name} is missing in {}",
                        self.dir.display()
                    )),
                    _ => io_err(name, e),
                })
        }) {
            Ok(f) => {
                same_regular_file(&self.dir.join(name), &f)?;
                Ok(f)
            }
            Err(StorageError::StoreState(m)) if m.contains("does not exist") => {
                Err(StorageError::StoreState(format!(
                    "required store file {name} is missing in {}",
                    self.dir.display()
                )))
            }
            Err(e) => Err(e),
        };
        let files = Files {
            meta: existing(META_FILE)?,
            content: existing(CONTENT_FILE)?,
            _lock: lock,
            read_only,
        };
        *self.files.lock().expect("files") = Some(Arc::new(files));
        Ok(())
    }
}

impl FsIo {
    /// Syncs a directory so its entries are durable.
    fn sync_path(&self, dir: &Path) -> Result<(), StorageError> {
        #[cfg(test)]
        self.synced_dirs
            .lock()
            .expect("synced")
            .push(dir.to_path_buf());
        File::open(dir)
            .and_then(|d| d.sync_all())
            .map_err(|e| io_err(&format!("sync directory {}", dir.display()), e))
    }
}

/// Refuses a path that exists but is not a regular file (for example a symlink or a FIFO), before
/// anything opens it. A missing path is left for the caller to report.
fn refuse_non_regular(path: &Path) -> Result<(), StorageError> {
    match fs::symlink_metadata(path) {
        Ok(m) if m.file_type().is_file() => Ok(()),
        Ok(_) => Err(StorageError::StoreState(format!(
            "{} is not a regular file; store files may not be symlinks or special files",
            path.display()
        ))),
        Err(e) if e.kind() == ErrorKind::NotFound => Err(StorageError::StoreState(format!(
            "{} does not exist",
            path.display()
        ))),
        Err(e) => Err(io_err("inspect store file", e)),
    }
}

/// Confirms that the opened handle is the regular file at `path` itself.
fn same_regular_file(path: &Path, f: &File) -> Result<(), StorageError> {
    let at = fs::symlink_metadata(path).map_err(|e| io_err("inspect store file", e))?;
    let opened = f.metadata().map_err(|e| io_err("inspect store file", e))?;
    if !at.file_type().is_file() || at.dev() != opened.dev() || at.ino() != opened.ino() {
        return Err(StorageError::StoreState(format!(
            "{} changed or is not a regular file; refusing it",
            path.display()
        )));
    }
    Ok(())
}

fn pick(files: &Files, f: FileId) -> &File {
    match f {
        FileId::Meta => &files.meta,
        FileId::Content => &files.content,
    }
}

impl Io for FsIo {
    fn create(&self) -> Result<(), StorageError> {
        // Directories this call creates, outermost first. Each new entry lives in its parent, so
        // every such parent must be synced or a crash can lose the whole store directory.
        let mut new_dirs = Vec::new();
        let mut p = self.dir.as_path();
        while !p.exists() {
            new_dirs.push(p.to_path_buf());
            match p.parent() {
                Some(q) if !q.as_os_str().is_empty() => p = q,
                _ => break,
            }
        }
        new_dirs.reverse();
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&self.dir)
            .map_err(|e| io_err("create store directory", e))?;
        for d in &new_dirs {
            let parent = d
                .parent()
                .filter(|q| !q.as_os_str().is_empty())
                .unwrap_or(Path::new("."));
            self.sync_path(parent)?;
        }
        let lock = self.take_lock()?;
        // Only an absent or empty directory (or one holding just the lock file, left by a creation
        // that stopped early) may receive a new store: never initialize beside existing data.
        let entries = fs::read_dir(&self.dir).map_err(|e| io_err("read store directory", e))?;
        for entry in entries {
            let name = entry
                .map_err(|e| io_err("read store directory", e))?
                .file_name();
            let name = name.to_string_lossy();
            if name == LOCK_FILE {
                continue;
            }
            let why = if name == META_FILE || name == CONTENT_FILE {
                "already contains a store"
            } else {
                "is not empty"
            };
            return Err(StorageError::StoreState(format!(
                "{} {why} ({name}); refusing to create a store there",
                self.dir.display()
            )));
        }
        let new = |name: &str| {
            let path = self.dir.join(name);
            let f = OpenOptions::new()
                .read(true)
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&path)
                .map_err(|e| io_err(name, e))?;
            same_regular_file(&path, &f)?;
            Ok::<File, StorageError>(f)
        };
        let files = Files {
            meta: new(META_FILE)?,
            content: new(CONTENT_FILE)?,
            _lock: lock,
            read_only: false,
        };
        *self.files.lock().expect("files") = Some(Arc::new(files));
        Ok(())
    }

    fn open(&self) -> Result<(), StorageError> {
        self.open_files(false)
    }

    fn open_read_only(&self) -> Result<(), StorageError> {
        self.open_files(true)
    }

    fn read_at(&self, file: FileId, offset: u64, buf: &mut [u8]) -> Result<(), StorageError> {
        let (files, f) = self.file(file)?;
        pick(&files, f)
            .read_exact_at(buf, offset)
            .map_err(|e| match e.kind() {
                ErrorKind::UnexpectedEof => StorageError::Integrity(format!(
                    "{}: read past end at {offset}+{}",
                    file.name(),
                    buf.len()
                )),
                _ => io_err(file.name(), e),
            })
    }

    fn write_at(&self, file: FileId, offset: u64, data: &[u8]) -> Result<(), StorageError> {
        let (files, f) = self.file(file)?;
        files.writable()?;
        pick(&files, f)
            .write_all_at(data, offset)
            .map_err(|e| io_err(file.name(), e))
    }

    fn len(&self, file: FileId) -> Result<u64, StorageError> {
        let (files, f) = self.file(file)?;
        Ok(pick(&files, f)
            .metadata()
            .map_err(|e| io_err(file.name(), e))?
            .len())
    }

    fn sync(&self, file: FileId) -> Result<(), StorageError> {
        let (files, f) = self.file(file)?;
        files.writable()?;
        pick(&files, f)
            .sync_all()
            .map_err(|e| io_err(file.name(), e))
    }

    fn sync_dir(&self) -> Result<(), StorageError> {
        if let Ok(files) = self.files() {
            files.writable()?;
        }
        self.sync_path(&self.dir.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn creating_a_store_makes_every_new_directory_entry_durable() {
        let base = std::env::temp_dir().join(format!("cce-dirsync-{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        fs::create_dir_all(&base).unwrap();
        let store = base.join("a").join("b").join("store");
        let io = FsIo::new(&store);
        io.create().unwrap();
        io.sync_dir().unwrap();
        let synced = io.synced_dirs.lock().unwrap().clone();
        // The entries for a, a/b and a/b/store live in their parents; each parent must be synced.
        for parent in [
            base.clone(),
            base.join("a"),
            base.join("a").join("b"),
            store.clone(),
        ] {
            assert!(
                synced.contains(&parent),
                "{} not synced; synced: {synced:?}",
                parent.display()
            );
        }
        // Positive neighbour: an existing directory's parent is not required to change.
        let _ = fs::remove_dir_all(&base);
    }

    fn scratch(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("cce-io-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn opening_refuses_store_files_that_are_symlinks() {
        let base = scratch("symlink");
        let store = base.join("store");
        let io = FsIo::new(&store);
        io.create().unwrap();
        drop(io);
        // Positive neighbour: the untouched store opens.
        FsIo::new(&store).open().unwrap();
        let outside = base.join("victim");
        fs::write(&outside, b"must not change").unwrap();
        fs::remove_file(store.join(CONTENT_FILE)).unwrap();
        std::os::unix::fs::symlink(&outside, store.join(CONTENT_FILE)).unwrap();
        let err = FsIo::new(&store)
            .open()
            .expect_err("symlinked store file accepted");
        assert!(
            matches!(
                err,
                StorageError::StoreState(_) | StorageError::Integrity(_)
            ),
            "{err:?}"
        );
        assert_eq!(fs::read(&outside).unwrap(), b"must not change");
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn new_store_files_are_private_to_their_owner() {
        use std::os::unix::fs::PermissionsExt;
        let base = scratch("perms");
        let store = base.join("store");
        FsIo::new(&store).create().unwrap();
        let mode = |p: &Path| fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&store), 0o700, "store directory mode");
        for f in [META_FILE, CONTENT_FILE, LOCK_FILE] {
            assert_eq!(mode(&store.join(f)), 0o600, "{f} mode");
        }
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn creating_refuses_a_directory_that_already_holds_other_files() {
        let names = |d: &PathBuf| {
            let mut v: Vec<String> = fs::read_dir(d)
                .unwrap()
                .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
                .collect();
            v.sort();
            v
        };
        // Files of an older layout, or anything else, must never be silently joined by a new store.
        for stray in ["meta.old", "notes.txt"] {
            let store = scratch(&format!("create-stray-{stray}"));
            fs::create_dir_all(&store).unwrap();
            fs::write(store.join(stray), b"x").unwrap();
            let before = names(&store);
            let err = FsIo::new(&store).create().unwrap_err();
            assert!(
                matches!(&err, StorageError::StoreState(m) if m.contains("not empty")),
                "{stray}: {err:?}"
            );
            let after: Vec<String> = names(&store)
                .into_iter()
                .filter(|n| n != LOCK_FILE)
                .collect();
            assert_eq!(after, before, "{stray}: nothing created beside it");
        }
        // Positive neighbours: an existing empty directory, and one left with only the lock file
        // by a creation that stopped before writing any store file.
        let empty = scratch("create-empty-dir");
        fs::create_dir_all(&empty).unwrap();
        FsIo::new(&empty).create().unwrap();
        let lock_only = scratch("create-lock-only");
        fs::create_dir_all(&lock_only).unwrap();
        fs::write(lock_only.join(LOCK_FILE), b"").unwrap();
        FsIo::new(&lock_only).create().unwrap();
    }
}
