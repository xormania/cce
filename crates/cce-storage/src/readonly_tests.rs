//! Read-only inspection open: lock interplay with owners and other readers, selection identical to
//! a read-write open, no writes or syncs of any kind, and refusal of staging and commits.

use std::path::{Path, PathBuf};

use cce_core::storage::{
    AttemptId, Change, ChangeSet, CommitError, ContentRef, ContextId, HistoryPosition,
    Precondition, ReadView, StorageEngine, StorageError,
};

use crate::engine::Inner;
use crate::format::{Descriptor, PAGE_SIZE, StoreId};
use crate::io::{FileId, FsIo, Io, LOCK_FILE};
use crate::sim::{Persist, SimIo};
use crate::{NativeReadOnly, NativeStore, NativeView};

const STORE: StoreId = StoreId(*b"read-only-tests!");

fn cid(s: &str) -> ContextId {
    ContextId::new(s.as_bytes().to_vec()).unwrap()
}

fn sim_create(sim: &SimIo) -> NativeStore {
    NativeStore {
        inner: Inner::create(Box::new(sim.clone()), STORE, Default::default()).unwrap(),
    }
}

fn try_open(sim: &SimIo) -> Result<NativeStore, StorageError> {
    Inner::open(Box::new(sim.clone()), Default::default()).map(|inner| NativeStore { inner })
}

fn try_open_ro(sim: &SimIo) -> Result<NativeReadOnly, StorageError> {
    Inner::open_read_only(Box::new(sim.clone()), Default::default())
        .map(|inner| NativeReadOnly { inner })
}

fn put(s: &NativeStore, attempt: &str, id: &str, body: &str) -> Result<u64, CommitError> {
    let cs = ChangeSet {
        changes: vec![Change::PutContext {
            id: cid(id),
            expect: Precondition::Any,
            body: body.as_bytes().to_vec(),
            content: None,
        }],
        ..ChangeSet::default()
    };
    s.commit(
        AttemptId::new(attempt.as_bytes().to_vec()).unwrap(),
        s.begin().unwrap(),
        cs,
    )
    .map(|r| r.position.0)
}

/// Records A and B and every history position, through any view.
fn logical_state(mut v: NativeView) -> Vec<String> {
    let mut out = Vec::new();
    for id in ["A", "B"] {
        out.push(format!("{id}: {:?}", v.context(&cid(id)).unwrap()));
    }
    for h in v.history(HistoryPosition(0)).unwrap() {
        out.push(format!("history {:?}", h.unwrap().position));
    }
    out
}

fn images(sim: &SimIo) -> Vec<Option<(Vec<u8>, Vec<u8>)>> {
    [FileId::Meta, FileId::Content]
        .into_iter()
        .map(|f| sim.file_images_for_test(f))
        .collect()
}

fn descriptor(inner: &Inner) -> Descriptor {
    inner.snapshot().unwrap().desc
}

/// A released simulated store with two commits: A at position 1, B at position 2.
fn two_commits() -> (SimIo, Vec<String>) {
    let sim = SimIo::new();
    let s = sim_create(&sim);
    assert_eq!(put(&s, "a1", "A", "one"), Ok(1));
    assert_eq!(put(&s, "a2", "B", "two"), Ok(2));
    let state = logical_state(s.begin().unwrap());
    drop(s);
    sim.release();
    (sim, state)
}

fn assert_busy<T>(r: Result<T, StorageError>, what: &str) {
    match r {
        Err(StorageError::Busy(_)) => {}
        Err(e) => panic!("{what}: expected Busy, got {e:?}"),
        Ok(_) => panic!("{what}: expected Busy, but it opened"),
    }
}

/// The engine's own refusal, before any I/O (not an adapter refusing a write further down).
fn assert_read_only_refusal(e: &StorageError, what: &str) {
    assert!(
        matches!(e, StorageError::StoreState(m) if m.contains("refused on a read-only store handle")),
        "{what}: {e:?}"
    );
}

/// The filesystem adapter's refusal of a write or sync through a read-only handle.
fn assert_adapter_refusal(e: &StorageError, what: &str) {
    assert!(
        matches!(e, StorageError::StoreState(m) if m.contains("read-only store handle: writes and syncs are refused")),
        "{what}: {e:?}"
    );
}

#[test]
fn simulated_lock_admits_one_owner_or_any_number_of_readers() {
    let sim = SimIo::new();
    let owner = sim_create(&sim);
    assert_busy(try_open_ro(&sim), "read-only open under an owner");
    assert_busy(try_open(&sim), "read-write open under an owner");
    drop(owner);
    sim.release();

    let first = try_open_ro(&sim).expect("first reader");
    let second = try_open_ro(&sim).expect("second reader");
    assert_busy(try_open(&sim), "read-write open under readers");
    drop((first, second));
    sim.release();

    // Positive neighbour: with every holder gone, an owner opens and keeps readers out again.
    let owner = try_open(&sim).expect("owner after readers");
    assert_busy(try_open_ro(&sim), "read-only open under the new owner");
    drop(owner);
}

