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

// ----- reuse of retired metadata pages ---------------------------------------------------------

/// Records in the fixed live set the churn below rewrites.
const LIVE: usize = 8;

fn put_body(s: &NativeStore, attempt: &str, id: &str, body: &str) {
    let cs = ChangeSet {
        changes: vec![Change::PutContext {
            id: cid(id),
            expect: Precondition::Any,
            body: body.as_bytes().to_vec(),
            content: None,
        }],
        ..ChangeSet::default()
    };
    s.commit(
        AttemptId::new(attempt.as_bytes().to_vec()).unwrap(),
        s.begin().unwrap(),
        cs,
    )
    .unwrap();
}

/// Commits `range` of the churn: commit `i` sets record `K{i % LIVE}` to `v{i}`. Every commit is
/// audited when `sim` is given.
fn churn(s: &NativeStore, sim: Option<&SimIo>, range: std::ops::Range<usize>) {
    for i in range {
        put_body(
            s,
            &format!("c{i}"),
            &format!("K{}", i % LIVE),
            &format!("v{i}"),
        );
        if let Some(sim) = sim {
            crate::audit::check_retirement(sim).unwrap_or_else(|e| panic!("after churn {i}: {e}"));
        }
    }
}

/// The live set's bodies after the first `commits` churn commits (an independent model).
fn expected_bodies(commits: usize) -> Vec<Option<Vec<u8>>> {
    (0..LIVE)
        .map(|k| {
            (0..commits)
                .rev()
                .find(|i| i % LIVE == k)
                .map(|i| format!("v{i}").into_bytes())
        })
        .collect()
}

fn bodies(v: &mut crate::NativeView) -> Vec<Option<Vec<u8>>> {
    (0..LIVE)
        .map(|k| v.context(&cid(&format!("K{k}"))).unwrap().map(|r| r.body))
        .collect()
}

fn desc(s: &NativeStore) -> crate::format::Descriptor {
    s.inner.snapshot().unwrap().desc
}

/// Pages the newest published commit wrote over retired pages (below the previous cursor).
fn reused_by_last_commit(sim: &SimIo) -> std::collections::BTreeSet<u64> {
    let slots = published(sim).unwrap();
    let (prev, cur) = (slots[0], slots[1]);
    reach(sim, &cur)
        .unwrap()
        .incarnations
        .into_iter()
        .filter(|(p, inc)| *inc == cur.generation && *p < prev.next_page)
        .map(|(p, _)| p)
        .collect()
}

#[test]
fn a_held_reader_keeps_its_pages_from_reuse_and_releasing_it_lets_them_be_reused() {
    let sim = SimIo::new();
    let s = sim_create(&sim);
    churn(&s, Some(&sim), 0..LIVE * 2);
    let mut held = s.begin().unwrap();
    let d0 = desc(&s);
    let r0 = reach(&sim, &d0).unwrap();
    let written0 = s.stats().unwrap().pages_written;

    churn(&s, Some(&sim), LIVE * 2..LIVE * 8);
    // While held: nothing the held view reaches was reused, and it reads its whole state ...
    assert_eq!(
        reach(&sim, &d0).unwrap(),
        r0,
        "a page the held view reaches was reused"
    );
    assert_eq!(bodies(&mut held), expected_bodies(LIVE * 2));
    // ... while space retired before the held generation was reused: fewer pages were allocated
    // than written.
    let d1 = desc(&s);
    let r1 = reach(&sim, &d1).unwrap();
    let written = s.stats().unwrap().pages_written - written0;
    assert!(
        d1.next_page - d0.next_page < written,
        "nothing was reused while the reader was held ({written} pages written, {} allocated)",
        d1.next_page - d0.next_page
    );

    drop(held);
    churn(&s, Some(&sim), LIVE * 8..LIVE * 14);
    // Released: pages only the released generation still reached are reused now.
    let r2 = reach(&sim, &desc(&s)).unwrap();
    let protected: Vec<u64> = r0.pages.difference(&r1.pages).copied().collect();
    assert!(!protected.is_empty());
    // A page still holding its old incarnation, reachable or as a retirement record, was never
    // rewritten; every other one was reused at least once after the release (the first half shows
    // none was before it).
    let still_original = |p: &u64| {
        let original = r0.incarnations[p];
        r2.incarnations.get(p) == Some(&original)
            || r2.retired.get(p).is_some_and(|(_, inc)| *inc == original)
    };
    assert!(
        !protected.iter().any(still_original),
        "pages the released reader had protected were never reused: {protected:?}"
    );
}

