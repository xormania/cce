//! Engine behavior tests through the logical port (simulated I/O unless stated).

use crate::test_support::{attempt, create_on, open_on};

use std::io::Read;

use cce_core::storage::{
    Change, ChangeSet, CommitError, ContentBinding, ContextId, HistoryPosition,
    Precondition, ReadView, Revision, StorageEngine, StorageError,
};

use crate::NativeStore;
use crate::engine::Inner;
use crate::format::StoreId;
use crate::sim::SimIo;

fn store() -> NativeStore {
    create_on(Box::new(SimIo::new()), StoreId([3; 16]), Default::default()).unwrap()
}

fn put(id: &str, expect: Precondition, body: &[u8], content: Option<usize>) -> Change {
    Change::PutContext {
        id: ContextId::new(id.as_bytes().to_vec()).unwrap(),
        expect,
        body: body.to_vec(),
        content,
    }
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
    create_on(Box::new(sim.clone()), StoreId([3; 16]), Default::default()).unwrap()
}

fn reopen(sim: &SimIo) -> NativeStore {
    open_on(Box::new(sim.clone()), Default::default()).expect("open")
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
    let want = cce_core::storage::ContentRef {
        digest: cce_core::storage::ContentDigest(forged),
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
fn history_position_exhaustion_is_rejected_without_wrapping() {
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
    // The next generation's record claims the last representable history position.
    d.generation += 1;
    d.history_pos = u64::MAX;
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
    .expect("commit panicked at history exhaustion");
    assert!(
        matches!(&r, Err(CommitError::Rejected(StorageError::InvalidRequest(m))) if m.contains("overflow")),
        "{r:?}"
    );
    assert!(
        revision(&s, "A").is_some(),
        "existing state must stay readable"
    );
}

#[test]
fn an_oversized_manifest_reference_is_refused_before_allocation() {
    // A small segment quota keeps the largest valid manifest small, so a crafted reference only a
    // little beyond it (backed by real bytes, within committed content) shows the size bound.
    let opts = crate::engine::Options {
        max_content_segments: 4,
        ..Default::default()
    };
    let sim = SimIo::new();
    let s = create_on(Box::new(sim.clone()), StoreId([3; 16]), opts).unwrap();
    let staged = s.stage_content(&mut &b"bytes"[..]).unwrap();
    let cs = ChangeSet {
        changes: vec![put("B", Precondition::Absent, b"b", Some(0))],
        content: vec![ContentBinding::from(&staged)],
        ..ChangeSet::default()
    };
    commit(&s, "x", cs).unwrap();
    let cur = s.inner.snapshot().unwrap().desc;
    // Crafted metadata: the content record points at a manifest far larger than any valid one.
    let filler = vec![7u8; 1 << 20];
    sim.write_at(FileId::Content, cur.content_len, &filler)
        .unwrap();
    sim.sync(FileId::Content).unwrap();
    let huge = ExtentRef {
        offset: 0,
        len: filler.len() as u64,
        digest: [0; 32],
    };
    let mut t = TreeTxn::new(&*s.inner, cur.root);
    t.put(key(P_CONTENT, &[&staged.content.digest.0]), huge.encode())
        .unwrap();
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
    // Enough committed content that the bounds check alone would not stop the read.
    let d = Descriptor {
        generation: cur.generation + 1,
        root,
        next_page: sink.1,
        content_len: cur.content_len + filler.len() as u64,
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
    let s = open_on(Box::new(sim.clone()), opts).expect("open");
    let r = s.begin().unwrap().content(&staged.content).map(|_| ());
    assert!(
        matches!(&r, Err(StorageError::Integrity(m)) if m.contains("manifest") && m.contains("limit")),
        "{r:?}"
    );
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
        fired: Arc<AtomicBool>,
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
    let fired = Arc::new(AtomicBool::new(false));
    let s = NativeStore {
        inner: Inner::create(
            Box::new(StageAfterContentSync {
                sim: sim.clone(),
                store: hook.clone(),
                fired: fired.clone(),
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
    assert!(
        fired.load(Ordering::SeqCst),
        "the staging after the content barrier did not happen"
    );
    drop(s);
    // Power is lost: unsynced writes vanish. The acknowledged commit must still open.
    let after_power_loss = sim.after_crash(Persist::None);
    let s = reopen(&after_power_loss);
    assert!(revision(&s, "A").is_some());
}

#[test]
fn a_page_counter_whose_byte_offset_is_unrepresentable_refuses_the_commit() {
    let s = store();
    commit(
        &s,
        "a",
        changes(vec![put("A", Precondition::Absent, b"a", None)]),
    )
    .unwrap();
    // The next node page would lie beyond the largest representable file offset.
    s.inner
        .set_cursors_for_test(Some(u64::MAX / PAGE_SIZE as u64 + 1), None);
    let r = commit(
        &s,
        "b",
        changes(vec![put("B", Precondition::Absent, b"b", None)]),
    );
    assert!(
        matches!(&r, Err(CommitError::Rejected(StorageError::InvalidRequest(m))) if m.contains("exhausted")),
        "{r:?}"
    );
    assert!(revision(&s, "A").is_some(), "existing state stays readable");
    assert!(
        revision(&s, "B").is_none(),
        "nothing from the refused commit is visible"
    );
}

#[test]
fn a_content_cursor_near_its_limit_refuses_staging_without_wrapping() {
    let s = store();
    s.inner.set_cursors_for_test(None, Some(u64::MAX - 50));
    let r = s.stage_content(&mut &[1u8; 100][..]).map(|_| ());
    assert!(
        matches!(&r, Err(StorageError::InvalidRequest(m)) if m.contains("exhausted")),
        "{r:?}"
    );
    // A manifest that would cross the limit is refused too (an empty stream writes only a manifest).
    s.inner.set_cursors_for_test(None, Some(u64::MAX - 20));
    let r = s.stage_content(&mut &[][..]).map(|_| ());
    assert!(
        matches!(&r, Err(StorageError::InvalidRequest(m)) if m.contains("exhausted")),
        "{r:?}"
    );
}

#[test]
fn a_history_extent_that_would_cross_the_content_limit_refuses_the_commit() {
    let s = store();
    s.inner.set_cursors_for_test(None, Some(u64::MAX - 8));
    let r = commit(
        &s,
        "a",
        changes(vec![put("A", Precondition::Absent, b"a", None)]),
    );
    assert!(
        matches!(&r, Err(CommitError::Rejected(StorageError::InvalidRequest(m))) if m.contains("exhausted")),
        "{r:?}"
    );
    assert!(revision(&s, "A").is_none());
}

/// Operations one small content-free commit performs (its last four: content sync, metadata sync,
/// publication record write, publication sync).
fn ops_per_small_commit() -> u64 {
    let probe = SimIo::new();
    let p = sim_store(&probe);
    commit(
        &p,
        "p1",
        changes(vec![put("A", Precondition::Absent, b"a", None)]),
    )
    .unwrap();
    let before = probe.ops();
    commit(
        &p,
        "p2",
        changes(vec![put("B", Precondition::Absent, b"b", None)]),
    )
    .unwrap();
    probe.ops() - before
}

#[test]
fn a_fenced_store_still_answers_definitively_for_attempts_it_can_see() {
    use cce_core::storage::{AttemptOutcome, CommitReceipt};
    let per_commit = ops_per_small_commit();
    let sim = SimIo::new();
    let s = sim_store(&sim);
    let a1 = changes(vec![put("A", Precondition::Absent, b"a", None)]);
    let b = || changes(vec![put("B", Precondition::Absent, b"b", None)]);
    commit(&s, "a1", a1.clone()).unwrap();
    sim.fail_nth_op(per_commit);
    let r = commit(&s, "a2", b());
    assert!(matches!(r, Err(CommitError::Indeterminate(_))), "{r:?}");

    assert_eq!(
        s.outcome(&attempt("a1")).unwrap(),
        AttemptOutcome::Committed(CommitReceipt {
            attempt: attempt("a1"),
            position: HistoryPosition(1),
        }),
        "a commit published before the fence is still definitive"
    );
    assert_eq!(s.outcome(&attempt("a2")).unwrap(), AttemptOutcome::Unknown);
    assert_eq!(
        s.outcome(&attempt("never")).unwrap(),
        AttemptOutcome::Absent
    );

    // A committed attempt keeps its receipt; a different request under its id is a mismatch.
    assert_eq!(commit(&s, "a1", a1).unwrap(), 1);
    let r = commit(
        &s,
        "a1",
        changes(vec![put("A", Precondition::Absent, b"other", None)]),
    );
    assert!(matches!(r, Err(CommitError::AttemptMismatch)), "{r:?}");
    // The uncertain attempt stays uncertain: never reported as rejected.
    let r = commit(&s, "a2", b());
    assert!(matches!(r, Err(CommitError::Indeterminate(_))), "{r:?}");
    // A new attempt is refused without effect while fenced.
    let r = commit(
        &s,
        "a3",
        changes(vec![put("C", Precondition::Absent, b"c", None)]),
    );
    assert!(
        matches!(&r, Err(CommitError::Rejected(StorageError::StoreState(m))) if m.contains("fenced")),
        "{r:?}"
    );
    assert!(revision(&s, "C").is_none());
}

#[test]
fn a_failed_data_barrier_fences_writes() {
    use cce_core::storage::AttemptOutcome;
    let per_commit = ops_per_small_commit();
    let sim = SimIo::new();
    let s = sim_store(&sim);
    commit(
        &s,
        "a1",
        changes(vec![put("A", Precondition::Absent, b"a", None)]),
    )
    .unwrap();
    // Fail the content sync of the next commit's data barrier.
    sim.fail_nth_op(per_commit - 3);
    let r = commit(
        &s,
        "a2",
        changes(vec![put("B", Precondition::Absent, b"b", None)]),
    );
    assert!(
        matches!(r, Err(CommitError::Rejected(StorageError::Io(_)))),
        "{r:?}"
    );
    // Nothing was published for it.
    assert_eq!(s.outcome(&attempt("a2")).unwrap(), AttemptOutcome::Absent);
    // Bytes the failed barrier covered may be lost: no later write may rely on them.
    let r = commit(
        &s,
        "a3",
        changes(vec![put("C", Precondition::Absent, b"c", None)]),
    );
    assert!(
        matches!(&r, Err(CommitError::Rejected(StorageError::StoreState(m))) if m.contains("fenced")),
        "{r:?}"
    );
    assert!(s.stage_content(&mut &b"x"[..]).is_err());
    assert!(revision(&s, "A").is_some());
}

#[test]
fn request_fields_beyond_the_encodable_limit_are_refused_and_the_store_keeps_committing() {
    let s = NativeStore {
        inner: Inner::create(
            Box::new(SimIo::new()),
            StoreId([3; 16]),
            crate::engine::Options {
                max_request_field: 8,
                ..Default::default()
            },
        )
        .unwrap(),
    };
    let evidence = |label: &[u8], bytes: &[u8]| ChangeSet {
        changes: vec![put("E", Precondition::Any, b"e", None)],
        evidence: vec![cce_core::storage::Evidence {
            label: label.to_vec(),
            bytes: bytes.to_vec(),
        }],
        ..ChangeSet::default()
    };
    let too_long = [
        (
            "body",
            changes(vec![put("A", Precondition::Absent, &[1; 9], None)]),
        ),
        ("label", evidence(&[1; 9], b"ok")),
        ("evidence bytes", evidence(b"ok", &[1; 9])),
    ];
    for (what, cs) in too_long {
        let r = commit(&s, what, cs);
        assert!(
            matches!(&r, Err(CommitError::Rejected(StorageError::InvalidRequest(m))) if m.contains("limit")),
            "{what}: {r:?}"
        );
    }
    assert!(revision(&s, "A").is_none() && revision(&s, "E").is_none());
    // Fields at the limit commit, so the commit path is still serving requests.
    assert_eq!(commit(&s, "fits", evidence(&[1; 8], &[2; 8])).unwrap(), 1);
}

#[test]
fn request_lists_beyond_the_limit_are_refused_before_encoding() {
    use crate::records::check_request_bounds;
    let ev = || cce_core::storage::Evidence {
        label: b"l".to_vec(),
        bytes: b"b".to_vec(),
    };
    let three_changes = changes(vec![
        put("A", Precondition::Any, b"a", None),
        put("B", Precondition::Any, b"b", None),
        put("C", Precondition::Any, b"c", None),
    ]);
    let three_evidence = ChangeSet {
        evidence: vec![ev(), ev(), ev()],
        ..ChangeSet::default()
    };
    for (what, cs) in [
        ("change list", three_changes),
        ("evidence list", three_evidence),
    ] {
        let r = check_request_bounds(&cs, 2, u64::MAX);
        assert!(
            matches!(&r, Err(StorageError::InvalidRequest(m)) if m.starts_with(what)),
            "{what}: {r:?}"
        );
        assert!(
            check_request_bounds(&cs, 3, u64::MAX).is_ok(),
            "{what} at the limit"
        );
    }
}

#[test]
fn a_request_whose_fields_fit_but_whose_whole_encoding_does_not_is_refused() {
    let s = NativeStore {
        inner: Inner::create(
            Box::new(SimIo::new()),
            StoreId([3; 16]),
            crate::engine::Options {
                max_request_len: 200,
                ..Default::default()
            },
        )
        .unwrap(),
    };
    let with_evidence = |n: usize| ChangeSet {
        changes: vec![put("E", Precondition::Any, b"e", None)],
        evidence: (0..n)
            .map(|_| cce_core::storage::Evidence {
                label: b"l".to_vec(),
                bytes: vec![7; 100],
            })
            .collect(),
        ..ChangeSet::default()
    };
    // Every field is within the limit, but the encoded request as a whole is not: it is stored
    // as one length-prefixed field of the history entry.
    let r = commit(&s, "two", with_evidence(2));
    assert!(
        matches!(&r, Err(CommitError::Rejected(StorageError::InvalidRequest(m))) if m.starts_with("encoded request")),
        "{r:?}"
    );
    assert!(revision(&s, "E").is_none());
    // Positive neighbour: one such field keeps the whole request within the limit.
    assert_eq!(commit(&s, "one", with_evidence(1)).unwrap(), 1);
}

#[test]
fn the_computed_request_length_matches_the_encoding_for_every_change_shape() {
    use crate::records::{encode_request, encoded_request_len};
    use cce_core::storage::{
        ContentBinding, ContentDigest, ContentRef, Evidence, Revision, StagingToken,
    };
    let id = |n: usize| ContextId::new(vec![b'c'; n]).unwrap();
    let rid = |n: usize| cce_core::storage::RelationshipId::new(vec![b'r'; n]).unwrap();
    let kind = cce_core::storage::RelationshipKind::new(b"k".to_vec()).unwrap();
    let preconditions = [
        Precondition::Any,
        Precondition::Absent,
        Precondition::AtRevision(Revision(u64::MAX)),
    ];
    let mut checked = 0;
    for p in &preconditions {
        for content in [None, Some(0), Some(usize::MAX)] {
            for n in [1, 255] {
                let changes = [
                    Change::PutContext {
                        id: id(n),
                        expect: *p,
                        body: vec![1; n],
                        content,
                    },
                    Change::RemoveContext {
                        id: id(n),
                        expect: *p,
                    },
                    Change::PutRelationship {
                        id: rid(n),
                        expect: *p,
                        kind: kind.clone(),
                        source: id(n),
                        target: id(1),
                    },
                    Change::RemoveRelationship {
                        id: rid(n),
                        expect: *p,
                    },
                ];
                for take in 0..=changes.len() {
                    for evidence in 0..=2 {
                        let cs = ChangeSet {
                            changes: changes[..take].to_vec(),
                            content: (0..evidence)
                                .map(|i| ContentBinding {
                                    token: StagingToken(vec![i as u8]),
                                    content: ContentRef {
                                        digest: ContentDigest([i as u8; 32]),
                                        len: i as u64,
                                    },
                                })
                                .collect(),
                            evidence: (0..evidence)
                                .map(|i| Evidence {
                                    label: vec![2; i],
                                    bytes: vec![3; n],
                                })
                                .collect(),
                        };
                        assert_eq!(
                            encoded_request_len(&cs),
                            encode_request(&cs).len() as u64,
                            "{cs:?}"
                        );
                        checked += 1;
                    }
                }
            }
        }
    }
    assert_eq!(checked, 3 * 3 * 2 * 5 * 3);
}
