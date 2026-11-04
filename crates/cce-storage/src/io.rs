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

const LEAF_OPEN_FLAGS: i32 = libc::O_NOFOLLOW | libc::O_NONBLOCK;
const DIRECTORY_OPEN_FLAGS: i32 = libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_NONBLOCK;

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
    dir: File,
    meta: File,
    content: File,
    /// Held for the life of the adapter; the OS lock is released when it closes.
    _lock: File,
    /// Opened for reading only, under shared locks: writes and syncs are refused.
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

/// The positional calls the adapter makes on a store file. Each may transfer fewer bytes than
/// asked or be interrupted; [`read_full`] and [`write_full`] handle that.
trait Positional {
    fn pread(&self, buf: &mut [u8], offset: u64) -> std::io::Result<usize>;
    fn pwrite(&self, data: &[u8], offset: u64) -> std::io::Result<usize>;
    fn fsync(&self) -> std::io::Result<()>;
}

impl Positional for File {
    fn pread(&self, buf: &mut [u8], offset: u64) -> std::io::Result<usize> {
        FileExt::read_at(self, buf, offset)
    }
    fn pwrite(&self, data: &[u8], offset: u64) -> std::io::Result<usize> {
        FileExt::write_at(self, data, offset)
    }
    fn fsync(&self) -> std::io::Result<()> {
        self.sync_all()
    }
}

