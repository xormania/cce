//! Physical maintenance: each task does real physical work, leaves every logical record, relation
//! and history entry as it was, adds no history, and keeps the store's space accounting sound.

use std::collections::BTreeMap;
use std::io::Read;

use cce_core::storage::{
    AttemptId, Change, ChangeSet, ContentBinding, ContextId, Direction, HistoryEntry,
    HistoryPosition, Precondition, ReadView, Relationship, RelationshipId, RelationshipKind,
    RelationshipQuery, StorageEngine,
};

use crate::engine::{Inner, Options};
use crate::format::StoreId;
use crate::records::Manifest;
use crate::sim::SimIo;
use crate::{MaintenanceBudget, MaintenanceTask, NativeStore};

const STORE: StoreId = StoreId(*b"maintenance-test");
const IDS: [&str; 4] = ["A", "B", "C", "D"];

fn cid(s: &str) -> ContextId {
    ContextId::new(s.as_bytes().to_vec()).unwrap()
}

fn create(sim: &SimIo) -> NativeStore {
    NativeStore {
        inner: Inner::create(Box::new(sim.clone()), STORE, Options::default()).unwrap(),
    }
}

fn open(sim: &SimIo) -> NativeStore {
    sim.release();
    NativeStore {
        inner: Inner::open(Box::new(sim.clone()), Options::default()).unwrap(),
    }
}

fn commit(s: &NativeStore, attempt: &str, cs: ChangeSet) {
    s.commit(
        AttemptId::new(attempt.as_bytes().to_vec()).unwrap(),
        s.begin().unwrap(),
        cs,
    )
    .unwrap();
}

fn put(id: &str, body: &str, content: Option<usize>) -> Change {
    Change::PutContext {
        id: cid(id),
        expect: Precondition::Any,
        body: body.as_bytes().to_vec(),
        content,
    }
}

/// Everything a store holds over the contexts in [`IDS`], read through the port: records with their
/// exact content bytes, relationships from both ends, and every history entry whole.
/// A context record: revision, body and its content's exact bytes.
type Record = (u64, Vec<u8>, Option<Vec<u8>>);

#[derive(Debug, PartialEq, Eq)]
struct Logical {
    contexts: BTreeMap<String, Record>,
    outgoing: BTreeMap<(String, Vec<u8>), Relationship>,
    incoming: BTreeMap<(String, Vec<u8>), Relationship>,
    history: Vec<HistoryEntry>,
}

fn logical(s: &NativeStore) -> Logical {
    let mut v = s.begin().unwrap();
    let mut out = Logical {
        contexts: BTreeMap::new(),
        outgoing: BTreeMap::new(),
        incoming: BTreeMap::new(),
        history: Vec::new(),
    };
    for name in IDS {
        let id = cid(name);
        if let Some(r) = v.context(&id).unwrap() {
            let bytes = r.content.map(|c| {
                let mut b = Vec::new();
                v.content(&c).unwrap().read_to_end(&mut b).unwrap();
                b
            });
            out.contexts
                .insert(name.into(), (r.revision.0, r.body, bytes));
        }
        for (d, into) in [
            (Direction::Outgoing, &mut out.outgoing),
            (Direction::Incoming, &mut out.incoming),
        ] {
            let q = RelationshipQuery {
                from: id.clone(),
                direction: d,
                kind: None,
                after: None,
            };
            for r in v.relationships(&q).unwrap() {
                let r = r.unwrap();
                into.insert((name.into(), r.id.as_bytes().to_vec()), r);
            }
        }
    }
    out.history = v
        .history(HistoryPosition(0))
        .unwrap()
        .map(|h| h.unwrap())
        .collect();
    out
}

