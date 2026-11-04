//! Crash cut sweeps over commits that reshape the metadata tree: one that splits a leaf below the
//! root, one that splits the root branch (the tree grows a level) and one whose deletions shrink
//! the root back to a leaf. Each starts from a compact deterministic store just before the event;
//! the probe proves on the real engine that the commit causes it.

use crate::test_support::{create_on, open_on};

use std::sync::{Arc, Mutex};

use cce_core::storage::{
    AttemptId, AttemptOutcome, Change, ChangeSet, CommitError, CommitReceipt, ContextId,
    HistoryEntry, HistoryPosition, Precondition, ReadView, StorageEngine,
};

use crate::NativeStore;
use crate::format::{PageRef, StoreId};
use crate::io::{Io, Op, Traced};
use crate::repro_tests::{PROCESS_EXIT, survival, with_repro};
use crate::sim::{Persist, SimIo};
use crate::tree::{Node, NodeSource};

const STORE: StoreId = StoreId(*b"split-sweep-test");
/// Long identifiers make long separators, so branches fill after a few dozen leaves.
const ID_LEN: usize = 200;

fn rid(i: usize) -> ContextId {
    let mut id = format!("R{i:04}").into_bytes();
    id.resize(ID_LEN, b'x');
    ContextId::new(id).unwrap()
}

type Trace = Arc<Mutex<Vec<Op>>>;

fn create(sim: &SimIo, trace: &Trace) -> NativeStore {
    let io: Box<dyn Io> = Box::new(Traced {
        inner: sim.clone(),
        trace: trace.clone(),
    });
    create_on(io, STORE, Default::default()).unwrap()
}

/// Step `i` of the fixture: record `i` with a large body, in a commit of its own.
fn step(s: &NativeStore, i: usize) {
    s.commit(
        AttemptId::new(format!("s{i}").into_bytes()).unwrap(),
        s.begin().unwrap(),
        ChangeSet {
            changes: vec![Change::PutContext {
                id: rid(i),
                expect: Precondition::Any,
                body: vec![i as u8; 900],
                content: None,
            }],
            ..ChangeSet::default()
        },
    )
    .unwrap();
}

/// Nodes per level of the current tree, root first.
fn shape(s: &NativeStore) -> Vec<usize> {
    let root = s.inner.snapshot().unwrap().desc.root;
    let mut levels = Vec::new();
    let mut level: Vec<PageRef> = vec![root];
    while !level.is_empty() {
        levels.push(level.len());
        let mut next = Vec::new();
        for at in level {
            if let Node::Branch { children, .. } = &*s.inner.load(at).unwrap() {
                next.extend(children.iter().copied());
            }
        }
        level = next;
    }
    levels
}

