//! Hostile filesystem topology at the store boundary.

mod common;

use std::fs::{self, OpenOptions};
use std::os::unix::fs::{FileTypeExt, MetadataExt, OpenOptionsExt, symlink};
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

use cce_core::storage::StorageEngine;
use cce_storage::NativeStore;

use common::{Driver, Step, test_dir};

const STORE_ID: [u8; 16] = *b"fs-security-test";
const LOCK: &str = "LOCK";
const META: &str = "meta.cce";
const CONTENT: &str = "content.cce";
#[derive(Clone, Copy, Debug)]
enum OpenMode {
    ReadWrite,
    ReadOnly,
}

impl OpenMode {
    fn command(self, dir: &Path) -> String {
        let op = match self {
            Self::ReadWrite => "open",
            Self::ReadOnly => "open-ro",
        };
        format!("{op} {}", dir.display())
    }
}

#[derive(Clone, Copy, Debug)]
enum SpecialFile {
    SymlinkToRegular,
    DanglingSymlink,
    Fifo,
    Socket,
    Directory,
}

#[derive(Debug, PartialEq, Eq)]
struct EntrySnapshot {
    relative: PathBuf,
    kind: &'static str,
    mode: u32,
    dev: u64,
    ino: u64,
    nlink: u64,
    len: u64,
    target: Option<PathBuf>,
    bytes: Option<Vec<u8>>,
}

fn snapshot_tree(root: &Path) -> Vec<EntrySnapshot> {
    fn visit(root: &Path, path: &Path, out: &mut Vec<EntrySnapshot>) {
        let mut entries: Vec<_> = fs::read_dir(path)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .collect();
        entries.sort();
        for entry in entries {
            let metadata = fs::symlink_metadata(&entry).unwrap();
            let file_type = metadata.file_type();
            let kind = if file_type.is_file() {
                "file"
            } else if file_type.is_dir() {
                "directory"
            } else if file_type.is_symlink() {
                "symlink"
            } else if file_type.is_fifo() {
                "fifo"
            } else if file_type.is_socket() {
                "socket"
            } else if file_type.is_block_device() {
                "block-device"
            } else if file_type.is_char_device() {
                "character-device"
            } else {
                "other"
            };
            out.push(EntrySnapshot {
                relative: entry.strip_prefix(root).unwrap().to_path_buf(),
                kind,
                mode: metadata.mode(),
                dev: metadata.dev(),
                ino: metadata.ino(),
                nlink: metadata.nlink(),
                len: metadata.len(),
                target: file_type
                    .is_symlink()
                    .then(|| fs::read_link(&entry).unwrap()),
                bytes: file_type.is_file().then(|| fs::read(&entry).unwrap()),
            });
            if file_type.is_dir() {
                visit(root, &entry, out);
            }
        }
    }

    let mut out = Vec::new();
    visit(root, root, &mut out);
    out
}

fn create_store(dir: &Path) {
    drop(NativeStore::create(dir, STORE_ID).expect("create fixture store"));
}

fn fresh_lock(path: &Path) {
    OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .expect("create replacement lock");
}

fn assert_driver_refused(command: &str, what: &str, message: &str) {
    let mut driver = Driver::spawn();
    let step = driver.step(command);
    let lines = match step {
        Step::Answered(lines) => lines,
        Step::Hung { lines } => panic!("{what} hung; output: {lines:?}"),
        Step::Exited { lines, code } => {
            panic!("{what} exited before answering ({code:?}); output: {lines:?}")
        }
    };
    assert!(
        lines
            .iter()
            .any(|line| line.starts_with("error ") && line.contains(message)),
        "{what} was not refused for {message:?}: {lines:?}"
    );
    assert_eq!(
        driver.finish().and_then(|status| status.code()),
        Some(1),
        "a refused driver exits unsuccessfully"
    );
}

fn assert_open_refused(dir: &Path, mode: OpenMode, message: &str) {
    assert_driver_refused(
        &mode.command(dir),
        &format!("{mode:?} open of {}", dir.display()),
        message,
    );
}

fn assert_read_only_open_succeeds(dir: &Path) {
    let mut driver = Driver::spawn();
    assert_eq!(
        driver.step(&format!("open-ro {}", dir.display())),
        Step::Answered(vec!["ok opened read-only".into()])
    );
    assert_eq!(
        driver.step("drop-store"),
        Step::Answered(vec!["ok store dropped".into()])
    );
    assert_eq!(driver.finish().and_then(|status| status.code()), Some(0));
}

