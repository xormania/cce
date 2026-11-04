//! Attempt identity: a reused attempt id either returns the original receipt for the identical
//! request or is refused as a mismatch, without changing state, across concurrency and reopen.

use std::io::Read;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Barrier, Condvar, Mutex};
use std::time::{Duration, Instant};

use cce_core::storage::{
    AttemptId, AttemptOutcome, Change, ChangeSet, CommitError, CommitReceipt, ContentBinding,
    ContextId, Evidence, HistoryEntry, HistoryPosition, Precondition, ReadView, StorageEngine,
    StorageError,
};

use crate::NativeStore;
use crate::engine::Inner;
use crate::format::{Descriptor, StoreId};
use crate::io::{FileId, Io, Traced};
use crate::repro_tests::{survival, with_repro};
use crate::sim::{Persist, SimIo};

const STORE: StoreId = StoreId(*b"attempt-id-store");
const IDS: [&str; 3] = ["A", "B", "S"];
const PAYLOAD: &[u8] = b"content bound by the attempt";

fn cid(s: &str) -> ContextId {
    ContextId::new(s.as_bytes().to_vec()).unwrap()
}

fn attempt(s: &str) -> AttemptId {
    AttemptId::new(s.as_bytes().to_vec()).unwrap()
}

fn sim_store(sim: &SimIo) -> NativeStore {
    NativeStore {
        inner: Inner::create(Box::new(sim.clone()), STORE, Default::default()).unwrap(),
    }
}

fn reopen(sim: &SimIo) -> NativeStore {
    NativeStore {
        inner: Inner::open(Box::new(sim.clone()), Default::default()).expect("open"),
    }
}

/// One field of the baseline request changed.
#[derive(Clone, Copy, Debug)]
enum Variant {
    Identical,
    Body,
    Precondition,
    Evidence,
    ContentDigest,
}

/// The baseline request (or a variant of it), with freshly staged content each time, so every
/// call uses a new staging handle and location.
fn request(s: &NativeStore, v: Variant) -> ChangeSet {
    let payload: &[u8] = match v {
        Variant::ContentDigest => b"different content, different digest",
        _ => PAYLOAD,
    };
    let staged = s.stage_content(&mut &payload[..]).unwrap();
    ChangeSet {
        changes: vec![Change::PutContext {
            id: cid("A"),
            expect: match v {
                Variant::Precondition => Precondition::Any,
                _ => Precondition::Absent,
            },
            body: match v {
                Variant::Body => b"other body".to_vec(),
                _ => b"body".to_vec(),
            },
            content: Some(0),
        }],
        content: vec![ContentBinding::from(&staged)],
        evidence: vec![Evidence {
            label: b"decision".to_vec(),
            bytes: match v {
                Variant::Evidence => b"other evidence".to_vec(),
                _ => b"evidence".to_vec(),
            },
        }],
        ..ChangeSet::default()
    }
}

fn submit(s: &NativeStore, a: &str, v: Variant) -> Result<CommitReceipt, CommitError> {
    let cs = request(s, v);
    s.commit(attempt(a), s.begin().unwrap(), cs)
}

fn seed(s: &NativeStore) {
    let r = s
        .commit(
            attempt("seed"),
            s.begin().unwrap(),
            ChangeSet {
                changes: vec![Change::PutContext {
                    id: cid("S"),
                    expect: Precondition::Absent,
                    body: b"seed".to_vec(),
                    content: None,
                }],
                ..ChangeSet::default()
            },
        )
        .unwrap();
    assert_eq!(r.position, HistoryPosition(1));
}

/// Logical state through the port: records with content bytes, and every retained history entry.
fn logical_state(s: &NativeStore) -> Vec<String> {
    let mut v = s.begin().unwrap();
    let mut out = Vec::new();
    for id in IDS {
        match v.context(&cid(id)).unwrap() {
            None => out.push(format!("{id} absent")),
            Some(r) => {
                let mut bytes = Vec::new();
                if let Some(c) = r.content {
                    v.content(&c).unwrap().read_to_end(&mut bytes).unwrap();
                }
                out.push(format!("{r:?} bytes {bytes:?}"));
            }
        }
    }
    for h in v.history(HistoryPosition(0)).unwrap() {
        out.push(format!("{:?}", h.unwrap()));
    }
    out
}

