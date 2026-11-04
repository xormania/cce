//! Space reuse groundwork: reader pins (which committed generations live views and content readers
//! still read) and an independent reachability audit of both publication slots.

use std::collections::BTreeMap;
use std::io::Read;

use cce_core::storage::{
    AttemptId, Change, ChangeSet, CommitError, ContentBinding, ContextId, Precondition, ReadView,
    StorageEngine, StorageError,
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
    let build = |s: NativeStore| {
        churn(&s, None, 0..n);
        s
    };
    let probe = SimIo::new();
    let s = build(sim_create(&probe));
    let before = probe.ops();
    churn(&s, None, n..n + 1);
    let per_commit = probe.ops() - before;
    assert!(!reused_by_last_commit(&probe).is_empty());
    drop(s);
    let (old, new) = (expected_bodies(n), expected_bodies(n + 1));

    for cut in 0..=per_commit {
        let sim = SimIo::new();
        let trace = Trace::default();
        let s = build(traced_create(&sim, &trace));
        trace.lock().unwrap().clear();
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
        for (survived, img) in crash_images(&sim, 6) {
            let what = format!("reusing commit cut {cut}, {survived}");
            repro_case(&what, &img, &trace, || {
                let rec = NativeStore {
                    inner: Inner::open(Box::new(img.clone()), Default::default()).unwrap(),
                };
                crate::audit::check_retirement(&img).unwrap_or_else(|e| panic!("{what}: {e}"));
                let got = bodies(&mut rec.begin().unwrap());
                assert!(got == old || got == new, "{what}: {got:?}");
                if acked {
                    assert_eq!(got, new, "{what}: acknowledged commit lost");
                }
                // The recovered store keeps reusing correctly.
                churn(&rec, Some(&img), n + 1..n + 4);
            });
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

// ----- logical content removal -----------------------------------------------------------------

fn remove(s: &NativeStore, attempt: &str, id: &str) {
    s.commit(
        AttemptId::new(attempt.as_bytes().to_vec()).unwrap(),
        s.begin().unwrap(),
        ChangeSet {
            changes: vec![Change::RemoveContext {
                id: cid(id),
                expect: Precondition::Any,
            }],
            ..ChangeSet::default()
        },
    )
    .unwrap();
}

fn read_all(
    v: &mut crate::NativeView,
    c: &cce_core::storage::ContentRef,
) -> Result<Vec<u8>, StorageError> {
    let mut bytes = Vec::new();
    v.content(c)?
        .read_to_end(&mut bytes)
        .map_err(|e| StorageError::Io(e.to_string()))?;
    Ok(bytes)
}

#[test]
fn content_whose_last_record_is_gone_is_not_retained_but_an_older_view_still_reads_it() {
    let sim = SimIo::new();
    let s = sim_create(&sim);
    let payload: Vec<u8> = (0..CONTENT_CHUNK + 500).map(|i| (i % 251) as u8).collect();
    put(&s, "a1", "A", &payload);
    // A second record with the same bytes (staged again) shares the committed content; the second
    // staging is never indexed, so the commit retires it although no earlier state reached it.
    let before = reach(&sim, &desc(&s)).unwrap();
    put(&s, "a2", "B", &payload);
    let d = desc(&s);
    let after = reach(&sim, &d).unwrap();
    assert_eq!(
        after.manifests, before.manifests,
        "the duplicate was indexed"
    );
    let retired_now: Vec<_> = after
        .retired_extents
        .iter()
        .filter(|(_, (g, _))| *g == d.generation)
        .collect();
    assert_eq!(
        retired_now.len(),
        3,
        "the duplicate staging (manifest and two segments) was not retired: {retired_now:?}"
    );
    let c = s
        .begin()
        .unwrap()
        .context(&cid("A"))
        .unwrap()
        .unwrap()
        .content
        .unwrap();
    crate::audit::check_retirement(&sim).unwrap();

    remove(&s, "a3", "A");
    crate::audit::check_retirement(&sim).unwrap();
    // Still used by B: readable through history's reference.
    assert_eq!(read_all(&mut s.begin().unwrap(), &c).unwrap(), payload);

    let mut old = s.begin().unwrap();
    put_body(&s, "a4", "B", "no content any more");
    crate::audit::check_retirement(&sim).unwrap();
    // No record uses it: new views get exactly "not retained", while history still names it.
    let mut v = s.begin().unwrap();
    assert_eq!(read_all(&mut v, &c), Err(StorageError::ContentNotRetained));
    let named: Vec<_> = v
        .history(cce_core::storage::HistoryPosition(0))
        .unwrap()
        .map(|h| h.unwrap().content)
        .collect();
    assert_eq!(named[0], vec![c], "history no longer names the content");
    // The view that began before the removal still reads it whole.
    assert_eq!(read_all(&mut old, &c).unwrap(), payload);
    drop((v, old));

    // The same after a fresh open.
    drop(s);
    sim.release();
    let s = NativeStore {
        inner: Inner::open(Box::new(sim.clone()), Default::default()).unwrap(),
    };
    assert_eq!(
        read_all(&mut s.begin().unwrap(), &c),
        Err(StorageError::ContentNotRetained)
    );
    // Committing the same bytes again makes them readable again.
    put(&s, "a5", "C", &payload);
    assert_eq!(read_all(&mut s.begin().unwrap(), &c).unwrap(), payload);
    crate::audit::check_retirement(&sim).unwrap();
}

#[test]
fn every_commit_retires_exactly_the_content_it_stopped_reaching() {
    let sim = SimIo::new();
    let s = sim_create(&sim);
    // Records alternate between distinct payloads, sometimes sharing, sometimes dropping content.
    let mut present = std::collections::BTreeSet::new();
    let mut removed_at_some_point = false;
    for i in 0..60u32 {
        let id = format!("R{}", i % 5);
        let removing = i % 7 == 3 && present.contains(&id);
        if removing {
            present.remove(&id);
        } else {
            present.insert(id.clone());
        }
        match i % 7 {
            3 if removing => remove(&s, &format!("x{i}"), &id),
            5 => put_body(&s, &format!("x{i}"), &id, "plain"),
            _ => {
                let payload = vec![(i % 4) as u8; 1000 + (i % 4) as usize];
                put(&s, &format!("x{i}"), &id, &payload);
            }
        }
        crate::audit::check_retirement(&sim).unwrap_or_else(|e| panic!("after commit {i}: {e}"));
        let cur = *published(&sim).unwrap().last().unwrap();
        removed_at_some_point |= reach(&sim, &cur).unwrap().tombstones > 0;
    }
    let cur = *published(&sim).unwrap().last().unwrap();
    let r = reach(&sim, &cur).unwrap();
    assert!(!r.retired_extents.is_empty(), "no content was ever retired");
    assert!(removed_at_some_point, "no content was ever removed");
}

// ----- reuse of retired content space ----------------------------------------------------------

type Extents = std::collections::BTreeSet<(u64, u64)>;

fn staged_extents(s: &NativeStore, staged: &cce_core::storage::StagedContent) -> Extents {
    s.inner
        .staged_extents_for_test(staged)
        .unwrap()
        .into_iter()
        .collect()
}

fn content_extents(r: &crate::audit::Reach) -> Extents {
    r.manifests.union(&r.content_segments).copied().collect()
}

fn overlaps(a: &Extents, b: &Extents) -> bool {
    a.iter()
        .any(|x| b.iter().any(|y| x.0 < y.0 + y.1 && y.0 < x.0 + x.1))
}

fn payload(seed: u8, len: usize) -> Vec<u8> {
    (0..len)
        .map(|i| (i as u8).wrapping_mul(31).wrapping_add(seed))
        .collect()
}

fn commit_staged(
    s: &NativeStore,
    attempt: &str,
    id: &str,
    staged: &cce_core::storage::StagedContent,
) {
    let cs = ChangeSet {
        changes: vec![Change::PutContext {
            id: cid(id),
            expect: Precondition::Any,
            body: id.as_bytes().to_vec(),
            content: Some(0),
        }],
        content: vec![ContentBinding::from(staged)],
        ..ChangeSet::default()
    };
    s.commit(
        AttemptId::new(attempt.as_bytes().to_vec()).unwrap(),
        s.begin().unwrap(),
        cs,
    )
    .unwrap();
}

fn content_of(s: &NativeStore, id: &str) -> cce_core::storage::ContentRef {
    s.begin()
        .unwrap()
        .context(&cid(id))
        .unwrap()
        .unwrap()
        .content
        .unwrap()
}

/// Commits `bytes` as record `id`, then drops it again and advances one generation, so its space
/// is retired and no published state reaches it. Returns its reference and extents.
fn committed_then_removed(
    s: &NativeStore,
    sim: &SimIo,
    tag: &str,
    bytes: &[u8],
) -> (cce_core::storage::ContentRef, Extents) {
    put(s, &format!("{tag}-put"), tag, bytes);
    let c = content_of(s, tag);
    let before = content_extents(&reach(sim, &desc(s)).unwrap());
    remove(s, &format!("{tag}-remove"), tag);
    let after = content_extents(&reach(sim, &desc(s)).unwrap());
    put_body(s, &format!("{tag}-advance"), "advance", tag);
    (c, before.difference(&after).copied().collect())
}

#[test]
fn reused_content_space_is_never_read_as_the_content_that_was_removed() {
    let sim = SimIo::new();
    let s = sim_create(&sim);
    let x = payload(1, CONTENT_CHUNK + 300);
    let (cx, x_ext) = committed_then_removed(&s, &sim, "X", &x);
    assert_eq!(x_ext.len(), 3, "manifest and two segments: {x_ext:?}");

    let y = payload(2, CONTENT_CHUNK + 300);
    let sy = s.stage_content(&mut &y[..]).unwrap();
    assert!(
        overlaps(&staged_extents(&s, &sy), &x_ext),
        "the new content did not reuse the removed content's space"
    );
    commit_staged(&s, "y", "Y", &sy);
    crate::audit::check_retirement(&sim).unwrap();
    let cy = content_of(&s, "Y");

    let mut v = s.begin().unwrap();
    assert_eq!(read_all(&mut v, &cx), Err(StorageError::ContentNotRetained));
    assert_eq!(read_all(&mut v, &cy).unwrap(), y);
    drop(v);
    drop(s);
    sim.release();
    let s = NativeStore {
        inner: Inner::open(Box::new(sim.clone()), Default::default()).unwrap(),
    };
    let mut v = s.begin().unwrap();
    assert_eq!(read_all(&mut v, &cx), Err(StorageError::ContentNotRetained));
    assert_eq!(read_all(&mut v, &cy).unwrap(), y);
}

#[test]
fn a_reader_from_before_a_removal_keeps_its_content_space_until_released() {
    let sim = SimIo::new();
    let s = sim_create(&sim);
    let size = CONTENT_CHUNK + 300;
    // X is committed first (into fresh space); W is retired before the reader begins; X is
    // retired while it is held.
    let x = payload(4, size);
    put(&s, "x-put", "X", &x);
    let cx = content_of(&s, "X");
    let x_ext = content_extents(&reach(&sim, &desc(&s)).unwrap());
    let (_, w_ext) = committed_then_removed(&s, &sim, "W", &payload(3, size));
    assert!(!overlaps(&w_ext, &x_ext));
    let mut old = s.begin().unwrap();
    remove(&s, "x-remove", "X");
    put_body(&s, "x-advance", "advance", "x");

    // Held: the reader reads X whole (never the bytes of later content), X's space is not reused,
    // and W's space is.
    let sy = s.stage_content(&mut &payload(5, size)[..]).unwrap();
    let y_ext = staged_extents(&s, &sy);
    commit_staged(&s, "y", "Y", &sy);
    assert_eq!(
        read_all(&mut old, &cx),
        Ok(x.clone()),
        "the held reader did not read its content"
    );
    assert!(
        !overlaps(&y_ext, &x_ext),
        "space the held reader can reach was reused"
    );
    assert!(
        overlaps(&y_ext, &w_ext),
        "nothing was reused while the reader was held"
    );

    // Released: X's space is reused.
    drop(old);
    let sz = s.stage_content(&mut &payload(6, size)[..]).unwrap();
    assert!(
        overlaps(&staged_extents(&s, &sz), &x_ext),
        "space the released reader had protected was not reused"
    );
    commit_staged(&s, "z", "Z", &sz);
    crate::audit::check_retirement(&sim).unwrap();
}

#[test]
fn an_abandoned_or_pending_staging_keeps_its_space_until_resolved() {
    let sim = SimIo::new();
    let s = sim_create(&sim);
    let size = CONTENT_CHUNK + 300;
    let (_, w_ext) = committed_then_removed(&s, &sim, "W", &payload(7, size));
    let (_, x_ext) = committed_then_removed(&s, &sim, "X", &payload(8, size));
    let retired: Extents = w_ext.union(&x_ext).copied().collect();

    // Abandoned: staged into retired space, the handle dropped without a commit.
    let a = s.stage_content(&mut &payload(9, size)[..]).unwrap();
    let a_ext = staged_extents(&s, &a);
    assert!(
        overlaps(&a_ext, &retired),
        "the staging reused no retired space"
    );
    drop(a);

    // Pending: admitted while the worker is held, its ticket dropped.
    let gate = s.inner.pause_worker_for_test();
    let c_bytes = payload(10, size);
    let c = s.stage_content(&mut &c_bytes[..]).unwrap();
    let c_ext = staged_extents(&s, &c);
    let ticket = s
        .submit(
            AttemptId::new(*b"pending").unwrap(),
            s.begin().unwrap(),
            ChangeSet {
                changes: vec![Change::PutContext {
                    id: cid("C"),
                    expect: Precondition::Any,
                    body: b"C".to_vec(),
                    content: Some(0),
                }],
                content: vec![ContentBinding::from(&c)],
                ..ChangeSet::default()
            },
        )
        .unwrap();
    drop(c);
    let d = s.stage_content(&mut &payload(11, size)[..]).unwrap();
    let d_ext = staged_extents(&s, &d);
    assert!(
        !overlaps(&d_ext, &a_ext),
        "an abandoned staging's space was handed out again"
    );
    assert!(
        !overlaps(&d_ext, &c_ext),
        "a pending commit's payload space was handed out again"
    );
    drop(gate);
    ticket.wait().unwrap();
    assert_eq!(
        read_all(&mut s.begin().unwrap(), &content_of(&s, "C")).unwrap(),
        c_bytes
    );
    crate::audit::check_retirement(&sim).unwrap();

    // A fresh open forgets abandoned stagings: their space is reusable again.
    drop((d, s));
    sim.release();
    let s = NativeStore {
        inner: Inner::open(Box::new(sim.clone()), Default::default()).unwrap(),
    };
    let e = s.stage_content(&mut &payload(12, size)[..]).unwrap();
    assert!(
        overlaps(&staged_extents(&s, &e), &a_ext),
        "an abandoned staging's space never came back"
    );
}

#[test]
fn create_and_replace_cycles_keep_idle_content_space_constant() {
    let sim = SimIo::new();
    let s = sim_create(&sim);
    // Three records, each replaced by distinct content every few commits.
    let cycle = |s: &NativeStore, range: std::ops::Range<usize>| {
        for i in range {
            put(
                s,
                &format!("k{i}"),
                &format!("R{}", i % 3),
                &payload(i as u8, 20_000),
            );
            crate::audit::check_retirement(&sim).unwrap_or_else(|e| panic!("after cycle {i}: {e}"));
        }
    };
    let sample = |s: &NativeStore| {
        let d = desc(s);
        let r = reach(&sim, &d).unwrap();
        let history: u64 = r.history_extents.iter().map(|(_, len)| len).sum();
        (d.content_len, history, r.retired_extents.len())
    };
    cycle(&s, 0..30);
    let (warm_len, warm_history, warm_idle) = sample(&s);
    cycle(&s, 30..90);
    let (len, history, idle) = sample(&s);
    assert_eq!(
        idle, warm_idle,
        "idle (retired, unreused) content extents kept growing"
    );
    assert_eq!(
        len - warm_len,
        history - warm_history,
        "the content file grew beyond its retained history"
    );
}

/// A store whose removed content X is retired and reusable by the next staging.
fn with_reusable_content(
    sim: &SimIo,
    size: usize,
) -> (NativeStore, cce_core::storage::ContentRef, Extents) {
    reusable_content_in(sim_create(sim), sim, size)
}

/// Fills `s` (on `sim`) so that its removed content X is retired and reusable by the next staging.
fn reusable_content_in(
    s: NativeStore,
    sim: &SimIo,
    size: usize,
) -> (NativeStore, cce_core::storage::ContentRef, Extents) {
    put(&s, "keep", "K", &payload(20, 1000));
    let (cx, x_ext) = committed_then_removed(&s, sim, "X", &payload(21, size));
    (s, cx, x_ext)
}

type Trace = std::sync::Arc<std::sync::Mutex<Vec<crate::io::Op>>>;

/// A store on `sim` whose I/O operations are recorded in `trace`.
fn traced_create(sim: &SimIo, trace: &Trace) -> NativeStore {
    NativeStore {
        inner: Inner::create(
            Box::new(crate::io::Traced {
                inner: sim.clone(),
                trace: trace.clone(),
            }),
            STORE,
            Default::default(),
        )
        .unwrap(),
    }
}

/// The images a crash of `sim` now leaves, each with its label: power loss with no, all, and
/// `seeds` sampled subsets of the unsynced writes surviving, and a process exit that leaves them
/// cached.
fn crash_images(sim: &SimIo, seeds: u64) -> Vec<(String, SimIo)> {
    [crate::sim::Persist::None, crate::sim::Persist::All]
        .into_iter()
        .chain((1..=seeds).map(crate::sim::Persist::Seeded))
        .map(|p| (crate::repro_tests::survival(p), sim.after_crash(p)))
        .chain(std::iter::once((
            crate::repro_tests::PROCESS_EXIT.to_string(),
            sim.after_process_exit(),
        )))
        .collect()
}

/// Runs one recovery case; if it fails, the image and the recorded operations are saved for
/// offline reproduction (see the repro helpers).
fn repro_case(label: &str, img: &SimIo, trace: &Trace, case: impl FnOnce()) {
    let pristine = img.after_process_exit();
    let ops = trace.lock().unwrap().clone();
    crate::repro_tests::with_repro(label, &[("image", &pristine)], &ops, case);
}

#[test]
fn a_failure_while_staging_into_or_committing_reused_space_is_explicit_and_harmless() {
    let size = CONTENT_CHUNK + 300;
    let y = payload(22, size);
    // Probe: operations of staging Y (writes) and of committing it.
    let probe = SimIo::new();
    let (s, _, x_ext) = with_reusable_content(&probe, size);
    let start = probe.ops();
    let sy = s.stage_content(&mut &y[..]).unwrap();
    assert!(
        overlaps(&staged_extents(&s, &sy), &x_ext),
        "the probe reused nothing"
    );
    let staging_ops = probe.ops() - start;
    commit_staged(&s, "y", "Y", &sy);
    let total = probe.ops() - start;
    drop(s);

    let (mut staging_failures, mut commit_failures) = (0, 0);
    for n in 1..=total {
        let sim = SimIo::new();
        let (s, cx, x_ext) = with_reusable_content(&sim, size);
        let old = logical_view_state(&s);
        sim.fail_nth_op(n);
        let staged = s.stage_content(&mut &y[..]);
        let outcome = match &staged {
            Err(e) => {
                assert!(n <= staging_ops, "op {n}: staging failed late: {e:?}");
                staging_failures += 1;
                // The failed staging gave its reused space back: staging again reuses it.
                let again = s.stage_content(&mut &y[..]).unwrap();
                assert!(
                    overlaps(&staged_extents(&s, &again), &x_ext),
                    "op {n}: space taken by the failed staging was lost"
                );
                None
            }
            Ok(sy) => Some(s.commit(
                AttemptId::new(*b"y").unwrap(),
                s.begin().unwrap(),
                ChangeSet {
                    changes: vec![Change::PutContext {
                        id: cid("Y"),
                        expect: Precondition::Any,
                        body: b"Y".to_vec(),
                        content: Some(0),
                    }],
                    content: vec![ContentBinding::from(sy)],
                    ..ChangeSet::default()
                },
            )),
        };
        if let Some(r) = &outcome {
            assert!(
                r.is_err(),
                "op {n}: a commit with a failed operation succeeded"
            );
            commit_failures += 1;
        }
        drop((staged, s));
        // A fresh process: a whole old or new state, consistent retirement, and reuse works.
        let img = sim.after_process_exit();
        let rec = NativeStore {
            inner: Inner::open(Box::new(img.clone()), Default::default()).unwrap(),
        };
        crate::audit::check_retirement(&img).unwrap_or_else(|e| panic!("op {n}: {e}"));
        let got = logical_view_state(&rec);
        if !matches!(
            outcome,
            Some(Err(cce_core::storage::CommitError::Indeterminate(_)))
        ) {
            assert_eq!(got, old, "op {n}: a failed commit changed the state");
        }
        let mut v = rec.begin().unwrap();
        assert_eq!(read_all(&mut v, &cx), Err(StorageError::ContentNotRetained));
        if got != old {
            assert_eq!(
                read_all(&mut v, &content_of(&rec, "Y")).unwrap(),
                y,
                "op {n}"
            );
        }
        drop(v);
        let again = rec.stage_content(&mut &payload(23, size)[..]).unwrap();
        commit_staged(&rec, "after", "Z", &again);
        crate::audit::check_retirement(&img).unwrap_or_else(|e| panic!("op {n}: after: {e}"));
    }
    assert!(
        staging_failures > 0 && commit_failures > 0,
        "failures hit only one phase: {staging_failures} staging, {commit_failures} commit"
    );
}

/// Records with their bodies and content references, through a fresh view.
fn logical_view_state(s: &NativeStore) -> Vec<String> {
    let mut v = s.begin().unwrap();
    ["K", "X", "Y", "advance"]
        .iter()
        .map(|id| format!("{id}: {:?}", v.context(&cid(id)).unwrap()))
        .collect()
}

#[test]
fn every_cut_of_a_commit_binding_reused_content_recovers_old_or_new() {
    let size = CONTENT_CHUNK + 300;
    let y = payload(24, size);
    let probe = SimIo::new();
    let (s, _, x_ext) = with_reusable_content(&probe, size);
    let sy = s.stage_content(&mut &y[..]).unwrap();
    assert!(
        overlaps(&staged_extents(&s, &sy), &x_ext),
        "the probe reused nothing"
    );
    let before = probe.ops();
    commit_staged(&s, "y", "Y", &sy);
    let per_commit = probe.ops() - before;
    drop(s);

    for cut in 0..=per_commit {
        let sim = SimIo::new();
        let trace = Trace::default();
        let (s, cx, _) = reusable_content_in(traced_create(&sim, &trace), &sim, size);
        let old = logical_view_state(&s);
        let sy = s.stage_content(&mut &y[..]).unwrap();
        trace.lock().unwrap().clear();
        sim.crash_after(cut);
        let acked = s
            .commit(
                AttemptId::new(*b"y").unwrap(),
                s.begin().unwrap(),
                ChangeSet {
                    changes: vec![Change::PutContext {
                        id: cid("Y"),
                        expect: Precondition::Any,
                        body: b"Y".to_vec(),
                        content: Some(0),
                    }],
                    content: vec![ContentBinding::from(&sy)],
                    ..ChangeSet::default()
                },
            )
            .is_ok();
        drop((sy, s));
        for (survived, img) in crash_images(&sim, 6) {
            let what = format!("commit binding reused content, cut {cut}, {survived}");
            repro_case(&what, &img, &trace, || {
                let rec = NativeStore {
                    inner: Inner::open(Box::new(img.clone()), Default::default()).unwrap(),
                };
                crate::audit::check_retirement(&img).unwrap_or_else(|e| panic!("{what}: {e}"));
                let got = logical_view_state(&rec);
                let mut v = rec.begin().unwrap();
                assert_eq!(read_all(&mut v, &cx), Err(StorageError::ContentNotRetained));
                if got != old {
                    assert_eq!(
                        read_all(&mut v, &content_of(&rec, "Y")).unwrap(),
                        y,
                        "{what}"
                    );
                } else {
                    assert!(!acked, "{what}: acknowledged commit lost");
                }
            });
        }
    }
}

#[test]
fn staging_right_after_a_removal_leaves_the_fallback_state_readable() {
    let size = CONTENT_CHUNK + 300;
    let x = payload(30, size);
    let sim = SimIo::new();
    let s = sim_create(&sim);
    put(&s, "x", "X", &x);
    let cx = content_of(&s, "X");
    let live_gen = desc(&s).generation;
    remove(&s, "x-remove", "X");
    let g = desc(&s).generation;
    // X is retired by the newest commit; the other slot still holds the state where X is live.
    let r = reach(&sim, &desc(&s)).unwrap();
    assert!(
        r.retired_extents.values().any(|(at, _)| *at == g),
        "the removal retired nothing"
    );
    // Staging now must not write over X: the fallback publication record still reaches it.
    let sy = s.stage_content(&mut &payload(31, size)[..]).unwrap();
    let y_ext = staged_extents(&s, &sy);
    drop((sy, s));
    let img = sim.after_crash(crate::sim::Persist::All);
    // The newest record is damaged: recovery falls back to the state where X is live.
    use crate::io::{FileId, Io};
    let at = crate::format::Descriptor::slot(g) * PAGE_SIZE as u64;
    let mut slot = vec![0; PAGE_SIZE];
    img.read_at(FileId::Meta, at, &mut slot).unwrap();
    slot[40] ^= 0x5a;
    img.write_at(FileId::Meta, at, &slot).unwrap();
    img.sync(FileId::Meta).unwrap();
    let rec = NativeStore {
        inner: Inner::open(Box::new(img), Default::default()).unwrap(),
    };
    assert_eq!(desc(&rec).generation, live_gen);
    assert_eq!(
        read_all(&mut rec.begin().unwrap(), &cx).unwrap(),
        x,
        "the fallback state's content was overwritten by staging at {y_ext:?}"
    );
}

#[test]
fn a_partly_used_retired_extent_keeps_its_remainder_reusable() {
    let size = CONTENT_CHUNK + 300;
    let sim = SimIo::new();
    let (s, _, x_ext) = with_reusable_content(&sim, size);
    let big = *x_ext.iter().max_by_key(|(_, len)| *len).unwrap();
    // A small staging takes part of the large retired segment.
    let sy = s.stage_content(&mut &payload(32, 1000)[..]).unwrap();
    let y_ext = staged_extents(&s, &sy);
    assert!(
        y_ext.contains(&(big.0, 1000)),
        "{y_ext:?} did not start in {big:?}"
    );
    commit_staged(&s, "y", "Y", &sy);
    crate::audit::check_retirement(&sim).unwrap();
    let rest = (big.0 + 1000, big.1 - 1000);
    let r = reach(&sim, &desc(&s)).unwrap();
    assert_eq!(
        r.retired_extents.get(&rest.0).map(|(_, len)| *len),
        Some(rest.1),
        "the unused remainder was not kept retired"
    );
    // The remainder is reused by the next staging that fits it.
    put_body(&s, "advance", "advance", "y");
    let sz = s.stage_content(&mut &payload(33, 60_000)[..]).unwrap();
    assert!(
        overlaps(&staged_extents(&s, &sz), &Extents::from([rest])),
        "the remainder was not reused"
    );
    commit_staged(&s, "z", "Z", &sz);
    crate::audit::check_retirement(&sim).unwrap();
}

#[test]
fn stats_account_for_reused_pages_and_content() {
    let sim = SimIo::new();
    let s = sim_create(&sim);
    let (st0, d0) = (s.stats().unwrap(), desc(&s));
    churn(&s, Some(&sim), 0..LIVE * 6);
    let size = CONTENT_CHUNK + 300;
    committed_then_removed(&s, &sim, "X", &payload(40, size));
    let sy = s.stage_content(&mut &payload(41, size)[..]).unwrap();
    commit_staged(&s, "y", "Y", &sy);
    let (st, d) = (s.stats().unwrap(), desc(&s));

    assert!(st.pages_reused > st0.pages_reused, "no page was reused");
    assert!(
        st.content_bytes_reused > st0.content_bytes_reused,
        "no content was reused"
    );
    // Everything written either reused retired space or extended the files.
    assert_eq!(
        (st.pages_written - st0.pages_written) - (st.pages_reused - st0.pages_reused),
        d.next_page - d0.next_page,
        "page accounting"
    );
    assert_eq!(
        (st.content_bytes_written - st0.content_bytes_written)
            - (st.content_bytes_reused - st0.content_bytes_reused),
        d.content_len - d0.content_len,
        "content accounting"
    );
    // With no staging outstanding, everything retired is available for reuse.
    let retired: u64 = reach(&sim, &d)
        .unwrap()
        .retired_extents
        .values()
        .map(|(_, len)| len)
        .sum();
    assert_eq!(st.reusable_content_bytes, retired);
}

// ----- discarding staged content ---------------------------------------------------------------

fn copy_of(staged: &cce_core::storage::StagedContent) -> cce_core::storage::StagedContent {
    cce_core::storage::StagedContent {
        content: staged.content,
        token: staged.token.clone(),
    }
}

#[test]
fn discarded_staged_content_is_retired_by_the_next_commit_and_reused() {
    let sim = SimIo::new();
    let s = sim_create(&sim);
    put(&s, "keep", "K", &payload(50, 1000));
    let size = CONTENT_CHUNK + 300;
    let a = s.stage_content(&mut &payload(51, size)[..]).unwrap();
    let a_ext = staged_extents(&s, &a);
    s.discard_staged(a).unwrap();

    // The next commit retires the discarded staging's space ...
    put_body(&s, "c1", "advance", "1");
    crate::audit::check_retirement(&sim).unwrap();
    let d = desc(&s);
    let r = reach(&sim, &d).unwrap();
    for (offset, len) in &a_ext {
        assert_eq!(
            r.retired_extents.get(offset),
            Some(&(d.generation, *len)),
            "the discarded staging at {offset} was not retired by the next commit"
        );
    }
    // ... and once no publication slot reaches it, staging reuses it.
    put_body(&s, "c2", "advance", "2");
    let b = s.stage_content(&mut &payload(52, size)[..]).unwrap();
    assert!(
        overlaps(&staged_extents(&s, &b), &a_ext),
        "the discarded staging's space was not reused"
    );
    commit_staged(&s, "b", "B", &b);
    crate::audit::check_retirement(&sim).unwrap();
}

#[test]
fn discarding_a_staging_in_reused_space_returns_that_space_at_once() {
    let size = CONTENT_CHUNK + 300;
    let sim = SimIo::new();
    let (s, _, x_ext) = with_reusable_content(&sim, size);
    let a = s.stage_content(&mut &payload(53, size)[..]).unwrap();
    assert!(overlaps(&staged_extents(&s, &a), &x_ext));
    s.discard_staged(a).unwrap();
    let b = s.stage_content(&mut &payload(54, size)[..]).unwrap();
    assert!(
        overlaps(&staged_extents(&s, &b), &x_ext),
        "space the discarded staging reused was not returned"
    );
    commit_staged(&s, "b", "B", &b);
    crate::audit::check_retirement(&sim).unwrap();
}

#[test]
fn discarding_is_refused_for_pending_committed_repeated_or_foreign_handles() {
    let sim = SimIo::new();
    let s = sim_create(&sim);
    put(&s, "keep", "K", &payload(55, 1000));

    // Pending: an admitted commit binds it.
    let gate = s.inner.pause_worker_for_test();
    let c_bytes = payload(56, 5000);
    let c = s.stage_content(&mut &c_bytes[..]).unwrap();
    let ticket = s
        .submit(
            AttemptId::new(*b"pending").unwrap(),
            s.begin().unwrap(),
            ChangeSet {
                changes: vec![Change::PutContext {
                    id: cid("C"),
                    expect: Precondition::Any,
                    body: b"C".to_vec(),
                    content: Some(0),
                }],
                content: vec![ContentBinding::from(&c)],
                ..ChangeSet::default()
            },
        )
        .unwrap();
    match s.discard_staged(copy_of(&c)) {
        Err(StorageError::InvalidRequest(m)) => assert!(m.contains("admitted"), "{m}"),
        other => panic!("discarding a pending commit's content: {other:?}"),
    }
    drop(gate);
    ticket.wait().unwrap();
    let cc = content_of(&s, "C");
    assert_eq!(read_all(&mut s.begin().unwrap(), &cc).unwrap(), c_bytes);

    // Committed: the index owns its space now.
    assert_eq!(
        s.discard_staged(copy_of(&c)),
        Err(StorageError::ForeignHandle)
    );
    // Repeated.
    let d = s.stage_content(&mut &payload(57, 5000)[..]).unwrap();
    let again = copy_of(&d);
    s.discard_staged(d).unwrap();
    assert_eq!(s.discard_staged(again), Err(StorageError::ForeignHandle));
    // Another store's handle.
    let other = sim_create(&SimIo::new());
    let e = other.stage_content(&mut &payload(58, 5000)[..]).unwrap();
    assert_eq!(s.discard_staged(e), Err(StorageError::ForeignHandle));

    // None of that disturbed the committed state.
    put_body(&s, "after", "advance", "1");
    crate::audit::check_retirement(&sim).unwrap();
    assert_eq!(read_all(&mut s.begin().unwrap(), &cc).unwrap(), c_bytes);
}

// ----- walkers outside views -------------------------------------------------------------------

/// Parks the next tree walk of `outcome` or `height` until `go` is sent; `parked` signals it.
fn park_next_walk(s: &NativeStore) -> (std::sync::mpsc::Receiver<()>, std::sync::mpsc::Sender<()>) {
    let (parked_tx, parked) = std::sync::mpsc::channel();
    let (go, go_rx) = std::sync::mpsc::channel::<()>();
    assert!(
        s.inner
            .set_walk_hook_for_test(Some(Box::new(move || {
                parked_tx.send(()).unwrap();
                go_rx.recv().unwrap();
            })))
            .is_none()
    );
    (parked, go)
}

fn pages_reused(s: &NativeStore) -> u64 {
    s.inner
        .counters
        .pages_reused
        .load(std::sync::atomic::Ordering::SeqCst)
}

#[test]
fn outcome_and_stats_walk_their_own_state_while_commits_reuse_pages() {
    let sim = SimIo::new();
    // No node cache: every walk reads its pages from the file.
    let s = NativeStore {
        inner: Inner::create(
            Box::new(sim.clone()),
            STORE,
            crate::engine::Options {
                cache_nodes: 0,
                ..Default::default()
            },
        )
        .unwrap(),
    };
    churn(&s, None, 0..LIVE * 4);
    let receipt = cce_core::storage::CommitReceipt {
        attempt: AttemptId::new(*b"c3").unwrap(),
        position: cce_core::storage::HistoryPosition(4),
    };

    // outcome(): parked after choosing its state, while 48 commits reuse pages.
    let (parked, go) = park_next_walk(&s);
    let reused = pages_reused(&s);
    let answer = std::thread::scope(|sc| {
        let walker = sc.spawn(|| s.outcome(&receipt.attempt));
        parked.recv().unwrap();
        churn(&s, None, LIVE * 4..LIVE * 10);
        go.send(()).unwrap();
        walker.join().unwrap()
    });
    assert!(
        pages_reused(&s) > reused,
        "no page was reused while the walk was parked"
    );
    assert_eq!(
        answer,
        Ok(cce_core::storage::AttemptOutcome::Committed(
            receipt.clone()
        ))
    );

    // stats(): its height walk, parked the same way.
    let (parked, go) = park_next_walk(&s);
    let reused = pages_reused(&s);
    let height = std::thread::scope(|sc| {
        let walker = sc.spawn(|| s.stats().map(|st| st.tree_height));
        parked.recv().unwrap();
        churn(&s, None, LIVE * 10..LIVE * 16);
        go.send(()).unwrap();
        walker.join().unwrap()
    });
    assert!(
        pages_reused(&s) > reused,
        "no page was reused while the walk was parked"
    );
    assert!(matches!(height, Ok(h) if h >= 1), "{height:?}");
}

#[test]
fn reusable_space_never_overlaps_a_published_or_pinned_state() {
    let sim = SimIo::new();
    let s = sim_create(&sim);
    let mut held: Vec<crate::NativeView> = Vec::new();
    let (mut max_pages, mut max_extents) = (0, 0);
    for i in 0..80u32 {
        let id = format!("R{}", i % 4);
        if i % 7 == 6 && s.begin().unwrap().context(&cid(&id)).unwrap().is_some() {
            remove(&s, &format!("t{i}"), &id);
        } else {
            put(
                &s,
                &format!("t{i}"),
                &id,
                &payload(i as u8, 3000 + (i as usize % 3) * 1000),
            );
        }
        if i % 11 == 3 {
            held.push(s.begin().unwrap());
        }
        if i % 17 == 9 && !held.is_empty() {
            held.remove(0);
        }

        // Independently: nothing the engine would reuse now is reached by either publication
        // slot or by any live view's state.
        let (pages, extents) = s.inner.reusable_for_test().unwrap();
        let reusable: Extents = extents.iter().copied().collect();
        let mut states = published(&sim).unwrap();
        states.extend(held.iter().map(|v| v.snap.desc));
        for d in &states {
            let r = reach(&sim, d).unwrap();
            for p in &pages {
                assert!(
                    !r.pages.contains(p),
                    "commit {i}: reusable page {p} is reached by generation {}",
                    d.generation
                );
            }
            let reached: Extents = content_extents(&r)
                .union(&r.history_extents)
                .copied()
                .collect();
            assert!(
                !overlaps(&reusable, &reached),
                "commit {i}: reusable content overlaps generation {}",
                d.generation
            );
        }
        crate::audit::check_retirement(&sim).unwrap_or_else(|e| panic!("commit {i}: {e}"));
        max_pages = max_pages.max(pages.len());
        max_extents = max_extents.max(extents.len());
    }
    assert!(
        max_pages > 0 && max_extents > 0,
        "nothing was ever reusable"
    );
}

// ----- crash matrix across retiring and reusing commits ----------------------------------------

/// One step of the matrix script: each stages (possibly into reused space) and/or commits.
fn matrix_step(s: &NativeStore, k: usize) -> Result<(), String> {
    let size = CONTENT_CHUNK + 300;
    let commit = |cs: ChangeSet| {
        s.commit(
            AttemptId::new(format!("m{k}").into_bytes()).unwrap(),
            s.begin().map_err(|e| format!("{e:?}"))?,
            cs,
        )
        .map(|_| ())
        .map_err(|e| format!("{e:?}"))
    };
    let put_staged = |id: &str, bytes: Vec<u8>, removal| -> Result<(), String> {
        let staged = s
            .stage_content(&mut &bytes[..])
            .map_err(|e| format!("{e:?}"))?;
        commit(ChangeSet {
            changes: vec![Change::PutContext {
                id: cid(id),
                expect: Precondition::Any,
                body: id.as_bytes().to_vec(),
                content: Some(0),
            }],
            content: vec![ContentBinding::from(&staged)],
            removal,
            ..ChangeSet::default()
        })
    };
    match k {
        // Replace K's content: retires K's old content, reuses X's retired space.
        0 => put_staged(
            "K",
            payload(70, size),
            cce_core::storage::RemovalMode::PreserveViews,
        ),
        // Remove M0 (absent at first: put it) and retire.
        1 => put_staged(
            "M",
            payload(71, 5000),
            cce_core::storage::RemovalMode::PreserveViews,
        ),
        2 => commit(ChangeSet {
            changes: vec![Change::RemoveContext {
                id: cid("M"),
                expect: Precondition::Any,
            }],
            ..ChangeSet::default()
        }),
        // Requiring quiescence, with content that reuses retired space.
        3 => put_staged(
            "Q",
            payload(72, size),
            cce_core::storage::RemovalMode::RequireQuiescence,
        ),
        _ => unreachable!(),
    }
}

const MATRIX_STEPS: usize = 4;

/// Records K, M and Q with their content bytes.
fn matrix_state(s: &NativeStore) -> Vec<String> {
    let mut v = s.begin().unwrap();
    ["K", "M", "Q"]
        .iter()
        .map(|id| match v.context(&cid(id)).unwrap() {
            None => format!("{id} absent"),
            Some(r) => {
                let bytes = read_all(&mut v, &r.content.unwrap()).unwrap();
                format!(
                    "{id} rev {} {} bytes sum {}",
                    r.revision.0,
                    bytes.len(),
                    bytes.iter().map(|b| *b as u64).sum::<u64>()
                )
            }
        })
        .collect()
}

#[test]
fn every_cut_of_every_retiring_or_reusing_step_recovers_and_continues() {
    let size = CONTENT_CHUNK + 300;
    // Probe: operations of each step and the state before and after it.
    let probe = SimIo::new();
    let (s, _, _) = with_reusable_content(&probe, size);
    let mut ops = Vec::new();
    let mut states = vec![matrix_state(&s)];
    for k in 0..MATRIX_STEPS {
        let before = probe.ops();
        matrix_step(&s, k).unwrap();
        ops.push(probe.ops() - before);
        states.push(matrix_state(&s));
    }
    assert!(
        s.stats().unwrap().content_bytes_reused > 0,
        "the script reused no content"
    );
    let fin = states[MATRIX_STEPS].clone();
    drop(s);

    let mut checked = 0;
    for k in 0..MATRIX_STEPS {
        for cut in 0..=ops[k] {
            let sim = SimIo::new();
            let trace = Trace::default();
            let (s, _, _) = reusable_content_in(traced_create(&sim, &trace), &sim, size);
            for j in 0..k {
                matrix_step(&s, j).unwrap();
            }
            trace.lock().unwrap().clear();
            sim.crash_after(cut);
            let acked = matrix_step(&s, k).is_ok();
            drop(s);
            for (survived, img) in crash_images(&sim, 3) {
                let what = format!("matrix step {k} cut {cut}, {survived}");
                repro_case(&what, &img, &trace, || {
                    let rec = NativeStore {
                        inner: Inner::open(Box::new(img.clone()), Default::default()).unwrap(),
                    };
                    crate::audit::check_retirement(&img).unwrap_or_else(|e| panic!("{what}: {e}"));
                    let got = matrix_state(&rec);
                    assert!(got == states[k] || got == states[k + 1], "{what}: {got:?}");
                    if acked {
                        assert_eq!(got, states[k + 1], "{what}: acknowledged step lost");
                    }
                    // The recovered store carries on: finish the script, auditing each step.
                    let from = if got == states[k] { k } else { k + 1 };
                    for j in from..MATRIX_STEPS {
                        matrix_step(&rec, j).unwrap_or_else(|e| panic!("{what}: step {j}: {e}"));
                        crate::audit::check_retirement(&img)
                            .unwrap_or_else(|e| panic!("{what}: after step {j}: {e}"));
                    }
                    assert_eq!(matrix_state(&rec), fin, "{what}: final state");
                });
                checked += 1;
            }
        }
    }
    // Five power-loss choices and a process exit per cut.
    assert_eq!(checked, ops.iter().map(|n| (n + 1) * 6).sum::<u64>());
}

/// A store whose next commit both reuses retired tree pages and needs more pages than are
/// retired: the fixed live set churned, then one commit adding many large records.
fn growing_fixture(sim: &SimIo) -> NativeStore {
    let s = sim_create(sim);
    churn(&s, None, 0..LIVE * 6);
    s
}

fn growing_commit(s: &NativeStore) -> Result<cce_core::storage::CommitReceipt, CommitError> {
    let changes = (0..24)
        .map(|i| Change::PutContext {
            id: cid(&format!("G{i:02}")),
            expect: Precondition::Any,
            body: vec![i as u8; 900],
            content: None,
        })
        .collect();
    s.commit(
        AttemptId::new(*b"grow").unwrap(),
        s.begin().unwrap(),
        ChangeSet {
            changes,
            ..ChangeSet::default()
        },
    )
}

fn grown_records(s: &NativeStore) -> usize {
    let mut v = s.begin().unwrap();
    (0..24)
        .filter(|i| v.context(&cid(&format!("G{i:02}"))).unwrap().is_some())
        .count()
}

#[test]
fn a_failure_at_any_operation_of_a_commit_that_grows_metadata_and_reuses_pages_is_harmless() {
    // Probe: the commit reuses retired pages, extends the page cursor and retires pages.
    let probe = SimIo::new();
    let s = growing_fixture(&probe);
    let (before, reused) = (desc(&s), pages_reused(&s));
    let start = probe.ops();
    growing_commit(&s).unwrap();
    let total = probe.ops() - start;
    let after = desc(&s);
    assert!(
        pages_reused(&s) > reused,
        "the commit reused no retired page"
    );
    assert!(
        after.next_page > before.next_page,
        "the commit did not grow the metadata"
    );
    assert!(
        reach(&probe, &after)
            .unwrap()
            .retired
            .values()
            .any(|(g, _)| *g == after.generation),
        "the commit retired no page"
    );
    drop(s);

    let mut outcomes = std::collections::BTreeMap::new();
    for n in 1..=total {
        let sim = SimIo::new();
        let s = growing_fixture(&sim);
        sim.fail_nth_op(n);
        let r = growing_commit(&s);
        let kind = match &r {
            Ok(_) => panic!("op {n}: a commit with a failed operation succeeded"),
            Err(CommitError::Indeterminate(_)) => "indeterminate",
            Err(CommitError::Rejected(_)) => "rejected",
            Err(e) => panic!("op {n}: unexpected {e:?}"),
        };
        *outcomes.entry(kind).or_insert(0) += 1;
        drop(s);
        // A fresh process (the failed write may or may not have reached the files).
        for (what, img) in [
            ("process exit", sim.after_process_exit()),
            ("power loss", sim.after_crash(crate::sim::Persist::None)),
        ] {
            let rec = NativeStore {
                inner: Inner::open(Box::new(img.clone()), Default::default()).unwrap(),
            };
            crate::audit::check_retirement(&img).unwrap_or_else(|e| panic!("op {n} {what}: {e}"));
            let grown = grown_records(&rec);
            assert!(
                grown == 0 || (grown == 24 && kind == "indeterminate"),
                "op {n} {what}: {grown} of 24 records after a {kind} commit"
            );
            // Reuse keeps working on the recovered store.
            let reused = pages_reused(&rec);
            churn(&rec, Some(&img), LIVE * 6..LIVE * 9);
            assert!(
                pages_reused(&rec) > reused,
                "op {n} {what}: no reuse after recovery"
            );
        }
    }
    // Both kinds of failure were exercised: before the publication record (rejected) and at it.
    assert!(
        outcomes.contains_key("rejected") && outcomes.contains_key("indeterminate"),
        "{outcomes:?}"
    );
}