fn install_socket(path: &Path) -> UnixListener {
    static NEXT_SOCKET: AtomicU64 = AtomicU64::new(0);

    loop {
        let alias = Path::new("/tmp").join(format!(
            "cce-socket-dir-{}-{}",
            std::process::id(),
            NEXT_SOCKET.fetch_add(1, Ordering::Relaxed)
        ));
        match symlink(path.parent().unwrap(), &alias) {
            Ok(()) => {}
            Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(err) => panic!("create short socket-directory alias: {err}"),
        }
        let result = UnixListener::bind(alias.join(path.file_name().unwrap()));
        fs::remove_file(&alias).expect("remove short socket-directory alias");
        return result.expect("bind socket fixture through its short directory alias");
    }
}

fn install_special_file(
    base: &Path,
    store: &Path,
    leaf: &str,
    special: SpecialFile,
) -> Option<UnixListener> {
    let path = store.join(leaf);
    fs::remove_file(&path).unwrap();
    match special {
        SpecialFile::SymlinkToRegular => {
            let victim = base.join("outside-victim");
            fs::write(&victim, b"outside bytes must not change").unwrap();
            symlink(victim, path).unwrap();
            None
        }
        SpecialFile::DanglingSymlink => {
            symlink(base.join("missing-target"), path).unwrap();
            None
        }
        SpecialFile::Fifo => {
            let status = Command::new("mkfifo")
                .arg(&path)
                .status()
                .expect("run mkfifo");
            assert!(status.success(), "mkfifo failed: {status}");
            None
        }
        SpecialFile::Socket => Some(install_socket(&path)),
        SpecialFile::Directory => {
            fs::create_dir(path).unwrap();
            None
        }
    }
}

fn special_file_case(label: &str, leaf: &str, mode: OpenMode, special: SpecialFile) {
    let base = test_dir(label);
    let store = base.join("store");
    create_store(&store);
    let _socket = install_special_file(base.as_ref(), &store, leaf, special);
    let before = snapshot_tree(base.as_ref());

    assert_open_refused(&store, mode, "not a regular file");

    assert_eq!(
        snapshot_tree(base.as_ref()),
        before,
        "a refused {mode:?} open changed the {leaf} {special:?} fixture"
    );
}

macro_rules! special_file_tests {
    ($($name:ident: $leaf:expr, $mode:expr, $special:expr;)*) => {$ (
        #[test]
        fn $name() {
            special_file_case(stringify!($name), $leaf, $mode, $special);
        }
    )*};
}

special_file_tests! {
    rw_lock_refuses_symlink_to_regular: LOCK, OpenMode::ReadWrite, SpecialFile::SymlinkToRegular;
    rw_lock_refuses_dangling_symlink: LOCK, OpenMode::ReadWrite, SpecialFile::DanglingSymlink;
    rw_lock_refuses_fifo: LOCK, OpenMode::ReadWrite, SpecialFile::Fifo;
    rw_lock_refuses_socket: LOCK, OpenMode::ReadWrite, SpecialFile::Socket;
    rw_lock_refuses_directory: LOCK, OpenMode::ReadWrite, SpecialFile::Directory;
    ro_lock_refuses_symlink_to_regular: LOCK, OpenMode::ReadOnly, SpecialFile::SymlinkToRegular;
    ro_lock_refuses_dangling_symlink: LOCK, OpenMode::ReadOnly, SpecialFile::DanglingSymlink;
    ro_lock_refuses_fifo: LOCK, OpenMode::ReadOnly, SpecialFile::Fifo;
    ro_lock_refuses_socket: LOCK, OpenMode::ReadOnly, SpecialFile::Socket;
    ro_lock_refuses_directory: LOCK, OpenMode::ReadOnly, SpecialFile::Directory;

    rw_metadata_refuses_symlink_to_regular: META, OpenMode::ReadWrite, SpecialFile::SymlinkToRegular;
    rw_metadata_refuses_dangling_symlink: META, OpenMode::ReadWrite, SpecialFile::DanglingSymlink;
    rw_metadata_refuses_fifo: META, OpenMode::ReadWrite, SpecialFile::Fifo;
    rw_metadata_refuses_socket: META, OpenMode::ReadWrite, SpecialFile::Socket;
    rw_metadata_refuses_directory: META, OpenMode::ReadWrite, SpecialFile::Directory;
    ro_metadata_refuses_symlink_to_regular: META, OpenMode::ReadOnly, SpecialFile::SymlinkToRegular;
    ro_metadata_refuses_dangling_symlink: META, OpenMode::ReadOnly, SpecialFile::DanglingSymlink;
    ro_metadata_refuses_fifo: META, OpenMode::ReadOnly, SpecialFile::Fifo;
    ro_metadata_refuses_socket: META, OpenMode::ReadOnly, SpecialFile::Socket;
    ro_metadata_refuses_directory: META, OpenMode::ReadOnly, SpecialFile::Directory;

    rw_content_refuses_symlink_to_regular: CONTENT, OpenMode::ReadWrite, SpecialFile::SymlinkToRegular;
    rw_content_refuses_dangling_symlink: CONTENT, OpenMode::ReadWrite, SpecialFile::DanglingSymlink;
    rw_content_refuses_fifo: CONTENT, OpenMode::ReadWrite, SpecialFile::Fifo;
    rw_content_refuses_socket: CONTENT, OpenMode::ReadWrite, SpecialFile::Socket;
    rw_content_refuses_directory: CONTENT, OpenMode::ReadWrite, SpecialFile::Directory;
    ro_content_refuses_symlink_to_regular: CONTENT, OpenMode::ReadOnly, SpecialFile::SymlinkToRegular;
    ro_content_refuses_dangling_symlink: CONTENT, OpenMode::ReadOnly, SpecialFile::DanglingSymlink;
    ro_content_refuses_fifo: CONTENT, OpenMode::ReadOnly, SpecialFile::Fifo;
    ro_content_refuses_socket: CONTENT, OpenMode::ReadOnly, SpecialFile::Socket;
    ro_content_refuses_directory: CONTENT, OpenMode::ReadOnly, SpecialFile::Directory;
}

