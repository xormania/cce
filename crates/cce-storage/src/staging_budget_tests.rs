//! The aggregate staging budget: the total content length of outstanding stagings is bounded; a
//! staging that would exceed it fails explicitly and changes nothing, existing state stays
//! readable and committable, and committing or discarding releases budget.

use std::io::Read;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use cce_core::storage::{
    Change, ChangeSet, CommitError, ContentBinding, ContentRef, Precondition, ReadView,
    StagedContent, StorageEngine, StorageError,
};

use crate::NativeStore;
use crate::engine::{CONTENT_CHUNK, Options};
use crate::format::{PAGE_SIZE, StoreId};
use crate::io::{FileId, Io};
use crate::sim::SimIo;
use crate::test_support::{attempt, changes, cid, create_on, open_on, put_context};

const STORE: StoreId = StoreId(*b"staging-budget!!");
const CHUNK: u64 = CONTENT_CHUNK as u64;

fn options(budget: u64) -> Options {
    Options {
        max_staged_bytes: budget,
        ..Options::default()
    }
}

fn create(io: Box<dyn Io>, budget: u64) -> NativeStore {
    create_on(io, STORE, options(budget)).unwrap()
}

fn bytes_of(seed: u8, len: u64) -> Vec<u8> {
    (0..len)
        .map(|i| (i as u8).wrapping_mul(29) ^ seed)
        .collect()
}

fn stage(s: &NativeStore, bytes: &[u8]) -> Result<StagedContent, StorageError> {
    s.stage_content(&mut &bytes[..])
}

fn staged(s: &NativeStore, bytes: &[u8]) -> StagedContent {
    stage(s, bytes).unwrap_or_else(|e| panic!("a staging within the budget must succeed: {e:?}"))
}

fn binding(id: &str, staged: &StagedContent) -> ChangeSet {
    let mut cs = changes(vec![put_context(id, Precondition::Any, b"body", Some(0))]);
    cs.content.push(ContentBinding::from(staged));
    cs
}

fn exhausted(r: &Result<StagedContent, StorageError>) -> bool {
    matches!(r, Err(StorageError::ResourceExhausted(_)))
}

fn read(s: &NativeStore, id: &str) -> Option<Vec<u8>> {
    let mut v = s.begin().unwrap();
    let r = v.context(&cid(id)).unwrap()?;
    let mut out = Vec::new();
    v.content(&r.content.unwrap())
        .unwrap()
        .read_to_end(&mut out)
        .unwrap();
    Some(out)
}

#[test]
fn a_staging_over_the_budget_is_refused_explicitly_and_changes_nothing() {
    let sim = SimIo::new();
    let s = create(Box::new(sim.clone()), 3 * CHUNK);
    let committed = bytes_of(1, CHUNK);
    let c = staged(&s, &committed);
    s.commit(attempt("c"), s.begin().unwrap(), binding("C", &c))
        .unwrap_or_else(|e| panic!("commit c: {e:?}"));
    let a = staged(&s, &bytes_of(2, 2 * CHUNK));
    assert_eq!(s.stats().unwrap().staged_bytes, 2 * CHUNK);

    let content_end = sim.len(FileId::Content).unwrap();
    let ledger = s.inner.ledger_check_for_test();
    let r = stage(&s, &bytes_of(3, 2 * CHUNK));
    assert!(exhausted(&r), "over the budget: {r:?}");
    let Err(StorageError::ResourceExhausted(why)) = &r else {
        unreachable!()
    };
    for part in [
        format!("{}", 3 * CHUNK),
        format!("{}", 2 * CHUNK),
        "commit".into(),
        "discard".into(),
        "reopen".into(),
    ] {
        assert!(why.contains(&part), "the message must name {part:?}: {why}");
    }
    // Nothing of the refused staging remains: no staged share, no change to the reuse pool, and
    // the bytes it wrote before it was refused are not kept: the next staging starts where it
    // started.
    assert_eq!(s.stats().unwrap().staged_bytes, 2 * CHUNK, "staged share");
    assert_eq!(s.inner.ledger_check_for_test(), ledger, "reuse pool");
    crate::audit::check_retirement(&sim).unwrap_or_else(|e| panic!("after the refusal: {e}"));
    let d = staged(&s, &bytes_of(4, CHUNK));
    s.commit(attempt("d"), s.begin().unwrap(), binding("D", &d))
        .unwrap_or_else(|e| panic!("commit d: {e:?}"));
    let segments = s
        .inner
        .content_segments(&s.inner.snapshot().unwrap(), &d.content)
        .unwrap();
    assert_eq!(
        segments.first().map(|x| x.offset),
        Some(content_end),
        "the refused staging's bytes were kept"
    );

    // Existing state stays accessible and writable; the outstanding staging still commits.
    assert_eq!(read(&s, "C"), Some(committed), "committed content");
    s.commit(
        attempt("plain"),
        s.begin().unwrap(),
        changes(vec![put_context("P", Precondition::Absent, b"p", None)]),
    )
    .unwrap_or_else(|e| panic!("a commit staging nothing: {e:?}"));
    s.commit(attempt("a"), s.begin().unwrap(), binding("A", &a))
        .unwrap_or_else(|e| panic!("the outstanding staging must commit: {e:?}"));
    assert_eq!(read(&s, "A"), Some(bytes_of(2, 2 * CHUNK)));
    // Committing it released its share: the same staging fits now.
    assert_eq!(s.stats().unwrap().staged_bytes, 0);
    staged(&s, &bytes_of(3, 2 * CHUNK));
}

