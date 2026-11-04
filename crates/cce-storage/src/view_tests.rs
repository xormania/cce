//! Concurrent views and logical conflict validation. Schedules are explicit: the test decides the
//! order of view creation, intervening commits and the final commit.

use cce_core::storage::{
    AttemptId, Change, ChangeSet, CommitError, ContextId, Direction, Precondition, ReadView,
    RelationshipId, RelationshipKind, RelationshipQuery, StorageEngine,
};

use crate::NativeStore;
use crate::engine::Inner;
use crate::format::StoreId;
use crate::sim::SimIo;

fn store() -> NativeStore {
    NativeStore {
        inner: Inner::create(Box::new(SimIo::new()), StoreId([6; 16]), Default::default()).unwrap(),
    }
}

fn cid(s: &str) -> ContextId {
    ContextId::new(s.as_bytes().to_vec()).unwrap()
}

fn put(id: &str) -> Change {
    Change::PutContext {
        id: cid(id),
        expect: Precondition::Any,
        body: id.as_bytes().to_vec(),
        content: None,
    }
}

fn rel(id: &str, kind: &str, src: &str, dst: &str) -> Change {
    Change::PutRelationship {
        id: RelationshipId::new(id.as_bytes().to_vec()).unwrap(),
        expect: Precondition::Absent,
        kind: RelationshipKind::new(kind.as_bytes().to_vec()).unwrap(),
        source: cid(src),
        target: cid(dst),
    }
}

fn cs(changes: Vec<Change>) -> ChangeSet {
    ChangeSet {
        changes,
        ..ChangeSet::default()
    }
}

fn commit_now(s: &NativeStore, a: &str, c: Vec<Change>) {
    s.commit(
        AttemptId::new(a.as_bytes().to_vec()).unwrap(),
        s.begin().unwrap(),
        cs(c),
    )
    .unwrap();
}

fn commit_from(
    s: &NativeStore,
    a: &str,
    v: crate::NativeView,
    c: Vec<Change>,
) -> Result<u64, CommitError> {
    s.commit(AttemptId::new(a.as_bytes().to_vec()).unwrap(), v, cs(c))
        .map(|r| r.position.0)
}

fn deps(from: &str) -> RelationshipQuery {
    RelationshipQuery {
        from: cid(from),
        direction: Direction::Incoming,
        kind: Some(RelationshipKind::new(*b"requires").unwrap()),
        after: None,
    }
}

#[test]
fn an_absence_observation_conflicts_with_an_intervening_insert() {
    let s = store();
    let mut v = s.begin().unwrap();
    assert!(v.context(&cid("A")).unwrap().is_none());
    commit_now(&s, "other", vec![put("A")]);
    let r = commit_from(&s, "mine", v, vec![put("B")]);
    assert!(
        matches!(&r, Err(CommitError::Conflict(m)) if m.contains("\"A\"") || m.contains("A")),
        "{r:?}"
    );

    // Positive neighbour: an absence of something nobody wrote stays valid.
    let mut v = s.begin().unwrap();
    assert!(v.context(&cid("C")).unwrap().is_none());
    commit_now(&s, "other2", vec![put("D")]);
    commit_from(&s, "mine2", v, vec![put("E")])
        .expect("unrelated intervening write must not conflict");
}

#[test]
fn an_empty_dependency_range_conflicts_with_an_intervening_dependent() {
    let s = store();
    commit_now(&s, "seed", vec![put("X")]);
    // Stale schedule: no dependents observed; another writer adds one; the removal must not commit.
    let mut v = s.begin().unwrap();
    assert_eq!(v.relationships(&deps("X")).unwrap().count(), 0);
    commit_now(
        &s,
        "dependent",
        vec![put("Y"), rel("r1", "requires", "Y", "X")],
    );
    let removal = vec![Change::RemoveContext {
        id: cid("X"),
        expect: Precondition::Any,
    }];
    let r = commit_from(&s, "remove-x", v, removal.clone());
    assert!(
        matches!(r, Err(CommitError::Conflict(_))),
        "stale empty range was not protected: {r:?}"
    );

    // Fresh schedule: preparing again sees the dependent; a host that still removes does so knowingly.
    let mut v = s.begin().unwrap();
    assert_eq!(v.relationships(&deps("X")).unwrap().count(), 1);
    commit_from(&s, "remove-x-fresh", v, removal).expect("fresh preparation commits");
}

