//! Engine behavior tests through the logical port (simulated I/O unless stated).

use std::io::Read;

use scientdb_kernel::storage::{
    AttemptId, Change, ChangeSet, CommitError, ContentBinding, ContextId, HistoryPosition,
    Precondition, ReadView, Revision, StorageEngine, StorageError,
};

use crate::NativeStore;
use crate::engine::Inner;
use crate::format::StoreId;
use crate::sim::SimIo;

fn store() -> NativeStore {
    NativeStore {
        inner: Inner::create(Box::new(SimIo::new()), StoreId([3; 16]), Default::default()).unwrap(),
    }
}

fn put(id: &str, expect: Precondition, body: &[u8], content: Option<usize>) -> Change {
    Change::PutContext {
        id: ContextId::new(id.as_bytes().to_vec()).unwrap(),
        expect,
        body: body.to_vec(),
        content,
    }
}

fn attempt(s: &str) -> AttemptId {
    AttemptId::new(s.as_bytes().to_vec()).unwrap()
}

fn commit(s: &NativeStore, a: &str, cs: ChangeSet) -> Result<u64, CommitError> {
    s.commit(attempt(a), s.begin().unwrap(), cs)
        .map(|r| r.position.0)
}

fn changes(c: Vec<Change>) -> ChangeSet {
    ChangeSet {
        changes: c,
        ..ChangeSet::default()
    }
}

fn revision(s: &NativeStore, id: &str) -> Option<Revision> {
    s.begin()
        .unwrap()
        .context(&ContextId::new(id.as_bytes().to_vec()).unwrap())
        .unwrap()
        .map(|r| r.revision)
}

#[test]
fn a_recreated_record_never_reuses_a_revision_seen_before_removal() {
    let s = store();
    commit(
        &s,
        "a1",
        changes(vec![put("A", Precondition::Absent, b"v1", None)]),
    )
    .unwrap();
    let old = revision(&s, "A").unwrap();
    let id = ContextId::new(*b"A").unwrap();
    commit(
        &s,
        "a2",
        changes(vec![Change::RemoveContext {
            id,
            expect: Precondition::AtRevision(old),
        }]),
    )
    .unwrap();
    commit(
        &s,
        "a3",
        changes(vec![put("A", Precondition::Absent, b"recreated", None)]),
    )
    .unwrap();
    let new = revision(&s, "A").unwrap();
    assert_ne!(new, old, "recreated record reused revision {old:?}");
    // A writer holding the pre-removal revision must conflict, not overwrite the new record.
    let stale = commit(
        &s,
        "a4",
        changes(vec![put(
            "A",
            Precondition::AtRevision(old),
            b"stale",
            None,
        )]),
    );
    assert!(matches!(stale, Err(CommitError::Conflict(_))), "{stale:?}");
    // Positive neighbour: the current revision is accepted.
    commit(
        &s,
        "a5",
        changes(vec![put(
            "A",
            Precondition::AtRevision(new),
            b"fresh",
            None,
        )]),
    )
    .unwrap();
}

#[test]
fn an_oversized_record_is_rejected_without_panicking() {
    let s = store();
    let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        commit(
            &s,
            "big",
            changes(vec![put("A", Precondition::Absent, &vec![7; 70_000], None)]),
        )
    }));
    let r = r.expect("commit panicked on an oversized body");
    assert!(
        matches!(
            r,
            Err(CommitError::Rejected(StorageError::InvalidRequest(_)))
        ),
        "{r:?}"
    );
    // The store remains usable, and a small record still commits.
    commit(
        &s,
        "small",
        changes(vec![put("A", Precondition::Absent, b"ok", None)]),
    )
    .unwrap();
}