#[test]
fn a_read_only_open_selects_the_committed_state_and_writes_nothing() {
    let (sim, committed) = two_commits();
    let bytes = images(&sim);
    let ops = sim.ops();

    let ro = try_open_ro(&sim).unwrap();
    assert_eq!(logical_state(ro.begin().unwrap()), committed);
    let mut v = ro.begin().unwrap();
    let a = v.context(&cid("A")).unwrap().expect("A");
    assert_eq!(a.body, b"one");
    assert!(ro.recovery_diagnostics().is_empty());
    assert_eq!(ro.stats().unwrap().pages_written, 0);
    drop((v, ro));

    assert_eq!(sim.ops(), ops, "a read-only session issued writes or syncs");
    assert_eq!(
        images(&sim),
        bytes,
        "a read-only session changed the store bytes"
    );
}

#[test]
fn read_only_and_read_write_opens_skip_a_damaged_newest_record_alike() {
    let (sim, _) = two_commits();
    // Generation 3 (the second commit) lives in slot 1: damage one byte of its counters.
    let mut slot = vec![0; PAGE_SIZE];
    sim.read_at(FileId::Meta, PAGE_SIZE as u64, &mut slot)
        .unwrap();
    slot[40] ^= 0x5a;
    sim.write_at(FileId::Meta, PAGE_SIZE as u64, &slot).unwrap();
    sim.sync(FileId::Meta).unwrap();
    let bytes = images(&sim);

    let ro = try_open_ro(&sim).unwrap();
    let (ro_desc, ro_diag) = (descriptor(&ro.inner), ro.recovery_diagnostics().to_vec());
    let ro_state = logical_state(ro.begin().unwrap());
    drop(ro);
    assert_eq!(
        images(&sim),
        bytes,
        "the read-only open changed the damaged store"
    );
    sim.release();

    let rw = try_open(&sim).unwrap();
    assert_eq!(ro_desc, descriptor(&rw.inner), "selected record");
    assert_eq!(ro_desc.generation, 2);
    assert_eq!(ro_diag, rw.recovery_diagnostics(), "diagnostics");
    assert_eq!(ro_diag.len(), 1, "{ro_diag:?}");
    assert_eq!(
        ro_state,
        logical_state(rw.begin().unwrap()),
        "recovered state"
    );
}

#[test]
fn a_read_only_open_sees_an_unsynced_state_without_making_it_durable() {
    // Operations of one small commit, to fail the last one (the publication barrier).
    let probe = SimIo::new();
    let s = sim_create(&probe);
    put(&s, "a1", "A", "one").unwrap();
    let before = probe.ops();
    put(&s, "a2", "B", "two").unwrap();
    let per_commit = probe.ops() - before;
    let new = logical_state(s.begin().unwrap());
    drop(s);

    let sim = SimIo::new();
    let s = sim_create(&sim);
    put(&s, "a1", "A", "one").unwrap();
    let old = logical_state(s.begin().unwrap());
    sim.fail_nth_op(per_commit);
    assert!(matches!(
        put(&s, "a2", "B", "two"),
        Err(CommitError::Indeterminate(_))
    ));
    drop(s);

    // The owner exits; the operating system still holds the unsynced publication record.
    let exited = sim.after_process_exit();
    let ro = try_open_ro(&exited).unwrap();
    assert_eq!(
        logical_state(ro.begin().unwrap()),
        new,
        "visible to the reader"
    );
    drop(ro);
    // The read-only open did not make it durable: a power loss now still loses it.
    let lost = exited.after_crash(Persist::None);
    assert_eq!(
        logical_state(try_open(&lost).unwrap().begin().unwrap()),
        old
    );
    // Positive neighbour: a read-write open of the same image makes it durable.
    let exited = sim.after_process_exit();
    drop(try_open(&exited).unwrap());
    let kept = exited.after_crash(Persist::None);
    assert_eq!(
        logical_state(try_open(&kept).unwrap().begin().unwrap()),
        new
    );
}

