//! Space reuse groundwork: reader pins (which committed generations live views and content readers
//! still read) and an independent reachability audit of both publication slots.

use std::collections::BTreeMap;
use std::io::Read;

use cce_core::storage::{
    AttemptId, Change, ChangeSet, ContentBinding, ContextId, Precondition, ReadView, StorageEngine,
};

use crate::audit::{published, reach};
use crate::engine::{CONTENT_CHUNK, Inner};
use crate::format::{PAGE_SIZE, StoreId};
use crate::sim::SimIo;
use crate::{NativeReadOnly, NativeStore};

const STORE: StoreId = StoreId(*b"reuse-test-store");

fn cid(s: &str) -> ContextId {
    ContextId::new(s.as_bytes().to_vec()).unwrap()
}

fn sim_create(sim: &SimIo) -> NativeStore {
    NativeStore {
        inner: Inner::create(Box::new(sim.clone()), STORE, Default::default()).unwrap(),
    }
}

/// Puts record `id` with `payload` as its content.
fn put(s: &NativeStore, attempt: &str, id: &str, payload: &[u8]) -> u64 {
    let staged = s.stage_content(&mut &payload[..]).unwrap();
    let cs = ChangeSet {
        changes: vec![Change::PutContext {
            id: cid(id),
            expect: Precondition::Any,
            body: id.as_bytes().to_vec(),
            content: Some(0),
        }],
        content: vec![ContentBinding::from(&staged)],
        ..ChangeSet::default()
    };
    s.commit(
        AttemptId::new(attempt.as_bytes().to_vec()).unwrap(),
        s.begin().unwrap(),
        cs,
    )
    .unwrap()
    .position
    .0
}

fn pins(inner: &Inner) -> BTreeMap<u64, u32> {
    inner.pinned_generations()
}

fn generation(s: &NativeStore) -> u64 {
    s.inner.snapshot().unwrap().desc.generation
}

#[test]
fn a_view_pins_the_generation_it_reads_until_it_and_its_readers_are_gone() {
    let s = sim_create(&SimIo::new());
    put(&s, "a1", "A", &[1; 1000]);
    let g1 = generation(&s);
    assert!(pins(&s.inner).is_empty(), "a commit left a pin behind");

    let mut old = s.begin().unwrap();
    assert_eq!(pins(&s.inner), BTreeMap::from([(g1, 1)]));
    put(&s, "a2", "B", &[2; 1000]);
    let g2 = generation(&s);
    assert_eq!(g2, g1 + 1);
    // The old view still pins the generation it reads; the commit's own basis view is released.
    assert_eq!(pins(&s.inner), BTreeMap::from([(g1, 1)]));

    let new = s.begin().unwrap();
    assert_eq!(pins(&s.inner), BTreeMap::from([(g1, 1), (g2, 1)]));
    let content = old.context(&cid("A")).unwrap().unwrap().content.unwrap();
    let mut reader = old.content(&content).unwrap();
    assert_eq!(pins(&s.inner), BTreeMap::from([(g1, 2), (g2, 1)]));
    // A content reader outlives its view and keeps that view's generation pinned.
    drop(old);
    assert_eq!(pins(&s.inner), BTreeMap::from([(g1, 1), (g2, 1)]));
    let mut bytes = Vec::new();
    reader.read_to_end(&mut bytes).unwrap();
    assert_eq!(bytes, vec![1; 1000]);
    drop(reader);
    assert_eq!(pins(&s.inner), BTreeMap::from([(g2, 1)]));
    drop(new);
    assert!(pins(&s.inner).is_empty());
}

#[test]
fn read_only_views_and_readers_pin_their_generation() {
    let sim = SimIo::new();
    let s = sim_create(&sim);
    put(&s, "a1", "A", &[1; 1000]);
    let g = generation(&s);
    drop(s);
    sim.release();

    let ro = NativeReadOnly {
        inner: Inner::open_read_only(Box::new(sim.clone()), Default::default()).unwrap(),
    };
    let mut v = ro.begin().unwrap();
    assert_eq!(pins(&ro.inner), BTreeMap::from([(g, 1)]));
    let c = v.context(&cid("A")).unwrap().unwrap().content.unwrap();
    let reader = v.content(&c).unwrap();
    drop(v);
    assert_eq!(pins(&ro.inner), BTreeMap::from([(g, 1)]));
    drop(reader);
    assert!(pins(&ro.inner).is_empty());
}

