//! Recovery and open tests: failed opens, interrupted store creation, and the ordering between
//! publication durability and visibility.
//!
//! Expected states come from an independent run of the same logical requests, never from what
//! recovery chose.

use std::io::Read;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock, Weak};

use cce_core::storage::{
    AttemptId, AttemptOutcome, Change, ChangeSet, CommitError, ContentBinding, ContextId,
    Direction, Evidence, HistoryPosition, Precondition, ReadView, RelationshipId, RelationshipKind,
    RelationshipQuery, StagedContent, StorageEngine, StorageError,
};

use crate::NativeStore;
use crate::engine::Inner;
use crate::format::{PAGE_SIZE, StoreId};
use crate::io::{FileId, Io, Op, Traced};
use crate::sim::{Persist, SimIo};

const STORE: StoreId = StoreId(*b"recovery-store-1");
const IDS: [&str; 4] = ["A", "B", "C", "D"];

fn cid(s: &str) -> ContextId {
    ContextId::new(s.as_bytes().to_vec()).unwrap()
}

fn create(io: Box<dyn Io>) -> NativeStore {
    NativeStore {
        inner: Inner::create(io, STORE, Default::default()).expect("create"),
    }
}

fn try_open(io: Box<dyn Io>) -> Result<NativeStore, StorageError> {
    Inner::open(io, Default::default()).map(|inner| NativeStore { inner })
}

fn open(io: Box<dyn Io>) -> NativeStore {
    try_open(io).expect("recovery must open the store")
}

fn generation(s: &NativeStore) -> u64 {
    s.inner.snapshot().expect("snapshot").desc.generation
}

/// Both store files' durable and volatile images.
fn images(sim: &SimIo) -> Vec<Option<(Vec<u8>, Vec<u8>)>> {
    [FileId::Meta, FileId::Content]
        .into_iter()
        .map(|f| sim.file_images_for_test(f))
        .collect()
}

/// A complete logical request: records, both relationship directions, content, evidence.
fn request(store: &NativeStore, step: u32) -> Option<(AttemptId, ChangeSet)> {
    let staged = store.stage_content(&mut &payload(step)[..]).ok()?;
    Some(request_with(&staged, step))
}

fn payload(step: u32) -> Vec<u8> {
    (0..(150_000 + step as usize * 7))
        .map(|i| (i * 31 + step as usize) as u8)
        .collect()
}

/// The request of `step`, binding already staged content.
fn request_with(staged: &StagedContent, step: u32) -> (AttemptId, ChangeSet) {
    let mut cs = ChangeSet {
        content: vec![ContentBinding::from(staged)],
        ..ChangeSet::default()
    };
    let (a, b) = (IDS[step as usize % 4], IDS[(step as usize + 1) % 4]);
    cs.changes.push(Change::PutContext {
        id: cid(a),
        expect: Precondition::Any,
        body: format!("body {a} at step {step}").into_bytes(),
        content: Some(0),
    });
    cs.changes.push(Change::PutRelationship {
        id: RelationshipId::new(format!("r{step}").into_bytes()).unwrap(),
        expect: Precondition::Absent,
        kind: RelationshipKind::new(*b"depends").unwrap(),
        source: cid(a),
        target: cid(b),
    });
    cs.evidence.push(Evidence {
        label: b"decision".to_vec(),
        bytes: format!("evidence {step}").into_bytes(),
    });
    (attempt(step), cs)
}

fn attempt(step: u32) -> AttemptId {
    AttemptId::new(format!("attempt-{step}").into_bytes()).unwrap()
}

fn commit_step(store: &NativeStore, step: u32) -> bool {
    let Some((attempt, cs)) = request(store, step) else {
        return false;
    };
    let Ok(view) = store.begin() else {
        return false;
    };
    store.commit(attempt, view, cs).is_ok()
}

fn history_len(store: &NativeStore) -> usize {
    let mut v = store.begin().expect("begin");
    v.history(HistoryPosition(0))
        .expect("history")
        .collect::<Result<Vec<_>, _>>()
        .expect("history entry")
        .len()
}