#[test]
fn the_budget_holds_exactly_its_size() {
    let s = create(Box::new(SimIo::new()), 2 * CHUNK + 10);
    let _a = staged(&s, &bytes_of(1, CHUNK));
    let over = stage(&s, &bytes_of(2, CHUNK + 11));
    assert!(exhausted(&over), "one byte over: {over:?}");
    let _b = staged(&s, &bytes_of(2, CHUNK + 10));
    assert_eq!(s.stats().unwrap().staged_bytes, 2 * CHUNK + 10);
    let empty = stage(&s, &[]);
    assert!(empty.is_ok(), "empty content takes no budget: {empty:?}");
    let one = stage(&s, &[1]);
    assert!(exhausted(&one), "a full budget refuses one byte: {one:?}");
}

/// A source that must never be read.
struct Untouchable(Arc<AtomicBool>);

impl Read for Untouchable {
    fn read(&mut self, _: &mut [u8]) -> std::io::Result<usize> {
        self.0.store(true, Ordering::SeqCst);
        Ok(0)
    }
}

#[test]
fn a_declared_length_over_the_budget_is_refused_before_the_source_is_read() {
    let s = create(Box::new(SimIo::new()), CHUNK);
    let read = Arc::new(AtomicBool::new(false));
    let declared = ContentRef {
        digest: cce_core::storage::ContentDigest([7; 32]),
        len: CHUNK + 1,
    };
    let r = s.stage_content_expecting(&mut Untouchable(read.clone()), &declared);
    assert!(exhausted(&r), "declared over the budget: {r:?}");
    assert!(!read.load(Ordering::SeqCst), "the source was read");
}

#[test]
fn discarding_releases_budget_and_a_rejected_commit_keeps_it() {
    let s = create(Box::new(SimIo::new()), 2 * CHUNK);
    let a = staged(&s, &bytes_of(1, 2 * CHUNK));
    assert!(exhausted(&stage(&s, &bytes_of(2, 1))));
    // A commit rejected for a conflict keeps its staging outstanding.
    let held = s.begin().unwrap();
    s.commit(
        attempt("first"),
        s.begin().unwrap(),
        changes(vec![put_context("A", Precondition::Absent, b"x", None)]),
    )
    .unwrap();
    let mut cs = binding("A", &a);
    if let Change::PutContext { expect, .. } = &mut cs.changes[0] {
        *expect = Precondition::Absent;
    }
    let r = s.commit(attempt("loses"), held, cs);
    assert!(matches!(r, Err(CommitError::Conflict(_))), "{r:?}");
    assert_eq!(
        s.stats().unwrap().staged_bytes,
        2 * CHUNK,
        "rejected commit"
    );
    assert!(exhausted(&stage(&s, &bytes_of(2, 1))));
    s.discard_staged(a).unwrap();
    assert_eq!(s.stats().unwrap().staged_bytes, 0, "discarded");
    staged(&s, &bytes_of(2, 2 * CHUNK));
}

