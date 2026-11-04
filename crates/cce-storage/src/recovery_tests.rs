//! Recovery and open tests: failed opens, interrupted store creation, and the ordering between
//! publication durability and visibility.
//!
//! Expected states come from an independent run of the same logical requests, never from what
//! recovery chose.

use crate::test_support::{cid, create_on, open_on};

use std::io::Read;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock, Weak};

use cce_core::storage::{
    AttemptId, AttemptOutcome, Change, ChangeSet, CommitError, ContentBinding, Direction, Evidence, HistoryPosition, Precondition, ReadView, RelationshipId, RelationshipKind,
    RelationshipQuery, StagedContent, StorageEngine, StorageError,
};

use crate::NativeStore;
use crate::engine::Inner;
use crate::format::{PAGE_SIZE, StoreId};
use crate::io::{FileId, Io, Op, Traced};
use crate::repro_tests::{PROCESS_EXIT, survival, with_repro};
use crate::sim::{Persist, SimIo};

const STORE: StoreId = StoreId(*b"recovery-store-1");
const IDS: [&str; 4] = ["A", "B", "C", "D"];

fn create(io: Box<dyn Io>) -> NativeStore {
    create_on(io, STORE, Default::default()).expect("create")
}

fn try_open(io: Box<dyn Io>) -> Result<NativeStore, StorageError> {
    open_on(io, Default::default())
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
        let trace = Arc::new(Mutex::new(Vec::new()));
        let created = Inner::create(
            Box::new(Traced {
                inner: sim.clone(),
                trace: trace.clone(),
            }),
            STORE,
            Default::default(),
        );
        let acknowledged = created.is_ok();
        assert_eq!(acknowledged, cut == total, "cut {cut}: creation result");
        drop(created);
        for p in [Persist::None, Persist::All] {
            let img = sim.after_crash(p);
            let before = images(&img);
            let label = format!("create cut {cut}, {}", survival(p));
            let pristine = img.after_process_exit();
            let ops = trace.lock().unwrap().clone();
            with_repro(&label, &[("image", &pristine)], &ops, || {
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
                        assert!(
                            !acknowledged,
                            "cut {cut} {p:?}: acknowledged store lost its files"
                        );
                        refused += 1;
                    }
                    Err(StorageError::Integrity(m))
                        if m.contains("initialization never completed")
                            || m.contains("no valid publication record") =>
                    {
                        assert!(
                            !acknowledged,
                            "cut {cut} {p:?}: acknowledged store refused: {m}"
                        );
                        assert_eq!(
                            images(&img),
                            before,
                            "cut {cut} {p:?}: a refused open wrote to the store"
                        );
                        refused += 1;
                    }
                    Err(e) => panic!("cut {cut} {p:?}: unexpected open result {e:?}"),
                }
            });
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