/// The number of retained history entries; every one must be readable.
fn history_len(s: &NativeStore) -> usize {
    s.begin()
        .unwrap()
        .history(HistoryPosition(0))
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .expect("a retained history entry is unreadable")
        .len()
}

fn descriptor(s: &NativeStore) -> Descriptor {
    s.inner.snapshot().unwrap().desc
}

fn body_of(s: &NativeStore, id: &str) -> Option<Vec<u8>> {
    s.begin()
        .unwrap()
        .context(&cid(id))
        .unwrap()
        .map(|r| r.body)
}

/// Every single-field difference is refused as a mismatch and changes nothing; the identical
/// request, re-staged, returns the original receipt and changes nothing.
fn check_reuse(s: &NativeStore, original: &CommitReceipt, when: &str) {
    let state = logical_state(s);
    let desc = descriptor(s);
    for v in [
        Variant::Body,
        Variant::Precondition,
        Variant::Evidence,
        Variant::ContentDigest,
    ] {
        let r = submit(s, "x", v);
        assert_eq!(r, Err(CommitError::AttemptMismatch), "{when}: {v:?}");
        assert_eq!(logical_state(s), state, "{when}: {v:?} changed state");
        assert_eq!(descriptor(s), desc, "{when}: {v:?} published");
    }
    let r = submit(s, "x", Variant::Identical);
    assert_eq!(r.as_ref(), Ok(original), "{when}: identical retry");
    assert_eq!(
        logical_state(s),
        state,
        "{when}: identical retry changed state"
    );
    assert_eq!(descriptor(s), desc, "{when}: identical retry published");
    assert_eq!(
        s.outcome(&attempt("x")).unwrap(),
        AttemptOutcome::Committed(original.clone()),
        "{when}"
    );
}

#[test]
fn a_reused_attempt_id_returns_the_original_receipt_or_a_mismatch_before_and_after_reopen() {
    let sim = SimIo::new();
    let s = sim_store(&sim);
    seed(&s);
    let original = submit(&s, "x", Variant::Identical).unwrap();
    assert_eq!(original.position, HistoryPosition(2));
    assert_eq!(original.attempt, attempt("x"));
    assert_eq!(history_len(&s), 2);

    check_reuse(&s, &original, "same process");
    let state = logical_state(&s);
    drop(s);
    sim.release();

    let s = reopen(&sim);
    assert_eq!(logical_state(&s), state, "reopen changed state");
    check_reuse(&s, &original, "after reopen");
    assert_eq!(history_len(&s), 2);
    assert_eq!(body_of(&s, "A"), Some(b"body".to_vec()));
}

#[test]
fn queued_jobs_sharing_an_attempt_id_commit_once() {
    let s = sim_store(&SimIo::new());
    let generation = descriptor(&s).generation;
    let gate = s.inner.pause_worker_for_test();
    let tickets: Vec<_> = [Variant::Identical, Variant::Identical, Variant::Body]
        .into_iter()
        .map(|v| {
            let cs = request(&s, v);
            s.submit(attempt("x"), s.begin().unwrap(), cs).unwrap()
        })
        .collect();
    assert_eq!(s.outcome(&attempt("x")).unwrap(), AttemptOutcome::Pending);
    drop(gate);
    let results: Vec<_> = tickets.into_iter().map(|t| t.wait()).collect();
    let first = results[0].clone().expect("first job commits");
    assert_eq!(first.position, HistoryPosition(1));
    assert_eq!(results[1], Ok(first.clone()), "identical job: same receipt");
    assert_eq!(results[2], Err(CommitError::AttemptMismatch));
    assert_eq!(history_len(&s), 1, "exactly one logical commit");
    assert_eq!(descriptor(&s).generation, generation + 1);
    assert_eq!(
        body_of(&s, "A"),
        Some(b"body".to_vec()),
        "other body absent"
    );
    assert_eq!(
        s.outcome(&attempt("x")).unwrap(),
        AttemptOutcome::Committed(first)
    );
}