/// Every record the fixture can create, with revision and body, and every retained history entry
/// in full. A history entry that cannot be read fails the caller: counting entries would accept a
/// key whose entry is lost.
fn logical_state(s: &NativeStore, records: usize) -> Vec<String> {
    let mut v = s.begin().unwrap();
    let mut out: Vec<String> = (0..records)
        .map(|i| match v.context(&rid(i)).unwrap() {
            None => format!("{i} absent"),
            Some(r) => format!(
                "{i} rev {} body {} x {}",
                r.revision.0,
                r.body.len(),
                r.body[0]
            ),
        })
        .collect();
    let history: Vec<HistoryEntry> = v
        .history(HistoryPosition(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap_or_else(|e| panic!("a retained history entry is unreadable: {e:?}"));
    out.extend(history.iter().map(|h| format!("history {h:?}")));
    out
}

/// The commit a sweep cuts.
type SweptCommit = Box<dyn Fn(&NativeStore) -> Result<CommitReceipt, CommitError>>;

/// One transaction to sweep: the fixture steps before it, the commit itself (attempt "swept")
/// and how many records the state covers.
struct Case {
    name: &'static str,
    fixture: usize,
    commit: SweptCommit,
    records: usize,
}

fn swept(s: &NativeStore, changes: Vec<Change>) -> Result<CommitReceipt, CommitError> {
    s.commit(
        AttemptId::new(*b"swept").unwrap(),
        s.begin().unwrap(),
        ChangeSet {
            changes,
            ..ChangeSet::default()
        },
    )
}

/// The first fixture length whose next step changes the tree's shape as `event` says.
fn first_step_where(event: impl Fn(&[usize], &[usize]) -> bool) -> usize {
    let sim = SimIo::new();
    let s = create(&sim, &Trace::default());
    for k in 0..400 {
        let before = shape(&s);
        step(&s, k);
        if event(&before, &shape(&s)) {
            return k;
        }
    }
    panic!("no step of the fixture caused the event");
}

/// Sweeps every cut of the case's commit under every survival choice and a process exit.
fn sweep(case: &Case, event: impl Fn(&[usize], &[usize]) -> bool) {
    // Probe: the commit causes the event, and how many operations it takes.
    let probe = SimIo::new();
    let s = create(&probe, &Trace::default());
    for i in 0..case.fixture {
        step(&s, i);
    }
    let (before, old) = (shape(&s), logical_state(&s, case.records));
    let start = probe.ops();
    let receipt = (case.commit)(&s).unwrap();
    let total = probe.ops() - start;
    let (after, new) = (shape(&s), logical_state(&s, case.records));
    assert!(
        event(&before, &after),
        "{}: the commit did not cause the event ({before:?} -> {after:?})",
        case.name
    );
    assert_ne!(old, new);
    eprintln!(
        "{}: {} fixture commits, tree levels {before:?} -> {after:?}, {total} operations swept",
        case.name, case.fixture
    );
    // The publication record write is the second-to-last operation.
    let publication = total - 2;
    drop(s);

    for cut in 0..=total {
        let sim = SimIo::new();
        let trace = Trace::default();
        let s = create(&sim, &trace);
        for i in 0..case.fixture {
            step(&s, i);
        }
        trace.lock().unwrap().clear();
        sim.crash_after(cut);
        let acked = (case.commit)(&s).is_ok();
        assert_eq!(acked, cut == total, "{} cut {cut}", case.name);
        drop(s);
        let trace = trace.lock().unwrap().clone();
        let images = [Persist::None, Persist::All]
            .into_iter()
            .chain((1..=12).map(Persist::Seeded))
            .map(|p| (survival(p), sim.after_crash(p)))
            .chain(std::iter::once((
                PROCESS_EXIT.to_string(),
                sim.after_process_exit(),
            )));
        for (survived, img) in images {
            let label = format!("{} cut {cut}, {survived}", case.name);
            let pristine = img.after_process_exit();
            with_repro(&label, &[("image", &pristine)], &trace, || {
                let rec = open_on(Box::new(img.clone()), Default::default()).unwrap();
                let got = logical_state(&rec, case.records);
                assert!(
                    got == old || got == new,
                    "{label}: partial state:\n{got:#?}"
                );
                if acked {
                    assert_eq!(got, new, "{label}: acknowledged commit lost");
                }
                if cut <= publication && survived != PROCESS_EXIT {
                    assert_eq!(got, old, "{label}: visible before its publication record");
                }
                let outcome = rec.outcome(&receipt.attempt).unwrap();
                if got == new {
                    assert_eq!(
                        outcome,
                        AttemptOutcome::Committed(receipt.clone()),
                        "{label}"
                    );
                } else {
                    assert_eq!(outcome, AttemptOutcome::Absent, "{label}");
                }
                // The recovered store keeps working: one more commit, then a fresh open.
                rec.commit(
                    AttemptId::new(*b"next").unwrap(),
                    rec.begin().unwrap(),
                    ChangeSet {
                        changes: vec![Change::PutContext {
                            id: ContextId::new(*b"next").unwrap(),
                            expect: Precondition::Absent,
                            body: b"next".to_vec(),
                            content: None,
                        }],
                        ..ChangeSet::default()
                    },
                )
                .unwrap_or_else(|e| panic!("{label}: next commit: {e:?}"));
                let later = logical_state(&rec, case.records);
                drop(rec);
                img.release();
                let reopened = open_on(Box::new(img), Default::default()).unwrap();
                assert_eq!(
                    logical_state(&reopened, case.records),
                    later,
                    "{label}: reopen"
                );
            });
        }
    }
}

fn leaf_split(before: &[usize], after: &[usize]) -> bool {
    before.len() >= 2
        && after.len() == before.len()
        && after[after.len() - 1] == before[before.len() - 1] + 1
}

fn root_split(before: &[usize], after: &[usize]) -> bool {
    before.len() >= 2 && after.len() == before.len() + 1
}

#[test]
fn every_cut_of_a_commit_that_splits_a_leaf_below_the_root_recovers() {
    let k = first_step_where(leaf_split);
    sweep(
        &Case {
            name: "leaf split",
            fixture: k,
            // The same change as fixture step k, under the swept attempt.
            commit: Box::new(move |s| {
                swept(
                    s,
                    vec![Change::PutContext {
                        id: rid(k),
                        expect: Precondition::Any,
                        body: vec![k as u8; 900],
                        content: None,
                    }],
                )
            }),
            records: k + 1,
        },
        leaf_split,
    );
}

#[test]
fn every_cut_of_a_commit_that_splits_the_root_branch_recovers() {
    let k = first_step_where(root_split);
    sweep(
        &Case {
            name: "root branch split",
            fixture: k,
            commit: Box::new(move |s| {
                swept(
                    s,
                    vec![Change::PutContext {
                        id: rid(k),
                        expect: Precondition::Any,
                        body: vec![k as u8; 900],
                        content: None,
                    }],
                )
            }),
            records: k + 1,
        },
        root_split,
    );
}

#[test]
fn every_cut_of_a_commit_whose_deletions_shrink_the_root_recovers() {
    // The first store with a two-level tree; the swept commit removes all its records but one.
    let k = first_step_where(|b, a| b.len() == 1 && a.len() == 2) + 1;
    sweep(
        &Case {
            name: "root shrink",
            fixture: k,
            commit: Box::new(move |s| {
                swept(
                    s,
                    (1..k)
                        .map(|i| Change::RemoveContext {
                            id: rid(i),
                            expect: Precondition::Any,
                        })
                        .collect(),
                )
            }),
            records: k,
        },
        |b, a| b.len() >= 2 && a.len() == 1,
    );
}
