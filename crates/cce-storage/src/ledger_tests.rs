//! The reuse pool's accounting against the persisted retirement records: after every step of a
//! workload that commits, removes, stages into retired space, consumes part of a record,
//! discards and reopens, each record is exactly one free extent or reservation and each free
//! extent or reservation is exactly one record. Two planted mismatches show the check finds both
//! directions.

use cce_core::storage::{Change, ChangeSet, ContentBinding, Precondition, ReadView, StorageEngine};

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

/// Content-file extents the published state reaches.
fn reached(sim: &SimIo, s: &NativeStore) -> Vec<(u64, u64)> {
    let r = crate::audit::reach(sim, &s.inner.snapshot().unwrap().desc).unwrap();
    r.manifests
        .iter()
        .chain(&r.content_segments)
        .copied()
        .collect()
}

/// A discard that lands after a commit copied the queue of extents awaiting retirement, and
/// before its check: that publication retires only what it copied, the new extents stay queued,
/// and the next publication retires them with records.
#[test]
fn a_discard_during_a_commit_stays_queued_for_the_next_publication() {
    let sim = SimIo::new();
    let s = with_retired(&sim);
    // Larger than the retired space, so part of it is freshly written and must be queued.
    let p = s
        .stage_content(&mut &bytes_of(9, 6 * CONTENT_CHUNK)[..])
        .unwrap();
    let own: Vec<(u64, u64)> = s.inner.staged_extents_for_test(&p).unwrap();
    let inner = s.inner.clone();
    let discarded = std::sync::Arc::new(std::sync::Mutex::new(None));
    let seen = discarded.clone();
    s.inner.set_journal_hook_for_test(Box::new(move |_, _| {
        *seen.lock().unwrap() = Some(inner.discard_staged(p));
    }));
    advance(&s, "during");
    assert_eq!(*discarded.lock().unwrap(), Some(Ok(())), "the discard ran");
    // Each of the discarded staging's extents is free again (it was reserved space) or still
    // queued: the publication that ran meanwhile retired only what it had copied.
    let queued = s.inner.queued_for_test();
    let free: Vec<(u64, u64)> = s.inner.reusable_for_test().unwrap().1;
    for x in &own {
        assert!(
            queued.contains(x) || free.iter().any(|f| f.0 == x.0),
            "extent {x:?} of the discarded staging is neither free nor queued (queue {queued:?})"
        );
    }
    assert!(
        own.iter().any(|x| queued.contains(x)),
        "nothing of the discarded staging stayed queued: {queued:?}"
    );
    advance(&s, "after");
    ledger(&s, "after the next publication");
    let retired = crate::audit::reach(&sim, &s.inner.snapshot().unwrap().desc)
        .unwrap()
        .retired_extents;
    for x in own.iter().filter(|x| queued.contains(x)) {
        assert_eq!(
            retired.get(&x.0).map(|(_, len)| *len),
            Some(x.1),
            "queued extent {x:?} was not retired by the next publication"
        );
    }
    assert!(
        s.inner.queued_for_test().is_empty(),
        "the queue was not emptied"
    );
}