/// The complete logical state, read through the port.
fn logical_state(store: &NativeStore) -> Vec<String> {
    let mut v = store.begin().expect("begin");
    let mut out = Vec::new();
    for id in IDS {
        let id = cid(id);
        match v.context(&id).expect("context") {
            None => out.push(format!("{id:?} absent")),
            Some(r) => {
                out.push(format!("{id:?} rev {} body {:?}", r.revision.0, r.body));
                if let Some(c) = r.content {
                    let mut bytes = Vec::new();
                    v.content(&c)
                        .expect("content")
                        .read_to_end(&mut bytes)
                        .expect("read content");
                    out.push(format!(
                        "{id:?} content {} bytes, sum {}",
                        bytes.len(),
                        bytes.iter().map(|&b| b as u64).sum::<u64>()
                    ));
                }
            }
        }
        for d in [Direction::Outgoing, Direction::Incoming] {
            let q = RelationshipQuery {
                from: id.clone(),
                direction: d,
                kind: None,
                after: None,
            };
            for r in v.relationships(&q).expect("rels") {
                out.push(format!("{id:?} {d:?} {:?}", r.expect("rel")));
            }
        }
    }
    for h in v.history(HistoryPosition(0)).expect("history") {
        let h = h.expect("history entry");
        out.push(format!(
            "history {} {:?} {:?} {} changes",
            h.position.0,
            h.attempt,
            h.evidence,
            h.changes.len()
        ));
    }
    out
}

/// Independent expected states after each committed step, from an uninterrupted run.
fn reference_states(steps: u32) -> Vec<Vec<String>> {
    let s = create(Box::new(SimIo::new()));
    let mut states = vec![logical_state(&s)];
    for step in 1..=steps {
        assert!(commit_step(&s, step));
        states.push(logical_state(&s));
    }
    states
}

#[test]
fn failed_opens_change_nothing_and_a_clean_open_recovers_the_committed_state() {
    let expected = reference_states(2).pop().unwrap();

    let sim = SimIo::new();
    let s = create(Box::new(sim.clone()));
    assert!(commit_step(&s, 1));
    assert!(commit_step(&s, 2));
    drop(s);
    let img = sim.after_process_exit();
    let before = images(&img);

    // The first mutating operation of open is its content barrier.
    img.fail_nth_op(1);
    let err = try_open(Box::new(img.clone()))
        .err()
        .expect("open must fail");
    assert!(
        matches!(&err, StorageError::Io(m) if m.contains("simulated I/O error")),
        "{err:?}"
    );
    assert_eq!(
        images(&img),
        before,
        "a failed open changed the store files"
    );
    // A failed open does not give the ownership lock back.
    assert!(
        matches!(try_open(Box::new(img.clone())), Err(StorageError::Busy(_))),
        "the lock taken by a failed open must persist until released"
    );
    img.release();

    img.fail_next_read();
    let err = try_open(Box::new(img.clone()))
        .err()
        .expect("open must fail");
    assert!(
        matches!(&err, StorageError::Io(m) if m.contains("simulated read error")),
        "{err:?}"
    );
    assert_eq!(
        images(&img),
        before,
        "a failed open changed the store files"
    );
    img.release();

    let s = open(Box::new(img.clone()));
    assert_eq!(logical_state(&s), expected);
    assert_eq!(history_len(&s), 2);
    let first = images(&img);
    // Opening only makes existing bytes durable; it never writes.
    let volatile = |v: &[Option<(Vec<u8>, Vec<u8>)>]| -> Vec<Vec<u8>> {
        v.iter()
            .map(|f| f.clone().expect("file exists").1)
            .collect()
    };
    assert_eq!(volatile(&first), volatile(&before));
    for f in &first {
        let (durable, volatile) = f.as_ref().expect("file exists");
        assert_eq!(durable, volatile, "open left referenced bytes non-durable");
    }
    drop(s);
    img.release();

    let s = open(Box::new(img.clone()));
    assert_eq!(logical_state(&s), expected);
    assert_eq!(history_len(&s), 2);
    assert_eq!(
        images(&img),
        first,
        "reopening mutated or duplicated store bytes"
    );
}