#[test]
fn concurrent_identical_commits_with_one_attempt_id_commit_once() {
    const THREADS: usize = 8;
    let s = sim_store(&SimIo::new());
    let generation = descriptor(&s).generation;
    let start = Barrier::new(THREADS);
    let results: Vec<_> = std::thread::scope(|sc| {
        let handles: Vec<_> = (0..THREADS)
            .map(|_| {
                sc.spawn(|| {
                    let cs = request(&s, Variant::Identical);
                    let view = s.begin().unwrap();
                    start.wait();
                    s.commit(attempt("x"), view, cs)
                })
            })
            .collect();
        handles.into_iter().map(|h| h.join().unwrap()).collect()
    });
    let expected = CommitReceipt {
        attempt: attempt("x"),
        position: HistoryPosition(1),
    };
    for (i, r) in results.iter().enumerate() {
        assert_eq!(r, &Ok(expected.clone()), "thread {i}");
    }
    assert_eq!(history_len(&s), 1, "exactly one logical commit");
    assert_eq!(descriptor(&s).generation, generation + 1);
}

// ----- outcome after recovery ------------------------------------------------------------------

fn step_request(s: &NativeStore, step: u32) -> Option<ChangeSet> {
    let payload: Vec<u8> = (0..(20_000 + step as usize * 7))
        .map(|i| (i * 31 + step as usize) as u8)
        .collect();
    let staged = s.stage_content(&mut &payload[..]).ok()?;
    Some(ChangeSet {
        changes: vec![Change::PutContext {
            id: cid(IDS[step as usize % 2]),
            expect: Precondition::Any,
            body: format!("body at step {step}").into_bytes(),
            content: Some(0),
        }],
        content: vec![ContentBinding::from(&staged)],
        evidence: vec![Evidence {
            label: b"decision".to_vec(),
            bytes: format!("evidence {step}").into_bytes(),
        }],
        ..ChangeSet::default()
    })
}

fn step_attempt(step: u32) -> AttemptId {
    attempt(&format!("attempt-{step}"))
}

fn commit_step(s: &NativeStore, step: u32) -> Result<CommitReceipt, CommitError> {
    let cs =
        step_request(s, step).ok_or_else(|| CommitError::Indeterminate("staging failed".into()))?;
    let view = s
        .begin()
        .map_err(|e| CommitError::Indeterminate(e.to_string()))?;
    s.commit(step_attempt(step), view, cs)
}

fn committed(step: u32) -> AttemptOutcome {
    AttemptOutcome::Committed(CommitReceipt {
        attempt: step_attempt(step),
        position: HistoryPosition(step as u64),
    })
}

/// Logical states after one and after two steps, from an uninterrupted run.
fn reference_states() -> (Vec<String>, Vec<String>) {
    let s = sim_store(&SimIo::new());
    commit_step(&s, 1).unwrap();
    let old = logical_state(&s);
    commit_step(&s, 2).unwrap();
    (old, logical_state(&s))
}

/// Mutating operations of the second commit, staging included.
fn ops_of_second_commit() -> u64 {
    let probe = SimIo::new();
    let s = sim_store(&probe);
    commit_step(&s, 1).unwrap();
    let before = probe.ops();
    commit_step(&s, 2).unwrap();
    probe.ops() - before
}