#[test]
fn staging_and_commits_are_refused_on_a_read_only_handle() {
    let (sim, committed) = two_commits();
    let ops = sim.ops();
    let ro = Inner::open_read_only(Box::new(sim.clone()), Default::default()).unwrap();

    let e = ro.stage(&mut &b"bytes"[..]).unwrap_err();
    assert_read_only_refusal(&e, "stage");
    let declared = ContentRef {
        digest: cce_core::storage::ContentDigest([0; 32]),
        len: 5,
    };
    let e = ro
        .stage_checked(&mut &b"bytes"[..], Some(&declared))
        .unwrap_err();
    assert_read_only_refusal(&e, "stage with a declared reference");

    let cs = || ChangeSet {
        changes: vec![Change::PutContext {
            id: cid("C"),
            expect: Precondition::Absent,
            body: b"never".to_vec(),
            content: None,
        }],
        ..ChangeSet::default()
    };
    let attempt = AttemptId::new(*b"refused").unwrap();
    match ro.submit(attempt.clone(), ro.snapshot().unwrap(), Vec::new(), cs()) {
        Err(CommitError::Rejected(e)) => assert_read_only_refusal(&e, "submit"),
        Err(e) => panic!("submit: {e:?}"),
        Ok(_) => panic!("submit admitted on a read-only handle"),
    }
    match ro.commit(attempt, ro.snapshot().unwrap(), &[], cs()) {
        Err(CommitError::Rejected(e)) => assert_read_only_refusal(&e, "commit"),
        other => panic!("commit: {other:?}"),
    }
    let view = NativeReadOnly { inner: ro };
    assert_eq!(logical_state(view.begin().unwrap()), committed);
    drop(view);
    assert_eq!(sim.ops(), ops, "refused calls wrote or synced");

    // Positive neighbour: the same calls work through a read-write open.
    sim.release();
    let rw = try_open(&sim).unwrap();
    rw.stage_content(&mut &b"bytes"[..]).unwrap();
    assert_eq!(put(&rw, "accepted", "C", "now"), Ok(3));
}

// ----- filesystem adapter --------------------------------------------------------------------

struct TempDir(PathBuf);

impl TempDir {
    fn new(name: &str) -> Self {
        let p = std::env::temp_dir().join(format!("cce-ro-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        Self(p)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn listing(dir: &Path) -> Vec<String> {
    let mut v: Vec<String> = std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    v.sort();
    v
}

#[test]
fn filesystem_readers_share_the_lock_and_exclude_owners() {
    // Each open below uses its own lock file descriptor, so the lock behaves as between processes.
    let dir = TempDir::new("matrix");
    let owner = NativeStore::create(&dir.0, *b"read-only-matrix").unwrap();
    assert_eq!(put(&owner, "a1", "A", "one"), Ok(1));
    assert_busy(
        NativeStore::open_read_only(&dir.0),
        "read-only open under an owner",
    );
    drop(owner);

    let first = NativeStore::open_read_only(&dir.0).expect("first reader");
    let second = NativeStore::open_read_only(&dir.0).expect("second reader");
    assert_busy(NativeStore::open(&dir.0), "read-write open under readers");
    // A view outlives its handle and keeps the shared lock.
    let mut view = first.begin().unwrap();
    drop((first, second));
    assert_busy(NativeStore::open(&dir.0), "read-write open under a view");
    assert_eq!(view.context(&cid("A")).unwrap().expect("A").body, b"one");
    drop(view);

    let owner = NativeStore::open(&dir.0).expect("owner after readers");
    assert_busy(
        NativeStore::open_read_only(&dir.0),
        "read-only open under the new owner",
    );
    drop(owner);
}

#[test]
fn filesystem_read_only_open_without_a_lock_file_refuses_and_creates_nothing() {
    let dir = TempDir::new("no-lock");
    let s = NativeStore::create(&dir.0, *b"read-only-nolock").unwrap();
    assert_eq!(put(&s, "a1", "A", "one"), Ok(1));
    drop(s);
    std::fs::remove_file(dir.0.join(LOCK_FILE)).unwrap();
    let before = listing(&dir.0);

    match NativeStore::open_read_only(&dir.0) {
        Err(StorageError::StoreState(m)) => assert!(m.contains(LOCK_FILE), "{m}"),
        Err(e) => panic!("expected a refusal naming {LOCK_FILE}, got {e:?}"),
        Ok(_) => panic!("read-only open succeeded without a lock file"),
    }
    assert_eq!(listing(&dir.0), before, "the refused open created files");

    // Positive neighbour: a read-write open recreates the lock file; then readers work.
    drop(NativeStore::open(&dir.0).unwrap());
    assert!(dir.0.join(LOCK_FILE).exists());
    let ro = NativeStore::open_read_only(&dir.0).unwrap();
    let a = ro.begin().unwrap().context(&cid("A")).unwrap();
    assert_eq!(a.expect("A").body, b"one");
}

#[test]
fn a_filesystem_read_only_handle_refuses_writes_and_syncs() {
    let dir = TempDir::new("adapter");
    drop(NativeStore::create(&dir.0, *b"read-only-adapt!").unwrap());
    let meta = std::fs::read(dir.0.join(crate::io::META_FILE)).unwrap();

    let io = FsIo::new(&dir.0);
    io.open_read_only().unwrap();
    let mut buf = vec![0; 16];
    io.read_at(FileId::Meta, PAGE_SIZE as u64, &mut buf)
        .expect("reads work");
    for (what, r) in [
        ("write", io.write_at(FileId::Meta, 0, b"x")),
        ("sync", io.sync(FileId::Meta)),
        ("directory sync", io.sync_dir()),
    ] {
        match r {
            Err(e) => assert_adapter_refusal(&e, what),
            Ok(()) => panic!("{what} succeeded on a read-only handle"),
        }
    }
    drop(io);
    assert_eq!(
        std::fs::read(dir.0.join(crate::io::META_FILE)).unwrap(),
        meta
    );
}
