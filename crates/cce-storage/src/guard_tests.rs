//! Opens guarded by receipt information the caller retained: identity and a minimum logical
//! history position, checked before the open touches the store.

use cce_core::storage::{
    AttemptId, Change, ChangeSet, HistoryPosition, Precondition, StorageEngine, StorageError,
};

use crate::engine::{Inner, Options};
use crate::format::StoreId;
use crate::io::FileId;
use crate::sim::SimIo;
use crate::{NativeStore, ReceiptGuard};

const ID: [u8; 16] = *b"guard-test-store";

fn commit(s: &NativeStore, n: u32) -> Result<(), cce_core::storage::CommitError> {
    s.commit(
        AttemptId::new(format!("attempt-{n}").into_bytes()).unwrap(),
        s.begin().unwrap(),
        ChangeSet {
            changes: vec![Change::PutContext {
                id: cce_core::storage::ContextId::new(format!("c{n}").into_bytes()).unwrap(),
                expect: Precondition::Any,
                body: b"body".to_vec(),
                content: None,
            }],
            ..ChangeSet::default()
        },
    )
    .map(|_| ())
}

/// A store with `commits` commits, released.
fn store_with(commits: u32) -> SimIo {
    let sim = SimIo::new();
    let s = NativeStore {
        inner: Inner::create(Box::new(sim.clone()), StoreId(ID), Options::default()).unwrap(),
    };
    for n in 1..=commits {
        commit(&s, n).unwrap();
    }
    drop(s);
    sim.release();
    sim
}

/// Two commits durable, and a third whose publication record was written but not synced when
/// its process exited: an open without a guard makes it durable.
fn unsynced_third() -> SimIo {
    let probe = store_with(2);
    let p = NativeStore {
        inner: Inner::open(Box::new(probe.clone()), Options::default()).unwrap(),
    };
    let ops = probe.ops();
    commit(&p, 3).unwrap();
    let per_commit = probe.ops() - ops;
    drop(p);

    let sim = store_with(2);
    let s = NativeStore {
        inner: Inner::open(Box::new(sim.clone()), Options::default()).unwrap(),
    };
    // The last operation of a commit is the publication barrier.
    sim.crash_after(per_commit - 1);
    assert!(commit(&s, 3).is_err());
    drop(s);
    sim.after_process_exit()
}

fn guard(store_id: [u8; 16], min: u64) -> Option<ReceiptGuard> {
    Some(ReceiptGuard {
        store_id,
        min_history_position: HistoryPosition(min),
    })
}

/// Opens `sim` read-write or read-only with `guard`; returns the published history position and
/// generation.
fn open(
    sim: &SimIo,
    read_only: bool,
    guard: Option<ReceiptGuard>,
) -> Result<(u64, u64), StorageError> {
    let opts = Options {
        guard,
        ..Options::default()
    };
    let opened = if read_only {
        Inner::open_read_only(Box::new(sim.clone()), opts)
    } else {
        Inner::open(Box::new(sim.clone()), opts)
    };
    let published = opened.and_then(|inner| inner.snapshot().map(|s| s.desc));
    // The opening "process" ends either way.
    sim.release();
    published.map(|d| (d.history_pos, d.generation))
}

fn images(sim: &SimIo) -> Vec<Option<(Vec<u8>, Vec<u8>)>> {
    [FileId::Meta, FileId::Content]
        .into_iter()
        .map(|f| sim.file_images_for_test(f))
        .collect()
}

fn refused(r: Result<(u64, u64), StorageError>, says: &str) {
    match r {
        Err(StorageError::GuardRefused(why)) => assert!(why.contains(says), "{why}"),
        other => panic!("expected a guard refusal mentioning {says:?}, got {other:?}"),
    }
}

#[test]
fn a_guard_at_or_below_the_published_position_opens_normally() {
    let sim = store_with(2);
    for read_only in [false, true] {
        assert_eq!(open(&sim, read_only, None).unwrap().0, 2, "no guard");
        for min in 0..=2 {
            assert_eq!(
                open(&sim, read_only, guard(ID, min)).unwrap().0,
                2,
                "minimum {min}"
            );
        }
    }
}

#[test]
fn a_guard_naming_another_store_is_refused_and_touches_nothing() {
    let sim = unsynced_third();
    let before = images(&sim);
    for read_only in [false, true] {
        refused(
            open(&sim, read_only, guard(*b"some-other-store", 0)),
            "identity",
        );
        assert!(
            images(&sim) == before,
            "read_only {read_only}: the refused open changed the store"
        );
    }
}

#[test]
fn a_store_below_the_retained_position_is_refused_and_untouched() {
    let sim = unsynced_third();
    let before = images(&sim);
    for read_only in [false, true] {
        refused(open(&sim, read_only, guard(ID, 4)), "below");
        assert!(
            images(&sim) == before,
            "read_only {read_only}: the refused open changed the store"
        );
    }
    // Positive control: the image holds unsynced bytes that an ordinary open makes durable.
    let control = sim.after_process_exit();
    assert_eq!(open(&control, false, guard(ID, 3)).unwrap().0, 3);
    assert!(
        images(&control) != before,
        "an ordinary open synced nothing"
    );
    // A store restored from before the third commit is refused for a receipt of it.
    refused(open(&store_with(2), false, guard(ID, 3)), "below");
}

#[test]
fn the_guard_compares_the_history_position_not_the_generation() {
    // Creating a store publishes a generation at position 0: after two commits the generation is
    // three and the position two.
    let sim = store_with(2);
    assert_eq!(open(&sim, true, None).unwrap(), (2, 3));
    refused(open(&sim, false, guard(ID, 3)), "below");
    refused(open(&sim, true, guard(ID, 3)), "below");
}