fn hardlink_case(label: &str, leaf: &str, mode: OpenMode) {
    let base = test_dir(label);
    let store = base.join("store");
    create_store(&store);
    fs::hard_link(store.join(leaf), base.join("extra-link")).unwrap();
    let before = snapshot_tree(base.as_ref());

    assert_open_refused(&store, mode, "multiple hard links");

    assert_eq!(snapshot_tree(base.as_ref()), before);
}

macro_rules! hardlink_tests {
    ($($name:ident: $leaf:expr, $mode:expr;)*) => {$ (
        #[test]
        fn $name() {
            hardlink_case(stringify!($name), $leaf, $mode);
        }
    )*};
}

hardlink_tests! {
    rw_open_refuses_a_hardlinked_lock: LOCK, OpenMode::ReadWrite;
    ro_open_refuses_a_hardlinked_lock: LOCK, OpenMode::ReadOnly;
    ro_open_refuses_hardlinked_metadata: META, OpenMode::ReadOnly;
    rw_open_refuses_hardlinked_content: CONTENT, OpenMode::ReadWrite;
    ro_open_refuses_hardlinked_content: CONTENT, OpenMode::ReadOnly;
}

#[test]
fn rw_open_refuses_hardlinked_metadata() {
    hardlink_case(
        "rw_open_refuses_hardlinked_metadata",
        META,
        OpenMode::ReadWrite,
    );
}

#[test]
fn moved_store_files_cannot_be_opened_through_a_second_lock() {
    let base = test_dir("moved-store-files");
    let store = base.join("store");
    let owner = NativeStore::create(&store, STORE_ID).unwrap();
    let alias = base.join("alias");
    fs::create_dir(&alias).unwrap();
    fresh_lock(&alias.join(LOCK));
    fs::rename(store.join(META), alias.join(META)).unwrap();
    fs::rename(store.join(CONTENT), alias.join(CONTENT)).unwrap();
    let before = snapshot_tree(base.as_ref());

    assert_open_refused(&alias, OpenMode::ReadWrite, "owned through");
    assert_open_refused(&alias, OpenMode::ReadOnly, "owned through");

    drop(owner.begin().expect("the original owner remains usable"));
    assert_eq!(snapshot_tree(base.as_ref()), before);
}

fn moved_data_file_case(label: &str, moved: &str) {
    let base = test_dir(label);
    let store = base.join("store");
    let owner = NativeStore::create(&store, STORE_ID).unwrap();
    let alias = base.join("alias");
    fs::create_dir(&alias).unwrap();
    fresh_lock(&alias.join(LOCK));
    for leaf in [META, CONTENT] {
        if leaf == moved {
            fs::rename(store.join(leaf), alias.join(leaf)).unwrap();
        } else {
            fs::copy(store.join(leaf), alias.join(leaf)).unwrap();
        }
    }
    let before = snapshot_tree(base.as_ref());

    assert_open_refused(
        &alias,
        OpenMode::ReadWrite,
        &format!("owned through {moved}"),
    );
    assert_open_refused(
        &alias,
        OpenMode::ReadOnly,
        &format!("owned through {moved}"),
    );

    drop(owner.begin().expect("the original owner remains usable"));
    assert_eq!(snapshot_tree(base.as_ref()), before);
}

#[test]
fn moved_metadata_cannot_be_opened_through_a_second_lock() {
    moved_data_file_case("moved-metadata", META);
}

#[test]
fn moved_content_cannot_be_opened_through_a_second_lock() {
    moved_data_file_case("moved-content", CONTENT);
}

