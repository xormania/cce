//! Concurrent views: removals and boundary keys inside a protected relationship range, the exact
//! combined state of disjoint commits from one basis, and views taken from an earlier open of the
//! same store.

use std::io::Read;

use cce_core::storage::{
    AttemptId, AttemptOutcome, Change, ChangeSet, CommitError, ContentBinding, ContextId,
    Direction, Precondition, ReadView, Relationship, RelationshipId, RelationshipKind,
    RelationshipQuery, Revision, StorageEngine, StorageError,
};

use crate::NativeStore;
use crate::engine::Inner;
use crate::format::StoreId;
use crate::io::FileId;
use crate::sim::SimIo;

const STORE: StoreId = StoreId([7; 16]);

fn store_on(sim: &SimIo) -> NativeStore {
    NativeStore {
        inner: Inner::create(Box::new(sim.clone()), STORE, Default::default()).unwrap(),
    }
}

fn store() -> NativeStore {
    store_on(&SimIo::new())
}

fn cid(s: &[u8]) -> ContextId {
    ContextId::new(s.to_vec()).unwrap()
}

fn rid(s: &str) -> RelationshipId {
    RelationshipId::new(s.as_bytes().to_vec()).unwrap()
}

fn kind(s: &[u8]) -> RelationshipKind {
    RelationshipKind::new(s.to_vec()).unwrap()
}

fn attempt(s: &str) -> AttemptId {
    AttemptId::new(s.as_bytes().to_vec()).unwrap()
}

fn put(id: &[u8], body: &[u8]) -> Change {
    Change::PutContext {
        id: cid(id),
        expect: Precondition::Any,
        body: body.to_vec(),
        content: None,
    }
}

fn rel(id: &str, k: &[u8], src: &[u8], dst: &[u8]) -> Change {
    Change::PutRelationship {
        id: rid(id),
        expect: Precondition::Absent,
        kind: kind(k),
        source: cid(src),
        target: cid(dst),
    }
}

fn unrel(id: &str) -> Change {
    Change::RemoveRelationship {
        id: rid(id),
        expect: Precondition::Any,
    }
}

fn cs(changes: Vec<Change>) -> ChangeSet {
    ChangeSet {
        changes,
        ..ChangeSet::default()
    }
}

fn commit_now(s: &NativeStore, a: &str, c: Vec<Change>) -> u64 {
    s.commit(attempt(a), s.begin().unwrap(), cs(c))
        .unwrap()
        .position
        .0
}

fn commit_from(
    s: &NativeStore,
    a: &str,
    v: crate::NativeView,
    c: Vec<Change>,
) -> Result<u64, CommitError> {
    s.commit(attempt(a), v, cs(c)).map(|r| r.position.0)
}

/// Incoming `requires` relationships of X: the range every case below protects.
fn deps_of_x() -> RelationshipQuery {
    RelationshipQuery {
        from: cid(b"X"),
        direction: Direction::Incoming,
        kind: Some(kind(b"requires")),
        after: None,
    }
}

/// A store with three dependents of X, and a view that consumed only the first of them.
fn seeded_with_partial_view() -> (NativeStore, crate::NativeView) {
    let s = store();
    commit_now(
        &s,
        "seed",
        vec![
            put(b"X", b"x"),
            rel("r1", b"requires", b"A", b"X"),
            rel("r2", b"requires", b"B", b"X"),
            rel("r3", b"requires", b"C", b"X"),
            rel("out", b"requires", b"X", b"W"),
            rel("cite", b"cites", b"D", b"X"),
        ],
    );
    let mut v = s.begin().unwrap();
    let first: Vec<_> = v
        .relationships(&deps_of_x())
        .unwrap()
        .take(1)
        .map(|r| r.unwrap().id)
        .collect();
    assert_eq!(first, vec![rid("r1")]);
    (s, v)
}

fn assert_conflict_naming(r: &Result<u64, CommitError>, names: &[&str]) {
    match r {
        Err(CommitError::Conflict(m)) => {
            for n in names {
                assert!(m.contains(n), "conflict {m:?} does not name {n:?}");
            }
        }
        other => panic!("expected a conflict naming {names:?}, got {other:?}"),
    }
}

#[test]
fn removing_a_relationship_inside_a_partially_consumed_range_conflicts() {
    // The unconsumed part of the range and the consumed part are protected alike.
    for (removed, src) in [("r3", "C"), ("r1", "A")] {
        let (s, v) = seeded_with_partial_view();
        commit_now(&s, "remove", vec![unrel(removed)]);
        let r = commit_from(&s, "mine", v, vec![put(b"Q", b"q")]);
        assert_conflict_naming(
            &r,
            &[
                "incoming relationship index",
                "\"X\"",
                "\"requires\"",
                src,
                removed,
            ],
        );
        assert!(
            s.begin().unwrap().context(&cid(b"Q")).unwrap().is_none(),
            "a conflicting commit wrote"
        );
    }

    // Positive neighbours: removing a relationship of X outside the queried range (another
    // direction, another kind) does not conflict.
    for removed in ["out", "cite"] {
        let (s, v) = seeded_with_partial_view();
        commit_now(&s, "remove", vec![unrel(removed)]);
        commit_from(&s, "mine", v, vec![put(b"Q", b"q")])
            .unwrap_or_else(|e| panic!("removing {removed} outside the range: {e:?}"));
        assert!(s.begin().unwrap().context(&cid(b"Q")).unwrap().is_some());
    }
}

