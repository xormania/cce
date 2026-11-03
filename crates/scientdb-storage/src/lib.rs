//! Native storage engine for ScientDB.
//!
//! Physical structures are private. The public surface is the logical port from
//! `scientdb-kernel` plus constructors for a native store. See `docs/storage-format.md`
//! and `docs/storage-failure-model.md` for the implemented format and guarantees.

mod codec;
#[cfg(test)]
mod content_tests;
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

use engine::{Inner, Options, Snapshot};

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
        Self::create_with(dir, store_id, StoreOptions::default())
    }

    /// Like [`NativeStore::create`], with explicit tuning options.
    pub fn create_with(
        dir: &Path,
        store_id: [u8; 16],
        options: StoreOptions,
    ) -> Result<Self, StorageError> {
        let inner = Inner::create(
            Box::new(io::FsIo::new(dir)),
            format::StoreId(store_id),
            options.into(),
        )?;
        Ok(Self { inner })
    }

    /// Like [`NativeStore::open`], with explicit tuning options.
    pub fn open_with(dir: &Path, options: StoreOptions) -> Result<Self, StorageError> {
        Ok(Self {
            inner: Inner::open(Box::new(io::FsIo::new(dir)), options.into())?,
        })
    }

    /// Instrumentation counters and current tree shape. Counters never affect engine behavior.
    pub fn stats(&self) -> Result<StoreStats, StorageError> {
        let c = &self.inner.counters;
        let get = |a: &std::sync::atomic::AtomicU64| a.load(std::sync::atomic::Ordering::Relaxed);
        Ok(StoreStats {
            pages_written: get(&c.pages_written),
            page_bytes_written: get(&c.page_bytes_written),
            content_bytes_written: get(&c.content_bytes_written),
            node_cache_misses: get(&c.cache_misses),
            node_cache_evictions: get(&c.cache_evictions),
            node_cache_capacity: self.inner.cache_capacity(),
            tree_height: self.inner.height()?,
        })
    }

    /// Recovery diagnostics from opening this store, such as an ignored damaged publication slot.
    pub fn recovery_diagnostics(&self) -> &[String] {
        &self.inner.diagnostics
    }

    /// Opens an existing store in `dir`. Never initializes a new store in its place.
    pub fn open(dir: &Path) -> Result<Self, StorageError> {
        Self::open_with(dir, StoreOptions::default())
    }
}

/// Tuning that does not change stored bytes.
#[derive(Clone, Copy, Debug)]
pub struct StoreOptions {
    /// Decoded metadata nodes kept in memory. Larger stores still work with a small cache; they
    /// read more pages.
    pub cache_nodes: usize,
}

impl Default for StoreOptions {
    fn default() -> Self {
        Self {
            cache_nodes: engine::DEFAULT_CACHE_NODES,
        }
    }
}

impl From<StoreOptions> for Options {
    fn from(o: StoreOptions) -> Self {
        Options {
            cache_nodes: o.cache_nodes,
        }
    }
}

/// Instrumentation for one open store (since it was opened).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StoreStats {
    pub pages_written: u64,
    pub page_bytes_written: u64,
    pub content_bytes_written: u64,
    pub node_cache_misses: u64,
    pub node_cache_evictions: u64,
    pub node_cache_capacity: usize,
    /// Height of the current metadata tree (1 = a single leaf).
    pub tree_height: usize,
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
        Ok(Box::new(self.inner.relationships(&self.snap, query)?))
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

    fn stage_content_expecting(
        &self,
        bytes: &mut dyn Read,
        expected: &ContentRef,
    ) -> Result<StagedContent, StorageError> {
        self.inner.stage_checked(bytes, Some(expected))
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
