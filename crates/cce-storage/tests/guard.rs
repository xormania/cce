//! A guarded open through the public options, on a real store directory.

mod common;

use cce_core::storage::{
    AttemptId, Change, ChangeSet, ContextId, HistoryPosition, Precondition, StorageEngine,
    StorageError,
};
use cce_storage::{NativeStore, ReceiptGuard, StoreOptions};

const ID: [u8; 16] = *b"guard-store-test";

fn guarded(min: u64) -> StoreOptions {
    let mut o = StoreOptions::default();
    o.guard = Some(ReceiptGuard::new(ID, HistoryPosition(min)));
    o
}

#[test]
fn a_guarded_open_through_store_options_checks_the_retained_position() {
    let dir = common::test_dir("guarded-open").join("store");
    let s = NativeStore::create(&dir, ID).unwrap();
    let receipt = s
        .commit(
            AttemptId::new(*b"one").unwrap(),
            s.begin().unwrap(),
            ChangeSet {
                changes: vec![Change::PutContext {
                    id: ContextId::new(*b"A").unwrap(),
                    expect: Precondition::Any,
                    body: b"a".to_vec(),
                    content: None,
                }],
                ..ChangeSet::default()
            },
        )
        .unwrap();
    s.close().unwrap();
    let files = || ["meta.cce", "content.cce"].map(|f| std::fs::read(dir.join(f)).unwrap());
    let before = files();

    let refused = |r: Result<(), StorageError>| {
        assert!(
            matches!(&r, Err(StorageError::GuardRefused(why)) if why.contains("below")),
            "{r:?}"
        );
    };
    refused(NativeStore::open_with(&dir, guarded(receipt.position.0 + 1)).map(drop));
    refused(NativeStore::open_read_only_with(&dir, guarded(receipt.position.0 + 1)).map(drop));
    assert!(files() == before, "a refused open changed the store");

    // The retained receipt's own position, and no guard at all, open normally.
    drop(NativeStore::open_with(&dir, guarded(receipt.position.0)).unwrap());
    drop(NativeStore::open_read_only_with(&dir, guarded(receipt.position.0)).unwrap());
    drop(NativeStore::open(&dir).unwrap());
}