/// A store with records, content and relationships whose content file holds `leak` bytes that no
/// published structure reaches: a staging of `payload` bytes, covered by a later unrelated commit,
/// then abandoned (or, with `discard`, discarded) before the process ended. Reopened.
fn leaked(payload: usize, discard: bool) -> (SimIo, NativeStore, u64) {
    let sim = SimIo::new();
    let s = create(&sim);
    let a: Vec<u8> = (0..70_000u32).map(|i| (i % 241) as u8).collect();
    let staged_a = s.stage_content(&mut &a[..]).unwrap();
    commit(
        &s,
        "one",
        ChangeSet {
            changes: vec![
                put("A", "a", Some(0)),
                put("B", "b", None),
                Change::PutRelationship {
                    id: RelationshipId::new(*b"r1").unwrap(),
                    expect: Precondition::Absent,
                    kind: RelationshipKind::new(*b"cites").unwrap(),
                    source: cid("A"),
                    target: cid("B"),
                },
            ],
            content: vec![ContentBinding::from(&staged_a)],
            ..ChangeSet::default()
        },
    );
    let orphan: Vec<u8> = (0..payload as u32).map(|i| (i % 233) as u8).collect();
    let staged = s.stage_content(&mut &orphan[..]).unwrap();
    // A commit that does not bind the staging: its content length now covers the staged bytes,
    // and replacing A's content retires the old content (so retired extents exist too).
    let a2: Vec<u8> = (0..30_000u32).map(|i| (i % 239) as u8).collect();
    let staged_a2 = s.stage_content(&mut &a2[..]).unwrap();
    commit(
        &s,
        "two",
        ChangeSet {
            changes: vec![put("A", "a2", Some(0)), put("C", "c", None)],
            content: vec![ContentBinding::from(&staged_a2)],
            ..ChangeSet::default()
        },
    );
    if discard {
        s.discard_staged(staged).unwrap();
    }
    drop(s);
    // The staged bytes: its segments of at most 64 KiB each, and its manifest.
    let segments = payload.div_ceil(64 * 1024);
    let leak = payload as u64 + Manifest::encoded_len(segments);
    let s = open(&sim);
    (sim, s, leak)
}

/// The published state's audit: reachable structures are disjoint, nothing is retired twice, and no
/// retired extent overlaps a reachable one.
fn audit(sim: &SimIo) {
    let d = crate::audit::published(sim).unwrap().pop().unwrap();
    let r = crate::audit::reach(sim, &d).expect("audit");
    assert!(r.disjoint(), "reachable extents overlap");
    let reachable: Vec<(u64, u64)> = r
        .manifests
        .iter()
        .chain(&r.content_segments)
        .chain(&r.history_extents)
        .copied()
        .collect();
    for (offset, (_, len)) in &r.retired_extents {
        for (o, l) in &reachable {
            assert!(
                offset + len <= *o || o + l <= *offset,
                "retired {offset}+{len} overlaps reachable {o}+{l}"
            );
        }
    }
}

#[test]
fn reclaiming_content_holes_retires_exactly_the_leaked_bytes_and_changes_no_record() {
    for discard in [false, true] {
        let (sim, s, leak) = leaked(100_000, discard);
        let before = logical(&s);
        let generation = s.inner.snapshot().unwrap().desc.generation;
        let r = s
            .maintain(
                MaintenanceTask::ReclaimContentHoles,
                MaintenanceBudget::default(),
            )
            .unwrap();
        assert_eq!(r.content_bytes_reclaimed, leak, "discard {discard}");
        assert!(r.complete);
        assert_eq!(
            r.generation,
            generation + 1,
            "one physical generation published"
        );
        assert_eq!(logical(&s), before, "maintenance changed logical state");
        assert_eq!(
            s.inner.snapshot().unwrap().desc.history_pos,
            2,
            "maintenance added history"
        );
        audit(&sim);
        // Nothing left: a second call does nothing and publishes nothing.
        let again = s
            .maintain(
                MaintenanceTask::ReclaimContentHoles,
                MaintenanceBudget::default(),
            )
            .unwrap();
        assert_eq!(again.content_bytes_reclaimed, 0);
        assert_eq!(again.generation, r.generation);
        // The space is reusable under the ordinary rule (retired at most one generation before the
        // current one): after one more commit, new staging takes it instead of growing the file.
        commit(
            &s,
            "advance",
            ChangeSet {
                changes: vec![put("B", "b2", None)],
                ..ChangeSet::default()
            },
        );
        let reuse: Vec<u8> = vec![9; 50_000];
        let len_before = s.stats().unwrap().content_bytes_reused;
        let staged = s.stage_content(&mut &reuse[..]).unwrap();
        commit(
            &s,
            "three",
            ChangeSet {
                changes: vec![put("D", "d", Some(0))],
                content: vec![ContentBinding::from(&staged)],
                ..ChangeSet::default()
            },
        );
        assert!(
            s.stats().unwrap().content_bytes_reused > len_before,
            "the reclaimed space was not reused"
        );
        drop(s);
        let s = open(&sim);
        audit(&sim);
        assert_eq!(logical(&s).contexts["D"].2.as_deref(), Some(&reuse[..]));
    }
}