#[test]
fn replacing_the_lock_name_does_not_admit_another_owner_or_reader() {
    let base = test_dir("replace-lock-writer");
    let store = base.join("store");
    let owner = NativeStore::create(&store, STORE_ID).unwrap();
    fs::rename(store.join(LOCK), store.join("LOCK.owner")).unwrap();
    fresh_lock(&store.join(LOCK));
    let before = snapshot_tree(base.as_ref());

    assert_open_refused(&store, OpenMode::ReadWrite, "owned through");
    assert_open_refused(&store, OpenMode::ReadOnly, "owned through");

    drop(owner.begin().expect("the original owner remains usable"));
    assert_eq!(snapshot_tree(base.as_ref()), before);
}

#[test]
fn a_reopened_writer_keeps_ownership_after_lock_replacement() {
    let base = test_dir("replace-lock-reopened-writer");
    let store = base.join("store");
    create_store(&store);
    let owner = NativeStore::open(&store).unwrap();
    fs::rename(store.join(LOCK), store.join("LOCK.owner")).unwrap();
    fresh_lock(&store.join(LOCK));
    let before = snapshot_tree(base.as_ref());

    assert_open_refused(&store, OpenMode::ReadWrite, "owned through");
    assert_open_refused(&store, OpenMode::ReadOnly, "owned through");

    drop(owner.begin().expect("the reopened owner remains usable"));
    assert_eq!(snapshot_tree(base.as_ref()), before);
}

#[test]
fn data_file_locks_preserve_reader_sharing_after_lock_replacement() {
    let base = test_dir("replace-lock-reader");
    let store = base.join("store");
    create_store(&store);
    let reader = NativeStore::open_read_only(&store).unwrap();
    fs::rename(store.join(LOCK), store.join("LOCK.reader")).unwrap();
    fresh_lock(&store.join(LOCK));
    let before = snapshot_tree(base.as_ref());

    assert_open_refused(&store, OpenMode::ReadWrite, "owned through");
    assert_read_only_open_succeeds(&store);

    drop(reader);
    assert_eq!(snapshot_tree(base.as_ref()), before);
}

fn directory_symlink_case(label: &str, mode: OpenMode) {
    let base = test_dir(label);
    let store = base.join("store");
    create_store(&store);
    let alias = base.join("store-link");
    symlink(&store, &alias).unwrap();
    let before = snapshot_tree(base.as_ref());

    assert_open_refused(&alias, mode, "symlink");

    assert_eq!(snapshot_tree(base.as_ref()), before);
}

#[test]
fn read_write_open_refuses_a_symlinked_store_directory() {
    directory_symlink_case("rw-directory-symlink", OpenMode::ReadWrite);
}

#[test]
fn read_only_open_refuses_a_symlinked_store_directory() {
    directory_symlink_case("ro-directory-symlink", OpenMode::ReadOnly);
}

fn create_with_special_lock_case(label: &str, special: SpecialFile) {
    let base = test_dir(label);
    let store = base.join("store");
    fs::create_dir(&store).unwrap();
    fs::write(store.join(LOCK), b"").unwrap();
    let _socket = install_special_file(base.as_ref(), &store, LOCK, special);
    let before = snapshot_tree(base.as_ref());

    assert_driver_refused(
        &format!("create {}", store.display()),
        &format!("create with a {special:?} lock"),
        "not a regular file",
    );

    assert_eq!(snapshot_tree(base.as_ref()), before);
}

#[test]
fn create_refuses_a_symlink_lock() {
    create_with_special_lock_case("create-symlink-lock", SpecialFile::SymlinkToRegular);
}

#[test]
fn create_refuses_a_dangling_symlink_lock() {
    create_with_special_lock_case("create-dangling-lock", SpecialFile::DanglingSymlink);
}

#[test]
fn create_refuses_a_fifo_lock() {
    create_with_special_lock_case("create-fifo-lock", SpecialFile::Fifo);
}

#[test]
fn create_refuses_a_socket_lock() {
    create_with_special_lock_case("create-socket-lock", SpecialFile::Socket);
}

#[test]
fn create_refuses_a_directory_lock() {
    create_with_special_lock_case("create-directory-lock", SpecialFile::Directory);
}

#[test]
fn create_refuses_a_symlinked_store_directory_without_touching_its_target() {
    let base = test_dir("create-directory-symlink");
    let target = base.join("target");
    fs::create_dir(&target).unwrap();
    let alias = base.join("store-link");
    symlink(&target, &alias).unwrap();
    let before = snapshot_tree(base.as_ref());

    assert_driver_refused(
        &format!("create {}", alias.display()),
        "create through a directory symlink",
        "symlink",
    );

    assert_eq!(snapshot_tree(base.as_ref()), before);
}