/// A store after a lossy barrier failure of its second commit, with the results of the writes
/// attempted afterwards (checked by the caller, after reopening, so that a missing fence is
/// also observed by its effect on what reopening finds).
struct LossyRun {
    store: NativeStore,
    sim: SimIo,
    err: CommitError,
    x: StagedContent,
    y: StagedContent,
    /// (what was attempted, refused as fenced, the result).
    later: Vec<(&'static str, bool, String)>,
}

fn assert_later_writes_fenced(barrier: Barrier, later: &[(&'static str, bool, String)]) {
    for (what, fenced, result) in later {
        assert!(fenced, "{barrier:?}: {what} after the failure: {result}");
    }
}

/// After reopening: every attempt reported committed has its history entry, and every content
/// that entry binds reads back completely, as the bytes its request staged.
fn assert_committed_content_readable(barrier: Barrier, s: &NativeStore) {
    let mut v = s.begin().unwrap();
    let history: Vec<_> = v
        .history(HistoryPosition(0))
        .unwrap()
        .map(|h| h.unwrap())
        .collect();
    for step in 1..=4 {
        let AttemptOutcome::Committed(r) = s.outcome(&attempt(step)).unwrap() else {
            continue;
        };
        let entry = history
            .iter()
            .find(|h| h.position == r.position)
            .unwrap_or_else(|| panic!("{barrier:?}: committed attempt {step} has no history"));
        assert_eq!(entry.attempt, attempt(step), "{barrier:?}");
        for c in &entry.content {
            let mut bytes = Vec::new();
            let read = v.content(c).and_then(|mut r| {
                r.read_to_end(&mut bytes)
                    .map_err(|e| StorageError::Io(e.to_string()))
            });
            assert!(
                read.is_ok(),
                "{barrier:?}: attempt {step} is committed but its content is unreadable: {read:?}"
            );
            assert_eq!(bytes, payload(step), "{barrier:?}: attempt {step} content");
        }
    }
}

/// Commit 1 succeeds; commit 2 hits a lossy failure of `barrier`; then a commit of content staged
/// before the failure, a commit without content, and new staging are attempted.
fn lossy_failure_of(barrier: Barrier) -> LossyRun {
    let n = barrier_op(barrier);
    let sim = SimIo::new();
    sim.lossy_sync_failures();
    let s = create(Box::new(sim.clone()));
    assert!(commit_step(&s, 1));
    sim.fail_nth_op(n);
    let (x, y) = stage_two(&s);
    let (a, cs) = request_with(&x, 2);
    let err = s.commit(a, s.begin().unwrap(), cs).unwrap_err();

    // Writes must stay fenced: neither the content staged before the failure nor anything new
    // may be committed, and nothing new may be staged.
    let mut later = Vec::new();
    let (a3, cs3) = request_with(&y, 3);
    let late = s.commit(a3, s.begin().unwrap(), cs3);
    later.push((
        "commit of content staged before the failed barrier",
        is_fenced_refusal(&late),
        format!("{late:?}"),
    ));
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
    later.push((
        "commit without content",
        is_fenced_refusal(&late),
        format!("{late:?}"),
    ));
    let staged = s.stage_content(&mut &b"more"[..]);
    later.push((
        "staging",
        matches!(&staged, Err(StorageError::StoreState(m)) if m.contains("writes fenced")),
        format!("{staged:?}"),
    ));
    LossyRun {
        store: s,
        sim,
        err,
        x,
        y,
        later,
    }
}

/// Opens each image once for the semantic check, leaving it reusable.
fn check_content_after_reopen(barrier: Barrier, images: &[SimIo]) {
    for img in images {
        let s = open(Box::new(img.clone()));
        assert_committed_content_readable(barrier, &s);
        drop(s);
        img.release();
    }
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
    let LossyRun {
        store,
        sim,
        err,
        x,
        y,
        later,
    } = lossy_failure_of(barrier);
    assert!(
        matches!(&err, CommitError::Rejected(StorageError::Io(m)) if m.contains("simulated I/O error")),
        "{barrier:?}: {err:?}"
    );
    assert_eq!(store.outcome(&attempt(2)).unwrap(), AttemptOutcome::Absent);
    let power_loss = sim.after_crash(Persist::None);
    drop(store);
    sim.release();
    let images = vec![sim, power_loss];
    // Semantic check first: whatever was acknowledged must be readable after reopening.
    check_content_after_reopen(barrier, &images);
    assert_later_writes_fenced(barrier, &later);
    assert_recovers_pre_failure_state(barrier, images, &x, &y);
}

#[test]
fn a_lossy_failure_of_the_publication_barrier_is_indeterminate_and_recovers_a_whole_state() {
    let barrier = Barrier::Publication;
    let states = reference_states(1);
    let LossyRun {
        store: s,
        sim,
        err,
        x,
        y,
        later,
    } = lossy_failure_of(barrier);
    assert!(
        matches!(&err, CommitError::Indeterminate(m) if m.contains("uncertain")),
        "{err:?}"
    );
    assert_eq!(s.outcome(&attempt(2)).unwrap(), AttemptOutcome::Unknown);
    assert_later_writes_fenced(barrier, &later);
    assert_eq!(logical_state(&s), states[1]);
    drop(s);
    let images = vec![
        sim.after_process_exit(),
        sim.after_crash(Persist::None),
        sim.after_crash(Persist::All),
    ];
    // The failed barrier dropped the unsynced publication record, so every image holds the old
    // generation; recovery must present it whole, with outcomes that agree with it.
    check_content_after_reopen(barrier, &images);
    assert_recovers_pre_failure_state(barrier, images, &x, &y);
}

/// A fault injected while opening a crashed image.
#[derive(Clone, Copy, Debug)]
enum OpenFault {
    /// The n-th mutating operation of open fails with an I/O error.
    FailOp(u64),
    /// The first read of open fails.
    FailRead,
    /// The process crashes after n mutating operations of open.
    CrashAfter(u64),
}

/// What the outcomes of the two commits must be in a recovered state.
fn assert_outcomes_agree(s: &NativeStore, got: &[String], states: &[Vec<String>]) {
    assert!(
        matches!(s.outcome(&attempt(1)).unwrap(), AttemptOutcome::Committed(r) if r.position == HistoryPosition(1)),
        "acknowledged commit 1 lost"
    );
    let second = s.outcome(&attempt(2)).unwrap();
    if got == states[2].as_slice() {
        assert!(
            matches!(&second, AttemptOutcome::Committed(r) if r.position == HistoryPosition(2)),
            "state has commit 2 but its outcome is {second:?}"
        );
    } else {
        assert_eq!(second, AttemptOutcome::Absent, "state lacks commit 2");
    }
}

#[test]
fn a_fault_during_recovery_of_any_crashed_commit_leaves_the_image_recoverable() {
    let states = reference_states(2);
    let (old, new) = (&states[1], &states[2]);

    // Operations of the second commit (staging included), and of opening a store.
    let probe = SimIo::new();
    let s = create(Box::new(probe.clone()));
    assert!(commit_step(&s, 1));
    let before = probe.ops();
    assert!(commit_step(&s, 2));
    let total = probe.ops() - before;
    drop(s);
    let reopened = probe.after_process_exit();
    let before = reopened.ops();
    drop(open(Box::new(reopened.clone())));
    let open_ops = reopened.ops() - before;
    assert!(open_ops > 0, "open is expected to issue barriers");
    let faults: Vec<OpenFault> = (1..=open_ops)
        .map(OpenFault::FailOp)
        .chain([OpenFault::FailRead])
        .chain((0..open_ops).map(OpenFault::CrashAfter))
        .collect();

    // Opens `img` cleanly, checks outcomes and that a second open leaves the files unchanged, and
    // returns the recovered state. Reporting attempt 2 as committed acknowledges it, so from then
    // on it must survive a power loss.
    let recover = |img: &SimIo| -> Vec<String> {
        let s = open(Box::new(img.clone()));
        let got = logical_state(&s);
        assert!(&got == old || &got == new, "recovered a partial state");
        assert_outcomes_agree(&s, &got, &states);
        if &got == new {
            let lost = img.after_crash(Persist::None);
            assert_eq!(
                &logical_state(&open(Box::new(lost))),
                new,
                "a commit that recovery reported as committed was lost on power loss"
            );
        }
        let first = images(img);
        drop(s);
        img.release();
        let s = open(Box::new(img.clone()));
        assert_eq!(logical_state(&s), got);
        assert_eq!(images(img), first, "a second clean open changed the files");
        got
    };

    let mut checked = 0;
    // Evidence that the process-exit cases exercise what they are meant to: a faulty open that
    // made cached bytes durable, and a power loss after it that recovered a different whole state
    // than a fresh process would have.
    let made_durable = std::cell::Cell::new(0u64);
    let diverged = std::cell::Cell::new(0u64);
    for cut in 0..=total {
        let sim = SimIo::new();
        let trace = Arc::new(Mutex::new(Vec::new()));
        let s = create(Box::new(Traced {
            inner: sim.clone(),
            trace: trace.clone(),
        }));
        assert!(commit_step(&s, 1));
        trace.lock().unwrap().clear();
        sim.crash_after(cut);
        let acknowledged = commit_step(&s, 2);
        drop(s);
        let trace = trace.lock().unwrap().clone();
        // Power-loss images (only what survived), and the process-exit image (the commit's
        // unsynced writes are still in the operating system's cache).
        let mut crashed: Vec<(String, SimIo)> = [Persist::None, Persist::All]
            .into_iter()
            .chain((1..=12).map(Persist::Seeded))
            .map(|p| (survival(p), sim.after_crash(p)))
            .collect();
        crashed.push((PROCESS_EXIT.into(), sim.after_process_exit()));
        for (what, img) in &crashed {
            // What an uninterrupted recovery of this image gives.
            let expected = logical_state(&open(Box::new(img.after_process_exit())));
            assert!(
                &expected == old || &expected == new,
                "cut {cut} {what}: uninterrupted recovery gave a partial state"
            );
            if acknowledged {
                assert_eq!(&expected, new, "cut {cut} {what}: acknowledged commit lost");
            }
            for fault in &faults {
                let label = format!("commit cut {cut}, {what}, open fault {fault:?}");
                with_repro(&label, &[("image", img)], &trace, || {
                    let work = img.after_process_exit();
                    let before = images(&work);
                    match *fault {
                        OpenFault::FailOp(n) => work.fail_nth_op(n),
                        OpenFault::FailRead => work.fail_next_read(),
                        OpenFault::CrashAfter(n) => work.crash_after(n),
                    }
                    let e = try_open(Box::new(work.clone()))
                        .err()
                        .expect("open must fail");
                    let injected = match fault {
                        OpenFault::FailOp(_) => "simulated I/O error",
                        OpenFault::FailRead => "simulated read error",
                        OpenFault::CrashAfter(_) => "simulated crash",
                    };
                    assert!(
                        matches!(&e, StorageError::Io(m) if m.contains(injected)),
                        "{e:?}"
                    );
                    // The faulty open wrote nothing: each file's bytes are unchanged and its
                    // durable image is unchanged or its existing bytes made durable.
                    for (b, a) in before.iter().zip(images(&work)) {
                        let ((bd, bv), (ad, av)) = (b.clone().unwrap(), a.unwrap());
                        assert!(av == bv, "the faulty open ({e:?}) wrote to a file");
                        assert!(
                            ad == bd || ad == bv,
                            "the faulty open ({e:?}) made bytes durable that it did not hold"
                        );
                        if ad != bd {
                            made_durable.set(made_durable.get() + 1);
                        }
                    }

                    // A fresh process after the faulty one: the same as an uninterrupted
                    // recovery of the image.
                    let got = recover(&work.after_process_exit());
                    assert_eq!(got, expected, "recovery after a faulty open differs");
                    // Power lost right after the faulty open: a whole state, and an acknowledged
                    // commit survives.
                    let got = recover(&work.after_crash(Persist::None));
                    if got != expected {
                        diverged.set(diverged.get() + 1);
                    }
                    if acknowledged {
                        assert_eq!(&got, new, "acknowledged commit lost after power loss");
                    }
                });
                checked += 1;
            }
        }
    }
    assert_eq!(checked, (total + 1) * 15 * faults.len() as u64);
    assert!(
        made_durable.get() > 0,
        "no faulty open made cached bytes durable"
    );
    assert!(
        diverged.get() > 0,
        "power loss after a faulty open never changed the outcome"
    );
    eprintln!(
        "faults during recovery: {checked} cases, {} made cached bytes durable, {} diverged after power loss",
        made_durable.get(),
        diverged.get()
    );
}

#[test]
fn staged_content_is_not_visible_through_any_view_until_committed() {
    let sim = SimIo::new();
    let s = create(Box::new(sim.clone()));
    assert!(commit_step(&s, 1));
    let state = logical_state(&s);
    let earlier = s.begin().unwrap();
    let x = s.stage_content(&mut &payload(2)[..]).expect("stage");

    let assert_invisible = |s: &NativeStore, mut v: crate::NativeView, what: &str| {
        let r = v.content(&x.content).map(|_| ());
        assert!(
            matches!(&r, Err(StorageError::InvalidRequest(m)) if m.contains("not committed")),
            "{what}: staged content is readable: {r:?}"
        );
        let history: Vec<_> = v
            .history(HistoryPosition(0))
            .unwrap()
            .map(|h| h.unwrap())
            .collect();
        assert_eq!(history.len(), 1, "{what}: history gained an entry");
        assert!(
            history.iter().all(|h| !h.content.contains(&x.content)),
            "{what}: history references staged content"
        );
        assert_eq!(logical_state(s), state, "{what}: records changed");
    };
    // Views begun before and after staging, in the staging instance.
    assert_invisible(&s, earlier, "view begun before staging");
    assert_invisible(&s, s.begin().unwrap(), "view begun after staging");
    drop(s);
    // A later process: after a clean exit and after a power loss.
    for (img, what) in [
        (sim.after_process_exit(), "after process exit"),
        (sim.after_crash(Persist::All), "after power loss"),
    ] {
        let s = open(Box::new(img));
        assert_invisible(&s, s.begin().unwrap(), what);
    }

    // Positive neighbour: once committed, the same content is readable.
    let s = open(Box::new(sim.after_process_exit()));
    assert!(commit_step(&s, 2));
    let mut bytes = Vec::new();
    s.begin()
        .unwrap()
        .content(&x.content)
        .expect("committed content")
        .read_to_end(&mut bytes)
        .unwrap();
    assert_eq!(bytes, payload(2));
}

#[test]
fn a_failed_directory_barrier_fails_creation_and_no_store_appears() {
    // Find the directory barrier among creation's operations.
    let probe = SimIo::new();
    let trace = Arc::new(Mutex::new(Vec::new()));
    drop(create(Box::new(Traced {
        inner: probe.clone(),
        trace: trace.clone(),
    })));
    let ops = trace.lock().unwrap().clone();
    let k = ops
        .iter()
        .position(|op| *op == Op::SyncDir)
        .expect("creation syncs the directory") as u64
        + 1;
    assert_eq!(
        ops.iter().filter(|op| **op == Op::SyncDir).count(),
        1,
        "creation is expected to issue one directory barrier"
    );

    let sim = SimIo::new();
    sim.fail_nth_op(k);
    let r = Inner::create(Box::new(sim.clone()), STORE, Default::default());
    assert!(
        matches!(&r, Err(StorageError::Io(m)) if m.contains("simulated I/O error")),
        "{:?}",
        r.as_ref().err()
    );
    drop(r);
    // Nothing after the failed barrier ran: no store file holds any byte.
    for f in images(&sim) {
        let (durable, volatile) = f.expect("store file entry exists in the directory cache");
        assert!(durable.is_empty() && volatile.is_empty());
    }

    // Power loss: the entries were never made durable, so no store files exist.
    let lost = sim.after_crash(Persist::All);
    let e = try_open(Box::new(lost)).err().expect("no store");
    assert!(
        matches!(&e, StorageError::StoreState(m) if m.contains("missing")),
        "{e:?}"
    );
    // Process exit, or the entries became durable after all: empty files are an interrupted
    // initialization, refused and never reinitialized.
    let exited = sim.after_process_exit();
    sim.sync_dir().unwrap();
    let persisted = sim.after_crash(Persist::All);
    for img in [exited, persisted] {
        let e = try_open(Box::new(img.clone())).err().expect("no store");
        assert!(
            matches!(&e, StorageError::Integrity(m) if m.contains("initialization never completed")),
            "{e:?}"
        );
        assert!(images(&img).iter().all(|f| {
            f.as_ref()
                .is_some_and(|(d, v)| d.is_empty() && v.is_empty())
        }));
    }
    // Creating again over the leftover entries is refused rather than silently reused.
    let again = Inner::create(
        Box::new(sim.after_process_exit()),
        STORE,
        Default::default(),
    );
    assert!(
        matches!(&again, Err(StorageError::StoreState(m)) if m.contains("already contains")),
        "{:?}",
        again.as_ref().err()
    );
}
