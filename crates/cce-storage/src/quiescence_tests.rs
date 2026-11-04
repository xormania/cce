//! Quiescence-required commits: a commit carrying `RemovalMode::RequireQuiescence` commits only
//! while no other view or content reader of the store is live, and otherwise changes nothing.

use std::io::Read;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};

use cce_core::storage::{
    AttemptId, AttemptOutcome, Change, ChangeSet, CommitError, ContentBinding, ContextId,
    Precondition, ReadView, RemovalMode, StagedContent, StorageEngine, StorageError,
};

use crate::NativeStore;
use crate::engine::Inner;
use crate::format::StoreId;
use crate::sim::SimIo;

const STORE: StoreId = StoreId(*b"quiescence-tests");
/// Distinctive record id: refusal messages must never contain identifiers.
const TARGET: &str = "secret-target";

fn cid(s: &str) -> ContextId {
    ContextId::new(s.as_bytes().to_vec()).unwrap()
}

fn attempt(s: &str) -> AttemptId {
    AttemptId::new(s.as_bytes().to_vec()).unwrap()
}

fn sim_create(sim: &SimIo) -> NativeStore {
    NativeStore {
        inner: Inner::create(Box::new(sim.clone()), STORE, Default::default()).unwrap(),
    }
}

/// A store whose record `TARGET` holds `bytes` as content.
fn with_target(sim: &SimIo, bytes: &[u8]) -> NativeStore {
    let s = sim_create(sim);
    let staged = s.stage_content(&mut &bytes[..]).unwrap();
    s.commit(
        attempt("seed"),
        s.begin().unwrap(),
        ChangeSet {
            changes: vec![Change::PutContext {
                id: cid(TARGET),
                expect: Precondition::Absent,
                body: b"target".to_vec(),
                content: Some(0),
            }],
            content: vec![ContentBinding::from(&staged)],
            ..ChangeSet::default()
        },
    )
    .unwrap();
    s
}

/// The quiescent request: remove `TARGET` and put `B` with the staged content.
fn request(staged: &StagedContent, removal: RemovalMode) -> ChangeSet {
    ChangeSet {
        changes: vec![
            Change::RemoveContext {
                id: cid(TARGET),
                expect: Precondition::Any,
            },
            Change::PutContext {
                id: cid("B"),
                expect: Precondition::Absent,
                body: b"B".to_vec(),
                content: Some(0),
            },
        ],
        content: vec![ContentBinding::from(staged)],
        removal,
        ..ChangeSet::default()
    }
}

/// Prepares the request through the ordinary read path (a fresh basis view that reads what it
/// removes) and commits it requiring quiescence.
fn commit_quiescent(
    s: &NativeStore,
    staged: &StagedContent,
) -> Result<cce_core::storage::CommitReceipt, CommitError> {
    let mut basis = s.begin().unwrap();
    assert!(basis.context(&cid(TARGET)).unwrap().is_some());
    s.commit(
        attempt("q"),
        basis,
        request(staged, RemovalMode::RequireQuiescence),
    )
}

/// Records `TARGET` and `B` (with B's content bytes) through a fresh view.
fn state(s: &NativeStore) -> (bool, Option<Vec<u8>>) {
    let mut v = s.begin().unwrap();
    let target = v.context(&cid(TARGET)).unwrap().is_some();
    let b = v.context(&cid("B")).unwrap().map(|r| {
        let mut bytes = Vec::new();
        v.content(&r.content.unwrap())
            .unwrap()
            .read_to_end(&mut bytes)
            .unwrap();
        bytes
    });
    (target, b)
}

fn readers_active(r: Result<cce_core::storage::CommitReceipt, CommitError>) -> String {
    match r {
        Err(CommitError::ReadersActive(m)) => {
            assert!(!m.contains("secret"), "the refusal names a record: {m}");
            m
        }
        other => panic!("expected ReadersActive, got {other:?}"),
    }
}

