//! Shared requests and the state oracle of the crash and recovery tests: what a store holds, read
//! through the logical port, and what it must hold, built from the submitted requests alone.

use std::collections::BTreeMap;
use std::io::Read;

use cce_core::storage::{
    AttemptId, Change, ChangeSet, ContentBinding, ContentDigest, ContentRef, ContextId, Direction,
    Evidence, HistoryEntry, HistoryPosition, Precondition, ReadView, Relationship, RelationshipId,
    RelationshipKind, RelationshipQuery, Revision, StagedContent, StagingToken, StorageEngine,
};
use sha2::Digest;

use crate::NativeStore;
use crate::engine::{Inner, Options};
use crate::format::StoreId;
use crate::sim::SimIo;

pub(crate) const IDS: [&str; 4] = ["A", "B", "C", "D"];

pub(crate) use crate::test_support::cid;

/// The content bound by the request of `step`.
pub(crate) fn payload(step: u32) -> Vec<u8> {
    (0..(150_000 + step as usize * 7))
        .map(|i| (i * 31 + step as usize) as u8)
        .collect()
}

pub(crate) fn attempt(step: u32) -> AttemptId {
    AttemptId::new(format!("attempt-{step}").into_bytes()).unwrap()
}

/// The request of `step`, binding already staged content.
pub(crate) fn request_with(staged: &StagedContent, step: u32) -> (AttemptId, ChangeSet) {
    request_for(ContentBinding::from(staged), step)
}

/// The request of `step` with `binding` as its content: a context record with content, a
/// relationship (both index directions), and evidence.
fn request_for(binding: ContentBinding, step: u32) -> (AttemptId, ChangeSet) {
    let (a, b) = (IDS[step as usize % 4], IDS[(step as usize + 1) % 4]);
    let cs = ChangeSet {
        removal: Default::default(),
        changes: vec![
            Change::PutContext {
                id: cid(a),
                expect: Precondition::Any,
                body: format!("body {a} at step {step}").into_bytes(),
                content: Some(0),
            },
            Change::PutRelationship {
                id: RelationshipId::new(format!("r{step}").into_bytes()).unwrap(),
                expect: Precondition::Absent,
                kind: RelationshipKind::new(*b"depends").unwrap(),
                source: cid(a),
                target: cid(b),
            },
        ],
        content: vec![binding],
        evidence: vec![Evidence {
            label: b"decision".to_vec(),
            bytes: format!("evidence {step}").into_bytes(),
        }],
    };
    (attempt(step), cs)
}

/// The request of `step`, staging its content first.
pub(crate) fn request(store: &NativeStore, step: u32) -> Option<(AttemptId, ChangeSet)> {
    let staged = store.stage_content(&mut &payload(step)[..]).ok()?;
    Some(request_with(&staged, step))
}

pub(crate) fn commit_step(store: &NativeStore, step: u32) -> bool {
    let Some((attempt, cs)) = request(store, step) else {
        return false;
    };
    let Ok(view) = store.begin() else {
        return false;
    };
    store.commit(attempt, view, cs).is_ok()
}

/// Bytes compared exactly, shown by length and a hash prefix.
#[derive(Clone, PartialEq, Eq)]
pub(crate) struct Bytes(pub Vec<u8>);

impl std::fmt::Debug for Bytes {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let h = sha2::Sha256::digest(&self.0);
        write!(
            f,
            "{} bytes, sha256 {:02x}{:02x}{:02x}{:02x}…",
            self.0.len(),
            h[0],
            h[1],
            h[2],
            h[3]
        )
    }
}

/// One context record: revision, body, and its bound content with the content's exact bytes.
pub(crate) type ContextState = (u64, Vec<u8>, Option<(ContentRef, Bytes)>);

/// The complete logical state of a store over the contexts in [`IDS`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct State {
    pub contexts: BTreeMap<String, ContextState>,
    /// Relationships as listed from each context: (context, relationship id) → relationship.
    pub outgoing: BTreeMap<(String, Vec<u8>), Relationship>,
    pub incoming: BTreeMap<(String, Vec<u8>), Relationship>,
    /// Every retained history entry, whole.
    pub history: Vec<HistoryEntry>,
}

/// The oracle: the complete logical state, read through the port.
pub(crate) fn state(store: &NativeStore) -> State {
    let mut v = store.begin().expect("begin");
    let mut s = State {
        contexts: BTreeMap::new(),
        outgoing: BTreeMap::new(),
        incoming: BTreeMap::new(),
        history: Vec::new(),
    };
    for name in IDS {
        let id = cid(name);
        if let Some(r) = v.context(&id).expect("context") {
            let content = r.content.map(|c| {
                let mut bytes = Vec::new();
                v.content(&c)
                    .expect("content")
                    .read_to_end(&mut bytes)
                    .expect("read content");
                (c, Bytes(bytes))
            });
            s.contexts
                .insert(name.to_string(), (r.revision.0, r.body, content));
        }
        for (d, into) in [
            (Direction::Outgoing, &mut s.outgoing),
            (Direction::Incoming, &mut s.incoming),
        ] {
            let q = RelationshipQuery {
                from: id.clone(),
                direction: d,
                kind: None,
                after: None,
            };
            for r in v.relationships(&q).expect("relationships") {
                let r = r.expect("relationship");
                into.insert((name.to_string(), r.id.as_bytes().to_vec()), r);
            }
        }
    }
    s.history = v
        .history(HistoryPosition(0))
        .expect("history")
        .map(|h| h.expect("history entry"))
        .collect();
    s
}