#[test]
fn every_cut_of_store_creation_opens_the_empty_store_or_refuses_to_initialize() {
    let empty = reference_states(0).pop().unwrap();

    let probe = SimIo::new();
    drop(create(Box::new(probe.clone())));
    let total = probe.ops();
    assert!(total > 0);

    let mut opened = 0;
    let mut refused = 0;
    for cut in 0..=total {
        let sim = SimIo::new();
        sim.crash_after(cut);
        let created = Inner::create(Box::new(sim.clone()), STORE, Default::default());
        assert_eq!(created.is_ok(), cut == total, "cut {cut}: creation result");
        drop(created);
        for p in [Persist::None, Persist::All] {
            let img = sim.after_crash(p);
            let before = images(&img);
            match try_open(Box::new(img.clone())) {
                Ok(s) => {
                    assert_eq!(logical_state(&s), empty, "cut {cut} {p:?}");
                    assert_eq!(history_len(&s), 0, "cut {cut} {p:?}");
                    assert_eq!(generation(&s), 1, "cut {cut} {p:?}");
                    opened += 1;
                }
                // Before the directory barrier no store file survives: there is no store.
                Err(StorageError::StoreState(m))
                    if before.iter().all(Option::is_none) && m.contains("missing") =>
                {
                    refused += 1;
                }
                Err(StorageError::Integrity(m))
                    if m.contains("initialization never completed")
                        || m.contains("no valid publication record") =>
                {
                    assert_eq!(
                        images(&img),
                        before,
                        "cut {cut} {p:?}: a refused open wrote to the store"
                    );
                    refused += 1;
                }
                Err(e) => panic!("cut {cut} {p:?}: unexpected open result {e:?}"),
            }
        }
    }
    // The completed creation always opens; some interrupted creation is always refused.
    assert!(opened >= 2, "opened {opened}");
    assert!(refused > 0, "refused {refused}");
    assert_eq!(opened + refused, (total + 1) * 2);
}

#[test]
fn a_new_generation_is_invisible_until_its_publication_barrier_returns() {
    // Observes the store's visible generation at the barrier that follows each publication
    // record write.
    struct ObservePublication {
        sim: SimIo,
        store: Arc<OnceLock<Weak<Inner>>>,
        record_written: AtomicBool,
        seen: Arc<Mutex<Vec<u64>>>,
    }
    impl Io for ObservePublication {
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
            self.sim.write_at(file, offset, data)?;
            if file == FileId::Meta && offset < 2 * PAGE_SIZE as u64 {
                self.record_written.store(true, Ordering::SeqCst);
            }
            Ok(())
        }
        fn len(&self, file: FileId) -> Result<u64, StorageError> {
            self.sim.len(file)
        }
        fn sync(&self, file: FileId) -> Result<(), StorageError> {
            self.sim.sync(file)?;
            if file == FileId::Meta
                && self.record_written.swap(false, Ordering::SeqCst)
                && let Some(inner) = self.store.get().and_then(Weak::upgrade)
            {
                self.seen
                    .lock()
                    .unwrap()
                    .push(inner.snapshot()?.desc.generation);
            }
            Ok(())
        }
        fn sync_dir(&self) -> Result<(), StorageError> {
            self.sim.sync_dir()
        }
    }

    let hook = Arc::new(OnceLock::new());
    let seen = Arc::new(Mutex::new(Vec::new()));
    let io = ObservePublication {
        sim: SimIo::new(),
        store: hook.clone(),
        record_written: AtomicBool::new(false),
        seen: seen.clone(),
    };
    let s = create(Box::new(io));
    hook.set(Arc::downgrade(&s.inner)).unwrap();
    assert_eq!(generation(&s), 1);
    assert!(commit_step(&s, 1));
    assert_eq!(generation(&s), 2);
    assert!(commit_step(&s, 2));
    assert_eq!(generation(&s), 3);
    assert_eq!(
        *seen.lock().unwrap(),
        vec![1, 2],
        "a generation became visible before its publication barrier returned"
    );
}

