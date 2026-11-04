//! The space check a commit passes before writing anything: each test plants one faulty change of
//! space ownership in an otherwise ordinary commit and expects that commit to be rejected with
//! nothing published, the store unchanged and still usable. A last test plants a conflicting pool
//! change after publication, which can no longer be rejected and fences writes instead.

use cce_core::storage::{
    Change, ChangeSet, CommitError, ContentBinding, Precondition, ReadView, StorageEngine,
    StorageError,
};

use crate::NativeStore;
use crate::engine::{CONTENT_CHUNK, JournalHook, SpaceJournal};
use crate::format::StoreId;
use crate::records::retired_page_key;
use crate::sim::SimIo;
use crate::test_support::{attempt, changes, cid, create_on, put_context};
use crate::tree::TreeTxn;

const STORE: StoreId = StoreId(*b"seal-test-store!");

fn bytes_of(seed: u8, len: usize) -> Vec<u8> {
    (0..len)
        .map(|i| (i as u8).wrapping_mul(31).wrapping_add(seed))
        .collect()
}

fn ok(s: &NativeStore, name: &str, cs: ChangeSet) {
    let r = s.commit(attempt(name), s.begin().unwrap(), cs);
    assert!(r.is_ok(), "fixture commit {name}: {r:?}");
}

fn put_content(s: &NativeStore, name: &str, id: &str, bytes: &[u8]) {
    let staged = s.stage_content(&mut &bytes[..]).unwrap();
    ok(
        s,
        name,
        ChangeSet {
            changes: vec![put_context(id, Precondition::Any, id.as_bytes(), Some(0))],
            content: vec![ContentBinding::from(&staged)],
            ..ChangeSet::default()
        },
    );
}

/// A store with a tree of more than one level, a committed content `C`, and retired content in
/// the reuse pool (content `X` committed, removed and two generations on).
fn store(sim: &SimIo) -> NativeStore {
    let s = create_on(Box::new(sim.clone()), STORE, Default::default()).unwrap();
    for batch in 0..4 {
        ok(
            &s,
            &format!("fill-{batch}"),
            changes(
                (0..50)
                    .map(|i| {
                        let id = format!("R{:03}", batch * 50 + i);
                        put_context(&id, Precondition::Any, &[i as u8; 100], None)
                    })
                    .collect(),
            ),
        );
    }
    put_content(&s, "c", "C", &bytes_of(3, CONTENT_CHUNK + 300));
    put_content(&s, "x", "X", &bytes_of(1, CONTENT_CHUNK + 300));
    ok(
        &s,
        "x-removed",
        changes(vec![Change::RemoveContext {
            id: cid("X"),
            expect: Precondition::Any,
        }]),
    );
    for k in 0..2 {
        ok(
            &s,
            &format!("advance-{k}"),
            changes(vec![put_context("advance", Precondition::Any, b"a", None)]),
        );
    }
    assert!(s.inner.reusable_content_bytes() > 0, "no retired content");
    s
}

fn generation(s: &NativeStore) -> u64 {
    s.inner.snapshot().unwrap().desc.generation
}

/// Runs `cs` with `plant` applied to its journal and tree before the seal, and expects the commit
/// to be rejected for `why` with nothing published; the store then commits normally.
fn rejected_for(why: &str, cs: ChangeSet, plant: JournalHook) {
    let sim = SimIo::new();
    let s = store(&sim);
    let before = generation(&s);
    let body_c = s.begin().unwrap().context(&cid("C")).unwrap();
    s.inner.set_journal_hook_for_test(plant);
    let r = s.commit(attempt("planted"), s.begin().unwrap(), cs);
    match &r {
        Err(CommitError::Rejected(StorageError::StoreState(m)))
            if m.starts_with("space check failed") && m.contains(why) => {}
        other => panic!("expected a rejection for {why:?}, got {other:?}"),
    }
    assert_eq!(
        generation(&s),
        before,
        "{why}: the rejected commit published"
    );
    assert_eq!(
        s.inner.ledger_check_for_test(),
        Ok(()),
        "{why}: the pool after the rejection"
    );
    assert_eq!(
        s.begin().unwrap().context(&cid("C")).unwrap(),
        body_c,
        "{why}: committed state changed"
    );
    ok(
        &s,
        "after",
        changes(vec![put_context(
            "after",
            Precondition::Any,
            b"after",
            None,
        )]),
    );
    assert_eq!(crate::audit::check_retirement(&sim), Ok(()), "{why}");
}

fn touch_one() -> ChangeSet {
    changes(vec![put_context(
        "R100",
        Precondition::Any,
        b"changed",
        None,
    )])
}

fn remove_c() -> ChangeSet {
    changes(vec![Change::RemoveContext {
        id: cid("C"),
        expect: Precondition::Any,
    }])
}

/// A page of the frontier: the new tree still refers to it.
fn referenced(t: &TreeTxn) -> crate::format::PageRef {
    *t.frontier()
        .first()
        .expect("the commit leaves unchanged subtrees")
}