#[test]
fn a_quiescent_commit_is_refused_while_another_view_lives_and_commits_once_it_is_gone() {
    let sim = SimIo::new();
    let s = with_target(&sim, b"target bytes");
    let staged = s.stage_content(&mut &b"bee"[..]).unwrap();
    let other = s.begin().unwrap();
    let ops = sim.ops();
    let log = s.inner.conflict_log_for_test();

    let m = readers_active(commit_quiescent(&s, &staged));
    assert!(
        m.contains('1'),
        "the refusal does not count the live reader: {m}"
    );
    // Nothing happened: no write or sync, no outcome, no change, consistent retirement.
    assert_eq!(sim.ops(), ops, "the refused commit wrote or synced");
    assert_eq!(
        s.inner.conflict_log_for_test(),
        log,
        "the refused commit changed the conflict log"
    );
    assert_eq!(s.outcome(&attempt("q")).unwrap(), AttemptOutcome::Absent);
    assert_eq!(state(&s), (true, None));
    crate::audit::check_retirement(&sim).unwrap();

    // Once the other view is gone, the same request, with the same staged handle, commits.
    drop(other);
    let r = commit_quiescent(&s, &staged).unwrap();
    assert_eq!(
        s.outcome(&attempt("q")).unwrap(),
        AttemptOutcome::Committed(r)
    );
    assert_eq!(state(&s), (false, Some(b"bee".to_vec())));
    crate::audit::check_retirement(&sim).unwrap();
}

#[test]
fn a_reader_opened_from_the_basis_view_blocks_but_the_basis_itself_does_not() {
    let sim = SimIo::new();
    let s = with_target(&sim, b"target bytes");
    let staged = s.stage_content(&mut &b"bee"[..]).unwrap();

    // The basis view opens a content reader before it is submitted with the request.
    let mut basis = s.begin().unwrap();
    let c = basis
        .context(&cid(TARGET))
        .unwrap()
        .unwrap()
        .content
        .unwrap();
    let mut reader = basis.content(&c).unwrap();
    let m = readers_active(s.commit(
        attempt("q"),
        basis,
        request(&staged, RemovalMode::RequireQuiescence),
    ));
    // Exactly one reader counted: the content reader, not the consumed basis view.
    assert!(m.starts_with('1'), "{m}");
    // The reader still reads its content whole.
    let mut bytes = Vec::new();
    reader.read_to_end(&mut bytes).unwrap();
    assert_eq!(bytes, b"target bytes");

    drop(reader);
    commit_quiescent(&s, &staged).unwrap();
    assert_eq!(state(&s), (false, Some(b"bee".to_vec())));
}

#[test]
fn a_view_registering_during_a_quiescent_commit_gets_the_new_state() {
    let sim = SimIo::new();
    let s = with_target(&sim, b"target bytes");
    let staged = s.stage_content(&mut &b"bee"[..]).unwrap();
    let before = s.inner.snapshot().unwrap().desc.generation;

    // Inside the commit, a thread begins a view; the hook returns only once that registration is
    // waiting, so it certainly overlaps the commit.
    type Registration = (std::thread::JoinHandle<(u64, bool)>, bool);
    let joined: Arc<Mutex<Option<Registration>>> = Arc::new(Mutex::new(None));
    let (inner, slot) = (s.inner.clone(), joined.clone());
    assert!(
        s.inner
            .set_reuse_hook_for_test(Some(Box::new(move || {
                let reg = inner.clone();
                let handle = std::thread::spawn(move || {
                    let mut v = crate::NativeView::new(&reg).unwrap();
                    let target = v.context(&cid(TARGET)).unwrap().is_some();
                    (v.snap.desc.generation, target)
                });
                // Return once the registration waits for this commit, or has already finished.
                let until = std::time::Instant::now() + std::time::Duration::from_secs(10);
                let waited = loop {
                    if inner
                        .waiting_registrations_for_test()
                        .load(Ordering::SeqCst)
                        > 0
                    {
                        break true;
                    }
                    if handle.is_finished() || std::time::Instant::now() > until {
                        break false;
                    }
                    std::thread::yield_now();
                };
                *slot.lock().unwrap() = Some((handle, waited));
            })))
            .is_none()
    );
    let r = commit_quiescent(&s, &staged).unwrap();
    let handle = joined
        .lock()
        .unwrap()
        .take()
        .expect("the hook did not run inside the commit");
    let (handle, waited) = handle;
    let (generation, target_visible) = handle.join().unwrap();
    let after = s.inner.snapshot().unwrap().desc.generation;
    assert_eq!(after, before + 1);
    assert_eq!(
        generation, after,
        "a view registered during the quiescent commit read the old state"
    );
    assert!(!target_visible);
    assert!(
        waited,
        "the registration did not wait for the quiescent commit"
    );
    assert_eq!(
        s.outcome(&attempt("q")).unwrap(),
        AttemptOutcome::Committed(r)
    );
}