#[test]
fn writes_at_the_edges_of_a_protected_range_conflict_only_inside_it() {
    let lowest = [1u8];
    let highest = [0xFFu8; 255];
    // Just inside: the smallest and largest possible source, and a source containing a zero byte
    // (escaped in the key).
    let inside: [(&str, &[u8]); 3] = [
        ("in-low", &lowest),
        ("in-high", &highest),
        ("in-zero", b"B\0"),
    ];
    for (id, src) in inside {
        let (s, v) = seeded_with_partial_view();
        commit_now(&s, "edge", vec![rel(id, b"requires", src, b"X")]);
        let r = commit_from(&s, "mine", v, vec![put(b"Q", b"q")]);
        assert_conflict_naming(&r, &["incoming relationship index", id]);
    }

    // Just outside: a target that extends X (with and without a zero byte), a kind that extends
    // or truncates "requires", and the other direction of X. Each is the nearest key family to
    // the protected range without belonging to it.
    let outside: [Edge; 6] = [
        ("out-node-zero", b"requires", b"A", b"X\0"),
        ("out-node-longer", b"requires", b"A", b"XA"),
        ("out-kind-zero", b"requires\0", b"A", b"X"),
        ("out-kind-longer", b"requiress", b"A", b"X"),
        ("out-kind-shorter", b"require", b"A", b"X"),
        ("out-direction", b"requires", b"X", b"A"),
    ];
    for (id, k, src, dst) in outside {
        let (s, v) = seeded_with_partial_view();
        commit_now(&s, "edge", vec![rel(id, k, src, dst)]);
        commit_from(&s, "mine", v, vec![put(b"Q", b"q")])
            .unwrap_or_else(|e| panic!("{id} is outside the range: {e:?}"));
    }
}

/// Context records, content bytes, relationships of both directions, and history, as one value.
/// A relationship id with its kind, source and target.
type Edge = (&'static str, &'static [u8], &'static [u8], &'static [u8]);

/// A context id with its revision, body and bound content (length and bytes).
type ContextState = (Vec<u8>, Revision, Vec<u8>, Option<(u64, Vec<u8>)>);

#[derive(Debug, PartialEq)]
struct Contents {
    contexts: Vec<ContextState>,
    relationships: Vec<Relationship>,
    history: Vec<(u64, AttemptId, usize)>,
}

fn contents(s: &NativeStore, ids: &[&[u8]]) -> Contents {
    let mut v = s.begin().unwrap();
    let mut out = Contents {
        contexts: Vec::new(),
        relationships: Vec::new(),
        history: Vec::new(),
    };
    for id in ids {
        let Some(r) = v.context(&cid(id)).unwrap() else {
            continue;
        };
        let content = r.content.map(|c| {
            let mut bytes = Vec::new();
            v.content(&c).unwrap().read_to_end(&mut bytes).unwrap();
            assert_eq!(bytes.len() as u64, c.len);
            (c.len, bytes)
        });
        out.contexts
            .push((id.to_vec(), r.revision, r.body.clone(), content));
        for d in [Direction::Outgoing, Direction::Incoming] {
            let q = RelationshipQuery {
                from: cid(id),
                direction: d,
                kind: None,
                after: None,
            };
            out.relationships
                .extend(v.relationships(&q).unwrap().map(|r| r.unwrap()));
        }
    }
    for h in v.history(cce_core::storage::HistoryPosition(0)).unwrap() {
        let h = h.unwrap();
        out.history.push((h.position.0, h.attempt, h.changes.len()));
    }
    out
}