#[test]
fn zero_length_and_nonempty_stagings_are_distinct_handles() {
    let s = store();
    let empty = s.stage_content(&mut &b""[..]).unwrap();
    let full = s.stage_content(&mut &b"bytes"[..]).unwrap();
    let cs = ChangeSet {
        changes: vec![
            put("E", Precondition::Absent, b"e", Some(0)),
            put("F", Precondition::Absent, b"f", Some(1)),
        ],
        content: vec![ContentBinding::from(&empty), ContentBinding::from(&full)],
        ..ChangeSet::default()
    };
    commit(&s, "both", cs).expect("both staged handles are valid");
    let mut v = s.begin().unwrap();
    for (id, want) in [("E", &b""[..]), ("F", &b"bytes"[..])] {
        let c = v
            .context(&ContextId::new(id.as_bytes().to_vec()).unwrap())
            .unwrap()
            .unwrap()
            .content
            .unwrap();
        let mut got = Vec::new();
        v.content(&c).unwrap().read_to_end(&mut got).unwrap();
        assert_eq!(got, want);
    }
}

#[test]
fn retained_history_reports_the_content_each_commit_bound() {
    let s = store();
    let staged = s.stage_content(&mut &b"payload"[..]).unwrap();
    let cs = ChangeSet {
        changes: vec![put("A", Precondition::Absent, b"a", Some(0))],
        content: vec![ContentBinding::from(&staged)],
        ..ChangeSet::default()
    };
    commit(&s, "c1", cs).unwrap();
    // Replace the record so the current state no longer refers to that content.
    commit(
        &s,
        "c2",
        changes(vec![put("A", Precondition::Any, b"a2", None)]),
    )
    .unwrap();
    let mut v = s.begin().unwrap();
    let h: Vec<_> = v
        .history(HistoryPosition(0))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    assert_eq!(
        h[0].content,
        vec![staged.content],
        "history lost the bound content reference"
    );
    assert!(h[1].content.is_empty());
    let mut got = Vec::new();
    v.content(&h[0].content[0])
        .unwrap()
        .read_to_end(&mut got)
        .unwrap();
    assert_eq!(got, b"payload");
}

#[test]
fn a_retry_with_an_unrepresentable_content_index_is_not_matched_to_the_original() {
    let s = store();
    let staged = s.stage_content(&mut &b"x"[..]).unwrap();
    let mk = |idx: usize| ChangeSet {
        changes: vec![put("A", Precondition::Absent, b"a", Some(idx))],
        content: vec![ContentBinding::from(&staged)],
        ..ChangeSet::default()
    };
    commit(&s, "same", mk(0)).unwrap();
    // Positive neighbour: the identical request returns the original receipt.
    assert_eq!(commit(&s, "same", mk(0)).unwrap(), 1);
    let r = commit(&s, "same", mk(1usize << 32));
    assert!(
        r.is_err(),
        "a different request under the same attempt identity was accepted: {r:?}"
    );
}

// ----- security review findings ------------------------------------------------------------------

use crate::format::{Descriptor, PAGE_SIZE, PageKind, PageRef, encode_page};
use crate::io::{FileId, Io};
use crate::records::{ExtentRef, P_CONTENT, key};
use crate::sim::Persist;
use crate::tree::{Node, NodeSink, TreeTxn};

fn sim_store(sim: &SimIo) -> NativeStore {
    NativeStore {
        inner: Inner::create(Box::new(sim.clone()), StoreId([3; 16]), Default::default()).unwrap(),
    }
}

fn reopen(sim: &SimIo) -> NativeStore {
    NativeStore {
        inner: Inner::open(Box::new(sim.clone()), Default::default()).expect("open"),
    }
}

