//! The reuse pool's accounting against the persisted retirement records: after every step of a
//! workload that commits, removes, stages into retired space, consumes part of a record,
//! discards and reopens, each record is exactly one free extent or reservation and each free
//! extent or reservation is exactly one record. Two planted mismatches show the check finds both
//! directions.

use cce_core::storage::{Change, ChangeSet, ContentBinding, Precondition, StorageEngine};

use crate::NativeStore;
use crate::engine::CONTENT_CHUNK;
use crate::format::StoreId;
use crate::sim::SimIo;
use crate::test_support::{attempt, changes, cid, create_on, open_on, put_context};

const STORE: StoreId = StoreId(*b"ledger-test-stor");

fn bytes_of(seed: u8, len: usize) -> Vec<u8> {
    (0..len)
        .map(|i| (i as u8).wrapping_mul(31).wrapping_add(seed))
        .collect()
}

#[track_caller]
fn ledger(s: &NativeStore, step: &str) {
    assert_eq!(s.inner.ledger_check_for_test(), Ok(()), "after {step}");
}

fn commit(s: &NativeStore, name: &str, cs: ChangeSet) {
    let r = s.commit(attempt(name), s.begin().unwrap(), cs);
    assert!(r.is_ok(), "commit {name}: {r:?}");
    ledger(s, name);
}

fn put_content(s: &NativeStore, name: &str, id: &str, bytes: &[u8]) {
    let staged = s.stage_content(&mut &bytes[..]).unwrap();
    ledger(s, &format!("staging for {name}"));
    commit(
        s,
        name,
        ChangeSet {
            changes: vec![put_context(id, Precondition::Any, id.as_bytes(), Some(0))],
            content: vec![ContentBinding::from(&staged)],
            ..ChangeSet::default()
        },
    );
}

fn remove(s: &NativeStore, name: &str, id: &str) {
    commit(
        s,
        name,
        changes(vec![Change::RemoveContext {
            id: cid(id),
            expect: Precondition::Any,
        }]),
    );
}

fn advance(s: &NativeStore, name: &str) {
    commit(
        s,
        name,
        changes(vec![put_context(
            "advance",
            Precondition::Any,
            name.as_bytes(),
            None,
        )]),
    );
}

/// A store with retired content in the pool: A and B committed, removed, two generations on.
fn with_retired(sim: &SimIo) -> NativeStore {
    let s = create_on(Box::new(sim.clone()), STORE, Default::default()).unwrap();
    ledger(&s, "create");
    put_content(&s, "a", "A", &bytes_of(1, CONTENT_CHUNK + 300));
    put_content(&s, "b", "B", &bytes_of(2, 2 * CONTENT_CHUNK));
    remove(&s, "a-removed", "A");
    remove(&s, "b-removed", "B");
    advance(&s, "advance-1");
    advance(&s, "advance-2");
    assert!(s.inner.reusable_content_bytes() > 0, "nothing retired");
    s
}

#[test]
fn the_pool_matches_the_retirement_records_after_every_step() {
    let sim = SimIo::new();
    let s = with_retired(&sim);
    // Staging into retired space, consuming part of a record (a remainder stays retired).
    put_content(&s, "c", "C", &bytes_of(3, CONTENT_CHUNK + 100));
    // A staging that takes retired space and is then discarded: its reservations go back, its
    // own extents wait for the next commit.
    let d = s
        .stage_content(&mut &bytes_of(4, CONTENT_CHUNK + 50)[..])
        .unwrap();
    ledger(&s, "staging d");
    assert_eq!(s.discard_staged(d), Ok(()));
    ledger(&s, "discarding d");
    advance(&s, "advance-3");
    remove(&s, "c-removed", "C");
    advance(&s, "advance-4");
    advance(&s, "advance-5");
    put_content(&s, "e", "E", &bytes_of(5, 3 * CONTENT_CHUNK));
    // Held across a reopen of a fresh process: the pool is rebuilt from the records.
    let held = s
        .stage_content(&mut &bytes_of(6, CONTENT_CHUNK)[..])
        .unwrap();
    ledger(&s, "staging held");
    drop(held);
    drop(s);
    sim.release();
    let s = open_on(Box::new(sim.clone()), Default::default()).unwrap();
    ledger(&s, "reopen");
    put_content(&s, "f", "F", &bytes_of(7, CONTENT_CHUNK + 1));
}

#[test]
fn a_free_extent_without_a_record_is_found() {
    let sim = SimIo::new();
    let s = with_retired(&sim);
    s.inner.insert_free_for_test(u64::MAX / 2, 100, 1);
    let r = s.inner.ledger_check_for_test();
    assert!(
        r.as_ref().is_err_and(|m| m.contains("no record retires")),
        "{r:?}"
    );
}

#[test]
fn a_record_without_a_free_extent_or_reservation_is_found() {
    let sim = SimIo::new();
    let s = with_retired(&sim);
    assert!(s.inner.drop_free_for_test().is_some());
    let r = s.inner.ledger_check_for_test();
    assert!(
        r.as_ref()
            .is_err_and(|m| m.contains("neither free nor reserved")),
        "{r:?}"
    );
}
