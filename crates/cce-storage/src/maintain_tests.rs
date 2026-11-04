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
    /// Listings that failed (a damaged index): context and the error.
    broken: Vec<(String, String)>,
    history: Vec<HistoryEntry>,
}

fn logical(s: &NativeStore) -> Logical {
    let mut v = s.begin().unwrap();
    let mut out = Logical {
        contexts: BTreeMap::new(),
        outgoing: BTreeMap::new(),
        incoming: BTreeMap::new(),
        broken: Vec::new(),
        history: Vec::new(),
    };
    // The named contexts, and the many records of the page relocation fixture.
    let names = IDS
        .iter()
        .map(|n| n.to_string())
        .chain((0..400).map(|i| format!("k{i:04}")))
        .chain(["z".to_string()]);
    for name in names {
        let name = name.as_str();
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
                match r {
                    Ok(r) => {
                        into.insert((name.into(), r.id.as_bytes().to_vec()), r);
                    }
                    Err(e) => out.broken.push((name.into(), e.to_string())),
                }
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
    crate::audit::check_retirement(sim).expect("retirement records");
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

#[test]
fn reclaiming_holes_never_touches_space_a_pinned_reader_still_reads() {
    let (sim, s, leak) = leaked(100_000, false);
    // A view and a content reader, mid-read, pin A's content X; replacing X retires its space.
    let mut view = s.begin().unwrap();
    let x = view.context(&cid("A")).unwrap().unwrap().content.unwrap();
    let mut reader = view.content(&x).unwrap();
    let mut head = vec![0; 1000];
    reader.read_exact(&mut head).unwrap();
    let pinned = {
        let d = crate::audit::published(&sim).unwrap().pop().unwrap();
        crate::audit::reach(&sim, &d).unwrap()
    };
    let staged = s.stage_content(&mut &[4u8; 10_000][..]).unwrap();
    commit(
        &s,
        "replace-x",
        ChangeSet {
            changes: vec![put("A", "a3", Some(0))],
            content: vec![ContentBinding::from(&staged)],
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
        r.content_bytes_reclaimed, leak,
        "reclaimed more than the leak"
    );
    // What the step retired is disjoint from everything the pinned generation reaches.
    let newest = crate::audit::published(&sim).unwrap().pop().unwrap();
    let now = crate::audit::reach(&sim, &newest).unwrap();
    let reached: Vec<(u64, u64)> = pinned
        .manifests
        .iter()
        .chain(&pinned.content_segments)
        .chain(&pinned.history_extents)
        .copied()
        .collect();
    for (offset, (generation, len)) in &now.retired_extents {
        if *generation == r.generation {
            for (o, l) in &reached {
                assert!(
                    offset + len <= *o || o + l <= *offset,
                    "reclaimed {offset}+{len} overlaps pinned {o}+{l}"
                );
            }
        }
    }
    // Several commits that reuse space later; the pinned reader and view still read X exactly.
    for i in 0..3u8 {
        let staged = s.stage_content(&mut &vec![i; 40_000][..]).unwrap();
        commit(
            &s,
            &format!("reuse-{i}"),
            ChangeSet {
                changes: vec![put("D", "d", Some(0))],
                content: vec![ContentBinding::from(&staged)],
                ..ChangeSet::default()
            },
        );
    }
    let mut rest = Vec::new();
    reader.read_to_end(&mut rest).unwrap();
    let expected: Vec<u8> = (0..30_000u32).map(|i| (i % 239) as u8).collect();
    assert_eq!(
        [head, rest].concat(),
        expected,
        "the pinned reader read other bytes"
    );
    let mut again = Vec::new();
    view.content(&x).unwrap().read_to_end(&mut again).unwrap();
    assert_eq!(again, expected, "the pinned view read other bytes");
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

/// Records the largest single read and write of either file, to bound what a step holds, and per
/// file the largest read and the bytes read in all.
struct Sizes {
    sim: SimIo,
    largest: std::sync::Arc<std::sync::Mutex<(usize, usize)>>,
    reads: std::sync::Arc<std::sync::Mutex<FileReads>>,
}

impl crate::io::Io for Sizes {
    fn create(&self) -> Result<(), cce_core::storage::StorageError> {
        self.sim.create()
    }
    fn open(&self) -> Result<(), cce_core::storage::StorageError> {
        self.sim.open()
    }
    fn open_read_only(&self) -> Result<(), cce_core::storage::StorageError> {
        self.sim.open_read_only()
    }
    fn read_at(
        &self,
        file: crate::io::FileId,
        offset: u64,
        buf: &mut [u8],
    ) -> Result<(), cce_core::storage::StorageError> {
        let mut l = self.largest.lock().unwrap();
        l.0 = l.0.max(buf.len());
        drop(l);
        let mut r = self.reads.lock().unwrap();
        let f = &mut r[usize::from(file == crate::io::FileId::Content)];
        f.0 = f.0.max(buf.len());
        f.1 += buf.len() as u64;
        drop(r);
        self.sim.read_at(file, offset, buf)
    }
    fn write_at(
        &self,
        file: crate::io::FileId,
        offset: u64,
        data: &[u8],
    ) -> Result<(), cce_core::storage::StorageError> {
        let mut l = self.largest.lock().unwrap();
        l.1 = l.1.max(data.len());
        drop(l);
        self.sim.write_at(file, offset, data)
    }
    fn len(&self, file: crate::io::FileId) -> Result<u64, cce_core::storage::StorageError> {
        self.sim.len(file)
    }
    fn sync(&self, file: crate::io::FileId) -> Result<(), cce_core::storage::StorageError> {
        self.sim.sync(file)
    }
    fn sync_dir(&self) -> Result<(), cce_core::storage::StorageError> {
        self.sim.sync_dir()
    }
}

fn bytes_of(seed: u32, n: u32) -> Vec<u8> {
    (0..n).map(|i| (i.wrapping_mul(31) ^ seed) as u8).collect()
}

/// A content of 100 000 bytes low in the file whose space is retired and reusable, and B's
/// content of 100 000 bytes after it. Released.
fn fragmented() -> SimIo {
    let sim = SimIo::new();
    let s = create(&sim);
    for (attempt, id, seed) in [("one", "A", 1), ("two", "B", 2)] {
        let staged = s.stage_content(&mut &bytes_of(seed, 100_000)[..]).unwrap();
        commit(
            &s,
            attempt,
            ChangeSet {
                changes: vec![put(id, id, Some(0))],
                content: vec![ContentBinding::from(&staged)],
                ..ChangeSet::default()
            },
        );
    }
    // A no longer binds content: its manifest and segments are retired; one more commit makes
    // that space reusable.
    commit(
        &s,
        "three",
        ChangeSet {
            changes: vec![put("A", "a", None)],
            ..ChangeSet::default()
        },
    );
    commit(
        &s,
        "four",
        ChangeSet {
            changes: vec![put("C", "c", None)],
            ..ChangeSet::default()
        },
    );
    drop(s);
    sim.release();
    sim
}

/// The end of the highest content stream extent (manifest or segment) the published state reaches.
fn highest_content_end(sim: &SimIo) -> u64 {
    let d = crate::audit::published(sim).unwrap().pop().unwrap();
    let r = crate::audit::reach(sim, &d).unwrap();
    r.manifests
        .iter()
        .chain(&r.content_segments)
        .map(|(o, l)| o + l)
        .max()
        .unwrap_or(0)
}

#[test]
fn relocation_moves_content_into_lower_retired_space_and_changes_no_record() {
    let sim = fragmented();
    let largest = std::sync::Arc::new(std::sync::Mutex::new((0, 0)));
    let s = NativeStore {
        inner: Inner::open(
            Box::new(Sizes {
                sim: sim.clone(),
                largest: largest.clone(),
                reads: Default::default(),
            }),
            Options::default(),
        )
        .unwrap(),
    };
    let before = logical(&s);
    let high = highest_content_end(&sim);
    let history_pos = s.inner.snapshot().unwrap().desc.history_pos;
    *largest.lock().unwrap() = (0, 0);
    let r = s
        .maintain(MaintenanceTask::Relocate, MaintenanceBudget::default())
        .unwrap();
    let (read, written) = *largest.lock().unwrap();
    assert!(
        read <= 64 * 1024 && written <= 64 * 1024,
        "held {read} / {written} bytes at once"
    );
    assert_eq!(
        r.content_bytes_moved,
        100_000 + Manifest::encoded_len(2),
        "B's segments and its new manifest"
    );
    assert!(highest_content_end(&sim) < high, "nothing moved down");
    assert_eq!(logical(&s), before, "relocation changed logical state");
    assert_eq!(s.inner.snapshot().unwrap().desc.history_pos, history_pos);
    audit(&sim);
    drop(s);
    let s = open(&sim);
    assert_eq!(logical(&s), before, "the move did not survive reopening");
    assert_eq!(
        logical(&s).contexts["B"].2.as_deref(),
        Some(&bytes_of(2, 100_000)[..])
    );
}

/// Runs `maintain(Relocate)` on another thread and `between` on this one once the copy is done
/// and before the step publishes.
fn relocate_around(s: &NativeStore, between: impl FnOnce()) -> crate::MaintenanceReport {
    let (copied_tx, copied_rx) = std::sync::mpsc::channel();
    let (go_tx, go_rx) = std::sync::mpsc::channel::<()>();
    s.inner.set_relocate_hook_for_test(Some(Box::new(move || {
        copied_tx.send(()).unwrap();
        go_rx.recv().unwrap();
    })));
    std::thread::scope(|t| {
        let worker =
            t.spawn(|| s.maintain(MaintenanceTask::Relocate, MaintenanceBudget::default()));
        // A step that never reaches its publication must fail the test, not hang it.
        if copied_rx
            .recv_timeout(std::time::Duration::from_secs(60))
            .is_err()
        {
            s.inner.set_relocate_hook_for_test(None);
            let r = worker.join().unwrap();
            panic!("relocation never reached its publication: {r:?}");
        }
        between();
        go_tx.send(()).unwrap();
        worker.join().unwrap().unwrap()
    })
}

#[test]
fn a_commit_during_relocation_is_preserved_and_the_move_follows_it() {
    let sim = fragmented();
    let s = open(&sim);
    let generation = s.inner.snapshot().unwrap().desc.generation;
    let r = relocate_around(&s, || {
        commit(
            &s,
            "during",
            ChangeSet {
                changes: vec![put("D", "d", None)],
                ..ChangeSet::default()
            },
        );
        assert_eq!(
            s.inner.snapshot().unwrap().desc.generation,
            generation + 1,
            "the injected commit published first"
        );
    });
    assert_eq!(
        r.generation,
        generation + 2,
        "relocation published after the commit"
    );
    assert_eq!(r.content_bytes_moved, 100_000 + Manifest::encoded_len(2));
    let after = logical(&s);
    assert_eq!(
        after.contexts["D"].1, b"d",
        "the commit made during relocation was lost"
    );
    assert_eq!(
        after.history.len(),
        5,
        "history positions: four commits and the injected one"
    );
    assert_eq!(
        after.contexts["B"].2.as_deref(),
        Some(&bytes_of(2, 100_000)[..])
    );
    audit(&sim);
}

#[test]
fn a_move_whose_source_changed_during_relocation_is_dropped() {
    let sim = fragmented();
    let s = open(&sim);
    let r = relocate_around(&s, || {
        // B's content is replaced: the copied bytes are no longer the current content.
        let staged = s.stage_content(&mut &bytes_of(9, 20_000)[..]).unwrap();
        commit(
            &s,
            "replace",
            ChangeSet {
                changes: vec![put("B", "B", Some(0))],
                content: vec![ContentBinding::from(&staged)],
                ..ChangeSet::default()
            },
        );
    });
    assert_eq!(r.content_bytes_moved, 0, "a stale move was published");
    assert_eq!(
        logical(&s).contexts["B"].2.as_deref(),
        Some(&bytes_of(9, 20_000)[..])
    );
    audit(&sim);
    drop(s);
    let s = open(&sim);
    audit(&sim);
    assert_eq!(
        logical(&s).contexts["B"].2.as_deref(),
        Some(&bytes_of(9, 20_000)[..])
    );
}

#[test]
fn a_move_whose_content_was_committed_again_during_relocation_is_dropped() {
    let sim = fragmented();
    let s = open(&sim);
    let r = relocate_around(&s, || {
        // B gives up its content (now removed and retired), then E binds the same bytes again:
        // the same content, indexed afresh with another manifest.
        commit(
            &s,
            "release",
            ChangeSet {
                changes: vec![put("B", "B", None)],
                ..ChangeSet::default()
            },
        );
        let staged = s.stage_content(&mut &bytes_of(2, 100_000)[..]).unwrap();
        commit(
            &s,
            "again",
            ChangeSet {
                changes: vec![put("D", "d", Some(0))],
                content: vec![ContentBinding::from(&staged)],
                ..ChangeSet::default()
            },
        );
    });
    assert_eq!(
        r.content_bytes_moved, 0,
        "a move of a superseded manifest was published"
    );
    assert_eq!(
        logical(&s).contexts["D"].2.as_deref(),
        Some(&bytes_of(2, 100_000)[..])
    );
    audit(&sim);
    drop(s);
    let s = open(&sim);
    audit(&sim);
    assert_eq!(
        logical(&s).contexts["D"].2.as_deref(),
        Some(&bytes_of(2, 100_000)[..])
    );
}

#[test]
fn reuse_is_counted_only_for_space_a_publication_consumed() {
    let sim = fragmented();
    let s = open(&sim);
    let start = s.stats().unwrap().content_bytes_reused;
    let mut commits = 0;
    let r = relocate_around(&s, || {
        // B's content is replaced, so the copied move is dropped and its space returned.
        let pre = s.stats().unwrap().content_bytes_reused;
        let staged = s.stage_content(&mut &bytes_of(9, 20_000)[..]).unwrap();
        commit(
            &s,
            "replace",
            ChangeSet {
                changes: vec![put("B", "B", Some(0))],
                content: vec![ContentBinding::from(&staged)],
                ..ChangeSet::default()
            },
        );
        // What the replacing commit itself reused.
        commits = s.stats().unwrap().content_bytes_reused - pre;
    });
    assert_eq!(r.content_bytes_moved, 0);
    assert_eq!(
        s.stats().unwrap().content_bytes_reused,
        start + commits,
        "a dropped move counted as reuse"
    );
    // A staging that takes retired space and is then discarded consumed nothing either.
    let before = s.stats().unwrap();
    let staged = s.stage_content(&mut &bytes_of(7, 10_000)[..]).unwrap();
    assert!(
        s.stats().unwrap().reusable_content_bytes < before.reusable_content_bytes,
        "the staging took no retired space"
    );
    s.discard_staged(staged).unwrap();
    assert_eq!(
        s.stats().unwrap().content_bytes_reused,
        before.content_bytes_reused,
        "a discarded staging counted as reuse"
    );
}

/// A tree of many leaves whose low pages were vacated by later commits and are reusable, so that
/// the newest leaves sit above retired space. Released.
fn scattered() -> SimIo {
    scattered_with(400)
}

/// [`scattered`] with `records` records (at most 400).
fn scattered_with(records: usize) -> SimIo {
    let sim = SimIo::new();
    let s = create(&sim);
    let body = |i: usize, round: usize| format!("{i:04}-{round}-{}", "x".repeat(400));
    commit(
        &s,
        "many",
        ChangeSet {
            changes: (0..records)
                .map(|i| put(&format!("k{i:04}"), &body(i, 0), None))
                .collect(),
            ..ChangeSet::default()
        },
    );
    // Rewriting every record moves every leaf to fresh pages above the first commit's.
    commit(
        &s,
        "rewrite",
        ChangeSet {
            changes: (0..records)
                .map(|i| put(&format!("k{i:04}"), &body(i, 1), None))
                .collect(),
            ..ChangeSet::default()
        },
    );
    for a in ["three", "four"] {
        commit(
            &s,
            a,
            ChangeSet {
                changes: vec![put("z", a, None)],
                ..ChangeSet::default()
            },
        );
    }
    drop(s);
    sim.release();
    sim
}

fn tree_pages(sim: &SimIo) -> (std::collections::BTreeSet<u64>, crate::format::Descriptor) {
    let d = crate::audit::published(sim).unwrap().pop().unwrap();
    (crate::audit::reach(sim, &d).unwrap().pages, d)
}

#[test]
fn relocation_moves_tree_pages_into_lower_retired_pages_and_changes_no_record() {
    let sim = scattered();
    let s = open(&sim);
    let before = logical(&s);
    let (pages, d) = tree_pages(&sim);
    let r = s
        .maintain(MaintenanceTask::Relocate, MaintenanceBudget::default())
        .unwrap();
    let (after, e) = tree_pages(&sim);
    assert!(r.pages_moved > 0, "no page moved");
    assert!(
        after.last() < pages.last(),
        "the highest tree page did not move down: {:?} -> {:?}",
        pages.last(),
        after.last()
    );
    assert_eq!(
        e.next_page, d.next_page,
        "relocation wrote past the end of the tree pages"
    );
    assert_eq!(e.history_pos, d.history_pos);
    assert_eq!(logical(&s), before, "relocation changed logical state");
    audit(&sim);
    drop(s);
    let s = open(&sim);
    assert_eq!(logical(&s), before, "the move did not survive reopening");
}

#[test]
fn page_relocation_moves_no_more_pages_than_the_budget() {
    let sim = scattered();
    let s = open(&sim);
    let before = logical(&s);
    let (pages, d) = tree_pages(&sim);
    let r = s
        .maintain(
            MaintenanceTask::Relocate,
            MaintenanceBudget {
                max_pages: 2,
                ..MaintenanceBudget::default()
            },
        )
        .unwrap();
    assert_eq!(r.pages_moved, 2);
    assert!(
        !r.complete,
        "a step cut short by its budget reported itself complete"
    );
    let (after, e) = tree_pages(&sim);
    let top: Vec<u64> = pages.iter().rev().take(2).copied().collect();
    assert!(
        top.iter().all(|p| !after.contains(p)),
        "the two highest pages {top:?} are still in use"
    );
    assert_eq!(e.next_page, d.next_page);
    assert_eq!(logical(&s), before);
    audit(&sim);
}

/// Runs one relocation step and checks what it did to the tree pages: the `pages_moved` highest
/// pages are no longer in use, every page the step wrote is numbered below all of them, and the
/// metadata file did not grow.
fn relocate_checked(
    sim: &SimIo,
    s: &NativeStore,
    budget: MaintenanceBudget,
) -> crate::MaintenanceReport {
    let (pages, d) = tree_pages(sim);
    let r = s.maintain(MaintenanceTask::Relocate, budget).unwrap();
    let after = tree_pages(sim).1;
    let reach = crate::audit::reach(sim, &after).unwrap();
    let top: Vec<u64> = pages
        .iter()
        .rev()
        .take(r.pages_moved as usize)
        .copied()
        .collect();
    assert!(
        top.iter().all(|p| !reach.pages.contains(p)),
        "moved pages {top:?} are still in use"
    );
    if let Some(lowest) = top.last() {
        let written: Vec<u64> = reach
            .incarnations
            .iter()
            .filter(|(_, g)| **g == r.generation)
            .map(|(p, _)| *p)
            .collect();
        assert!(
            written.iter().all(|p| p < lowest),
            "the step wrote pages {written:?}, not all below the moved pages {top:?}"
        );
    }
    assert_eq!(
        after.next_page, d.next_page,
        "relocation wrote past the end of the tree pages"
    );
    r
}

#[test]
fn page_relocation_never_moves_pages_up_into_space_it_vacated() {
    let sim = scattered();
    let s = open(&sim);
    let first = relocate_checked(&sim, &s, MaintenanceBudget::default());
    assert!(first.pages_moved > 0);
    // Two commits make the pages the first step vacated reusable; they all lie above the tree now.
    for a in ["five", "six"] {
        commit(
            &s,
            a,
            ChangeSet {
                changes: vec![put("z", a, None)],
                ..ChangeSet::default()
            },
        );
    }
    let mid = logical(&s);
    relocate_checked(&sim, &s, MaintenanceBudget::default());
    assert_eq!(logical(&s), mid);
    audit(&sim);
}

#[test]
fn page_relocation_without_lower_retired_pages_does_nothing() {
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
        .maintain(MaintenanceTask::Relocate, MaintenanceBudget::default())
        .unwrap();
    assert_eq!((r.pages_moved, r.content_bytes_moved), (0, 0));
    assert_eq!(
        r.generation, generation,
        "a step with nothing to move published"
    );
    assert_eq!(s.inner.snapshot().unwrap().desc.generation, generation);
}

/// One maintenance step to sweep: its name, the released image it runs on, its task, its budget,
/// and whether that budget must leave work for a further step.
type Sweep = (
    &'static str,
    fn() -> SimIo,
    MaintenanceTask,
    MaintenanceBudget,
    bool,
);

const FULL: MaintenanceBudget = MaintenanceBudget {
    max_pages: 1024,
    max_content_bytes: 64 << 20,
};

/// Two contents of 100 000 bytes high in the file above retired space for both. Released.
fn fragmented_twice() -> SimIo {
    let sim = SimIo::new();
    let s = create(&sim);
    for (attempt, id, seed) in [
        ("one", "A", 1),
        ("two", "C", 3),
        ("three", "B", 2),
        ("four", "D", 4),
    ] {
        let staged = s.stage_content(&mut &bytes_of(seed, 100_000)[..]).unwrap();
        commit(
            &s,
            attempt,
            ChangeSet {
                changes: vec![put(id, id, Some(0))],
                content: vec![ContentBinding::from(&staged)],
                ..ChangeSet::default()
            },
        );
    }
    // A and C give up their content; two more commits make its space reusable.
    for (attempt, changes) in [
        ("five", vec![put("A", "a", None), put("C", "c", None)]),
        ("six", vec![put("z", "six", None)]),
        ("seven", vec![put("z", "seven", None)]),
    ] {
        commit(
            &s,
            attempt,
            ChangeSet {
                changes,
                ..ChangeSet::default()
            },
        );
    }
    drop(s);
    sim.release();
    sim
}

/// The page relocation fixture, smaller: still many leaves, and a faster sweep.
fn scattered_small() -> SimIo {
    scattered_with(120)
}

fn holes_image() -> SimIo {
    let (sim, s, _) = leaked(100_000, false);
    drop(s);
    sim.release();
    sim
}

fn damaged_image() -> SimIo {
    let (sim, s) = related();
    damage(&s);
    drop(s);
    sim.release();
    sim
}

const SWEEPS: [Sweep; 7] = [
    (
        "hole reclaim",
        holes_image,
        MaintenanceTask::ReclaimContentHoles,
        FULL,
        false,
    ),
    (
        "index rebuild",
        damaged_image,
        MaintenanceTask::RebuildRelationshipIndexes,
        FULL,
        false,
    ),
    (
        "content relocation",
        fragmented,
        MaintenanceTask::Relocate,
        FULL,
        false,
    ),
    (
        "page relocation",
        scattered_small,
        MaintenanceTask::Relocate,
        FULL,
        false,
    ),
    // Budgets that cut the work short: part of the leak, one of two contents, two pages.
    (
        "partial hole reclaim",
        holes_image,
        MaintenanceTask::ReclaimContentHoles,
        MaintenanceBudget {
            max_pages: 1024,
            max_content_bytes: 40_000,
        },
        true,
    ),
    (
        "partial content relocation",
        fragmented_twice,
        MaintenanceTask::Relocate,
        MaintenanceBudget {
            max_pages: 1024,
            max_content_bytes: 150_000,
        },
        true,
    ),
    (
        "partial page relocation",
        scattered_small,
        MaintenanceTask::Relocate,
        MaintenanceBudget {
            max_pages: 2,
            max_content_bytes: 64 << 20,
        },
        true,
    ),
];

/// Every cut of every maintenance step, after a process exit and after a power loss, recovers
/// either the state before the step or the state after it, passes the audit, and keeps working.
#[test]
fn every_cut_of_a_maintenance_step_recovers_the_state_before_or_after_it() {
    use crate::format::PAGE_SIZE;
    use crate::io::{FileId, Op, Traced};
    use crate::repro_tests::{PROCESS_EXIT, survival, with_repro};
    use crate::sim::Persist;
    use std::sync::{Arc, Mutex};
    let open_traced = |sim: &SimIo, trace: &Arc<Mutex<Vec<Op>>>| {
        sim.release();
        NativeStore {
            inner: Inner::open(
                Box::new(Traced {
                    inner: sim.clone(),
                    trace: trace.clone(),
                }),
                Options::default(),
            )
            .unwrap(),
        }
    };
    let (mut checked, mut cases) = (0, 0);
    for (name, image, task, budget, partial) in SWEEPS {
        let base = image();
        // The uncut step: what it changes, and how many mutating operations it makes.
        let probe = base.after_process_exit();
        let trace = Arc::new(Mutex::new(Vec::new()));
        let s = open_traced(&probe, &trace);
        let before = logical(&s);
        let generation = s.inner.snapshot().unwrap().desc.generation;
        trace.lock().unwrap().clear();
        let r = s.maintain(task, budget).unwrap();
        let ops = trace.lock().unwrap().clone();
        let total = ops.len() as u64;
        let after = logical(&s);
        assert!(
            r.generation > generation,
            "{name}: the step published nothing to sweep"
        );
        // A full step may still leave work (page relocation short of lower pages halves its
        // count); a budget-limited one must.
        assert!(
            !(partial && r.complete),
            "{name}: a budget-limited step reported itself complete"
        );
        drop(s);
        // A step may publish more than once (relocation: content, then pages); nothing may be
        // published before the first publication record write.
        let publication = ops
            .iter()
            .position(|op| {
                matches!(op, Op::Write { file: FileId::Meta, offset, .. } if *offset < 2 * PAGE_SIZE as u64)
            })
            .expect("a publication record write") as u64;
        cases += (total + 1) * 7;
        for cut in 0..=total {
            let sim = base.after_process_exit();
            let trace = Arc::new(Mutex::new(Vec::new()));
            let s = open_traced(&sim, &trace);
            trace.lock().unwrap().clear();
            sim.crash_after(cut);
            let acknowledged = s.maintain(task, budget).is_ok();
            assert_eq!(
                acknowledged,
                cut == total,
                "{name} cut {cut}: acknowledgment"
            );
            drop(s);
            let trace = trace.lock().unwrap().clone();
            let images = [(PROCESS_EXIT.to_string(), sim.after_process_exit())]
                .into_iter()
                .chain(
                    [Persist::None, Persist::All]
                        .into_iter()
                        .chain((1..=4).map(Persist::Seeded))
                        .map(|p| (survival(p), sim.after_crash(p))),
                );
            for (how, img) in images {
                let label = format!("{name} cut {cut} of {total}, {how}");
                let pristine = img.after_process_exit();
                with_repro(&label, &[("image", &pristine)], &trace, || {
                    let recovered = open(&img);
                    audit(&img);
                    let got = logical(&recovered);
                    assert!(
                        got == before || got == after,
                        "{label}: a state between the two"
                    );
                    let published = recovered.inner.snapshot().unwrap().desc.generation;
                    assert!(published <= r.generation, "{label}: published {published}");
                    if acknowledged {
                        assert_eq!(got, after, "{label}: acknowledged step lost");
                        assert_eq!(published, r.generation, "{label}: acknowledged step lost");
                    }
                    if cut <= publication {
                        assert_eq!(
                            published, generation,
                            "{label}: published before its record"
                        );
                        assert_eq!(got, before, "{label}: visible before its record");
                    }
                    // Further full steps finish the work, through the same checks.
                    let mut steps = 0;
                    while !recovered.maintain(task, FULL).unwrap().complete {
                        steps += 1;
                        assert!(steps < 8, "{label}: the work never completed");
                    }
                    // Once complete, there is nothing left to move or reclaim.
                    let idle = recovered.maintain(task, FULL).unwrap();
                    assert_eq!(
                        (
                            idle.content_bytes_moved,
                            idle.pages_moved,
                            idle.content_bytes_reclaimed
                        ),
                        (0, 0, 0),
                        "{label}: work remained after a step reported itself complete"
                    );
                    audit(&img);
                    assert_eq!(
                        logical(&recovered),
                        after,
                        "{label}: finishing changed state"
                    );
                    // The recovered store keeps working: commit, audit, reopen.
                    commit(
                        &recovered,
                        "after-recovery",
                        ChangeSet {
                            changes: vec![put("D", "after", None)],
                            ..ChangeSet::default()
                        },
                    );
                    audit(&img);
                    let next = logical(&recovered);
                    drop(recovered);
                    assert_eq!(
                        logical(&open(&img)),
                        next,
                        "{label}: reopening changed state"
                    );
                });
                checked += 1;
            }
        }
    }
    // Every cut of every step, after a process exit and after six power losses.
    assert_eq!(checked, cases);
}

/// Per file (metadata, content): the largest single read and the bytes read in all.
type FileReads = [(usize, u64); 2];

/// What each maintenance step holds and reads, measured through the I/O it makes: no step reads
/// or writes more than one tree page or one content segment at a time; hole reclaim reads no
/// content but manifests, the index rebuild reads no content at all, relocation reads only the
/// manifests and the content it moves; and no step reads more metadata than the file's tree pages
/// once over. (The lists a step builds in memory, the rebuild's index keys and the reclaim's
/// extents in use, grow with the metadata; the failure model states those bounds.)
#[test]
fn maintenance_steps_hold_one_page_or_segment_and_read_only_what_they_need() {
    use crate::engine::CONTENT_CHUNK;
    use crate::format::PAGE_SIZE;
    use std::sync::{Arc, Mutex};
    // The full-budget steps of the crash sweep.
    for (name, image, task, _, partial) in SWEEPS {
        if partial {
            continue;
        }
        let sim = image();
        let d = crate::audit::published(&sim).unwrap().pop().unwrap();
        let reach = crate::audit::reach(&sim, &d).unwrap();
        let manifests: u64 = reach.manifests.iter().map(|(_, l)| l).sum();
        let largest = Arc::new(Mutex::new((0, 0)));
        let reads: Arc<Mutex<FileReads>> = Arc::default();
        let s = NativeStore {
            inner: Inner::open(
                Box::new(Sizes {
                    sim: sim.clone(),
                    largest: largest.clone(),
                    reads: reads.clone(),
                }),
                Options::default(),
            )
            .unwrap(),
        };
        *largest.lock().unwrap() = (0, 0);
        *reads.lock().unwrap() = FileReads::default();
        let r = s.maintain(task, FULL).unwrap();
        let (read, written) = *largest.lock().unwrap();
        let [meta, content] = *reads.lock().unwrap();
        assert!(
            meta.1 <= d.next_page * PAGE_SIZE as u64,
            "{name}: read {} metadata bytes of {} pages",
            meta.1,
            d.next_page
        );
        assert!(
            meta.0 <= PAGE_SIZE,
            "{name}: a metadata read of {} bytes",
            meta.0
        );
        assert!(
            read <= CONTENT_CHUNK && written <= CONTENT_CHUNK,
            "{name}: held {read} / {written} bytes at once"
        );
        match task {
            MaintenanceTask::ReclaimContentHoles => assert!(
                content.1 <= manifests,
                "{name}: read {} content bytes, the manifests are {manifests}",
                content.1
            ),
            MaintenanceTask::RebuildRelationshipIndexes => {
                assert_eq!(content.1, 0, "{name}: read content")
            }
            _ => assert!(
                content.1 <= manifests + r.content_bytes_moved,
                "{name}: read {} content bytes beyond the manifests and what it moved",
                content.1
            ),
        }
    }
}