#[test]
fn retiring_a_page_the_new_tree_still_refers_to_is_rejected() {
    rejected_for(
        "is still referenced",
        touch_one(),
        Box::new(|j: &mut SpaceJournal, t: &mut TreeTxn<'_>| {
            let p = referenced(t);
            j.retired_pages.push(p);
            t.put(
                retired_page_key(j.generation, p.page),
                p.incarnation.to_le_bytes().to_vec(),
            )
            .unwrap();
        }),
    );
}

#[test]
fn retiring_a_page_twice_is_rejected() {
    rejected_for(
        "retired twice",
        touch_one(),
        Box::new(|j: &mut SpaceJournal, _: &mut TreeTxn<'_>| {
            let p = j.retired_pages[0];
            j.retired_pages.push(p);
        }),
    );
}

#[test]
fn reusing_a_page_the_new_tree_still_refers_to_is_rejected() {
    rejected_for(
        "reused page",
        touch_one(),
        Box::new(|j: &mut SpaceJournal, t: &mut TreeTxn<'_>| {
            j.reused_pages.push(referenced(t).page);
        }),
    );
}

#[test]
fn a_retirement_record_the_journal_does_not_hold_is_rejected() {
    rejected_for(
        "changed outside the journal",
        touch_one(),
        Box::new(|j: &mut SpaceJournal, t: &mut TreeTxn<'_>| {
            let p = referenced(t);
            t.put(
                retired_page_key(j.generation, p.page),
                p.incarnation.to_le_bytes().to_vec(),
            )
            .unwrap();
        }),
    );
}

#[test]
fn retiring_content_a_touched_live_entry_names_is_rejected() {
    // Removing C retires its content; plant the new state's entry for C's digest as still
    // naming that content.
    rejected_for(
        "retired content",
        remove_c(),
        Box::new(|j: &mut SpaceJournal, _: &mut TreeTxn<'_>| {
            let m = *j.retired_extents.first().expect("C's content is retired");
            j.live_manifests.push(m);
        }),
    );
}

#[test]
fn retiring_content_over_other_retired_content_is_rejected() {
    rejected_for(
        "retired over each other",
        remove_c(),
        Box::new(|j: &mut SpaceJournal, _: &mut TreeTxn<'_>| {
            let x = j.retired_extents[0];
            j.remainders.push((x.0, x.1, 0));
        }),
    );
}

#[test]
fn freeing_content_over_space_the_pool_accounts_for_is_rejected() {
    // X's retired extents are free in the pool: free the first of them once more.
    let free = {
        let sim = SimIo::new();
        let s = store(&sim);
        s.inner.reusable_for_test().unwrap().1[0]
    };
    rejected_for(
        "freed over space the pool accounts for",
        remove_c(),
        Box::new(move |j: &mut SpaceJournal, _: &mut TreeTxn<'_>| {
            j.remainders.push((free.0, free.1, 0));
        }),
    );
}

/// After publication nothing can be rejected: a pool change that conflicts with what the commit
/// frees leaves that extent out and fences writes, and the published state is intact.
#[test]
fn a_conflict_found_after_publication_fences_writes() {
    let sim = SimIo::new();
    let s = store(&sim);
    let inner = s.inner.clone();
    let extents = crate::audit::reach(&sim, &s.inner.snapshot().unwrap().desc)
        .unwrap()
        .manifests;
    let planted = *extents.iter().next().unwrap();
    s.inner.set_apply_hook_for_test(Box::new(move || {
        inner.insert_free_for_test(planted.0, planted.1, 0);
    }));
    let r = s.commit(attempt("remove-all"), s.begin().unwrap(), remove_c());
    assert!(r.is_ok(), "the commit itself is published: {r:?}");
    let why = s.inner.fence_reason().unwrap_or_default();
    assert!(
        why.contains("after publishing"),
        "a conflict found after publication did not fence writes: {why:?}"
    );
    assert_eq!(s.begin().unwrap().context(&cid("C")).unwrap(), None);
}

/// A staging smaller than the retired content it reuses consumes part of a retirement record:
/// the remainder stays retired at the record's own generation and the check accepts exactly that.
#[test]
fn a_commit_consuming_part_of_a_reserved_record_passes_the_check() {
    let sim = SimIo::new();
    let s = store(&sim);
    let free = s.inner.reusable_content_bytes();
    put_content(&s, "y", "Y", &bytes_of(5, CONTENT_CHUNK + 100));
    assert!(
        s.inner.reusable_content_bytes() > 0 && s.inner.reusable_content_bytes() < free,
        "the staging did not consume part of the retired content"
    );
    ok(&s, "next", touch_one());
    assert_eq!(s.inner.fence_reason(), None);
    assert_eq!(crate::audit::check_retirement(&sim), Ok(()));
}

#[test]
fn an_ordinary_commit_passes_the_check() {
    let sim = SimIo::new();
    let s = store(&sim);
    ok(&s, "touch", touch_one());
    ok(&s, "remove", remove_c());
    put_content(&s, "y", "Y", &bytes_of(5, CONTENT_CHUNK + 300));
    assert_eq!(s.inner.fence_reason(), None);
    assert_eq!(crate::audit::check_retirement(&sim), Ok(()));
}