#[test]
fn an_indeterminate_publication_leaves_new_views_on_the_old_state() {
    let states = reference_states(1);

    let probe = SimIo::new();
    let s = create(Box::new(probe.clone()));
    assert!(commit_step(&s, 1));
    let before = probe.ops();
    assert!(commit_step(&s, 2));
    let per_commit = probe.ops() - before;

    let sim = SimIo::new();
    let s = create(Box::new(sim.clone()));
    assert!(commit_step(&s, 1));
    let old = generation(&s);
    // The last operation of a commit is the publication barrier.
    sim.fail_nth_op(per_commit);
    let (attempt, cs) = request(&s, 2).expect("stage");
    let err = s
        .commit(attempt.clone(), s.begin().unwrap(), cs)
        .unwrap_err();
    assert!(
        matches!(&err, CommitError::Indeterminate(m) if m.contains("uncertain")),
        "{err:?}"
    );
    assert_eq!(s.outcome(&attempt).unwrap(), AttemptOutcome::Unknown);
    assert_eq!(generation(&s), old, "uncertain generation became visible");
    assert_eq!(logical_state(&s), states[1]);
    assert_eq!(history_len(&s), 1);
}

/// A barrier of a commit that a lossy sync failure can hit.
#[derive(Clone, Copy, Debug)]
enum Barrier {
    DataContent,
    DataMeta,
    Publication,
}

/// Stages the content of step 2 (bound by the commit under test) and of step 3 (staged before
/// that commit, bound only by a later one).
fn stage_two(s: &NativeStore) -> (StagedContent, StagedContent) {
    let x = s.stage_content(&mut &payload(2)[..]).expect("stage");
    let y = s.stage_content(&mut &payload(3)[..]).expect("stage");
    (x, y)
}

/// Which mutating operation, counted from just after the first commit, is `barrier` of the
/// second commit (staging of both contents included).
fn barrier_op(barrier: Barrier) -> u64 {
    let probe = SimIo::new();
    let trace = Arc::new(Mutex::new(Vec::new()));
    let s = create(Box::new(Traced {
        inner: probe.clone(),
        trace: trace.clone(),
    }));
    assert!(commit_step(&s, 1));
    trace.lock().unwrap().clear();
    let before = probe.ops();
    let (x, _y) = stage_two(&s);
    let (a, cs) = request_with(&x, 2);
    s.commit(a, s.begin().unwrap(), cs).expect("probe commit");
    let ops = trace.lock().unwrap().clone();
    assert_eq!(ops.len() as u64, probe.ops() - before);
    let slot = ops
        .iter()
        .rposition(|op| matches!(op, Op::Write { file: FileId::Meta, offset, .. } if *offset < 2 * PAGE_SIZE as u64))
        .expect("publication record write");
    let sync = |file| move |op: &Op| *op == Op::Sync { file };
    let i = match barrier {
        Barrier::DataContent => ops[..slot].iter().rposition(sync(FileId::Content)),
        Barrier::DataMeta => ops[..slot].iter().rposition(sync(FileId::Meta)),
        Barrier::Publication => ops[slot + 1..]
            .iter()
            .position(sync(FileId::Meta))
            .map(|j| slot + 1 + j),
    }
    .expect("barrier");
    i as u64 + 1
}

fn is_fenced_refusal(r: &Result<impl std::fmt::Debug, CommitError>) -> bool {
    matches!(r, Err(CommitError::Rejected(StorageError::StoreState(m))) if m.contains("writes fenced"))
}

/// Commit 1 succeeds; commit 2 hits a lossy failure of `barrier`. Returns the store, its
/// simulator, the commit error, and both staged contents.
fn lossy_failure_of(
    barrier: Barrier,
) -> (
    NativeStore,
    SimIo,
    CommitError,
    StagedContent,
    StagedContent,
) {
    let n = barrier_op(barrier);
    let sim = SimIo::new();
    sim.lossy_sync_failures();
    let s = create(Box::new(sim.clone()));
    assert!(commit_step(&s, 1));
    sim.fail_nth_op(n);
    let (x, y) = stage_two(&s);
    let (a, cs) = request_with(&x, 2);
    let err = s.commit(a, s.begin().unwrap(), cs).unwrap_err();

    // Writes stay fenced: neither the content staged before the failure nor anything new may be
    // committed, and nothing new may be staged.
    let (a3, cs3) = request_with(&y, 3);
    let late = s.commit(a3, s.begin().unwrap(), cs3);
    assert!(
        is_fenced_refusal(&late),
        "{barrier:?}: commit of content staged before the failed barrier: {late:?}"
    );
    let unbound = ChangeSet {
        changes: vec![Change::PutContext {
            id: cid("D"),
            expect: Precondition::Any,
            body: b"after failure".to_vec(),
            content: None,
        }],
        ..ChangeSet::default()
    };
    let late = s.commit(attempt(4), s.begin().unwrap(), unbound);
    assert!(is_fenced_refusal(&late), "{barrier:?}: {late:?}");
    let staged = s.stage_content(&mut &b"more"[..]);
    assert!(
        matches!(&staged, Err(StorageError::StoreState(m)) if m.contains("writes fenced")),
        "{barrier:?}: staging after the failure: {staged:?}"
    );
    (s, sim, err, x, y)
}

