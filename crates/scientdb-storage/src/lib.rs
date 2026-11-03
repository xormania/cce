//! Native storage engine for ScientDB.
//!
//! Physical structures are private. The public surface is the logical port from
//! `scientdb-kernel` plus constructors for a native store. See `docs/storage-format.md`
//! and `docs/storage-failure-model.md` for the implemented format and guarantees.

mod codec;
#[cfg(test)]
mod crash_tests;
mod engine;
#[cfg(test)]
mod engine_tests;
mod format;
mod io;
mod records;
#[cfg(test)]
mod sim;
mod tree;

use std::io::Read;
use std::path::Path;
use std::sync::Arc;

use scientdb_kernel::storage::{
    AttemptId, AttemptOutcome, ChangeSet, CommitError, CommitReceipt, ContentReader, ContentRef,
    ContextId, ContextRecord, HistoryEntry, HistoryPosition, ReadView, Relationship,
    RelationshipQuery, StagedContent, StorageEngine, StorageError,
};

use engine::{Inner, Snapshot};

/// A native store owned by this process.
///
/// Ownership of the store directory is exclusive and lasts until this handle and every view or
/// content reader derived from it are dropped.
pub struct NativeStore {
    inner: Arc<Inner>,
}

impl NativeStore {
    /// Creates a new store in `dir`. `store_id` is the caller-supplied identity recorded in every
    /// page; it lets the engine reject pages and handles from other stores. Refuses to create over
    /// an existing store.
    pub fn create(dir: &Path, store_id: [u8; 16]) -> Result<Self, StorageError> {
        Ok(Self {
            inner: Inner::create(Box::new(io::FsIo::new(dir)), format::StoreId(store_id))?,
        })
    }

    /// Recovery diagnostics from opening this store, such as an ignored damaged publication slot.
    pub fn recovery_diagnostics(&self) -> &[String] {
        &self.inner.diagnostics
    }

    /// Opens an existing store in `dir`. Never initializes a new store in its place.
    pub fn open(dir: &Path) -> Result<Self, StorageError> {
        Ok(Self {
            inner: Inner::open(Box::new(io::FsIo::new(dir)))?,
        })
    }
}

/// A consistent view of one committed state of a [`NativeStore`].
pub struct NativeView {
    inner: Arc<Inner>,
    snap: Snapshot,
}

impl ReadView for NativeView {
    fn context(&mut self, id: &ContextId) -> Result<Option<ContextRecord>, StorageError> {
        self.inner.context(&self.snap, id)
    }

    fn relationships(
        &mut self,
        query: &RelationshipQuery,
    ) -> Result<Box<dyn Iterator<Item = Result<Relationship, StorageError>> + '_>, StorageError>
    {
        Ok(Box::new(
            self.inner
                .relationships(&self.snap, query)?
                .into_iter()
                .map(Ok),
        ))
    }

    fn history(
        &mut self,
        start: HistoryPosition,
    ) -> Result<Box<dyn Iterator<Item = Result<HistoryEntry, StorageError>> + '_>, StorageError>
    {
        Ok(Box::new(self.inner.history(&self.snap, start)?))
    }

    fn content(&mut self, content: &ContentRef) -> Result<Box<dyn ContentReader>, StorageError> {
        Ok(Box::new(self.inner.content(&self.snap, content)?))
    }
}

impl StorageEngine for NativeStore {
    type View = NativeView;

    fn begin(&self) -> Result<NativeView, StorageError> {
        Ok(NativeView {
            inner: self.inner.clone(),
            snap: self.inner.snapshot()?,
        })
    }

    fn stage_content(&self, bytes: &mut dyn Read) -> Result<StagedContent, StorageError> {
        self.inner.stage(bytes)
    }

    fn commit(
        &self,
        attempt: AttemptId,
        basis: NativeView,
        changes: ChangeSet,
    ) -> Result<CommitReceipt, CommitError> {
        if !Arc::ptr_eq(&basis.inner, &self.inner) {
            return Err(CommitError::Rejected(StorageError::ForeignHandle));
        }
        self.inner.commit(attempt, basis.snap, changes)
    }

    fn outcome(&self, attempt: &AttemptId) -> Result<AttemptOutcome, StorageError> {
        self.inner.outcome(attempt)
    }

    fn close(self) -> Result<(), StorageError> {
        self.inner.close()
    }
}
