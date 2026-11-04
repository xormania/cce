//! Identifier lengths against the metadata key limit, through the port. A relationship's two
//! index keys hold its source, kind, target and id, each with every zero byte escaped to two
//! bytes and a two-byte terminator, after a prefix byte; a relationship whose keys would exceed
//! the key limit is refused before anything is applied, with an error naming it and the lengths.

use crate::test_support::{create_on, open_on};

use cce_core::storage::{
    AttemptId, Change, ChangeSet, CommitError, ContextId, Direction, HistoryPosition, Precondition,
    ReadView, RelationshipId, RelationshipKind, RelationshipQuery, Revision, StorageEngine,
    StorageError,
};

use crate::NativeStore;
use crate::format::StoreId;
use crate::sim::SimIo;
use crate::tree::MAX_KEY;

const STORE: StoreId = StoreId(*b"key-bounds-tests");
/// Largest total of (length + zero bytes) over a relationship's four identifiers that fits:
/// key = 1 + Σ(len + zeros + 2).
const LIMIT: usize = MAX_KEY - 9;

/// An identifier of `len` bytes starting with `tag`, with `zeros` of its other bytes zero.
fn ident(tag: u8, len: usize, zeros: usize) -> Vec<u8> {
    assert!(zeros < len && len <= 255);
    let mut v = vec![b'x'; len];
    v[0] = tag;
    for b in v.iter_mut().skip(1).take(zeros) {
        *b = 0;
    }
    v
}

fn identifiers(total: usize, salt: u8) -> [Vec<u8>; 4] {
    assert!(total >= 4);
    // Lengths: at least one byte each, then filled up to 255 each.
    let mut lens = [1usize; 4];
    let mut left = total.min(4 * 255) - 4;
    for x in lens.iter_mut() {
        let add = left.min(254);
        *x += add;
        left -= add;
    }
    // Zero bytes for the rest (each adds one), never the first byte, which tells ids apart.
    let mut extra = total.saturating_sub(4 * 255);
    let mut zeros = [0usize; 4];
    for (z, len) in zeros.iter_mut().zip(lens) {
        *z = extra.min(len - 1);
        extra -= *z;
    }
    assert_eq!(extra, 0, "{total} is beyond what identifiers can reach");
    let tags = *b"sktr";
    let ids: Vec<Vec<u8>> = (0..4)
        .map(|i| ident(tags[i].wrapping_add(salt), lens[i], zeros[i]))
        .collect();
    let sum: usize = ids
        .iter()
        .map(|v| v.len() + v.iter().filter(|b| **b == 0).count())
        .sum();
    assert_eq!(sum, total);
    [
        ids[0].clone(),
        ids[1].clone(),
        ids[2].clone(),
        ids[3].clone(),
    ]
}

fn sim_create(sim: &SimIo) -> NativeStore {
    create_on(Box::new(sim.clone()), STORE, Default::default()).unwrap()
}

fn commit(s: &NativeStore, attempt: &str, changes: Vec<Change>) -> Result<(), CommitError> {
    s.commit(
        AttemptId::new(attempt.as_bytes().to_vec()).unwrap(),
        s.begin().unwrap(),
        ChangeSet {
            changes,
            ..ChangeSet::default()
        },
    )
    .map(|_| ())
}

/// Commits the two endpoints, then returns the relationship change over `ids`.
fn relationship(s: &NativeStore, attempt: &str, ids: &[Vec<u8>; 4]) -> Change {
    let endpoint = |id: &[u8]| Change::PutContext {
        id: ContextId::new(id.to_vec()).unwrap(),
        expect: Precondition::Any,
        body: b"endpoint".to_vec(),
        content: None,
    };
    commit(s, attempt, vec![endpoint(&ids[0]), endpoint(&ids[2])]).unwrap();
    Change::PutRelationship {
        id: RelationshipId::new(ids[3].clone()).unwrap(),
        expect: Precondition::Absent,
        kind: RelationshipKind::new(ids[1].clone()).unwrap(),
        source: ContextId::new(ids[0].clone()).unwrap(),
        target: ContextId::new(ids[2].clone()).unwrap(),
    }
}

