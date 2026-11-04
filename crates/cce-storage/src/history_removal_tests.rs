//! Host-requested removal of designated history entries: exactly those entries are removed, their
//! payload is not kept, positions do not move, the removal survives reopening, and retrying an
//! attempt whose entry was removed returns its original receipt without restoring or appending
//! anything.

use std::io::Read;

use cce_core::storage::{
    AttemptOutcome, Change, ChangeSet, CommitError, CommitReceipt, ContentBinding, Direction,
    Evidence, HistoryEntry, HistoryPosition, Precondition, ReadView, RelationshipId,
    RelationshipKind, RelationshipQuery, StorageEngine, StorageError,
};

use crate::NativeStore;
use crate::audit::{published, reach};
use crate::format::{Descriptor, StoreId};
use crate::io::{FileId, Io};
use crate::sim::SimIo;
use crate::test_support::{attempt, changes, cid, create_on, open_on, put_context};

const STORE: StoreId = StoreId(*b"history-removal!");

fn create(sim: &SimIo) -> NativeStore {
    create_on(Box::new(sim.clone()), STORE, Default::default()).unwrap()
}

fn reopen(sim: &SimIo, s: NativeStore) -> NativeStore {
    drop(s);
    sim.release();
    open_on(Box::new(sim.clone()), Default::default())
        .unwrap_or_else(|e| panic!("the store must reopen: {e:?}"))
}

/// The newest valid publication record.
fn newest(sim: &SimIo) -> Descriptor {
    *published(sim).unwrap().last().unwrap()
}

fn commit(s: &NativeStore, id: &str, cs: ChangeSet) -> Result<CommitReceipt, CommitError> {
    s.commit(attempt(id), s.begin().unwrap(), cs)
}

fn committed(s: &NativeStore, id: &str, cs: ChangeSet) -> u64 {
    let r = commit(s, id, cs);
    assert!(r.is_ok(), "commit {id} was refused: {r:?}");
    r.unwrap().position.0
}

fn remove_history(positions: &[u64]) -> ChangeSet {
    changes(
        positions
            .iter()
            .map(|p| Change::RemoveHistory {
                position: HistoryPosition(*p),
            })
            .collect(),
    )
}

fn with_evidence(mut cs: ChangeSet, label: &str) -> ChangeSet {
    cs.evidence.push(Evidence {
        label: label.as_bytes().to_vec(),
        bytes: format!("evidence of {label}").into_bytes(),
    });
    cs
}

/// The retained history of the current state, every entry readable.
fn history(s: &NativeStore) -> Vec<HistoryEntry> {
    let mut v = s.begin().unwrap();
    v.history(HistoryPosition(0))
        .unwrap()
        .map(|h| h.unwrap_or_else(|e| panic!("a retained history entry is unreadable: {e:?}")))
        .collect()
}

fn positions(h: &[HistoryEntry]) -> Vec<u64> {
    h.iter().map(|e| e.position.0).collect()
}

fn body(s: &NativeStore, id: &str) -> Option<Vec<u8>> {
    s.begin()
        .unwrap()
        .context(&cid(id))
        .unwrap()
        .map(|r| r.body)
}

/// Three commits A, B, C with distinct records and evidence; returns their requests.
fn three_commits(s: &NativeStore) -> [ChangeSet; 3] {
    let reqs = ["a", "b", "c"].map(|n| {
        with_evidence(
            changes(vec![put_context(
                n,
                Precondition::Absent,
                format!("body of {n}").as_bytes(),
                None,
            )]),
            n,
        )
    });
    for (i, (n, cs)) in ["a", "b", "c"].iter().zip(&reqs).enumerate() {
        assert_eq!(committed(s, n, cs.clone()), i as u64 + 1);
    }
    reqs
}