#[test]
fn a_retry_after_reopen_acknowledges_only_durable_state() {
    // Count one commit's operations, then stop just before its publication barrier.
    let probe = SimIo::new();
    let p = sim_store(&probe);
    let before = probe.ops();
    commit(
        &p,
        "x",
        changes(vec![put("A", Precondition::Absent, b"a", None)]),
    )
    .unwrap();
    let per_commit = probe.ops() - before;

    let sim = SimIo::new();
    let s = sim_store(&sim);
    sim.crash_after(per_commit - 1);
    assert!(
        commit(
            &s,
            "x",
            changes(vec![put("A", Precondition::Absent, b"a", None)])
        )
        .is_err()
    );
    drop(s);
    // The process exited; the OS still holds the unsynced publication record.
    let cached = sim.after_process_exit();
    let s = reopen(&cached);
    let retry = commit(
        &s,
        "x",
        changes(vec![put("A", Precondition::Absent, b"a", None)]),
    );
    assert_eq!(
        retry.unwrap(),
        1,
        "identical retry returns the original receipt"
    );
    drop(s);
    // Power is lost after that acknowledgment: the acknowledged commit must survive.
    let after_power_loss = cached.after_crash(Persist::None);
    let s = reopen(&after_power_loss);
    assert!(
        revision(&s, "A").is_some(),
        "retry acknowledged a commit that was not durable"
    );
}

#[test]
fn a_history_extent_outside_committed_content_is_rejected_before_reading() {
    let sim = SimIo::new();
    let s = sim_store(&sim);
    commit(
        &s,
        "x",
        changes(vec![put("A", Precondition::Absent, b"a", None)]),
    )
    .unwrap();
    let bogus = ExtentRef {
        offset: 0,
        len: 1 << 20,
        digest: [0; 32],
    };
    let err = s.inner.read_extent(&bogus).unwrap_err();
    assert!(
        format!("{err:?}").contains("outside committed content"),
        "{err:?}"
    );
    let overflow = ExtentRef {
        offset: u64::MAX - 1,
        len: 16,
        digest: [0; 32],
    };
    assert!(
        format!("{:?}", s.inner.read_extent(&overflow).unwrap_err())
            .contains("outside committed content")
    );
}

#[test]
fn opening_rejects_cursors_that_contradict_the_store_files() {
    let sim = SimIo::new();
    let s = sim_store(&sim);
    let staged = s.stage_content(&mut &[5u8; 4096][..]).unwrap();
    let cs = ChangeSet {
        changes: vec![put("A", Precondition::Absent, b"a", Some(0))],
        content: vec![ContentBinding::from(&staged)],
        ..ChangeSet::default()
    };
    commit(&s, "x", cs).unwrap();
    drop(s);
    sim.release();
    // Positive neighbour: an intact store opens.
    drop(reopen(&sim));
    sim.release();
    // Committed content length beyond the content file: refuse rather than overwrite later.
    let short = sim.after_crash(Persist::All);
    short.truncate_for_test(FileId::Content, 100);
    let err = Inner::open(Box::new(short), Default::default())
        .err()
        .expect("open must fail");
    assert!(matches!(err, StorageError::Integrity(_)), "{err:?}");
}

#[test]
fn generation_exhaustion_is_rejected_without_losing_the_store() {
    let sim = SimIo::new();
    let s = sim_store(&sim);
    commit(
        &s,
        "x",
        changes(vec![put("A", Precondition::Absent, b"a", None)]),
    )
    .unwrap();
    let mut d = s.inner.snapshot().unwrap().desc;
    drop(s);
    // A publication record at the last representable generation.
    d.generation = u64::MAX;
    sim.write_at(
        FileId::Meta,
        Descriptor::slot(d.generation) * PAGE_SIZE as u64,
        &d.encode(),
    )
    .unwrap();
    sim.sync(FileId::Meta).unwrap();
    sim.release();
    let s = reopen(&sim);
    let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        commit(
            &s,
            "y",
            changes(vec![put("B", Precondition::Absent, b"b", None)]),
        )
    }))
    .expect("commit panicked at generation exhaustion");
    assert!(matches!(r, Err(CommitError::Rejected(_))), "{r:?}");
    assert!(revision(&s, "A").is_some());
}