#[test]
fn another_process_cannot_hold_a_view_while_the_store_is_owned() {
    // Quiescence counts this process's views only: no other process can hold the store open,
    // read-write or read-only, while it is owned.
    let dir = std::env::temp_dir().join(format!("cce-quiescence-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let owner = NativeStore::create(&dir, *b"quiescence-owner").unwrap();
    assert!(matches!(
        NativeStore::open_read_only(&dir),
        Err(StorageError::Busy(_))
    ));
    assert!(matches!(
        NativeStore::open(&dir),
        Err(StorageError::Busy(_))
    ));
    let staged = owner.stage_content(&mut &b"bee"[..]).unwrap();
    owner
        .commit(
            attempt("q"),
            owner.begin().unwrap(),
            ChangeSet {
                changes: vec![Change::PutContext {
                    id: cid("B"),
                    expect: Precondition::Absent,
                    body: b"B".to_vec(),
                    content: Some(0),
                }],
                content: vec![ContentBinding::from(&staged)],
                removal: RemovalMode::RequireQuiescence,
                ..ChangeSet::default()
            },
        )
        .unwrap();
    drop(owner);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn the_removal_mode_is_part_of_the_request() {
    let sim = SimIo::new();
    let s = with_target(&sim, b"target bytes");
    let staged = s.stage_content(&mut &b"bee"[..]).unwrap();
    s.commit(
        attempt("m"),
        s.begin().unwrap(),
        request(&staged, RemovalMode::PreserveViews),
    )
    .unwrap();
    let r = s.commit(
        attempt("m"),
        s.begin().unwrap(),
        request(&staged, RemovalMode::RequireQuiescence),
    );
    assert_eq!(r, Err(CommitError::AttemptMismatch));
}

#[test]
fn the_requests_own_basis_view_is_released_before_its_commit_runs() {
    let sim = SimIo::new();
    let s = with_target(&sim, b"target bytes");
    let staged = s.stage_content(&mut &b"bee"[..]).unwrap();
    // Once the commit is admitted, let the worker finish it before the submitting call returns:
    // whatever the submitter still holds then is live while the commit checks for readers.
    let inner = s.inner.clone();
    assert!(
        s.inner
            .set_admitted_hook_for_test(Some(Box::new(move || {
                let until = std::time::Instant::now() + std::time::Duration::from_secs(10);
                while inner.pending_attempts_for_test() > 0 {
                    assert!(std::time::Instant::now() < until, "the commit never ran");
                    std::thread::yield_now();
                }
            })))
            .is_none()
    );
    let r = commit_quiescent(&s, &staged);
    assert!(
        r.is_ok(),
        "the request's own basis view counted as a live reader: {r:?}"
    );
    assert_eq!(state(&s), (false, Some(b"bee".to_vec())));
}

/// Runs `walk` with a quiescent commit submitted from inside its tree walk; returns the commit's
/// result and the walk's own answer.
fn quiescent_commit_during_walk<T>(
    s: &NativeStore,
    walk: impl FnOnce(&NativeStore) -> T,
) -> (Result<cce_core::storage::CommitReceipt, CommitError>, T) {
    let staged = s.stage_content(&mut &b"bee"[..]).unwrap();
    let copy = StagedContent {
        content: staged.content,
        token: staged.token.clone(),
    };
    let (tx, rx) = std::sync::mpsc::channel();
    let inner = s.inner.clone();
    assert!(
        s.inner
            .set_walk_hook_for_test(Some(Box::new(move || {
                let s = NativeStore { inner };
                tx.send(commit_quiescent(&s, &copy)).unwrap();
            })))
            .is_none()
    );
    let answer = walk(s);
    let committed = rx.try_recv().expect("the walk did not reach its hook");
    (committed, answer)
}

#[test]
fn an_outcome_walk_in_progress_does_not_block_a_quiescent_commit() {
    let s = with_target(&SimIo::new(), b"target bytes");
    let (committed, answer) = quiescent_commit_during_walk(&s, |s| s.outcome(&attempt("seed")));
    assert!(
        committed.is_ok(),
        "an internal outcome walk counted as a live reader: {committed:?}"
    );
    assert!(
        matches!(answer, Ok(AttemptOutcome::Committed(_))),
        "{answer:?}"
    );
}

#[test]
fn a_statistics_walk_in_progress_does_not_block_a_quiescent_commit() {
    let s = with_target(&SimIo::new(), b"target bytes");
    let (committed, answer) =
        quiescent_commit_during_walk(&s, |s| s.stats().map(|st| st.tree_height));
    assert!(
        committed.is_ok(),
        "an internal statistics walk counted as a live reader: {committed:?}"
    );
    assert!(matches!(answer, Ok(h) if h >= 1), "{answer:?}");
}
