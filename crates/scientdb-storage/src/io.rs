//! The physical-I/O boundary. Every byte the engine persists goes through [`Io`].
//!
//! [`FsIo`] is the real filesystem adapter. The simulator (`sim`) implements the same trait for
//! crash testing; both run the same engine code.

use std::fs::{self, File, OpenOptions, TryLockError};
use std::io::ErrorKind;
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use scientdb_kernel::storage::StorageError;

pub(crate) const META_FILE: &str = "meta.sdb";
pub(crate) const CONTENT_FILE: &str = "content.sdb";
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
}

/// Real filesystem adapter for one store directory.
pub(crate) struct FsIo {
    dir: PathBuf,
    files: Mutex<Option<Arc<Files>>>,
}

impl FsIo {
    pub fn new(dir: &Path) -> Self {
        Self {
            dir: dir.to_path_buf(),
            files: Mutex::new(None),
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
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(self.dir.join(LOCK_FILE))
            .map_err(|e| io_err("open lock file", e))?;
        match lock.try_lock() {
            Ok(()) => Ok(lock),
            Err(TryLockError::WouldBlock) => Err(StorageError::Busy(format!(
                "store {} is owned by another process",
                self.dir.display()
            ))),
            Err(TryLockError::Error(e)) => Err(io_err("lock store", e)),
        }
    }
}

fn pick(files: &Files, f: FileId) -> &File {
    match f {
        FileId::Meta => &files.meta,
        FileId::Content => &files.content,
    }
}

impl Io for FsIo {
    fn create(&self) -> Result<(), StorageError> {
        fs::create_dir_all(&self.dir).map_err(|e| io_err("create store directory", e))?;
        let lock = self.take_lock()?;
        for name in [META_FILE, CONTENT_FILE] {
            if self.dir.join(name).exists() {
                return Err(StorageError::StoreState(format!(
                    "{} already contains a store ({name}); refusing to create over it",
                    self.dir.display()
                )));
            }
        }
        let new = |name: &str| {
            OpenOptions::new()
                .read(true)
                .write(true)
                .create_new(true)
                .open(self.dir.join(name))
                .map_err(|e| io_err(name, e))
        };
        let files = Files {
            meta: new(META_FILE)?,
            content: new(CONTENT_FILE)?,
            _lock: lock,
        };
        *self.files.lock().expect("files") = Some(Arc::new(files));
        Ok(())
    }

    fn open(&self) -> Result<(), StorageError> {
        if !self.dir.is_dir() {
            return Err(StorageError::StoreState(format!(
                "no store directory at {}",
                self.dir.display()
            )));
        }
        let lock = self.take_lock()?;
        let existing = |name: &str| match OpenOptions::new()
            .read(true)
            .write(true)
            .open(self.dir.join(name))
        {
            Ok(f) => Ok(f),
            Err(e) if e.kind() == ErrorKind::NotFound => Err(StorageError::StoreState(format!(
                "required store file {name} is missing in {}",
                self.dir.display()
            ))),
            Err(e) => Err(io_err(name, e)),
        };
        let files = Files {
            meta: existing(META_FILE)?,
            content: existing(CONTENT_FILE)?,
            _lock: lock,
        };
        *self.files.lock().expect("files") = Some(Arc::new(files));
        Ok(())
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
        pick(&files, f)
            .sync_all()
            .map_err(|e| io_err(file.name(), e))
    }

    fn sync_dir(&self) -> Result<(), StorageError> {
        File::open(&self.dir)
            .and_then(|d| d.sync_all())
            .map_err(|e| io_err("sync store directory", e))
    }
}