#[test]
fn a_reused_page_is_never_served_from_a_cached_earlier_incarnation() {
    let sim = SimIo::new();
    let s = sim_create(&sim);
    for i in 0..LIVE * 10 {
        churn(&s, None, i..i + 1);
        // Reading everything after each commit keeps the pages of each generation in the cache.
        let mut v = s.begin().unwrap();
        assert_eq!(bodies(&mut v), expected_bodies(i + 1), "after commit {i}");
    }
    let written = s.stats().unwrap().pages_written;
    assert!(
        written > desc(&s).next_page,
        "no page was reused ({written} pages written, {} allocated)",
        desc(&s).next_page
    );
}

#[test]
fn a_crash_during_a_reusing_commit_with_the_newest_record_damaged_recovers_the_fallback_state() {
    let n = LIVE * 6;
    let build = |sim: &SimIo| {
        let s = sim_create(sim);
        churn(&s, None, 0..n);
        s
    };
    // Probe an identical history: count the next commit's operations and confirm it reuses pages,
    // including pages its predecessor retired being available to a wrong rule.
    let probe = SimIo::new();
    let s = build(&probe);
    let g = desc(&s).generation;
    assert!(
        reach(&probe, &desc(&s))
            .unwrap()
            .retired
            .values()
            .any(|(r, _)| *r == g),
        "the newest commit retired nothing"
    );
    let before = probe.ops();
    churn(&s, None, n..n + 1);
    let per_commit = probe.ops() - before;
    assert!(
        !reused_by_last_commit(&probe).is_empty(),
        "the probed commit reused no page"
    );
    drop(s);

    let sim = SimIo::new();
    let s = build(&sim);
    assert_eq!(desc(&s).generation, g);
    // Everything of the next commit reaches the files except its publication record.
    sim.crash_after(per_commit - 2);
    let r = s.commit(
        AttemptId::new(*b"crashed").unwrap(),
        s.begin().unwrap(),
        ChangeSet {
            changes: vec![Change::PutContext {
                id: cid(&format!("K{}", n % LIVE)),
                expect: Precondition::Any,
                body: format!("v{n}").into_bytes(),
                content: None,
            }],
            ..ChangeSet::default()
        },
    );
    assert!(r.is_err(), "{r:?}");
    drop(s);
    let img = sim.after_crash(crate::sim::Persist::All);
    // The newest record (generation g) is damaged: recovery must fall back to g - 1 intact.
    use crate::io::{FileId, Io};
    let at = crate::format::Descriptor::slot(g) * PAGE_SIZE as u64;
    let mut slot = vec![0; PAGE_SIZE];
    img.read_at(FileId::Meta, at, &mut slot).unwrap();
    slot[40] ^= 0x5a;
    img.write_at(FileId::Meta, at, &slot).unwrap();
    img.sync(FileId::Meta).unwrap();

    let rec = NativeStore {
        inner: Inner::open(Box::new(img.clone()), Default::default()).unwrap(),
    };
    assert_eq!(desc(&rec).generation, g - 1);
    assert_eq!(
        bodies(&mut rec.begin().unwrap()),
        expected_bodies(n - 1),
        "the fallback state was damaged by reuse"
    );
    reach(&img, &desc(&rec)).unwrap();
}