#[test]
fn designated_history_is_removed_exactly_and_the_removal_survives_reopen() {
    let sim = SimIo::new();
    let s = create(&sim);
    let reqs = three_commits(&s);
    let before = history(&s);
    let mut old_view = s.begin().unwrap();

    let removal = committed(&s, "remove-b", remove_history(&[2]));
    assert_eq!(removal, 4, "the removal is a commit of its own");
    crate::audit::check_retirement(&sim).unwrap_or_else(|e| panic!("after the removal: {e}"));

    let expect_after = |s: &NativeStore, what: &str| {
        let h = history(s);
        assert_eq!(
            positions(&h),
            vec![1, 3, 4],
            "{what}: exactly the designated entry is gone and no position moved"
        );
        assert_eq!(h[0], before[0], "{what}: entry 1 is kept whole");
        assert_eq!(h[1], before[2], "{what}: entry 3 is kept whole");
        assert_eq!(
            h[2].changes,
            vec![Change::RemoveHistory {
                position: HistoryPosition(2)
            }],
            "{what}: the removal records what it removed"
        );
        // Records are not history: the record B wrote is still current.
        for n in ["a", "b", "c"] {
            assert_eq!(
                body(s, n),
                Some(format!("body of {n}").into_bytes()),
                "{what}: record {n}"
            );
        }
    };
    expect_after(&s, "after the removal");

    // A view that began before the removal still reads the entry whole.
    let old: Vec<HistoryEntry> = old_view
        .history(HistoryPosition(0))
        .unwrap()
        .map(|h| h.unwrap_or_else(|e| panic!("an older view lost a history entry: {e:?}")))
        .collect();
    assert_eq!(old, before, "an older view keeps its history");
    drop(old_view);

    let s = reopen(&sim, s);
    expect_after(&s, "after reopening");

    // Retrying B returns its original receipt and changes nothing.
    let retry = commit(&s, "b", reqs[1].clone());
    assert_eq!(
        retry,
        Ok(CommitReceipt {
            attempt: attempt("b"),
            position: HistoryPosition(2)
        }),
        "a retry of a removed attempt returns its original receipt"
    );
    assert_eq!(
        s.outcome(&attempt("b")).unwrap(),
        AttemptOutcome::Committed(CommitReceipt {
            attempt: attempt("b"),
            position: HistoryPosition(2)
        })
    );
    // A different request under B's identity is still refused.
    let other = commit(&s, "b", reqs[0].clone());
    assert!(
        matches!(other, Err(CommitError::AttemptMismatch)),
        "a different request under a removed attempt's identity: {other:?}"
    );
    // Retrying the removal itself returns its receipt; it is not refused as already removed.
    let again = commit(&s, "remove-b", remove_history(&[2]));
    assert_eq!(
        again,
        Ok(CommitReceipt {
            attempt: attempt("remove-b"),
            position: HistoryPosition(4)
        }),
        "a retry of the removal returns its original receipt"
    );
    expect_after(&s, "after the retries");
    assert_eq!(newest(&sim).history_pos, 4, "the retries appended nothing");
}

#[test]
fn every_cut_of_a_removal_commit_recovers_the_state_before_or_after_it() {
    use crate::repro_tests::{PROCESS_EXIT, survival};
    use crate::sim::Persist;
    let build = |sim: &SimIo| {
        let s = create(sim);
        let reqs = three_commits(&s);
        (s, reqs)
    };
    let probe = SimIo::new();
    let (s, _) = build(&probe);
    let start = probe.ops();
    committed(&s, "remove-b", remove_history(&[2]));
    let total = probe.ops() - start;
    drop(s);
    let (old, new) = (vec![1, 2, 3], vec![1, 3, 4]);
    for cut in 0..=total {
        let sim = SimIo::new();
        let (s, reqs) = build(&sim);
        sim.crash_after(cut);
        let acked = commit(&s, "remove-b", remove_history(&[2])).is_ok();
        assert_eq!(acked, cut == total, "cut {cut}");
        drop(s);
        let images = [Persist::None, Persist::All]
            .into_iter()
            .chain((1..=6).map(Persist::Seeded))
            .map(|p| (survival(p), sim.after_crash(p)))
            .chain(std::iter::once((
                PROCESS_EXIT.to_string(),
                sim.after_process_exit(),
            )));
        for (survived, img) in images {
            let what = format!("cut {cut}, {survived}");
            let rec = open_on(Box::new(img.clone()), Default::default())
                .unwrap_or_else(|e| panic!("{what}: recovery must open the store: {e:?}"));
            crate::audit::check_retirement(&img)
                .unwrap_or_else(|e| panic!("{what}: after recovery: {e}"));
            let got = positions(&history(&rec));
            assert!(got == old || got == new, "{what}: partial removal {got:?}");
            if acked {
                assert_eq!(got, new, "{what}: acknowledged removal lost");
            }
            // B's receipt holds either way; retrying the removal ends removed, at position 4.
            assert_eq!(
                commit(&rec, "b", reqs[1].clone()).map(|r| r.position.0),
                Ok(2),
                "{what}: retry of B"
            );
            assert_eq!(
                commit(&rec, "remove-b", remove_history(&[2])).map(|r| r.position.0),
                Ok(4),
                "{what}: retry of the removal"
            );
            assert_eq!(positions(&history(&rec)), new, "{what}: after the retries");
            crate::audit::check_retirement(&img)
                .unwrap_or_else(|e| panic!("{what}: after the retries: {e}"));
        }
    }
}

