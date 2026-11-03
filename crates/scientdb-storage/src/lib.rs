//! Native storage engine for ScientDB.
//!
//! Physical structures are private. The public surface is the logical port from
//! `scientdb-kernel` plus constructors for a native store.

mod codec;
mod format;
mod io;
mod records;

use std::io::Read;
use std::path::Path;

use scientdb_kernel::storage::{
    AttemptId, AttemptOutcome, ChangeSet, CommitError, CommitReceipt, ContentReader, ContentRef,
    ContextId, ContextRecord, HistoryEntry, HistoryPosition, ReadView, Relationship,
    RelationshipQuery, StagedContent, StorageEngine, StorageError,
};

/// A native store owned by this process.
pub struct NativeStore {
    _private: (),
}

impl NativeStore {
    /// Creates a new store in `dir`, which must not already contain a store.
    pub fn create(_dir: &Path) -> Result<Self, StorageError> {
        Err(StorageError::NotImplemented("create"))
    }

    /// Opens an existing store in `dir`.
    pub fn open(_dir: &Path) -> Result<Self, StorageError> {
        Err(StorageError::NotImplemented("open"))
    }
}

/// A consistent view of one committed state of a [`NativeStore`].
pub struct NativeView {
    _private: (),
}

impl ReadView for NativeView {
    fn context(&mut self, _id: &ContextId) -> Result<Option<ContextRecord>, StorageError> {
        Err(StorageError::NotImplemented("context"))
    }

    fn relationships(
        &mut self,
        _query: &RelationshipQuery,
    ) -> Result<Box<dyn Iterator<Item = Result<Relationship, StorageError>> + '_>, StorageError>
    {
        Err(StorageError::NotImplemented("relationships"))
    }

    fn history(
        &mut self,
        _start: HistoryPosition,
    ) -> Result<Box<dyn Iterator<Item = Result<HistoryEntry, StorageError>> + '_>, StorageError>
    {
        Err(StorageError::NotImplemented("history"))
    }

    fn content(&mut self, _content: &ContentRef) -> Result<Box<dyn ContentReader>, StorageError> {
        Err(StorageError::NotImplemented("content"))
    }
}

impl StorageEngine for NativeStore {
    type View = NativeView;

    fn begin(&self) -> Result<NativeView, StorageError> {
        Err(StorageError::NotImplemented("begin"))
    }

    fn stage_content(&self, _bytes: &mut dyn Read) -> Result<StagedContent, StorageError> {
        Err(StorageError::NotImplemented("stage_content"))
    }

    fn commit(
        &self,
        _attempt: AttemptId,
        _basis: NativeView,
        _changes: ChangeSet,
    ) -> Result<CommitReceipt, CommitError> {
        Err(CommitError::Rejected(StorageError::NotImplemented(
            "commit",
        )))
    }

    fn outcome(&self, _attempt: &AttemptId) -> Result<AttemptOutcome, StorageError> {
        Err(StorageError::NotImplemented("outcome"))
    }

    fn close(self) -> Result<(), StorageError> {
        Err(StorageError::NotImplemented("close"))
    }
}
