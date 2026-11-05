//! Test composition for the private storage adapters. Scenario assertions stay in their suites.

use cce_core::storage::{AttemptId, Change, ChangeSet, ContextId, Precondition, StorageError};

use crate::NativeStore;
use crate::engine::{Inner, Options};
use crate::format::StoreId;
use crate::io::Io;

#[path = "../tests/common/resources.rs"]
pub(crate) mod resources;

pub(crate) fn create_on(
    io: Box<dyn Io>,
    id: StoreId,
    options: Options,
) -> Result<NativeStore, StorageError> {
    Inner::create(io, id, options).map(|inner| NativeStore { inner })
}

pub(crate) fn open_on(io: Box<dyn Io>, options: Options) -> Result<NativeStore, StorageError> {
    Inner::open(io, options).map(|inner| NativeStore { inner })
}

pub(crate) fn cid(s: &str) -> ContextId {
    ContextId::new(s.as_bytes().to_vec()).unwrap()
}

pub(crate) fn attempt(s: &str) -> AttemptId {
    AttemptId::new(s.as_bytes().to_vec()).unwrap()
}

pub(crate) fn put_context(
    id: &str,
    expect: Precondition,
    body: &[u8],
    content: Option<usize>,
) -> Change {
    Change::PutContext {
        id: cid(id),
        expect,
        body: body.to_vec(),
        content,
    }
}

/// Most rounds a commit may take to settle its page allocation in the tests that check it. The
/// suites observe at most 4; the engine itself rejects a commit only past a larger limit.
pub(crate) const SETTLE_ROUNDS_BOUND: usize = 6;

pub(crate) fn changes(changes: Vec<Change>) -> ChangeSet {
    ChangeSet {
        changes,
        ..ChangeSet::default()
    }
}
