//! A multi-level metadata tree read back from the store files by a fresh open: every key, every
//! value and the full ordered iteration, against a model built by the test.

use crate::test_support::{create_on};

use std::collections::{BTreeMap, HashSet};

use cce_core::storage::{
    AttemptId, Change, ChangeSet, ContextId, HistoryPosition, Precondition, ReadView, Revision,
    StorageEngine,
};

use crate::NativeStore;
use crate::engine::{Inner, Options, context_key};
use crate::format::StoreId;
use crate::records::{
    AttemptValue, ContextValue, P_ATTEMPT, P_CONTEXT, P_HISTORY, fingerprint, history_key, key,
};
use crate::sim::{Persist, SimIo};
use crate::tree::{self, Cursor, NodeSource};

fn cid(i: u32) -> ContextId {
    ContextId::new(format!("ctx-{i:06}").into_bytes()).unwrap()
}

fn body(i: u32, step: u32) -> Vec<u8> {
    format!("body of {i} written at step {step}, padded to grow leaves").into_bytes()
}

#[test]
fn a_tree_of_height_three_reads_back_whole_after_reopening_from_the_files() {
    const COMMITS: u32 = 10;
    const PER_COMMIT: u32 = 1200;
    const IDS: u32 = 9000;

    let sim = SimIo::new();
    let s = create_on(Box::new(sim.clone()), StoreId([8; 16]), Default::default()).unwrap();
    // Model of every key the tree must hold: contexts and attempt records exactly; history keys
    // by position (their values are physical extents, checked through the history port below).
    let mut model: BTreeMap<Vec<u8>, Vec<u8>> = BTreeMap::new();
    let mut live: BTreeMap<u32, (Revision, Vec<u8>)> = BTreeMap::new();
    let mut attempts = Vec::new();
    for step in 1..=COMMITS {
        let pos = step as u64;
        let mut changes = Vec::new();
        let mut touched = HashSet::new();
        for j in 0..PER_COMMIT {
            // Spread writes over the id space; later steps overwrite earlier records.
            let i = (j * 7919 + step * 104_729) % IDS;
            if !touched.insert(i) {
                continue;
            }
            if step > 2 && j % 10 == 0 && live.contains_key(&i) {
                changes.push(Change::RemoveContext {
                    id: cid(i),
                    expect: Precondition::Any,
                });
                live.remove(&i);
                model.remove(&context_key(&cid(i)));
            } else {
                changes.push(Change::PutContext {
                    id: cid(i),
                    expect: Precondition::Any,
                    body: body(i, step),
                    content: None,
                });
                live.insert(i, (Revision(pos), body(i, step)));
                model.insert(
                    context_key(&cid(i)),
                    ContextValue {
                        revision: pos,
                        body: body(i, step),
                        content: None,
                    }
                    .encode(),
                );
            }
        }
        let attempt = AttemptId::new(format!("bulk-{step}").into_bytes()).unwrap();
        let cs = ChangeSet {
            changes,
            ..ChangeSet::default()
        };
        model.insert(
            key(P_ATTEMPT, &[attempt.as_bytes()]),
            AttemptValue {
                position: pos,
                fingerprint: fingerprint(&attempt, &cs),
            }
            .encode(),
        );
        let receipt = s.commit(attempt.clone(), s.begin().unwrap(), cs).unwrap();
        assert_eq!(receipt.position, HistoryPosition(pos));
        attempts.push(attempt);
    }
    let height = s.inner.height().unwrap();
    assert!(
        height >= 3,
        "tree height {height}; the test needs at least 3"
    );
    drop(s);

    // Only what reached the disk: a power loss right now, then a fresh open with a small node
    // cache so the walk below reloads pages from the files.
    let img = sim.after_crash(Persist::None);
    let s = NativeStore {
        inner: Inner::open(
            Box::new(img),
            Options {
                cache_nodes: 8,
                ..Default::default()
            },
        )
        .unwrap(),
    };
    assert_eq!(s.inner.height().unwrap(), height);
    let root = s.inner.snapshot().unwrap().desc.root;
    let src: &dyn NodeSource = &*s.inner;

    // Full ordered iteration.
    let all: Vec<(Vec<u8>, Vec<u8>)> = Cursor::new(src, root, b"", None, u64::MAX)
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert!(
        all.windows(2).all(|w| w[0].0 < w[1].0),
        "iteration is not strictly ascending"
    );
    let (history, rest): (Vec<_>, Vec<_>) =
        all.iter().cloned().partition(|(k, _)| k[0] == P_HISTORY);
    let history_keys: Vec<_> = history.into_iter().map(|(k, _)| k).collect();
    assert_eq!(
        history_keys,
        (1..=COMMITS as u64).map(history_key).collect::<Vec<_>>()
    );
    assert!(
        rest.iter()
            .all(|(k, _)| k[0] == P_CONTEXT || k[0] == P_ATTEMPT),
        "unexpected key family in the tree"
    );
    // Every context and attempt key with its exact value, in order, and nothing else.
    let expected: Vec<(Vec<u8>, Vec<u8>)> = model.clone().into_iter().collect();
    assert_eq!(rest.len(), expected.len());
    assert_eq!(rest, expected, "iteration differs from the model");

    // Every key by point lookup agrees with the iteration.
    for (k, v) in &all {
        assert_eq!(
            tree::get(src, root, k).unwrap().as_ref(),
            Some(v),
            "point lookup of {k:?}"
        );
    }
    // Removed and never-written records are absent by point lookup too.
    for i in 0..IDS {
        let k = context_key(&cid(i));
        if !model.contains_key(&k) {
            assert_eq!(tree::get(src, root, &k).unwrap(), None, "id {i}");
        }
    }

    // The same through the logical port.
    let mut v = s.begin().unwrap();
    for i in 0..IDS {
        let got = v.context(&cid(i)).unwrap().map(|r| (r.revision, r.body));
        assert_eq!(got, live.get(&i).cloned(), "context {i}");
    }
    let hist: Vec<_> = v
        .history(HistoryPosition(0))
        .unwrap()
        .map(|h| {
            let h = h.unwrap();
            (h.position.0, h.attempt)
        })
        .collect();
    assert_eq!(
        hist,
        attempts
            .into_iter()
            .enumerate()
            .map(|(n, a)| (n as u64 + 1, a))
            .collect::<Vec<_>>()
    );
}