#[test]
fn the_protected_domain_is_the_whole_queried_range_and_nothing_else() {
    let s = store();
    commit_now(
        &s,
        "seed",
        vec![
            put("X"),
            rel("r1", "requires", "A", "X"),
            rel("r2", "requires", "B", "X"),
        ],
    );
    // Partially consume the range: read only the first dependent.
    let mut v = s.begin().unwrap();
    assert_eq!(v.relationships(&deps("X")).unwrap().take(1).count(), 1);
    // An insertion after the consumed part, still inside the queried range, conflicts.
    commit_now(&s, "late", vec![rel("r3", "requires", "Z", "X")]);
    assert!(matches!(
        commit_from(&s, "m1", v, vec![put("Q")]),
        Err(CommitError::Conflict(_))
    ));

    // A different kind, or a different node, is outside the queried range: no conflict.
    let mut v = s.begin().unwrap();
    let _ = v.relationships(&deps("X")).unwrap().count();
    commit_now(
        &s,
        "other-kind",
        vec![
            rel("r4", "cites", "C", "X"),
            rel("r5", "requires", "C", "W"),
        ],
    );
    commit_from(&s, "m2", v, vec![put("Q2")])
        .expect("writes outside the queried range must not conflict");
}

#[test]
fn disjoint_changes_from_one_basis_both_commit_and_keep_each_other() {
    let s = store();
    commit_now(&s, "seed", vec![put("A"), put("B")]);
    let mut v1 = s.begin().unwrap();
    let mut v2 = s.begin().unwrap();
    assert!(v1.context(&cid("A")).unwrap().is_some());
    assert!(v2.context(&cid("B")).unwrap().is_some());
    commit_from(
        &s,
        "first",
        v1,
        vec![put("A"), rel("ra", "cites", "A", "B")],
    )
    .unwrap();
    commit_from(
        &s,
        "second",
        v2,
        vec![put("B"), rel("rb", "cites", "B", "A")],
    )
    .expect("disjoint change from the same basis");
    let mut now = s.begin().unwrap();
    let out_a: Vec<_> = now
        .relationships(&RelationshipQuery {
            from: cid("A"),
            direction: Direction::Outgoing,
            kind: None,
            after: None,
        })
        .unwrap()
        .map(|r| r.unwrap().id)
        .collect();
    let out_b: Vec<_> = now
        .relationships(&RelationshipQuery {
            from: cid("B"),
            direction: Direction::Outgoing,
            kind: None,
            after: None,
        })
        .unwrap()
        .map(|r| r.unwrap().id)
        .collect();
    assert_eq!(out_a.len(), 1, "the first commit's relationship was lost");
    assert_eq!(out_b.len(), 1);
}

#[test]
fn competing_updates_to_one_observed_record_do_not_both_commit() {
    let s = store();
    commit_now(&s, "seed", vec![put("A")]);
    let mut v1 = s.begin().unwrap();
    let mut v2 = s.begin().unwrap();
    let a1 = v1.context(&cid("A")).unwrap().unwrap();
    let a2 = v2.context(&cid("A")).unwrap().unwrap();
    assert_eq!(a1.revision, a2.revision);
    commit_from(&s, "w1", v1, vec![put("A")]).unwrap();
    let r = commit_from(&s, "w2", v2, vec![put("A")]);
    match r {
        Err(CommitError::Conflict(m)) => {
            assert!(m.contains('A'), "conflict should name the record: {m}")
        }
        other => panic!("second writer overwrote the first: {other:?}"),
    }
}

#[test]
fn a_basis_older_than_retained_conflict_history_is_refused_explicitly() {
    let s = NativeStore {
        inner: Inner::create(
            Box::new(SimIo::new()),
            StoreId([6; 16]),
            crate::engine::Options {
                conflict_window: 2,
                ..Default::default()
            },
        )
        .unwrap(),
    };
    let mut old = s.begin().unwrap();
    let _ = old.context(&cid("A")).unwrap();
    for i in 0..3 {
        commit_now(&s, &format!("c{i}"), vec![put(&format!("N{i}"))]);
    }
    let r = commit_from(&s, "late", old, vec![put("Z")]);
    assert!(
        matches!(&r, Err(CommitError::Conflict(m)) if m.contains("retained")),
        "{r:?}"
    );
    // The store is unaffected and a fresh basis commits.
    commit_now(&s, "fresh", vec![put("Z")]);
}