#[test]
fn reclaiming_on_a_store_without_holes_does_nothing() {
    let sim = SimIo::new();
    let s = create(&sim);
    commit(
        &s,
        "one",
        ChangeSet {
            changes: vec![put("A", "a", None)],
            ..ChangeSet::default()
        },
    );
    let generation = s.inner.snapshot().unwrap().desc.generation;
    let r = s
        .maintain(
            MaintenanceTask::ReclaimContentHoles,
            MaintenanceBudget::default(),
        )
        .unwrap();
    assert_eq!(r.content_bytes_reclaimed, 0);
    assert!(r.complete);
    assert_eq!(r.generation, generation, "nothing to do publishes nothing");
}

#[test]
fn a_pending_staging_of_this_process_is_not_reclaimed() {
    let sim = SimIo::new();
    let s = create(&sim);
    let pending: Vec<u8> = (0..90_000u32).map(|i| (i % 229) as u8).collect();
    let staged = s.stage_content(&mut &pending[..]).unwrap();
    // A later commit covers the staged bytes while the staging is still pending.
    commit(
        &s,
        "cover",
        ChangeSet {
            changes: vec![put("B", "b", None)],
            ..ChangeSet::default()
        },
    );
    let r = s
        .maintain(
            MaintenanceTask::ReclaimContentHoles,
            MaintenanceBudget::default(),
        )
        .unwrap();
    assert_eq!(
        r.content_bytes_reclaimed, 0,
        "pending staged bytes were reclaimed"
    );
    commit(
        &s,
        "bind",
        ChangeSet {
            changes: vec![put("A", "a", Some(0))],
            content: vec![ContentBinding::from(&staged)],
            ..ChangeSet::default()
        },
    );
    assert_eq!(logical(&s).contexts["A"].2.as_deref(), Some(&pending[..]));
    audit(&sim);
}

fn rel(id: &str, source: &str, target: &str) -> Change {
    Change::PutRelationship {
        id: RelationshipId::new(id.as_bytes().to_vec()).unwrap(),
        expect: Precondition::Absent,
        kind: RelationshipKind::new(*b"cites").unwrap(),
        source: cid(source),
        target: cid(target),
    }
}

/// Relationships as submitted: (id, source, target, revision = the committing position).
const EDGES: [(&str, &str, &str, u64); 4] = [
    ("r1", "A", "B", 1),
    ("r2", "B", "C", 1),
    ("r3", "C", "A", 2),
    ("r4", "A", "D", 2),
];

/// Relationships as listed from each context: (context, relationship id) -> relationship.
type Listing = BTreeMap<(String, Vec<u8>), Relationship>;

/// Expected listings from the submitted relationships alone, never from an index.
fn expected_edges() -> (Listing, Listing) {
    let (mut out, mut inc) = (BTreeMap::new(), BTreeMap::new());
    for (id, s, t, rev) in EDGES {
        let r = Relationship {
            id: RelationshipId::new(id.as_bytes().to_vec()).unwrap(),
            kind: RelationshipKind::new(*b"cites").unwrap(),
            source: cid(s),
            target: cid(t),
            revision: cce_core::storage::Revision(rev),
        };
        out.insert((s.to_string(), id.as_bytes().to_vec()), r.clone());
        inc.insert((t.to_string(), id.as_bytes().to_vec()), r);
    }
    (out, inc)
}