#[test]
fn a_retry_of_a_committed_attempt_is_never_refused_but_restaging_waits_for_budget() {
    let s = create(Box::new(SimIo::new()), 2 * CHUNK);
    let bytes = bytes_of(1, CHUNK);
    let a = staged(&s, &bytes);
    let request = binding("A", &a);
    let receipt = s
        .commit(attempt("a"), s.begin().unwrap(), request.clone())
        .unwrap();
    // Fill the budget with another outstanding staging.
    let other = staged(&s, &bytes_of(2, 2 * CHUNK));
    // The retry with the original request needs no staging: it returns the receipt.
    let retry = s.commit(attempt("a"), s.begin().unwrap(), request);
    assert_eq!(retry, Ok(receipt.clone()), "a retry under a full budget");
    // A host that restages the identical bytes is refused until budget is released.
    assert!(
        exhausted(&stage(&s, &bytes)),
        "restaging under a full budget"
    );
    s.discard_staged(other).unwrap();
    let again = staged(&s, &bytes);
    let retry = s.commit(attempt("a"), s.begin().unwrap(), binding("A", &again));
    assert_eq!(retry, Ok(receipt), "a retry with restaged bytes");
}

/// Serves `data`, and discards `discard` through `inner` when the second segment's bytes are first
/// asked for: in the middle of the staging that reads it.
struct DiscardingSource {
    inner: Arc<crate::engine::Inner>,
    discard: Option<StagedContent>,
    data: Vec<u8>,
    at: usize,
}

impl Read for DiscardingSource {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if self.at >= CONTENT_CHUNK
            && let Some(d) = self.discard.take()
        {
            self.inner
                .discard_staged(d)
                .unwrap_or_else(|e| panic!("the discard during the staging: {e:?}"));
        }
        let n = buf.len().min(self.data.len() - self.at);
        buf[..n].copy_from_slice(&self.data[self.at..self.at + n]);
        self.at += n;
        Ok(n)
    }
}

#[test]
fn a_discard_during_a_staging_releases_budget_for_it() {
    let s = create(Box::new(SimIo::new()), 2 * CHUNK);
    let a = staged(&s, &bytes_of(1, CHUNK));
    // The first segment fits beside A; the second fits only once A is discarded.
    let data = bytes_of(2, 2 * CHUNK);
    let mut source = DiscardingSource {
        inner: s.inner.clone(),
        discard: Some(a),
        data: data.clone(),
        at: 0,
    };
    let b = s.stage_content(&mut source);
    assert!(source.discard.is_none(), "the source never discarded A");
    let b = b.unwrap_or_else(|e| panic!("budget released mid-stream was not counted: {e:?}"));
    assert_eq!(b.content.len, 2 * CHUNK);
    assert_eq!(s.stats().unwrap().staged_bytes, 2 * CHUNK);
}

/// I/O over a simulator that fails the metadata sync after the next publication record write,
/// once armed: that publication's outcome is uncertain.
struct FailRecordSync {
    sim: SimIo,
    armed: Arc<AtomicBool>,
    fail: Arc<AtomicBool>,
}

impl Io for FailRecordSync {
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
        if file == FileId::Meta
            && offset < 2 * PAGE_SIZE as u64
            && self.armed.swap(false, Ordering::SeqCst)
        {
            self.fail.store(true, Ordering::SeqCst);
        }
        self.sim.write_at(file, offset, data)
    }
    fn len(&self, file: FileId) -> Result<u64, StorageError> {
        self.sim.len(file)
    }
    fn sync(&self, file: FileId) -> Result<(), StorageError> {
        if file == FileId::Meta && self.fail.swap(false, Ordering::SeqCst) {
            return Err(StorageError::Io("injected: record sync failed".into()));
        }
        self.sim.sync(file)
    }
    fn sync_dir(&self) -> Result<(), StorageError> {
        self.sim.sync_dir()
    }
}

#[test]
fn an_uncertain_publication_holds_its_stagings_budget_until_reopen() {
    let sim = SimIo::new();
    let armed = Arc::new(AtomicBool::new(false));
    let io = FailRecordSync {
        sim: sim.clone(),
        armed: armed.clone(),
        fail: Arc::new(AtomicBool::new(false)),
    };
    let s = create(Box::new(io), 2 * CHUNK);
    let a = staged(&s, &bytes_of(1, 2 * CHUNK));
    armed.store(true, Ordering::SeqCst);
    let r = s.commit(attempt("a"), s.begin().unwrap(), binding("A", &a));
    assert!(matches!(r, Err(CommitError::Indeterminate(_))), "{r:?}");
    assert_eq!(
        s.stats().unwrap().staged_bytes,
        2 * CHUNK,
        "the uncertain commit's staging stays outstanding"
    );
    // Writes are fenced now; the budget itself is only freed by reopening.
    drop(s);
    sim.release();
    let s = open_on(Box::new(sim.clone()), options(2 * CHUNK)).unwrap();
    assert_eq!(s.stats().unwrap().staged_bytes, 0, "after reopening");
    staged(&s, &bytes_of(2, 2 * CHUNK));
}