#[test]
fn an_old_view_keeps_its_whole_state_while_new_views_see_the_update() {
    use cce_core::storage::ContentBinding;
    use std::io::Read;
    let s = store();
    let v1 = s.stage_content(&mut &b"first content"[..]).unwrap();
    s.commit(
        AttemptId::new(*b"c1").unwrap(),
        s.begin().unwrap(),
        ChangeSet {
            changes: vec![Change::PutContext {
                id: cid("A"),
                expect: Precondition::Absent,
                body: b"old".to_vec(),
                content: Some(0),
            }],
            content: vec![ContentBinding::from(&v1)],
            ..ChangeSet::default()
        },
    )
    .unwrap();
    let mut old = s.begin().unwrap();
    let v2 = s.stage_content(&mut &b"second content"[..]).unwrap();
    s.commit(
        AttemptId::new(*b"c2").unwrap(),
        s.begin().unwrap(),
        ChangeSet {
            changes: vec![
                Change::PutContext {
                    id: cid("A"),
                    expect: Precondition::Any,
                    body: b"new".to_vec(),
                    content: Some(0),
                },
                rel("r", "cites", "A", "B"),
            ],
            content: vec![ContentBinding::from(&v2)],
            ..ChangeSet::default()
        },
    )
    .unwrap();
    let mut new = s.begin().unwrap();
    assert_eq!(old.context(&cid("A")).unwrap().unwrap().body, b"old");
    assert_eq!(new.context(&cid("A")).unwrap().unwrap().body, b"new");
    let q = RelationshipQuery {
        from: cid("A"),
        direction: Direction::Outgoing,
        kind: None,
        after: None,
    };
    assert_eq!(
        old.relationships(&q).unwrap().count(),
        0,
        "old view saw a later relationship"
    );
    assert_eq!(new.relationships(&q).unwrap().count(), 1);
    // A content reader opened from the old view stays valid after that view is dropped.
    let c = old.context(&cid("A")).unwrap().unwrap().content.unwrap();
    let mut r = old.content(&c).unwrap();
    drop(old);
    let mut got = Vec::new();
    r.read_to_end(&mut got).unwrap();
    assert_eq!(got, b"first content");
}

#[test]
fn a_basis_from_another_store_is_rejected() {
    use cce_core::storage::StorageError;
    let a = store();
    let b = store();
    let foreign = b.begin().unwrap();
    let r = a.commit(AttemptId::new(*b"x").unwrap(), foreign, cs(vec![put("A")]));
    assert_eq!(r, Err(CommitError::Rejected(StorageError::ForeignHandle)));
    commit_now(&a, "own", vec![put("A")]);
}

fn store_with(opts: crate::engine::Options) -> NativeStore {
    NativeStore {
        inner: Inner::create(Box::new(SimIo::new()), StoreId([7; 16]), opts).unwrap(),
    }
}

fn wait_resolved(s: &NativeStore, a: &AttemptId) -> cce_core::storage::AttemptOutcome {
    use cce_core::storage::AttemptOutcome;
    let until = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        match s.outcome(a).unwrap() {
            AttemptOutcome::Pending if std::time::Instant::now() < until => {
                std::thread::yield_now()
            }
            other => return other,
        }
    }
}

#[test]
fn an_admitted_commit_completes_after_its_caller_goes_away() {
    use cce_core::storage::{AttemptOutcome, HistoryPosition};
    let s = store();
    let gate = s.inner.pause_worker_for_test();
    let a = AttemptId::new(*b"abandoned").unwrap();
    let ticket = s
        .submit(a.clone(), s.begin().unwrap(), cs(vec![put("A")]))
        .unwrap();
    // Admitted but not yet published: the outcome is pending, not absent.
    assert_eq!(s.outcome(&a).unwrap(), AttemptOutcome::Pending);
    drop(ticket);
    drop(gate);
    match wait_resolved(&s, &a) {
        AttemptOutcome::Committed(r) => assert_eq!(r.position, HistoryPosition(1)),
        other => panic!("abandoned commit was lost: {other:?}"),
    }
    let mut v = s.begin().unwrap();
    assert_eq!(
        v.history(HistoryPosition(0)).unwrap().count(),
        1,
        "the commit must apply exactly once"
    );
}

#[test]
fn a_full_commit_queue_refuses_admission_explicitly_and_reads_continue() {
    use cce_core::storage::StorageError;
    let s = store_with(crate::engine::Options {
        commit_queue: 1,
        ..Default::default()
    });
    commit_now(&s, "seed", vec![put("S")]);
    let gate = s.inner.pause_worker_for_test();
    let first = s
        .submit(
            AttemptId::new(*b"q1").unwrap(),
            s.begin().unwrap(),
            cs(vec![put("A")]),
        )
        .unwrap();
    let mut queued = Vec::new();
    let mut refused = None;
    for i in 0..4 {
        match s.submit(
            AttemptId::new(format!("q{}", i + 2).into_bytes()).unwrap(),
            s.begin().unwrap(),
            cs(vec![put(&format!("B{i}"))]),
        ) {
            Ok(t) => queued.push(t),
            Err(e) => {
                refused = Some(e);
                break;
            }
        }
    }
    assert!(
        matches!(refused, Some(CommitError::Rejected(StorageError::Busy(_)))),
        "no backpressure: {refused:?}"
    );
    // Committed state stays readable while writes are backed up.
    assert!(s.begin().unwrap().context(&cid("S")).unwrap().is_some());
    drop(gate);
    first.wait().unwrap();
    for t in queued {
        t.wait().unwrap();
    }
}