#[test]
fn a_view_registered_while_a_commit_chooses_pages_to_reuse_keeps_its_state() {
    let sim = SimIo::new();
    let s = sim_create(&sim);
    churn(&s, Some(&sim), 0..LIVE * 4);
    let before = desc(&s);
    let r_before = reach(&sim, &before).unwrap();

    let (tx, rx) = std::sync::mpsc::channel();
    let inner = s.inner.clone();
    assert!(
        s.inner
            .set_reuse_hook_for_test(Some(Box::new(move || {
                tx.send(crate::NativeView::new(&inner).unwrap()).unwrap();
            })))
            .is_none()
    );
    churn(&s, Some(&sim), LIVE * 4..LIVE * 4 + 1);
    let mut v = rx
        .try_recv()
        .expect("the hook did not run inside the commit");
    assert_eq!(v.snap.desc, before, "the view registered on another state");
    assert!(
        !reused_by_last_commit(&sim).is_empty(),
        "the commit reused nothing while the view registered"
    );

    churn(&s, Some(&sim), LIVE * 4 + 1..LIVE * 12);
    assert_eq!(bodies(&mut v), expected_bodies(LIVE * 4));
    assert_eq!(reach(&sim, &before).unwrap(), r_before);
}

#[test]
fn every_cut_of_a_reusing_commit_recovers_old_or_new_with_consistent_retirement() {
    let n = LIVE * 6;
    let build = |sim: &SimIo| {
        let s = sim_create(sim);
        churn(&s, None, 0..n);
        s
    };
    let probe = SimIo::new();
    let s = build(&probe);
    let before = probe.ops();
    churn(&s, None, n..n + 1);
    let per_commit = probe.ops() - before;
    assert!(!reused_by_last_commit(&probe).is_empty());
    drop(s);
    let (old, new) = (expected_bodies(n), expected_bodies(n + 1));

    for cut in 0..=per_commit {
        let sim = SimIo::new();
        let s = build(&sim);
        sim.crash_after(cut);
        let acked = s
            .commit(
                AttemptId::new(format!("c{n}").into_bytes()).unwrap(),
                s.begin().unwrap(),
                ChangeSet {
                    changes: vec![Change::PutContext {
                        id: cid(&format!("K{}", n % LIVE)),
                        expect: Precondition::Any,
                        body: format!("v{n}").into_bytes(),
                        content: None,
                    }],
                    ..ChangeSet::default()
                },
            )
            .is_ok();
        drop(s);
        let persists = [crate::sim::Persist::None, crate::sim::Persist::All]
            .into_iter()
            .chain((1..=6).map(crate::sim::Persist::Seeded));
        for p in persists {
            let img = sim.after_crash(p);
            let rec = NativeStore {
                inner: Inner::open(Box::new(img.clone()), Default::default()).unwrap(),
            };
            crate::audit::check_retirement(&img).unwrap_or_else(|e| panic!("cut {cut} {p:?}: {e}"));
            let got = bodies(&mut rec.begin().unwrap());
            assert!(got == old || got == new, "cut {cut} {p:?}: {got:?}");
            if acked {
                assert_eq!(got, new, "cut {cut} {p:?}: acknowledged commit lost");
            }
            // The recovered store keeps reusing correctly.
            churn(&rec, Some(&img), n + 1..n + 4);
        }
    }
}

#[test]
fn a_fixed_live_set_keeps_idle_metadata_space_constant() {
    // Each commit also adds a retained history entry and attempt receipt, so reachable metadata
    // legitimately grows. What must not grow is space that is neither reachable nor reused.
    let sim = SimIo::new();
    let s = sim_create(&sim);
    let sample = |s: &NativeStore| {
        let d = desc(s);
        let r = reach(&sim, &d).unwrap();
        // Every allocated node page is reachable or retired (the audit checks this per commit).
        assert_eq!(d.next_page - 2, (r.pages.len() + r.retired.len()) as u64);
        (d.next_page, r.pages.len() as u64, r.retired.len())
    };
    churn(&s, Some(&sim), 0..LIVE * 20);
    let (warm_next, warm_reachable, warm_idle) = sample(&s);
    churn(&s, Some(&sim), LIVE * 20..LIVE * 48);
    let (next, reachable, idle) = sample(&s);
    assert!(reachable > warm_reachable, "retained history did not grow");
    assert_eq!(
        idle, warm_idle,
        "idle (retired, unreused) pages kept growing"
    );
    assert_eq!(
        next - warm_next,
        reachable - warm_reachable,
        "allocation grew beyond the growth of reachable (retained) metadata"
    );
}
