//! A request whose later change fails publishes none of its earlier changes: records, both
//! relationship directions, history, the attempt's outcome and bound content all stay as they
//! were, also for a fresh process, and the same attempt can then commit a valid request.

use crate::test_support::{cid, create_on, open_on, put_context as put};

use std::io::Read;

use cce_core::storage::{
    AttemptId, AttemptOutcome, Change, ChangeSet, CommitError, ContentBinding, ContentRef,
    Direction, HistoryPosition, Precondition, ReadView, RelationshipId, RelationshipKind,
    RelationshipQuery, Revision, StagedContent, StorageEngine, StorageError,
};

use crate::NativeStore;
use crate::format::StoreId;
use crate::sim::SimIo;

const STORE: StoreId = StoreId(*b"bundle-atomicity");

/// A store holding records A and B and nothing else.
fn seeded(sim: &SimIo) -> NativeStore {
    let s = create_on(Box::new(sim.clone()), STORE, Default::default()).unwrap();
    s.commit(
        AttemptId::new(*b"seed").unwrap(),
        s.begin().unwrap(),
        ChangeSet {
            changes: vec![
                put("A", Precondition::Absent, b"a", None),
                put("B", Precondition::Absent, b"b", None),
            ],
            ..ChangeSet::default()
        },
    )
    .unwrap();
    s
}

/// The valid head of every bundle: new record C bound to staged content, and relationship R
/// from A to B.
fn valid_head(staged: &StagedContent) -> ChangeSet {
    ChangeSet {
        changes: vec![
            put("C", Precondition::Absent, b"c", Some(0)),
            Change::PutRelationship {
                id: RelationshipId::new(*b"R").unwrap(),
                expect: Precondition::Absent,
                kind: RelationshipKind::new(*b"depends").unwrap(),
                source: cid("A"),
                target: cid("B"),
            },
        ],
        content: vec![ContentBinding::from(staged)],
        ..ChangeSet::default()
    }
}

/// Everything a reader can observe that the bundle would change.
fn observe(s: &NativeStore, content: &ContentRef) -> Vec<String> {
    let mut v = s.begin().unwrap();
    let mut out = Vec::new();
    for id in ["A", "B", "C", "D"] {
        out.push(format!("{id}: {:?}", v.context(&cid(id)).unwrap()));
    }
    for id in ["A", "B"] {
        for direction in [Direction::Outgoing, Direction::Incoming] {
            let q = RelationshipQuery {
                from: cid(id),
                direction,
                kind: None,
                after: None,
            };
            let rels: Vec<_> = v.relationships(&q).unwrap().map(Result::unwrap).collect();
            out.push(format!("{id} {direction:?}: {rels:?}"));
        }
    }
    for h in v.history(HistoryPosition(0)).unwrap() {
        out.push(format!("history {:?}", h.unwrap()));
    }
    out.push(match v.content(content) {
        Ok(mut r) => {
            let mut bytes = Vec::new();
            r.read_to_end(&mut bytes).unwrap();
            format!("content readable, {} bytes", bytes.len())
        }
        Err(e) => format!("content: {e:?}"),
    });
    out
}

/// Runs the failing bundle `valid head + failing`, checks it changed nothing (now and for a
/// fresh process), then commits the valid head under the same attempt.
fn assert_atomic(failing: Change, expected: impl Fn(&CommitError) -> bool) {
    let sim = SimIo::new();
    let s = seeded(&sim);
    let bytes = b"content bound to C ".repeat(100);
    let staged = s.stage_content(&mut &bytes[..]).unwrap();
    let before = observe(&s, &staged.content);
    let generation = s.inner.snapshot().unwrap().desc.generation;
    let attempt = AttemptId::new(*b"bundle").unwrap();

    let mut cs = valid_head(&staged);
    cs.changes.push(failing);
    let r = s.commit(attempt.clone(), s.begin().unwrap(), cs);
    // The state first: whatever the commit answered, nothing of the bundle may be visible.
    assert_eq!(
        observe(&s, &staged.content),
        before,
        "the failed bundle published part of itself ({r:?})"
    );
    assert_eq!(s.inner.snapshot().unwrap().desc.generation, generation);
    assert_eq!(s.outcome(&attempt).unwrap(), AttemptOutcome::Absent);
    match &r {
        Err(e) if expected(e) => {}
        other => panic!("unexpected result: {other:?}"),
    }

    // A fresh process sees the same.
    let content = staged.content;
    drop((staged, s));
    sim.release();
    let s = open_on(Box::new(sim.clone()), Default::default()).unwrap();
    assert_eq!(observe(&s, &content), before, "after reopen");
    assert_eq!(s.outcome(&attempt).unwrap(), AttemptOutcome::Absent);

    // The same attempt now commits the valid part, and all of it is visible.
    let staged = s.stage_content(&mut &bytes[..]).unwrap();
    s.commit(attempt.clone(), s.begin().unwrap(), valid_head(&staged))
        .unwrap();
    let after = observe(&s, &content);
    assert!(after.iter().any(|l| l.starts_with("C: Some")), "{after:?}");
    assert!(
        after
            .iter()
            .any(|l| l.starts_with("A Outgoing: [Relationship")),
        "{after:?}"
    );
    assert!(
        after
            .iter()
            .any(|l| l.starts_with("B Incoming: [Relationship")),
        "{after:?}"
    );
    assert!(
        after.contains(&format!("content readable, {} bytes", bytes.len())),
        "{after:?}"
    );
}

#[test]
fn a_bundle_failing_on_a_stale_precondition_publishes_nothing() {
    assert_atomic(
        put("B", Precondition::AtRevision(Revision(999)), b"stale", None),
        |e| matches!(e, CommitError::Conflict(_)),
    );
}

#[test]
fn a_bundle_failing_on_removing_a_missing_record_publishes_nothing() {
    assert_atomic(
        Change::RemoveContext {
            id: cid("missing"),
            expect: Precondition::Any,
        },
        |e| matches!(e, CommitError::Rejected(StorageError::InvalidRequest(m)) if m.contains("absent")),
    );
}

#[test]
fn a_bundle_failing_on_an_oversized_record_publishes_nothing() {
    assert_atomic(
        put("D", Precondition::Absent, &vec![7; 70_000], None),
        |e| matches!(e, CommitError::Rejected(StorageError::InvalidRequest(m)) if m.contains("too large")),
    );
}

#[test]
fn a_bundle_naming_one_identity_twice_publishes_nothing() {
    assert_atomic(
        put("C", Precondition::Any, b"again", None),
        |e| matches!(e, CommitError::Rejected(StorageError::InvalidRequest(m)) if m.contains("twice")),
    );
}