#[test]
fn outcome_after_any_cut_matches_the_recovered_state() {
    let (old, new) = reference_states();
    assert_ne!(old, new);
    let total = ops_of_second_commit();
    let (mut saw_old, mut saw_new) = (false, false);
    for cut in 0..=total {
        let sim = SimIo::new();
        let trace = Arc::new(Mutex::new(Vec::new()));
        let s = NativeStore {
            inner: Inner::create(
                Box::new(Traced {
                    inner: sim.clone(),
                    trace: trace.clone(),
                }),
                STORE,
                Default::default(),
            )
            .unwrap(),
        };
        commit_step(&s, 1).unwrap();
        trace.lock().unwrap().clear();
        sim.crash_after(cut);
        let acknowledged = commit_step(&s, 2).is_ok();
        assert_eq!(acknowledged, cut == total, "cut {cut}");
        drop(s);
        let trace = trace.lock().unwrap().clone();
        let persists = [Persist::None, Persist::All]
            .into_iter()
            .chain((1..=12).map(Persist::Seeded));
        for p in persists {
            let img = sim.after_crash(p);
            let label = format!("attempt cut {cut}, {}", survival(p));
            with_repro(&label, &[("image", &img)], &trace, || {
                let s = reopen(&img.after_process_exit());
                let got = logical_state(&s);
                assert_eq!(s.outcome(&step_attempt(1)).unwrap(), committed(1));
                let second = s.outcome(&step_attempt(2)).unwrap();
                if got == new {
                    saw_new = true;
                    assert_eq!(second, committed(2), "cut {cut} {p:?}: new state");
                } else {
                    saw_old = true;
                    assert_eq!(got, old, "cut {cut} {p:?}: partial state");
                    assert_eq!(second, AttemptOutcome::Absent, "cut {cut} {p:?}: old state");
                }
            });
        }
    }
    assert!(saw_old && saw_new, "cuts covered only one outcome");
}

#[test]
fn an_uncertain_publication_resolves_on_reopen_by_what_survived() {
    let (old, new) = reference_states();
    let per_commit = ops_of_second_commit();

    let sim = SimIo::new();
    let s = sim_store(&sim);
    commit_step(&s, 1).unwrap();
    // The last operation of a commit is the publication barrier.
    sim.fail_nth_op(per_commit);
    let r = commit_step(&s, 2);
    assert!(matches!(r, Err(CommitError::Indeterminate(_))), "{r:?}");
    assert_eq!(
        s.outcome(&step_attempt(2)).unwrap(),
        AttemptOutcome::Unknown
    );
    drop(s);

    // The process exits; the operating system still holds the publication record.
    let s = reopen(&sim.after_process_exit());
    assert_eq!(s.outcome(&step_attempt(2)).unwrap(), committed(2));
    assert_eq!(logical_state(&s), new);
    drop(s);

    // Power is lost instead; the unsynced publication record is gone.
    let s = reopen(&sim.after_crash(Persist::None));
    assert_eq!(s.outcome(&step_attempt(2)).unwrap(), AttemptOutcome::Absent);
    assert_eq!(logical_state(&s), old);
    assert_eq!(s.outcome(&step_attempt(1)).unwrap(), committed(1));
}

// ----- racing submissions and outcome ordering -------------------------------------------------

/// The request `variant` submits in `round`: one record per round; variants differ in body and
/// evidence.
fn race_request(round: u32, variant: usize) -> ChangeSet {
    ChangeSet {
        changes: vec![Change::PutContext {
            id: cid(&format!("R{round}")),
            expect: Precondition::Any,
            body: format!("variant {variant}").into_bytes(),
            content: None,
        }],
        evidence: vec![Evidence {
            label: b"variant".to_vec(),
            bytes: vec![variant as u8],
        }],
        ..ChangeSet::default()
    }
}