/// Between a commit's publication and its update of the pool, another staging is discarded
/// and a new one takes space: the commit frees what it retired, the discarded reservations are
/// free again, the new staging keeps its own, nothing is accounted twice, and every content
/// reads back exactly.
#[test]
fn staging_and_discard_between_publication_and_the_pool_update_keep_both_changes() {
    let sim = SimIo::new();
    let s = with_retired(&sim);
    put_content(&s, "k", "K", &bytes_of(10, CONTENT_CHUNK + 700));
    advance(&s, "k-advance-1");
    advance(&s, "k-advance-2");
    let p = s
        .stage_content(&mut &bytes_of(11, CONTENT_CHUNK)[..])
        .unwrap();
    let (held_p, _) = s.inner.reserved_bytes_for_test();
    assert!(held_p > 0, "the discarded staging reserved nothing");
    // The interleaved commit also binds a staging of its own, so it consumes reservations while
    // the new staging holds some.
    let r = s
        .stage_content(&mut &bytes_of(13, CONTENT_CHUNK / 2)[..])
        .unwrap();
    assert!(
        s.inner.reserved_bytes_for_test().0 > held_p,
        "the committed staging reserved nothing"
    );
    let k_before: Vec<(u64, u64)> = reached(&sim, &s);
    let inner = s.inner.clone();
    let q_bytes = bytes_of(12, CONTENT_CHUNK + 5);
    let q_data = q_bytes.clone();
    let out = std::sync::Arc::new(std::sync::Mutex::new(None));
    let put = out.clone();
    s.inner.set_apply_hook_for_test(Box::new(move || {
        let d = inner.discard_staged(p);
        let q = inner.stage_checked(&mut &q_data[..], None);
        *put.lock().unwrap() = Some((d, q));
    }));
    commit(
        &s,
        "k-removed",
        ChangeSet {
            changes: vec![
                Change::RemoveContext {
                    id: cid("K"),
                    expect: Precondition::Any,
                },
                put_context("R", Precondition::Any, b"R", Some(0)),
            ],
            content: vec![ContentBinding::from(&r)],
            ..ChangeSet::default()
        },
    );
    let (d, q) = out.lock().unwrap().take().expect("the hook ran");
    assert_eq!(d, Ok(()), "the discard between publication and pool update");
    let q = q.expect("the staging between publication and pool update");
    assert!(
        s.inner.reserved_bytes_for_test().0 > 0,
        "the new staging's reservations did not survive the commit's pool update"
    );
    // K's extents, retired by the commit, are free now (or reserved by the new staging).
    let k_gone: Vec<(u64, u64)> = k_before
        .iter()
        .copied()
        .filter(|x| !reached(&sim, &s).contains(x))
        .collect();
    assert!(!k_gone.is_empty(), "the commit retired nothing");
    ledger(&s, "after the interleaved commit");
    commit(
        &s,
        "q",
        ChangeSet {
            changes: vec![put_context("Q", Precondition::Any, b"Q", Some(0))],
            content: vec![ContentBinding::from(&q)],
            ..ChangeSet::default()
        },
    );
    let c = s
        .begin()
        .unwrap()
        .context(&cid("Q"))
        .unwrap()
        .unwrap()
        .content
        .unwrap();
    let mut got = Vec::new();
    std::io::Read::read_to_end(&mut s.begin().unwrap().content(&c).unwrap(), &mut got).unwrap();
    assert_eq!(
        got, q_bytes,
        "the staging taken between publication and pool update"
    );
    advance(&s, "final");
    assert_eq!(crate::audit::check_retirement(&sim), Ok(()));
    drop(s);
    sim.release();
    let s = open_on(Box::new(sim.clone()), Default::default()).unwrap();
    ledger(&s, "reopen");
}

/// The check refuses to judge a store while a staging is taking space: what it has taken is
/// neither free nor reserved until it registers.
#[test]
fn the_check_refuses_while_a_staging_is_taking_space() {
    let sim = SimIo::new();
    let s = with_retired(&sim);
    struct CheckMidway<'a> {
        bytes: Vec<u8>,
        at: usize,
        s: &'a NativeStore,
        seen: Option<Result<(), String>>,
    }
    impl std::io::Read for CheckMidway<'_> {
        fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
            if self.at == CONTENT_CHUNK && self.seen.is_none() {
                self.seen = Some(self.s.inner.ledger_check_for_test());
            }
            let end = self
                .bytes
                .len()
                .min(self.at + out.len())
                .min(if self.at < CONTENT_CHUNK {
                    CONTENT_CHUNK
                } else {
                    usize::MAX
                });
            let n = end - self.at;
            out[..n].copy_from_slice(&self.bytes[self.at..end]);
            self.at = end;
            Ok(n)
        }
    }
    let mut source = CheckMidway {
        bytes: bytes_of(8, 2 * CONTENT_CHUNK),
        at: 0,
        s: &s,
        seen: None,
    };
    let staged = s.stage_content(&mut source).unwrap();
    let seen = source.seen.take().expect("checked while staging");
    assert!(
        seen.as_ref().is_err_and(|m| m.contains("not quiescent")),
        "{seen:?}"
    );
    drop(staged);
    ledger(&s, "after the staging registered");
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