#[test]
fn disjoint_commits_from_one_basis_produce_exactly_their_combined_state() {
    let bytes_a: Vec<u8> = (0..70_000u32).map(|i| (i * 7) as u8).collect();
    let bytes_b: Vec<u8> = (0..90_000u32).map(|i| (i * 13 + 1) as u8).collect();
    let first = |s: &NativeStore| {
        let c = s.stage_content(&mut &bytes_a[..]).unwrap();
        ChangeSet {
            changes: vec![
                Change::PutContext {
                    id: cid(b"A"),
                    expect: Precondition::Any,
                    body: b"A from first".to_vec(),
                    content: Some(0),
                },
                rel("ra", b"cites", b"A", b"B"),
            ],
            content: vec![ContentBinding::from(&c)],
            ..ChangeSet::default()
        }
    };
    let second = |s: &NativeStore| {
        let c = s.stage_content(&mut &bytes_b[..]).unwrap();
        ChangeSet {
            changes: vec![
                Change::PutContext {
                    id: cid(b"B"),
                    expect: Precondition::Any,
                    body: b"B from second".to_vec(),
                    content: Some(0),
                },
                rel("rb", b"cites", b"B", b"A"),
            ],
            content: vec![ContentBinding::from(&c)],
            ..ChangeSet::default()
        }
    };
    let seed = vec![put(b"A", b"a0"), put(b"B", b"b0")];
    let ids: [&[u8]; 2] = [b"A", b"B"];

    // Concurrent schedule: both views share one basis, each reads only its own record.
    let s = store();
    commit_now(&s, "seed", seed.clone());
    let mut v1 = s.begin().unwrap();
    let mut v2 = s.begin().unwrap();
    assert!(v1.context(&cid(b"A")).unwrap().is_some());
    assert!(v2.context(&cid(b"B")).unwrap().is_some());
    let (c1, c2) = (first(&s), second(&s));
    assert_eq!(s.commit(attempt("first"), v1, c1).unwrap().position.0, 2);
    assert_eq!(s.commit(attempt("second"), v2, c2).unwrap().position.0, 3);
    let got = contents(&s, &ids);

    // Independent expectation: the same requests applied one after the other, each from a fresh
    // view of the newest state.
    let r = store();
    commit_now(&r, "seed", seed);
    let c1 = first(&r);
    r.commit(attempt("first"), r.begin().unwrap(), c1).unwrap();
    let c2 = second(&r);
    r.commit(attempt("second"), r.begin().unwrap(), c2).unwrap();
    assert_eq!(got, contents(&r, &ids));

    // And spelled out: each record carries its own commit's revision, body and content bytes.
    assert_eq!(
        got.contexts,
        vec![
            (
                b"A".to_vec(),
                Revision(2),
                b"A from first".to_vec(),
                Some((bytes_a.len() as u64, bytes_a.clone()))
            ),
            (
                b"B".to_vec(),
                Revision(3),
                b"B from second".to_vec(),
                Some((bytes_b.len() as u64, bytes_b.clone()))
            ),
        ]
    );
    let summary: Vec<_> = got
        .relationships
        .iter()
        .map(|r| {
            (
                r.id.clone(),
                r.source.clone(),
                r.target.clone(),
                r.kind.clone(),
                r.revision,
            )
        })
        .collect();
    let ra = (rid("ra"), cid(b"A"), cid(b"B"), kind(b"cites"), Revision(2));
    let rb = (rid("rb"), cid(b"B"), cid(b"A"), kind(b"cites"), Revision(3));
    // A: outgoing ra, incoming rb; B: outgoing rb, incoming ra.
    assert_eq!(summary, vec![ra.clone(), rb.clone(), rb, ra]);
    assert_eq!(
        got.history,
        vec![
            (1, attempt("seed"), 2),
            (2, attempt("first"), 2),
            (3, attempt("second"), 2)
        ]
    );
}

#[test]
fn a_view_from_an_earlier_open_of_the_same_store_is_refused_and_writes_nothing() {
    let sim = SimIo::new();
    let s1 = store_on(&sim);
    commit_now(&s1, "seed", vec![put(b"A", b"a")]);
    let mut stale = s1.begin().unwrap();
    assert!(stale.context(&cid(b"A")).unwrap().is_some());
    let staged = s1
        .stage_content(&mut &b"staged by the first open"[..])
        .unwrap();
    s1.close().unwrap();
    sim.release();

    let s2 = NativeStore {
        inner: Inner::open(Box::new(sim.clone()), Default::default()).unwrap(),
    };
    let files = |sim: &SimIo| [FileId::Meta, FileId::Content].map(|f| sim.file_images_for_test(f));
    let before = files(&sim);
    let state = contents(&s2, &[b"A", b"B"]);

    let r = s2.commit(attempt("stale-view"), stale, cs(vec![put(b"B", b"b")]));
    assert_eq!(r, Err(CommitError::Rejected(StorageError::ForeignHandle)));
    // Content staged by the earlier open cannot be bound through the new one either.
    let r = s2.commit(
        attempt("stale-token"),
        s2.begin().unwrap(),
        ChangeSet {
            changes: vec![Change::PutContext {
                id: cid(b"B"),
                expect: Precondition::Any,
                body: b"b".to_vec(),
                content: Some(0),
            }],
            content: vec![ContentBinding::from(&staged)],
            ..ChangeSet::default()
        },
    );
    assert_eq!(r, Err(CommitError::Rejected(StorageError::ForeignHandle)));

    assert_eq!(files(&sim), before, "a refused commit wrote to the store");
    assert_eq!(contents(&s2, &[b"A", b"B"]), state);
    for a in ["stale-view", "stale-token"] {
        assert_eq!(s2.outcome(&attempt(a)).unwrap(), AttemptOutcome::Absent);
    }
    // The new open itself is healthy.
    assert_eq!(commit_now(&s2, "fresh", vec![put(b"B", b"b")]), 2);
}