fn related() -> (SimIo, NativeStore) {
    let sim = SimIo::new();
    let s = create(&sim);
    commit(
        &s,
        "one",
        ChangeSet {
            changes: vec![
                put("A", "a", None),
                put("B", "b", None),
                put("C", "c", None),
                rel("r1", "A", "B"),
                rel("r2", "B", "C"),
            ],
            ..ChangeSet::default()
        },
    );
    commit(
        &s,
        "two",
        ChangeSet {
            changes: vec![
                put("D", "d", None),
                rel("r3", "C", "A"),
                rel("r4", "A", "D"),
            ],
            ..ChangeSet::default()
        },
    );
    (sim, s)
}

/// Damages both indexes with checksum-valid records: r1's incoming entry is gone, and an outgoing
/// entry names a relationship that does not exist.
fn damage(s: &NativeStore) {
    use crate::records::{P_IN, P_OUT, key};
    s.inner
        .publish_edit_for_test(|t| {
            t.delete(&key(P_IN, &[b"B", b"cites", b"A", b"r1"]))
                .unwrap();
            t.put(key(P_OUT, &[b"C", b"cites", b"D", b"ghost"]), Vec::new())
                .unwrap();
        })
        .unwrap();
}

#[test]
fn rebuilding_the_relationship_indexes_restores_the_submitted_edges_only_when_asked() {
    let (sim, s) = related();
    let (out, inc) = expected_edges();
    let before = logical(&s);
    assert_eq!(
        (before.outgoing.clone(), before.incoming.clone()),
        (out.clone(), inc.clone())
    );
    damage(&s);
    // The damage shows through the port: r1 is missing from B's incoming listing, and listing C's
    // outgoing relationships fails on the entry without a record.
    let mut v = s.begin().unwrap();
    let incoming_b: Vec<_> = v
        .relationships(&RelationshipQuery {
            from: cid("B"),
            direction: Direction::Incoming,
            kind: None,
            after: None,
        })
        .unwrap()
        .map(|r| r.unwrap().id)
        .collect();
    assert!(incoming_b.is_empty(), "{incoming_b:?}");
    let ghost = v
        .relationships(&RelationshipQuery {
            from: cid("C"),
            direction: Direction::Outgoing,
            kind: None,
            after: None,
        })
        .unwrap()
        .collect::<Result<Vec<_>, _>>();
    assert!(ghost.is_err(), "the stray entry was not noticed");
    drop(v);
    // Opening again repairs nothing.
    drop(s);
    let s = open(&sim);
    assert!(
        s.begin()
            .unwrap()
            .relationships(&RelationshipQuery {
                from: cid("C"),
                direction: Direction::Outgoing,
                kind: None,
                after: None,
            })
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .is_err(),
        "opening repaired the index"
    );
    let generation = s.inner.snapshot().unwrap().desc.generation;
    let r = s
        .maintain(
            MaintenanceTask::RebuildRelationshipIndexes,
            MaintenanceBudget::default(),
        )
        .unwrap();
    assert!(r.complete);
    assert_eq!(r.generation, generation + 1);
    assert_eq!(r.index_entries_written, 2 * EDGES.len() as u64);
    let after = logical(&s);
    assert_eq!((after.outgoing.clone(), after.incoming.clone()), (out, inc));
    assert_eq!(after.contexts, before.contexts);
    assert_eq!(after.history, before.history, "maintenance changed history");
    audit(&sim);
    drop(s);
    let s = open(&sim);
    assert_eq!(logical(&s), after, "the rebuild did not survive reopening");
}

#[test]
fn a_rebuild_causes_no_conflict_with_a_view_that_read_the_indexes() {
    let (_sim, s) = related();
    let mut v = s.begin().unwrap();
    let _ = v
        .relationships(&RelationshipQuery {
            from: cid("A"),
            direction: Direction::Outgoing,
            kind: None,
            after: None,
        })
        .unwrap()
        .count();
    s.maintain(
        MaintenanceTask::RebuildRelationshipIndexes,
        MaintenanceBudget::default(),
    )
    .unwrap();
    s.commit(
        AttemptId::new(*b"after").unwrap(),
        v,
        ChangeSet {
            changes: vec![put("B", "b2", None)],
            ..ChangeSet::default()
        },
    )
    .expect("a physical rebuild must not conflict with a logical read");
}
