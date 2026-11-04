//! A removal that leaves a small leaf beside a sibling too large to merge with, through ordinary
//! commits: the sibling must not be retired while the published tree still reaches it. The six
//! commits are a shrunk sequence found by a generated model of the engine; keys, bodies and attempt
//! names are reproduced exactly, since they decide the tree's shape.

use std::collections::BTreeMap;

use cce_core::storage::{
    Change, ChangeSet, ContextId, Precondition, ReadView, RelationshipId, RelationshipKind,
    StorageEngine,
};

use crate::NativeStore;
use crate::format::StoreId;
use crate::sim::SimIo;
use crate::test_support::{attempt, changes, cid, create_on, open_on};

const STORE: StoreId = StoreId(*b"model-test-store");
const CONTEXTS: [&str; 10] = ["a", "ab", "b", "c", "d", "e", "f", "g", "h", "i"];

fn ctx(i: u8) -> ContextId {
    cid(CONTEXTS[i as usize])
}

/// Bodies ending in 3 are 1650 bytes longer, in 2 700 bytes longer, others short.
fn body(n: u8, id: &ContextId) -> Vec<u8> {
    let mut b = format!("body {n} of {id:?}").into_bytes();
    let pad = match n % 4 {
        3 => 1650,
        2 => 700,
        _ => 0,
    };
    b.resize(b.len() + pad, b'.');
    b
}

fn put(i: u8, n: u8) -> Change {
    let id = ctx(i);
    Change::PutContext {
        body: body(n, &id),
        id,
        expect: Precondition::Any,
        content: None,
    }
}

fn remove(i: u8) -> Change {
    Change::RemoveContext {
        id: ctx(i),
        expect: Precondition::Any,
    }
}

/// Every context's body, read through a fresh view.
fn bodies(s: &NativeStore) -> BTreeMap<&'static str, Option<Vec<u8>>> {
    let mut v = s.begin().unwrap();
    CONTEXTS
        .iter()
        .map(|c| (*c, v.context(&cid(c)).unwrap().map(|r| r.body)))
        .collect()
}

#[test]
fn a_removal_beside_a_sibling_too_large_to_merge_retires_nothing_reachable() {
    let steps: Vec<ChangeSet> = vec![
        changes(vec![put(2, 3), put(6, 3), put(8, 0), put(9, 3)]),
        changes(vec![
            put(2, 0),
            Change::PutRelationship {
                id: RelationshipId::new(b"r0".to_vec()).unwrap(),
                expect: Precondition::Any,
                kind: RelationshipKind::new(b"k".to_vec()).unwrap(),
                source: ctx(0),
                target: ctx(3),
            },
        ]),
        changes(vec![put(0, 227), remove(2), put(4, 114), put(7, 151)]),
        changes(vec![put(4, 239)]),
        changes(vec![put(2, 142)]),
        // The removal of "a" leaves its leaf small beside a sibling too large to take it.
        changes(vec![remove(0), put(1, 75), remove(7)]),
    ];
    let sim = SimIo::new();
    let s = create_on(Box::new(sim.clone()), STORE, Default::default()).unwrap();
    let mut expected: BTreeMap<&'static str, Option<Vec<u8>>> =
        CONTEXTS.iter().map(|c| (*c, None)).collect();
    for (n, cs) in steps.into_iter().enumerate() {
        for c in &cs.changes {
            match c {
                Change::PutContext { id, body, .. } => {
                    let name = CONTEXTS.iter().find(|c| cid(c) == *id).unwrap();
                    expected.insert(name, Some(body.clone()));
                }
                Change::RemoveContext { id, .. } => {
                    let name = CONTEXTS.iter().find(|c| cid(c) == *id).unwrap();
                    expected.insert(name, None);
                }
                _ => {}
            }
        }
        let r = s.commit(
            attempt(&format!("attempt-{}", n + 1)),
            s.begin().unwrap(),
            cs,
        );
        assert!(r.is_ok(), "step {}: {r:?}", n + 1);
        assert_eq!(
            crate::audit::check_retirement(&sim),
            Ok(()),
            "after step {}",
            n + 1
        );
    }
    assert_eq!(bodies(&s), expected, "after the steps");

    // Two more generations make every retired page reusable; further commits then reuse them.
    for k in 0..6u8 {
        let i = 4 + k % 4;
        let cs = changes(vec![put(i, k * 4 + 3)]);
        let (name, b) = (CONTEXTS[i as usize], body(k * 4 + 3, &ctx(i)));
        let r = s.commit(attempt(&format!("later-{k}")), s.begin().unwrap(), cs);
        assert!(r.is_ok(), "later commit {k}: {r:?}");
        expected.insert(name, Some(b));
        assert_eq!(
            crate::audit::check_retirement(&sim),
            Ok(()),
            "after later commit {k}"
        );
        assert_eq!(bodies(&s), expected, "after later commit {k}");
    }
    drop(s);
    sim.release();
    let s = open_on(Box::new(sim.clone()), Default::default()).unwrap();
    assert_eq!(bodies(&s), expected, "after reopen");
}