/// The relationships around `source` and `target` in both directions, and the history length.
fn observe(s: &NativeStore, ids: &[Vec<u8>; 4]) -> (Vec<String>, usize) {
    let mut v = s.begin().unwrap();
    let mut rels = Vec::new();
    for (id, direction) in [
        (&ids[0], Direction::Outgoing),
        (&ids[2], Direction::Incoming),
    ] {
        let q = RelationshipQuery {
            from: ContextId::new(id.clone()).unwrap(),
            direction,
            kind: None,
            after: None,
        };
        for r in v.relationships(&q).unwrap() {
            rels.push(format!("{direction:?} {:?}", r.unwrap()));
        }
    }
    let history = v.history(HistoryPosition(0)).unwrap().count();
    (rels, history)
}

/// The precise refusal: names the relationship, its key length and the key limit.
fn assert_refused(r: Result<(), CommitError>, total: usize) {
    let key = total + 9;
    match r {
        Err(CommitError::Rejected(StorageError::InvalidRequest(m))) => {
            assert!(m.contains("relationship"), "{m}");
            assert!(m.contains(&format!("{key} bytes")), "{m}");
            assert!(m.contains(&format!("{MAX_KEY}-byte key limit")), "{m}");
            assert!(!m.contains("entry limit"), "{m}");
        }
        other => panic!("total {total}: expected a precise refusal, got {other:?}"),
    }
}

#[test]
fn a_relationship_at_the_key_limit_commits_and_one_byte_more_is_refused_precisely() {
    let sim = SimIo::new();
    let s = sim_create(&sim);

    let over = identifiers(LIMIT + 1, 0);
    let rel = relationship(&s, "over-endpoints", &over);
    let before = observe(&s, &over);
    assert_refused(commit(&s, "over", vec![rel]), LIMIT + 1);
    assert_eq!(
        observe(&s, &over),
        before,
        "the refused relationship changed the state"
    );

    let at = identifiers(LIMIT, 1);
    let rel = relationship(&s, "at-endpoints", &at);
    commit(&s, "at", vec![rel]).unwrap();
    let (rels, _) = observe(&s, &at);
    assert_eq!(
        rels.len(),
        2,
        "both directions of the relationship: {rels:?}"
    );

    // A fresh process sees the same.
    drop(s);
    sim.release();
    let s = open_on(Box::new(sim.clone()), Default::default()).unwrap();
    // Later commits added history; the refused relationship is still nowhere.
    assert_eq!(observe(&s, &over).0, before.0);
    assert_eq!(observe(&s, &at).0.len(), 2);
}

#[test]
fn an_all_zero_relationship_is_refused_precisely_and_the_store_carries_on() {
    let sim = SimIo::new();
    let s = sim_create(&sim);
    let zeros = identifiers(4 * 255 * 2 - 4, 2);
    assert!(zeros.iter().all(|v| v[1..].iter().all(|b| *b == 0)));
    let rel = relationship(&s, "zero-endpoints", &zeros);
    let before = observe(&s, &zeros);
    assert_refused(commit(&s, "zeros", vec![rel]), 4 * 255 * 2 - 4);
    assert_eq!(observe(&s, &zeros), before);
    // Later requests are unaffected.
    let small = identifiers(40, 3);
    let rel = relationship(&s, "small-endpoints", &small);
    commit(&s, "small", vec![rel]).unwrap();
}

#[test]
fn an_oversized_relationship_is_refused_before_any_change_is_applied() {
    let sim = SimIo::new();
    let s = sim_create(&sim);
    let over = identifiers(LIMIT + 1, 4);
    let rel = relationship(&s, "endpoints", &over);
    // The first change would fail its precondition when applied; the oversized relationship is
    // refused before that, so its own error is the answer.
    let stale = Change::PutContext {
        id: ContextId::new(over[0].clone()).unwrap(),
        expect: Precondition::AtRevision(Revision(999)),
        body: b"stale".to_vec(),
        content: None,
    };
    assert_refused(commit(&s, "both", vec![stale, rel]), LIMIT + 1);
}

#[test]
fn four_full_length_identifiers_without_zero_bytes_fit() {
    let sim = SimIo::new();
    let s = sim_create(&sim);
    let full = identifiers(4 * 255, 5);
    assert!(full.iter().all(|v| v.len() == 255 && !v.contains(&0)));
    let rel = relationship(&s, "full-endpoints", &full);
    commit(&s, "full", vec![rel]).unwrap();
    assert_eq!(observe(&s, &full).0.len(), 2, "both directions");
    drop(s);
    sim.release();
    let s = open_on(Box::new(sim.clone()), Default::default()).unwrap();
    assert_eq!(
        observe(&s, &full).0.len(),
        2,
        "both directions after reopen"
    );
}
