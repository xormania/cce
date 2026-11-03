//! Engine behavior tests through the logical port (simulated I/O unless stated).

use std::io::Read;

use scientdb_kernel::storage::{
    AttemptId, Change, ChangeSet, CommitError, ContentBinding, ContextId, HistoryPosition,
    Precondition, ReadView, Revision, StorageEngine, StorageError,
};

use crate::NativeStore;
use crate::engine::Inner;
use crate::format::StoreId;
use crate::sim::SimIo;

fn store() -> NativeStore {
    NativeStore {
        inner: Inner::create(Box::new(SimIo::new()), StoreId([3; 16])).unwrap(),
    }
}

fn put(id: &str, expect: Precondition, body: &[u8], content: Option<usize>) -> Change {
    Change::PutContext {
        id: ContextId::new(id.as_bytes().to_vec()).unwrap(),
        expect,
        body: body.to_vec(),
        content,
    }
}

fn attempt(s: &str) -> AttemptId {
    AttemptId::new(s.as_bytes().to_vec()).unwrap()
}

fn commit(s: &NativeStore, a: &str, cs: ChangeSet) -> Result<u64, CommitError> {
    s.commit(attempt(a), s.begin().unwrap(), cs)
        .map(|r| r.position.0)
}

fn changes(c: Vec<Change>) -> ChangeSet {
    ChangeSet {
        changes: c,
        ..ChangeSet::default()
    }
}

fn revision(s: &NativeStore, id: &str) -> Option<Revision> {
    s.begin()
        .unwrap()
        .context(&ContextId::new(id.as_bytes().to_vec()).unwrap())
        .unwrap()
        .map(|r| r.revision)
}

#[test]
fn a_recreated_record_never_reuses_a_revision_seen_before_removal() {
    let s = store();
    commit(
        &s,
        "a1",
        changes(vec![put("A", Precondition::Absent, b"v1", None)]),
    )
    .unwrap();
    let old = revision(&s, "A").unwrap();
    let id = ContextId::new(*b"A").unwrap();
    commit(
        &s,
        "a2",
        changes(vec![Change::RemoveContext {
            id,
            expect: Precondition::AtRevision(old),
        }]),
    )
    .unwrap();
    commit(
        &s,
        "a3",
        changes(vec![put("A", Precondition::Absent, b"recreated", None)]),
    )
    .unwrap();
    let new = revision(&s, "A").unwrap();
    assert_ne!(new, old, "recreated record reused revision {old:?}");
    // A writer holding the pre-removal revision must conflict, not overwrite the new record.
    let stale = commit(
        &s,
        "a4",
        changes(vec![put(
            "A",
            Precondition::AtRevision(old),
            b"stale",
            None,
        )]),
    );
    assert!(matches!(stale, Err(CommitError::Conflict(_))), "{stale:?}");
    // Positive neighbour: the current revision is accepted.
    commit(
        &s,
        "a5",
        changes(vec![put(
            "A",
            Precondition::AtRevision(new),
            b"fresh",
            None,
        )]),
    )
    .unwrap();
}

#[test]
fn an_oversized_record_is_rejected_without_panicking() {
    let s = store();
    let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        commit(
            &s,
            "big",
            changes(vec![put("A", Precondition::Absent, &vec![7; 70_000], None)]),
        )
    }));
    let r = r.expect("commit panicked on an oversized body");
    assert!(
        matches!(
            r,
            Err(CommitError::Rejected(StorageError::InvalidRequest(_)))
        ),
        "{r:?}"
    );
    // The store remains usable, and a small record still commits.
    commit(
        &s,
        "small",
        changes(vec![put("A", Precondition::Absent, b"ok", None)]),
    )
    .unwrap();
}

#[test]
fn zero_length_and_nonempty_stagings_are_distinct_handles() {
    let s = store();
    let empty = s.stage_content(&mut &b""[..]).unwrap();
    let full = s.stage_content(&mut &b"bytes"[..]).unwrap();
    let cs = ChangeSet {
        changes: vec![
            put("E", Precondition::Absent, b"e", Some(0)),
            put("F", Precondition::Absent, b"f", Some(1)),
        ],
        content: vec![ContentBinding::from(&empty), ContentBinding::from(&full)],
        ..ChangeSet::default()
    };
    commit(&s, "both", cs).expect("both staged handles are valid");
    let mut v = s.begin().unwrap();
    for (id, want) in [("E", &b""[..]), ("F", &b"bytes"[..])] {
        let c = v
            .context(&ContextId::new(id.as_bytes().to_vec()).unwrap())
            .unwrap()
            .unwrap()
            .content
            .unwrap();
        let mut got = Vec::new();
        v.content(&c).unwrap().read_to_end(&mut got).unwrap();
        assert_eq!(got, want);
    }
}

#[test]
fn retained_history_reports_the_content_each_commit_bound() {
    let s = store();
    let staged = s.stage_content(&mut &b"payload"[..]).unwrap();
    let cs = ChangeSet {
        changes: vec![put("A", Precondition::Absent, b"a", Some(0))],
        content: vec![ContentBinding::from(&staged)],
        ..ChangeSet::default()
    };
    commit(&s, "c1", cs).unwrap();
    // Replace the record so the current state no longer refers to that content.
    commit(
        &s,
        "c2",
        changes(vec![put("A", Precondition::Any, b"a2", None)]),
    )
    .unwrap();
    let mut v = s.begin().unwrap();
    let h: Vec<_> = v
        .history(HistoryPosition(0))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    assert_eq!(
        h[0].content,
        vec![staged.content],
        "history lost the bound content reference"
    );
    assert!(h[1].content.is_empty());
    let mut got = Vec::new();
    v.content(&h[0].content[0])
        .unwrap()
        .read_to_end(&mut got)
        .unwrap();
    assert_eq!(got, b"payload");
}

#[test]
fn a_retry_with_an_unrepresentable_content_index_is_not_matched_to_the_original() {
    let s = store();
    let staged = s.stage_content(&mut &b"x"[..]).unwrap();
    let mk = |idx: usize| ChangeSet {
        changes: vec![put("A", Precondition::Absent, b"a", Some(idx))],
        content: vec![ContentBinding::from(&staged)],
        ..ChangeSet::default()
    };
    commit(&s, "same", mk(0)).unwrap();
    // Positive neighbour: the identical request returns the original receipt.
    assert_eq!(commit(&s, "same", mk(0)).unwrap(), 1);
    let r = commit(&s, "same", mk(1usize << 32));
    assert!(
        r.is_err(),
        "a different request under the same attempt identity was accepted: {r:?}"
    );
}