#[test]
fn a_panicking_commit_leaves_no_attempt_pending_and_fences_writes() {
    use cce_core::storage::AttemptOutcome;
    let s = store();
    let gate = s.inner.pause_worker_for_test();
    let [a, b, c] = ["a", "b", "c"].map(|x| AttemptId::new(x.as_bytes().to_vec()).unwrap());
    s.inner.panic_next_commit_for_test();
    let ta = s
        .submit(a.clone(), s.begin().unwrap(), cs(vec![put("A")]))
        .unwrap();
    let tb = s
        .submit(b.clone(), s.begin().unwrap(), cs(vec![put("B")]))
        .unwrap();
    let tc = s
        .submit(c.clone(), s.begin().unwrap(), cs(vec![put("C")]))
        .unwrap();
    drop(gate);

    // The attempt whose commit panicked is uncertain, never left pending.
    let r = ta.wait();
    assert!(matches!(r, Err(CommitError::Indeterminate(_))), "{r:?}");
    assert_eq!(wait_resolved(&s, &a), AttemptOutcome::Unknown);
    // Queued attempts are answered and resolved too; none of them commits behind the fence.
    for (t, x) in [(tb, &b), (tc, &c)] {
        let r = t.wait();
        assert!(matches!(r, Err(CommitError::Rejected(_))), "{r:?}");
        assert_ne!(wait_resolved(&s, x), AttemptOutcome::Pending);
    }
    for id in ["A", "B", "C"] {
        assert!(s.begin().unwrap().context(&cid(id)).unwrap().is_none());
    }
    // The worker is still serving: a new request gets an explicit fenced refusal, not a hang.
    let d = AttemptId::new(b"d".to_vec()).unwrap();
    let r = s.commit(d, s.begin().unwrap(), cs(vec![put("D")]));
    assert!(
        matches!(&r, Err(CommitError::Rejected(cce_core::storage::StorageError::StoreState(m))) if m.contains("fenced")),
        "{r:?}"
    );
}

#[test]
fn closing_waits_for_every_admitted_commit_and_refuses_new_ones() {
    use cce_core::storage::{AttemptOutcome, StorageError};
    use std::sync::Arc;
    let s = Arc::new(store());
    let gate = s.inner.pause_worker_for_test();
    let [a, b, late] = ["a", "b", "late"].map(|x| AttemptId::new(x.as_bytes().to_vec()).unwrap());
    let ta = s
        .submit(a.clone(), s.begin().unwrap(), cs(vec![put("A")]))
        .unwrap();
    let tb = s
        .submit(b.clone(), s.begin().unwrap(), cs(vec![put("B")]))
        .unwrap();
    // Close from another thread; record what the store says the moment close returns.
    let closer = {
        let s = s.clone();
        let (a, b) = (a.clone(), b.clone());
        std::thread::spawn(move || {
            s.inner.close().unwrap();
            (s.outcome(&a).unwrap(), s.outcome(&b).unwrap())
        })
    };
    // Close has begun (new work is refused) while the admitted commits still wait.
    let mut refused = false;
    for _ in 0..10 {
        match s.submit(late.clone(), s.begin().unwrap(), cs(vec![put("L")])) {
            Err(CommitError::Rejected(StorageError::StoreState(m))) if m.contains("closing") => {
                refused = true;
                break;
            }
            Err(e) => panic!("unexpected refusal: {e:?}"),
            // Admitted before close began: it must still complete before close returns.
            Ok(t) => drop(t),
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    assert!(refused, "close never began refusing new commits");
    assert!(
        !closer.is_finished(),
        "close returned while admitted commits were waiting"
    );
    drop(gate);
    let (oa, ob) = closer.join().unwrap();
    assert!(matches!(oa, AttemptOutcome::Committed(_)), "{oa:?}");
    assert!(matches!(ob, AttemptOutcome::Committed(_)), "{ob:?}");
    assert!(ta.wait().is_ok() && tb.wait().is_ok());
}