#[test]
fn the_audit_reaches_exactly_the_pages_and_extents_of_each_published_state() {
    let sim = SimIo::new();
    let s = sim_create(&sim);
    // Two segments of content: one full chunk and a remainder.
    let payload = vec![7u8; CONTENT_CHUNK + 100];
    put(&s, "a1", "A", &payload);
    let first = s.inner.snapshot().unwrap().desc;

    let slots = published(&sim).unwrap();
    assert_eq!(
        slots.iter().map(|d| d.generation).collect::<Vec<_>>(),
        [first.generation - 1, first.generation],
        "both slots are valid, in generation order"
    );
    let r1 = reach(&sim, &first).unwrap();
    assert!(!r1.pages.is_empty());
    assert!(r1.pages.iter().all(|&p| p >= 2 && p < first.next_page));
    assert_eq!(r1.manifests.len(), 1, "{:?}", r1.manifests);
    assert_eq!(r1.content_segments.len(), 2, "{:?}", r1.content_segments);
    assert_eq!(r1.history_extents.len(), 1);
    let end = |x: &(u64, u64)| x.0 + x.1;
    assert!(
        r1.content_segments
            .iter()
            .chain(&r1.history_extents)
            .all(|x| end(x) <= first.content_len)
    );
    assert!(r1.disjoint(), "overlapping extents: {r1:?}");

    // A second commit: the earlier state stays reachable, unchanged, from its slot.
    put(&s, "a2", "B", &[9; 10]);
    let second = s.inner.snapshot().unwrap().desc;
    let slots = published(&sim).unwrap();
    assert_eq!(
        slots.iter().map(|d| d.generation).collect::<Vec<_>>(),
        [first.generation, second.generation]
    );
    assert_eq!(
        reach(&sim, &first).unwrap(),
        r1,
        "the previous state changed"
    );
    let r2 = reach(&sim, &second).unwrap();
    assert!(r2.disjoint());
    assert_eq!(r2.history_extents.len(), 2);
    assert_eq!(r2.manifests.len(), 2);
    assert!(r2.manifests.is_superset(&r1.manifests));
    assert_eq!(r2.content_segments.len(), 3);
    assert!(r2.content_segments.is_superset(&r1.content_segments));
    // Copy-on-write: the new root is a page the earlier state never used.
    assert!(!r1.pages.contains(&second.root.page));
    assert!(
        r2.pages
            .iter()
            .all(|&p| (p * PAGE_SIZE as u64) < sim_meta_len(&sim))
    );
}

fn sim_meta_len(sim: &SimIo) -> u64 {
    use crate::io::{FileId, Io};
    sim.len(FileId::Meta).unwrap()
}

#[test]
fn every_commit_retires_exactly_the_pages_it_stopped_reaching() {
    let sim = SimIo::new();
    let s = sim_create(&sim);
    for i in 0..40u8 {
        put(&s, &format!("a{i}"), &format!("K{i:03}"), &[i; 50]);
        crate::audit::check_retirement(&sim).unwrap_or_else(|e| panic!("after commit {i}: {e}"));
    }
    let cur = *published(&sim).unwrap().last().unwrap();
    let r = reach(&sim, &cur).unwrap();
    assert!(r.pages.len() > 2, "the tree never outgrew a single path");
    // Nothing leaks: every metadata page ever written is either reachable or retired.
    let written: std::collections::BTreeSet<u64> = (2..cur.next_page).collect();
    let accounted: std::collections::BTreeSet<u64> =
        r.pages.iter().chain(r.retired.keys()).copied().collect();
    assert_eq!(accounted, written);
}