#[test]
fn racing_different_requests_under_one_attempt_id_commit_exactly_one() {
    const THREADS: usize = 8;
    let s = sim_store(&SimIo::new());
    // Rounds with pairwise different requests, then rounds where half the threads submit one
    // request and half another.
    let rounds = [8, 8, 8, 8, 8, 2, 2, 2, 2, 2];
    for (round, distinct) in (1u32..).zip(rounds) {
        let id = attempt(&format!("race-{round}"));
        let start = Barrier::new(THREADS);
        let results: Vec<(usize, Result<CommitReceipt, CommitError>)> = std::thread::scope(|sc| {
            let handles: Vec<_> = (0..THREADS)
                .map(|t| {
                    let (s, id, start) = (&s, id.clone(), &start);
                    sc.spawn(move || {
                        let variant = t % distinct;
                        let view = s.begin().unwrap();
                        start.wait();
                        (variant, s.commit(id, view, race_request(round, variant)))
                    })
                })
                .collect();
            handles.into_iter().map(|h| h.join().unwrap()).collect()
        });

        // The winner is whichever request retained history recorded for the attempt.
        let entries: Vec<HistoryEntry> = s
            .begin()
            .unwrap()
            .history(HistoryPosition(0))
            .unwrap()
            .map(Result::unwrap)
            .filter(|h| h.attempt == id)
            .collect();
        assert_eq!(
            entries.len(),
            1,
            "round {round}: history entries for the attempt"
        );
        let entry = &entries[0];
        assert_eq!(
            entry.position,
            HistoryPosition(round as u64),
            "round {round}"
        );
        let winner = entry.evidence[0].bytes[0] as usize;
        let request = race_request(round, winner);
        assert_eq!(
            entry.changes, request.changes,
            "round {round}: recorded changes"
        );
        assert_eq!(
            entry.evidence, request.evidence,
            "round {round}: recorded evidence"
        );

        let receipt = CommitReceipt {
            attempt: id.clone(),
            position: entry.position,
        };
        for (t, (variant, r)) in results.iter().enumerate() {
            if *variant == winner {
                assert_eq!(r, &Ok(receipt.clone()), "round {round} thread {t}");
            } else {
                assert_eq!(
                    r,
                    &Err(CommitError::AttemptMismatch),
                    "round {round} thread {t}"
                );
            }
        }
        assert_eq!(
            results.iter().filter(|(_, r)| r.is_ok()).count(),
            THREADS / distinct,
            "round {round}: acknowledged submissions"
        );
        assert_eq!(
            body_of(&s, &format!("R{round}")),
            Some(format!("variant {winner}").into_bytes()),
            "round {round}: stored state is the winner's request"
        );
        assert_eq!(s.outcome(&id).unwrap(), AttemptOutcome::Committed(receipt));
    }
    assert_eq!(history_len(&s), rounds.len());
}

/// Holds a commit at its next content barrier while armed, until released: the commit is then in
/// flight, its effects written but not yet published.
#[derive(Clone, Default)]
struct Gate(Arc<GateState>);

#[derive(Default)]
struct GateState {
    armed: AtomicBool,
    reached: AtomicBool,
    open: Mutex<bool>,
    opened: Condvar,
}

impl Gate {
    fn arm(&self) {
        *self.0.open.lock().unwrap() = false;
        self.0.reached.store(false, Ordering::SeqCst);
        self.0.armed.store(true, Ordering::SeqCst);
    }

    fn release(&self) {
        self.0.armed.store(false, Ordering::SeqCst);
        *self.0.open.lock().unwrap() = true;
        self.0.opened.notify_all();
    }

    fn reached(&self) -> bool {
        self.0.reached.load(Ordering::SeqCst)
    }

    /// Called at a content barrier: blocks while the gate is armed and closed.
    fn pass(&self) {
        if self.0.armed.load(Ordering::SeqCst) {
            self.0.reached.store(true, Ordering::SeqCst);
            let mut open = self.0.open.lock().unwrap();
            while !*open {
                open = self.0.opened.wait(open).unwrap();
            }
        }
    }
}

/// Simulated I/O whose content barriers pass through a [`Gate`].
struct GatedIo {
    inner: SimIo,
    gate: Gate,
}

impl Io for GatedIo {
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
        self.inner.write_at(file, offset, data)
    }
    fn len(&self, file: FileId) -> Result<u64, StorageError> {
        self.inner.len(file)
    }
    fn sync(&self, file: FileId) -> Result<(), StorageError> {
        if file == FileId::Content {
            self.gate.pass();
        }
        self.inner.sync(file)
    }
    fn sync_dir(&self) -> Result<(), StorageError> {
        self.inner.sync_dir()
    }
}