#[test]
fn content_is_served_only_for_the_requested_digest() {
    let sim = SimIo::new();
    let s = sim_store(&sim);
    let b = s.stage_content(&mut &b"bytes of B"[..]).unwrap();
    let cs = ChangeSet {
        changes: vec![put("B", Precondition::Absent, b"b", Some(0))],
        content: vec![ContentBinding::from(&b)],
        ..ChangeSet::default()
    };
    commit(&s, "x", cs).unwrap();
    // Crafted metadata maps a different digest of the same length to B's extent.
    let cur = s.inner.snapshot().unwrap().desc;
    let bx = crate::tree::get(&*s.inner, cur.root, &key(P_CONTENT, &[&b.content.digest.0]))
        .unwrap()
        .unwrap();
    let forged = [0xAB; 32];
    let mut t = TreeTxn::new(&*s.inner, cur.root);
    t.put(key(P_CONTENT, &[&forged]), bx).unwrap();
    struct Sink<'a>(&'a SimIo, u64, u64);
    impl NodeSink for Sink<'_> {
        fn write(&mut self, n: &Node) -> Result<PageRef, StorageError> {
            let at = PageRef {
                page: self.1,
                incarnation: self.2,
            };
            self.0.write_at(
                FileId::Meta,
                at.page * PAGE_SIZE as u64,
                &encode_page(StoreId([3; 16]), PageKind::Node, at, &n.encode())?,
            )?;
            self.1 += 1;
            Ok(at)
        }
    }
    let mut sink = Sink(&sim, cur.next_page, cur.generation + 1);
    let root = t.finish(&mut sink).unwrap();
    let d = Descriptor {
        generation: cur.generation + 1,
        root,
        next_page: sink.1,
        ..cur
    };
    sim.write_at(
        FileId::Meta,
        Descriptor::slot(d.generation) * PAGE_SIZE as u64,
        &d.encode(),
    )
    .unwrap();
    sim.sync(FileId::Meta).unwrap();
    drop(s);
    sim.release();
    let s = reopen(&sim);
    let mut v = s.begin().unwrap();
    let want = scientdb_kernel::storage::ContentRef {
        digest: scientdb_kernel::storage::ContentDigest(forged),
        len: b.content.len,
    };
    let served = v.content(&want).map(|mut r| {
        let mut out = Vec::new();
        r.read_to_end(&mut out).map(|_| out)
    });
    assert!(
        !matches!(served, Ok(Ok(_))),
        "served other content under a forged digest: {served:?}"
    );
    // Positive neighbour: the genuine reference still reads.
    let mut out = Vec::new();
    v.content(&b.content)
        .unwrap()
        .read_to_end(&mut out)
        .unwrap();
    assert_eq!(out, b"bytes of B");
}

#[test]
fn a_store_in_the_previous_format_version_is_refused_without_modification() {
    let sim = SimIo::new();
    let s = sim_store(&sim);
    commit(
        &s,
        "x",
        changes(vec![put("A", Precondition::Absent, b"a", None)]),
    )
    .unwrap();
    let d = s.inner.snapshot().unwrap().desc;
    drop(s);
    // Rewrite the publication record as format version 1 (the single-leaf node encoding).
    let slot = Descriptor::slot(d.generation) * PAGE_SIZE as u64;
    let mut rec = d.encode();
    rec[4..6].copy_from_slice(&1u16.to_le_bytes());
    // magic 4 + version 2 + store 16 + six u64 fields = 70 bytes, then the CRC-32.
    let crc = crc32fast::hash(&rec[..70]);
    rec[70..74].copy_from_slice(&crc.to_le_bytes());
    sim.write_at(FileId::Meta, slot, &rec).unwrap();
    sim.sync(FileId::Meta).unwrap();
    let mut before = vec![0; sim.len(FileId::Meta).unwrap() as usize];
    sim.read_at(FileId::Meta, 0, &mut before).unwrap();
    sim.release();
    let err = Inner::open(Box::new(sim.clone()), Default::default())
        .err()
        .expect("an older format version opened");
    assert!(matches!(err, StorageError::Unsupported(_)), "{err:?}");
    let mut after = vec![0; before.len()];
    sim.read_at(FileId::Meta, 0, &mut after).unwrap();
    assert_eq!(before, after, "refusing the store modified it");
}