/// Reads all of `buf` at `offset`. A call interrupted by a signal is repeated and a short read
/// continues where it stopped; a call that reads nothing has reached the end of the file
/// (`UnexpectedEof`). Any other error is returned as it is, without retrying.
fn read_full(h: &dyn Positional, mut buf: &mut [u8], mut offset: u64) -> std::io::Result<()> {
    while !buf.is_empty() {
        match h.pread(buf, offset) {
            Ok(0) => return Err(ErrorKind::UnexpectedEof.into()),
            Ok(n) => {
                buf = &mut buf[n..];
                offset += n as u64;
            }
            Err(e) if e.kind() == ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

/// Writes all of `data` at `offset`. A call interrupted by a signal is repeated and a short write
/// continues where it stopped; a call that writes nothing is an error (`WriteZero`). Any other
/// error, such as a full disk, is returned as it is, without retrying; the bytes written before
/// it stay written.
fn write_full(h: &dyn Positional, mut data: &[u8], mut offset: u64) -> std::io::Result<()> {
    while !data.is_empty() {
        match h.pwrite(data, offset) {
            Ok(0) => return Err(ErrorKind::WriteZero.into()),
            Ok(n) => {
                data = &data[n..];
                offset += n as u64;
            }
            Err(e) if e.kind() == ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

/// A fault the test seam makes one positional call return instead of (or before) the real call.
#[cfg(test)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Fault {
    /// The call fails as interrupted by a signal before transferring anything.
    Interrupted,
    /// The real call is made for at most this many bytes.
    Short(usize),
    /// The call transfers nothing and reports zero bytes.
    Zero,
    /// The call fails with this operating-system error number.
    Os(i32),
}

/// Which positional call the seam is asked about.
#[cfg(test)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Call {
    Read,
    Write,
    Sync,
}

/// Decides, for each positional call (file, call, bytes asked), whether to inject a fault.
#[cfg(test)]
pub(crate) type Injector = Box<dyn FnMut(FileId, Call, usize) -> Option<Fault> + Send>;

/// One positional call seen by the seam: file, call, bytes asked and the fault injected, if any.
#[cfg(test)]
pub(crate) type SeamCall = (FileId, Call, usize, Option<Fault>);

/// The test seam under the adapter's file handles: every positional call is offered to the
/// injector first; calls it leaves alone, and the rest of a short one, go to the real file.
#[cfg(test)]
#[derive(Default)]
pub(crate) struct Faults {
    injector: Mutex<Option<Injector>>,
    /// Every positional call made while an injector was set, with the fault injected, if any.
    pub calls: Mutex<Vec<SeamCall>>,
}

#[cfg(test)]
impl Faults {
    pub fn set(&self, injector: Injector) {
        *self.injector.lock().expect("injector") = Some(injector);
    }

    pub fn clear(&self) {
        *self.injector.lock().expect("injector") = None;
    }

    fn decide(&self, file: FileId, call: Call, len: usize) -> Option<Fault> {
        let mut injector = self.injector.lock().expect("injector");
        let i = injector.as_mut()?;
        let fault = i(file, call, len);
        self.calls
            .lock()
            .expect("calls")
            .push((file, call, len, fault));
        fault
    }
}

/// A store file as the adapter uses it: the real file, under the test seam in test builds.
struct Handle<'a> {
    file: &'a File,
    #[cfg(test)]
    id: FileId,
    #[cfg(test)]
    faults: &'a Faults,
}

impl Positional for Handle<'_> {
    fn pread(&self, buf: &mut [u8], offset: u64) -> std::io::Result<usize> {
        #[cfg(test)]
        match self.faults.decide(self.id, Call::Read, buf.len()) {
            Some(Fault::Interrupted) => return Err(ErrorKind::Interrupted.into()),
            Some(Fault::Short(n)) => {
                let n = n.min(buf.len());
                return self.file.pread(&mut buf[..n], offset);
            }
            Some(Fault::Zero) => return Ok(0),
            Some(Fault::Os(e)) => return Err(std::io::Error::from_raw_os_error(e)),
            None => {}
        }
        self.file.pread(buf, offset)
    }

    fn pwrite(&self, data: &[u8], offset: u64) -> std::io::Result<usize> {
        #[cfg(test)]
        match self.faults.decide(self.id, Call::Write, data.len()) {
            Some(Fault::Interrupted) => return Err(ErrorKind::Interrupted.into()),
            Some(Fault::Short(n)) => return self.file.pwrite(&data[..n.min(data.len())], offset),
            Some(Fault::Zero) => return Ok(0),
            Some(Fault::Os(e)) => return Err(std::io::Error::from_raw_os_error(e)),
            None => {}
        }
        self.file.pwrite(data, offset)
    }

    fn fsync(&self) -> std::io::Result<()> {
        #[cfg(test)]
        match self.faults.decide(self.id, Call::Sync, 0) {
            Some(Fault::Interrupted) => return Err(ErrorKind::Interrupted.into()),
            Some(Fault::Os(e)) => return Err(std::io::Error::from_raw_os_error(e)),
            Some(Fault::Short(_) | Fault::Zero) | None => {}
        }
        self.file.fsync()
    }
}

#[cfg(test)]
type LeafOpenHook = Arc<dyn Fn(&Path) + Send + Sync>;

/// Real filesystem adapter for one store directory.
pub(crate) struct FsIo {
    dir: PathBuf,
    files: Mutex<Option<Arc<Files>>>,
    /// Directories this adapter synced, in order (test observation only).
    #[cfg(test)]
    synced_dirs: Mutex<Vec<PathBuf>>,
    /// The test seam under the file handles.
    #[cfg(test)]
    faults: Arc<Faults>,
    /// Runs after a leaf is classified and before it is opened.
    #[cfg(test)]
    leaf_open_hook: Mutex<Option<LeafOpenHook>>,
}

impl FsIo {
    pub fn new(dir: &Path) -> Self {
        Self {
            dir: dir.to_path_buf(),
            files: Mutex::new(None),
            #[cfg(test)]
            synced_dirs: Mutex::new(Vec::new()),
            #[cfg(test)]
            faults: Arc::default(),
            #[cfg(test)]
            leaf_open_hook: Mutex::new(None),
        }
    }

    /// An adapter whose positional calls go through `faults` first.
    #[cfg(test)]
    pub(crate) fn with_faults(dir: &Path, faults: Arc<Faults>) -> Self {
        Self {
            faults,
            ..Self::new(dir)
        }
    }

    /// An adapter that pauses a test between classifying and opening a store leaf.
    #[cfg(test)]
    fn with_leaf_open_hook(dir: &Path, hook: LeafOpenHook) -> Self {
        let io = Self::new(dir);
        *io.leaf_open_hook.lock().expect("leaf open hook") = Some(hook);
        io
    }

    #[cfg(test)]
    fn before_leaf_open(&self, path: &Path) {
        let hook = self.leaf_open_hook.lock().expect("leaf open hook").clone();
        if let Some(hook) = hook {
            hook(path);
        }
    }

    #[cfg(not(test))]
    fn before_leaf_open(&self, _path: &Path) {}

    fn handle<'a>(&'a self, files: &'a Files, f: FileId) -> Handle<'a> {
        Handle {
            file: pick(files, f),
            #[cfg(test)]
            id: f,
            #[cfg(test)]
            faults: &self.faults,
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
        self.before_leaf_open(&path);
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags(LEAF_OPEN_FLAGS)
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
        self.before_leaf_open(&path);
        let lock = OpenOptions::new()
            .read(true)
            .custom_flags(LEAF_OPEN_FLAGS)
            .open(&path)
            .map_err(|e| io_err("open lock file", e))?;
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

    /// Locks an opened data file so moving it cannot bypass the `LOCK` pathname.
    fn lock_data_file(&self, file: &File, name: &str, read_only: bool) -> Result<(), StorageError> {
        let result = if read_only {
            file.try_lock_shared()
        } else {
            file.try_lock()
        };
        match result {
            Ok(()) => Ok(()),
            Err(TryLockError::WouldBlock) => Err(StorageError::Busy(format!(
                "store {} is owned through {name} by another process",
                self.dir.display()
            ))),
            Err(TryLockError::Error(e)) => Err(io_err(&format!("lock {name}"), e)),
        }
    }

    /// Opens the existing store files with ownership: exclusive for read-write, shared for
    /// read-only (files opened without write access).
    fn open_files(&self, read_only: bool) -> Result<(), StorageError> {
        let dir = open_store_directory(&self.dir)?;
        let lock = if read_only {
            self.take_shared_lock()?
        } else {
            self.take_lock()?
        };
        let existing = |name: &str| {
            let path = self.dir.join(name);
            match refuse_non_regular(&path).and_then(|()| {
                self.before_leaf_open(&path);
                OpenOptions::new()
                    .read(true)
                    .write(!read_only)
                    .custom_flags(LEAF_OPEN_FLAGS)
                    .open(&path)
                    .map_err(|e| match e.kind() {
                        ErrorKind::NotFound => StorageError::StoreState(format!(
                            "required store file {name} is missing in {}",
                            self.dir.display()
                        )),
                        _ => io_err(name, e),
                    })
            }) {
                Ok(file) => {
                    same_regular_file(&path, &file)?;
                    self.lock_data_file(&file, name, read_only)?;
                    Ok(file)
                }
                Err(StorageError::StoreState(m)) if m.contains("does not exist") => {
                    Err(StorageError::StoreState(format!(
                        "required store file {name} is missing in {}",
                        self.dir.display()
                    )))
                }
                Err(e) => Err(e),
            }
        };
        let files = Files {
            dir,
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

/// Opens and pins the store directory itself without following a final symlink.
fn open_store_directory(path: &Path) -> Result<File, StorageError> {
    let at = fs::symlink_metadata(path).map_err(|e| match e.kind() {
        ErrorKind::NotFound => {
            StorageError::StoreState(format!("no store directory at {}", path.display()))
        }
        _ => io_err("inspect store directory", e),
    })?;
    if !at.file_type().is_dir() {
        return Err(StorageError::StoreState(format!(
            "{} is not a directory or is a symlink; refusing it",
            path.display()
        )));
    }
    let opened = OpenOptions::new()
        .read(true)
        .custom_flags(DIRECTORY_OPEN_FLAGS)
        .open(path)
        .map_err(|e| io_err("open store directory", e))?;
    let actual = opened
        .metadata()
        .map_err(|e| io_err("inspect store directory", e))?;
    if !actual.file_type().is_dir() || at.dev() != actual.dev() || at.ino() != actual.ino() {
        return Err(StorageError::StoreState(format!(
            "{} changed or is not a directory; refusing it",
            path.display()
        )));
    }
    Ok(opened)
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

/// Confirms that the opened handle is the regular file at `path` itself and has one name.
fn same_regular_file(path: &Path, f: &File) -> Result<(), StorageError> {
    let at = fs::symlink_metadata(path).map_err(|e| io_err("inspect store file", e))?;
    let opened = f.metadata().map_err(|e| io_err("inspect store file", e))?;
    if !at.file_type().is_file() || at.dev() != opened.dev() || at.ino() != opened.ino() {
        return Err(StorageError::StoreState(format!(
            "{} changed or is not a regular file; refusing it",
            path.display()
        )));
    }
    if at.nlink() != 1 || opened.nlink() != 1 {
        return Err(StorageError::StoreState(format!(
            "{} has multiple hard links; refusing it",
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
        let dir = open_store_directory(&self.dir)?;
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
            // The lock file stays: removing it after releasing the lock could delete the name of a
            // file another opener has just locked, letting a later opener take a second lock.
            return Err(StorageError::StoreState(format!(
                "{} {why} ({name}); refusing to create a store there",
                self.dir.display()
            )));
        }
        let new = |name: &str| {
            let path = self.dir.join(name);
            let file = OpenOptions::new()
                .read(true)
                .write(true)
                .create_new(true)
                .mode(0o600)
                .custom_flags(LEAF_OPEN_FLAGS)
                .open(&path)
                .map_err(|e| io_err(name, e))?;
            same_regular_file(&path, &file)?;
            self.lock_data_file(&file, name, false)?;
            Ok::<File, StorageError>(file)
        };
        let files = Files {
            dir,
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
        read_full(&self.handle(&files, f), buf, offset).map_err(|e| match e.kind() {
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
        write_full(&self.handle(&files, f), data, offset).map_err(|e| io_err(file.name(), e))
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
        // A failed sync is never retried, an interrupted one included: the error is reported.
        self.handle(&files, f)
            .fsync()
            .map_err(|e| io_err(file.name(), e))
    }

    fn sync_dir(&self) -> Result<(), StorageError> {
        let Ok(files) = self.files() else {
            return self.sync_path(&self.dir);
        };
        files.writable()?;
        #[cfg(test)]
        self.synced_dirs
            .lock()
            .expect("synced")
            .push(self.dir.clone());
        files
            .dir
            .sync_all()
            .map_err(|e| io_err(&format!("sync directory {}", self.dir.display()), e))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn creating_a_store_makes_every_new_directory_entry_durable() {
        let base = scratch("dirsync");
        let store = base.join("a").join("b").join("store");
        let io = FsIo::new(&store);
        io.create().unwrap();
        io.sync_dir().unwrap();
        let synced = io.synced_dirs.lock().unwrap().clone();
        // The entries for a, a/b and a/b/store live in their parents; each parent must be synced.
        for parent in [
            base.to_path_buf(),
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
    }

    #[test]
    fn directory_sync_uses_the_opened_directory_after_its_name_moves() {
        let base = scratch("pinned-directory-sync");
        let store = base.join("store");
        let moved = base.join("moved");
        let io = FsIo::new(&store);
        io.create().unwrap();
        fs::rename(&store, &moved).unwrap();

        let result = io.sync_dir();
        assert!(
            result.is_ok(),
            "directory sync followed the mutable pathname: {result:?}"
        );

        assert!(!store.exists());
        assert!(moved.is_dir());
    }

    #[test]
    fn identity_check_refuses_a_path_replaced_after_open() {
        let base = scratch("post-open-identity");
        let path = base.join("leaf");
        let moved = base.join("opened");
        fs::write(&path, b"opened bytes").unwrap();
        let opened = File::open(&path).unwrap();
        fs::rename(&path, &moved).unwrap();
        fs::write(&path, b"replacement bytes").unwrap();

        let err = same_regular_file(&path, &opened).expect_err("replacement was accepted");
        assert!(matches!(err, StorageError::StoreState(_)), "{err:?}");
        assert_eq!(fs::read(moved).unwrap(), b"opened bytes");
        assert_eq!(fs::read(path).unwrap(), b"replacement bytes");
    }

    use crate::test_support::resources::test_dir as scratch;

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
    }

    #[test]
    fn read_only_open_refuses_a_fifo_swapped_after_classification_without_blocking() {
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::mpsc::{self, RecvTimeoutError};
        use std::time::Duration;

        let base = scratch("fifo-swap");
        let store = base.join("store");
        FsIo::new(&store).create().unwrap();
        let lock_path = store.join(LOCK_FILE);
        let swapped = Arc::new(AtomicBool::new(false));
        let hook_path = lock_path.clone();
        let hook_swapped = Arc::clone(&swapped);
        let hook: LeafOpenHook = Arc::new(move |path| {
            if path == hook_path && !hook_swapped.swap(true, Ordering::SeqCst) {
                fs::remove_file(path).unwrap();
                let status = std::process::Command::new("mkfifo")
                    .arg(path)
                    .status()
                    .expect("run mkfifo");
                assert!(status.success(), "mkfifo failed: {status}");
            }
        });
        let io = FsIo::with_leaf_open_hook(&store, hook);
        let (tx, rx) = mpsc::channel();
        let worker = std::thread::spawn(move || {
            let _ = tx.send(io.open_read_only());
        });

        let result = match rx.recv_timeout(Duration::from_secs(2)) {
            Ok(result) => result,
            Err(RecvTimeoutError::Timeout) => {
                assert!(swapped.load(Ordering::SeqCst), "the FIFO swap did not run");
                let wake = OpenOptions::new()
                    .write(true)
                    .open(&lock_path)
                    .expect("wake the blocked FIFO open");
                let _ = rx.recv_timeout(Duration::from_secs(2));
                drop(wake);
                worker.join().unwrap();
                let opened_promptly = false;
                assert!(
                    opened_promptly,
                    "read-only open blocked after a checked path became a FIFO"
                );
                unreachable!("the blocking-open assertion always fails");
            }
            Err(RecvTimeoutError::Disconnected) => {
                worker.join().unwrap();
                panic!("read-only open worker exited without a result");
            }
        };
        worker.join().unwrap();
        let err = result.expect_err("raced FIFO was accepted");
        assert!(matches!(err, StorageError::StoreState(_)), "{err:?}");
        assert!(swapped.load(Ordering::SeqCst), "the FIFO swap did not run");
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
    }

    #[test]
    fn creating_refuses_a_directory_that_already_holds_other_files() {
        let names = |d: &Path| {
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
            // Only the lock file may appear: a refused create never deletes the lock file, since
            // another opener may already hold a lock on it.
            let mut expected = before.clone();
            expected.push(LOCK_FILE.to_string());
            expected.sort();
            assert_eq!(
                names(&store),
                expected,
                "{stray}: nothing but the lock file created"
            );
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