#[test]
fn a_retry_after_removal_restores_no_content_relationship_or_history() {
    let sim = SimIo::new();
    let s = create(&sim);
    let bytes = b"content kept only while the record uses it".to_vec();
    let staged = s.stage_content(&mut &bytes[..]).unwrap();
    let rel = RelationshipId::new(b"r".to_vec()).unwrap();
    let mut b = changes(vec![
        put_context("x", Precondition::Absent, b"x", Some(0)),
        put_context("y", Precondition::Absent, b"y", None),
        Change::PutRelationship {
            id: rel.clone(),
            expect: Precondition::Absent,
            kind: RelationshipKind::new(b"k".to_vec()).unwrap(),
            source: cid("x"),
            target: cid("y"),
        },
    ]);
    b.content.push(ContentBinding::from(&staged));
    let content = staged.content;
    assert_eq!(committed(&s, "b", b.clone()), 1);
    // The host removes the records, then the history that wrote them.
    committed(
        &s,
        "drop-records",
        changes(vec![
            Change::RemoveRelationship {
                id: rel.clone(),
                expect: Precondition::Any,
            },
            Change::RemoveContext {
                id: cid("x"),
                expect: Precondition::Any,
            },
        ]),
    );
    committed(&s, "drop-history", remove_history(&[1]));
    let h = positions(&history(&s));
    let s = reopen(&sim, s);

    let retry = commit(&s, "b", b);
    assert_eq!(
        retry.map(|r| r.position.0),
        Ok(1),
        "the retry returns the original receipt"
    );
    let mut v = s.begin().unwrap();
    assert_eq!(v.context(&cid("x")).unwrap(), None, "record restored");
    let out = v
        .relationships(&RelationshipQuery {
            from: cid("x"),
            direction: Direction::Outgoing,
            kind: None,
            after: None,
        })
        .unwrap()
        .count();
    assert_eq!(out, 0, "relationship restored");
    let mut read = Vec::new();
    let served = v
        .content(&content)
        .and_then(|mut r| {
            r.read_to_end(&mut read)
                .map_err(|e| StorageError::Io(e.to_string()))
        })
        .map(|_| read);
    assert!(
        matches!(served, Err(StorageError::ContentNotRetained)),
        "content restored: {served:?}"
    );
    assert_eq!(positions(&history(&s)), h, "history restored or appended");
}

#[test]
fn only_retained_positions_can_be_designated_and_a_refusal_changes_nothing() {
    let sim = SimIo::new();
    let s = create(&sim);
    three_commits(&s);
    committed(&s, "remove-b", remove_history(&[2]));
    let before = history(&s);
    let generation = newest(&sim).generation;
    for (what, cs) in [
        ("position 0", remove_history(&[0])),
        ("a position not yet written", remove_history(&[9])),
        ("the removal's own position", remove_history(&[5])),
        ("an already removed position", remove_history(&[2])),
        ("one position twice", remove_history(&[1, 1])),
        (
            "a valid and an already removed position",
            remove_history(&[1, 2]),
        ),
    ] {
        let r = commit(&s, &format!("bad {what}"), cs);
        assert!(
            matches!(
                r,
                Err(CommitError::Rejected(StorageError::InvalidRequest(_)))
            ),
            "{what}: {r:?}"
        );
        assert_eq!(
            history(&s),
            before,
            "{what}: a refused removal changed history"
        );
        assert_eq!(
            newest(&sim).generation,
            generation,
            "{what}: a refused removal published"
        );
    }
}

#[test]
fn a_removed_entry_keeps_no_payload_and_its_space_is_retired() {
    let sim = SimIo::new();
    let s = create(&sim);
    let payload = b"PAYLOAD-ONLY-HISTORY-HELD-0123456789".repeat(8);
    committed(
        &s,
        "secret",
        changes(vec![put_context("p", Precondition::Absent, &payload, None)]),
    );
    // The record goes; only history holds the payload now.
    committed(
        &s,
        "drop",
        changes(vec![Change::RemoveContext {
            id: cid("p"),
            expect: Precondition::Any,
        }]),
    );
    let held = reach(&sim, &newest(&sim)).unwrap();
    committed(&s, "forget", remove_history(&[1]));
    let now = reach(&sim, &newest(&sim)).unwrap();
    let gone: Vec<(u64, u64)> = held
        .history_extents
        .difference(&now.history_extents)
        .copied()
        .collect();
    assert_eq!(
        gone.len(),
        1,
        "exactly the removed entry's extent left history"
    );
    let (offset, len) = gone[0];
    assert_eq!(
        now.retired_extents.get(&offset).map(|(_, l)| *l),
        Some(len),
        "the removed entry's extent is retired"
    );
    // Nothing the new state reaches holds the payload: not the history, not the attempt index.
    let mut file = vec![0; sim.len(FileId::Content).unwrap() as usize];
    sim.read_at(FileId::Content, 0, &mut file).unwrap();
    for (o, l) in now.history_extents.iter().chain(&now.manifests) {
        let bytes = &file[*o as usize..(*o + *l) as usize];
        assert!(
            !bytes.windows(payload.len()).any(|w| w == payload),
            "a reachable extent at {o} still holds the payload"
        );
    }
    crate::audit::check_retirement(&sim).unwrap_or_else(|e| panic!("after the removal: {e}"));
}