#[test]
fn node_references_outside_committed_metadata_are_refused_before_reading() {
    use crate::tree::NodeSource;
    let sim = SimIo::new();
    let s = sim_store(&sim);
    commit(
        &s,
        "x",
        changes(vec![put("A", Precondition::Absent, b"a", None)]),
    )
    .unwrap();
    let d = s.inner.snapshot().unwrap().desc;
    let bad = [
        PageRef {
            page: d.next_page,
            incarnation: d.generation + 1,
        },
        PageRef {
            page: d.root.page,
            incarnation: d.generation + 1,
        },
        PageRef {
            page: 0,
            incarnation: 1,
        },
        PageRef {
            page: u64::MAX / 2,
            incarnation: 1,
        },
    ];
    for at in bad {
        let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| s.inner.load(at)));
        let r = r.unwrap_or_else(|_| panic!("loading {at:?} panicked"));
        assert!(
            matches!(&r, Err(StorageError::Integrity(m)) if m.contains("not committed")),
            "{at:?}: {r:?}"
        );
    }
    // Positive neighbour: the committed root loads.
    assert!(s.inner.load(d.root).is_ok());
}

#[test]
fn a_commit_publishes_only_content_covered_by_its_data_barrier() {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, OnceLock, Weak};

    use crate::io::{FileId, Io};

    // Stages more content right after the commit's content barrier, as a concurrent staging
    // call could: those bytes are not durable and must not be published.
    struct StageAfterContentSync {
        sim: SimIo,
        store: Arc<OnceLock<Weak<Inner>>>,
        fired: AtomicBool,
    }
    impl Io for StageAfterContentSync {
        fn create(&self) -> Result<(), StorageError> {
            self.sim.create()
        }
        fn open(&self) -> Result<(), StorageError> {
            self.sim.open()
        }
        fn read_at(&self, file: FileId, offset: u64, buf: &mut [u8]) -> Result<(), StorageError> {
            self.sim.read_at(file, offset, buf)
        }
        fn write_at(&self, file: FileId, offset: u64, data: &[u8]) -> Result<(), StorageError> {
            self.sim.write_at(file, offset, data)
        }
        fn len(&self, file: FileId) -> Result<u64, StorageError> {
            self.sim.len(file)
        }
        fn sync(&self, file: FileId) -> Result<(), StorageError> {
            self.sim.sync(file)?;
            if file == FileId::Content
                && let Some(inner) = self.store.get().and_then(Weak::upgrade)
                && !self.fired.swap(true, Ordering::SeqCst)
            {
                inner.stage(&mut &[9u8; 100][..])?;
            }
            Ok(())
        }
        fn sync_dir(&self) -> Result<(), StorageError> {
            self.sim.sync_dir()
        }
    }

    let sim = SimIo::new();
    let hook = Arc::new(OnceLock::new());
    let s = NativeStore {
        inner: Inner::create(
            Box::new(StageAfterContentSync {
                sim: sim.clone(),
                store: hook.clone(),
                fired: AtomicBool::new(false),
            }),
            StoreId([3; 16]),
            Default::default(),
        )
        .unwrap(),
    };
    hook.set(Arc::downgrade(&s.inner)).unwrap();
    commit(
        &s,
        "x",
        changes(vec![put("A", Precondition::Absent, b"a", None)]),
    )
    .expect("commit acknowledged");
    drop(s);
    // Power is lost: unsynced writes vanish. The acknowledged commit must still open.
    let after_power_loss = sim.after_crash(Persist::None);
    let s = reopen(&after_power_loss);
    assert!(revision(&s, "A").is_some());
}