/// A seeded store (attempt "seed" committed at position 1) over gated simulated I/O.
fn gated_store() -> (NativeStore, Gate) {
    let gate = Gate::default();
    let io = GatedIo {
        inner: SimIo::new(),
        gate: gate.clone(),
    };
    let s = NativeStore {
        inner: Inner::create(Box::new(io), STORE, Default::default()).unwrap(),
    };
    seed(&s);
    (s, gate)
}

fn wait_until(what: &str, done: impl Fn() -> bool) {
    let until = Instant::now() + Duration::from_secs(10);
    while !done() {
        assert!(Instant::now() < until, "timed out waiting: {what}");
        std::thread::yield_now();
    }
}

#[test]
fn outcome_moves_from_pending_to_committed_without_passing_through_absent() {
    let (s, gate) = gated_store();
    let a = attempt("x");
    assert_eq!(s.outcome(&a).unwrap(), AttemptOutcome::Absent);
    let cs = request(&s, Variant::Identical);

    gate.arm();
    let ticket = s.submit(a.clone(), s.begin().unwrap(), cs).unwrap();
    let polled = AtomicBool::new(false);
    let observed = std::thread::scope(|sc| {
        // Records every change of the answer until the commit resolves.
        let poller = sc.spawn(|| {
            let until = Instant::now() + Duration::from_secs(10);
            let mut seen: Vec<AttemptOutcome> = Vec::new();
            loop {
                let o = s.outcome(&a).unwrap();
                polled.store(true, Ordering::SeqCst);
                if seen.last() != Some(&o) {
                    seen.push(o.clone());
                }
                if matches!(o, AttemptOutcome::Committed(_)) || Instant::now() > until {
                    return seen;
                }
            }
        });
        // Release only once the commit is held at its barrier and the poller has answered.
        wait_until("commit at its data barrier, poller running", || {
            gate.reached() && polled.load(Ordering::SeqCst)
        });
        // In flight: written, not published. The answer must still be pending.
        assert_eq!(
            s.outcome(&a).unwrap(),
            AttemptOutcome::Pending,
            "in-flight commit"
        );
        gate.release();
        poller.join().unwrap()
    });
    let receipt = ticket.wait().unwrap();
    assert_eq!(receipt.position, HistoryPosition(2));
    assert_eq!(
        observed,
        [AttemptOutcome::Pending, AttemptOutcome::Committed(receipt)],
        "outcome sequence while the commit resolved"
    );
}

#[test]
fn outcome_looks_at_pending_attempts_before_the_published_state() {
    // An attempt leaves `pending` only after its publication. Consulting pending first and the
    // published state second is what makes a finishing commit impossible to miss; no schedule
    // can reliably expose the reverse, so the order of the lock acquisitions themselves is
    // checked (recorded inside the locks, whatever code path takes them).
    use crate::engine::take_lock_trace_for_test as trace;
    // `pending` exactly once and first; afterwards only the published state (the tree lookup
    // consults it again for page bounds).
    let pending_then_state = |t: Vec<&str>| {
        assert!(
            t.len() >= 2 && t[0] == "pending" && t[1..].iter().all(|x| *x == "state"),
            "lookup order {t:?}"
        )
    };
    let (s, gate) = gated_store();
    let a = attempt("x");
    let cs = request(&s, Variant::Identical);
    gate.arm();
    let ticket = s.submit(a.clone(), s.begin().unwrap(), cs).unwrap();
    wait_until("commit at its data barrier", || gate.reached());
    // In flight: answered from `pending` alone.
    trace();
    assert_eq!(s.outcome(&a).unwrap(), AttemptOutcome::Pending);
    assert_eq!(trace(), ["pending"]);
    gate.release();
    let receipt = ticket.wait().unwrap();
    // Finished, and never seen: pending first, then the published state.
    trace();
    assert_eq!(s.outcome(&a).unwrap(), AttemptOutcome::Committed(receipt));
    pending_then_state(trace());
    trace();
    assert_eq!(
        s.outcome(&attempt("never")).unwrap(),
        AttemptOutcome::Absent
    );
    pending_then_state(trace());
}