pub(crate) fn history_len(store: &NativeStore) -> usize {
    state(store).history.len()
}

/// The request of `step` as submitted, for the model: its content bound by digest and length
/// alone (the staging token is not part of what is stored).
fn submitted(step: u32) -> (AttemptId, ChangeSet) {
    let bytes = payload(step);
    let content = ContentRef {
        digest: ContentDigest(sha2::Sha256::digest(&bytes).into()),
        len: bytes.len() as u64,
    };
    request_for(
        ContentBinding {
            content,
            token: StagingToken(Vec::new()),
        },
        step,
    )
}

/// What a store must hold after steps 1..=`steps`, built from the submitted requests alone: each
/// step commits at history position `step`, and every record it writes takes that revision.
pub(crate) fn expected(steps: u32) -> State {
    let mut s = State {
        contexts: BTreeMap::new(),
        outgoing: BTreeMap::new(),
        incoming: BTreeMap::new(),
        history: Vec::new(),
    };
    for step in 1..=steps {
        let pos = step as u64;
        let (attempt, cs) = submitted(step);
        for change in &cs.changes {
            match change {
                Change::PutContext {
                    id, body, content, ..
                } => {
                    let bound = content.map(|i| (cs.content[i].content, Bytes(payload(step))));
                    s.contexts.insert(name_of(id), (pos, body.clone(), bound));
                }
                Change::RemoveContext { id, .. } => {
                    s.contexts.remove(&name_of(id));
                }
                Change::PutRelationship {
                    id,
                    kind,
                    source,
                    target,
                    ..
                } => {
                    let r = Relationship {
                        id: id.clone(),
                        kind: kind.clone(),
                        source: source.clone(),
                        target: target.clone(),
                        revision: Revision(pos),
                    };
                    let key = id.as_bytes().to_vec();
                    s.outgoing.insert((name_of(source), key.clone()), r.clone());
                    s.incoming.insert((name_of(target), key), r);
                }
                Change::RemoveRelationship { id, .. } => {
                    s.outgoing.retain(|(_, k), _| k != id.as_bytes());
                    s.incoming.retain(|(_, k), _| k != id.as_bytes());
                }
                Change::RemoveHistory { .. } => {
                    unreachable!("the oracle's submissions do not remove history")
                }
            }
        }
        s.history.push(HistoryEntry {
            position: HistoryPosition(pos),
            attempt,
            evidence: cs.evidence.clone(),
            changes: cs.changes.clone(),
            content: cs.content.iter().map(|b| b.content).collect(),
        });
    }
    s
}

fn name_of(id: &ContextId) -> String {
    String::from_utf8(id.as_bytes().to_vec()).unwrap()
}

/// The expected states after 0..=`steps` steps.
pub(crate) fn reference_states(steps: u32) -> Vec<State> {
    (0..=steps).map(expected).collect()
}

/// A store after step 1, then step 2 with `alter` applied to its payload and request.
fn store_after(alter: impl FnOnce(&mut Vec<u8>, &mut ChangeSet)) -> NativeStore {
    let s = NativeStore {
        inner: Inner::create(
            Box::new(SimIo::new()),
            StoreId(*b"oracle-test-stor"),
            Options::default(),
        )
        .unwrap(),
    };
    assert!(commit_step(&s, 1));
    let mut bytes = payload(2);
    let mut probe = request_with(&s.stage_content(&mut &bytes[..]).unwrap(), 2).1;
    alter(&mut bytes, &mut probe);
    let staged = s.stage_content(&mut &bytes[..]).unwrap();
    let (a, mut cs) = request_with(&staged, 2);
    cs.changes = probe.changes;
    s.commit(a, s.begin().unwrap(), cs).unwrap();
    s
}

#[test]
fn the_oracle_tells_apart_history_changes_with_equal_counts() {
    let plain = state(&store_after(|_, _| {}));
    // A history change that differs while every count stays the same.
    let precondition = store_after(|_, cs| {
        if let Change::PutRelationship { expect, .. } = &mut cs.changes[1] {
            *expect = Precondition::Any;
        }
    });
    assert_ne!(
        state(&precondition),
        plain,
        "a different history change went unseen"
    );
}

#[test]
fn the_oracle_tells_apart_content_of_equal_length_and_byte_sum() {
    let plain = state(&store_after(|_, _| {}));
    // Two bytes swapped: the same length and the same byte sum.
    let swapped = store_after(|bytes, _| bytes.swap(0, 1));
    assert_ne!(
        state(&swapped),
        plain,
        "different content bytes went unseen"
    );
}

#[test]
fn the_model_matches_an_uninterrupted_run() {
    let s = NativeStore {
        inner: Inner::create(
            Box::new(SimIo::new()),
            StoreId(*b"oracle-test-stor"),
            Options::default(),
        )
        .unwrap(),
    };
    assert_eq!(state(&s), expected(0));
    for step in 1..=5 {
        assert!(commit_step(&s, step));
        assert_eq!(state(&s), expected(step), "after step {step}");
    }
}
