//! Concurrent views and logical conflict validation. Schedules are explicit: the test decides the
//! order of view creation, intervening commits and the final commit.

use scientdb_kernel::storage::{
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