/// Opens each image after a failed second commit: the store must show exactly the state after
/// commit 1, no failed or refused attempt, no content the failed commit referenced, and keep
/// working.
fn assert_recovers_pre_failure_state(
    barrier: Barrier,
    images: Vec<SimIo>,
    x: &StagedContent,
    y: &StagedContent,
) {
    let states = reference_states(3);
    for img in images {
        let s = open(Box::new(img));
        assert_eq!(logical_state(&s), states[1], "{barrier:?}");
        assert!(
            matches!(s.outcome(&attempt(1)).unwrap(), AttemptOutcome::Committed(r) if r.position == HistoryPosition(1)),
            "{barrier:?}: acknowledged commit lost"
        );
        for step in [2, 3, 4] {
            assert_eq!(
                s.outcome(&attempt(step)).unwrap(),
                AttemptOutcome::Absent,
                "{barrier:?}: attempt {step}"
            );
        }
        let mut v = s.begin().unwrap();
        for c in [x, y] {
            let r = v.content(&c.content).map(|_| ());
            assert!(
                matches!(&r, Err(StorageError::InvalidRequest(m)) if m.contains("not committed")),
                "{barrier:?}: content of a failed commit is reachable: {r:?}"
            );
        }
        // The store keeps working and converges on the uninterrupted run.
        assert!(
            commit_step(&s, 2),
            "{barrier:?}: store unusable after recovery"
        );
        assert!(
            commit_step(&s, 3),
            "{barrier:?}: store unusable after recovery"
        );
        assert_eq!(logical_state(&s), states[3], "{barrier:?}");
    }
}

#[test]
fn a_lossy_failure_of_the_content_data_barrier_fences_and_recovers_the_old_state() {
    data_barrier_case(Barrier::DataContent);
}

#[test]
fn a_lossy_failure_of_the_metadata_data_barrier_fences_and_recovers_the_old_state() {
    data_barrier_case(Barrier::DataMeta);
}

fn data_barrier_case(barrier: Barrier) {
    let states = reference_states(1);
    let (s, sim, err, x, y) = lossy_failure_of(barrier);
    assert!(
        matches!(&err, CommitError::Rejected(StorageError::Io(m)) if m.contains("simulated I/O error")),
        "{barrier:?}: {err:?}"
    );
    assert_eq!(s.outcome(&attempt(2)).unwrap(), AttemptOutcome::Absent);
    assert_eq!(logical_state(&s), states[1], "{barrier:?}");
    drop(s);
    let power_loss = sim.after_crash(Persist::None);
    sim.release();
    assert_recovers_pre_failure_state(barrier, vec![sim, power_loss], &x, &y);
}

#[test]
fn a_lossy_failure_of_the_publication_barrier_is_indeterminate_and_recovers_a_whole_state() {
    let barrier = Barrier::Publication;
    let states = reference_states(1);
    let (s, sim, err, x, y) = lossy_failure_of(barrier);
    assert!(
        matches!(&err, CommitError::Indeterminate(m) if m.contains("uncertain")),
        "{err:?}"
    );
    assert_eq!(s.outcome(&attempt(2)).unwrap(), AttemptOutcome::Unknown);
    assert_eq!(logical_state(&s), states[1]);
    drop(s);
    let images = vec![
        sim.after_process_exit(),
        sim.after_crash(Persist::None),
        sim.after_crash(Persist::All),
    ];
    // The failed barrier dropped the unsynced publication record, so every image holds the old
    // generation; recovery must present it whole, with outcomes that agree with it.
    assert_recovers_pre_failure_state(barrier, images, &x, &y);
}
