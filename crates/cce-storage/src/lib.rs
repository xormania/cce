//! Native storage engine for CCE.
//!
//! Physical structures are private. The public surface is the logical port from
//! `cce-core` plus constructors for a native store. See `docs/storage-format.md`
//! and `docs/storage-failure-model.md` for the implemented format and guarantees.

#[cfg(test)]
mod attempt_tests;
#[cfg(test)]
mod bundle_tests;
mod codec;
#[cfg(test)]
mod content_tests;
#[cfg(test)]
mod crash_tests;
#[cfg(test)]
mod descriptor_tests;
mod engine;
#[cfg(test)]
mod engine_tests;
mod format;
mod io;
#[cfg(test)]
mod reader_race_tests;
#[cfg(test)]
mod readonly_tests;
mod records;
#[cfg(test)]
mod recovery_tests;
#[cfg(test)]
mod repro_tests;
#[cfg(test)]
mod sim;
mod tree;
#[cfg(test)]
mod tree_reopen_tests;
#[cfg(test)]
mod view_gap_tests;
#[cfg(test)]
mod view_tests;

use std::io::Read;
use std::path::Path;
use std::sync::Arc;

use cce_core::storage::{
    AttemptId, AttemptOutcome, ChangeSet, CommitError, CommitReceipt, ContentReader, ContentRef,
    ContextId, ContextRecord, HistoryEntry, HistoryPosition, ReadView, Relationship,
    RelationshipQuery, StagedContent, StorageEngine, StorageError,
};

use engine::{Inner, Observation, Options, Snapshot};

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
        let opts: Options = options.into();
        let inner = Inner::create(
            Box::new(io::FsIo::new(dir)),
            format::StoreId(store_id),
            opts,
        )?;
        Ok(Self { inner })
    }

    /// Like [`NativeStore::open`], with explicit tuning options.
    pub fn open_with(dir: &Path, options: StoreOptions) -> Result<Self, StorageError> {
        let opts: Options = options.into();
        let inner = Inner::open(Box::new(io::FsIo::new(dir)), opts)?;
        Ok(Self { inner })
    }

    /// Instrumentation counters and current tree shape. Counters never affect engine behavior.
    pub fn stats(&self) -> Result<StoreStats, StorageError> {
        stats_of(&self.inner)
    }

    /// Admits a commit without waiting for it. The commit proceeds even if the ticket is dropped.
    pub fn submit(
        &self,
        attempt: AttemptId,
        basis: NativeView,
        changes: ChangeSet,
    ) -> Result<CommitTicket, CommitError> {
        if !Arc::ptr_eq(&basis.inner, &self.inner) {
            return Err(CommitError::Rejected(StorageError::ForeignHandle));
        }
        Ok(CommitTicket {
            rx: self
                .inner
                .submit(attempt, basis.snap, basis.observed, changes)?,
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

    /// Opens an existing store in `dir` for read-only inspection. See [`NativeReadOnly`].
    pub fn open_read_only(dir: &Path) -> Result<NativeReadOnly, StorageError> {
        Self::open_read_only_with(dir, StoreOptions::default())
    }

    /// Like [`NativeStore::open_read_only`], with explicit tuning options.
    pub fn open_read_only_with(
        dir: &Path,
        options: StoreOptions,
    ) -> Result<NativeReadOnly, StorageError> {
        let inner = Inner::open_read_only(Box::new(io::FsIo::new(dir)), options.into())?;
        Ok(NativeReadOnly { inner })
    }
}

/// A read-only inspection handle on a store that no process owns.
///
/// Opening selects the committed state exactly as a read-write open does, in memory only: it never
/// writes, syncs, repairs or creates anything. While a process owns the store, the open fails with
/// [`StorageError::Busy`]; while this handle, or any view or content reader derived from it, is
/// alive, read-write opens fail with `Busy` and further read-only opens succeed.
///
/// A state the previous owner wrote but never synced may be visible here without being durable; it
/// becomes durable only when a read-write open recovers the store. Attempt outcomes are therefore
/// not offered: reconcile them through a read-write [`NativeStore`].
pub struct NativeReadOnly {
    inner: Arc<Inner>,
}

impl NativeReadOnly {
    /// A consistent view of the selected committed state.
    pub fn begin(&self) -> Result<NativeView, StorageError> {
        Ok(NativeView {
            inner: self.inner.clone(),
            snap: self.inner.snapshot()?,
            observed: Vec::new(),
        })
    }

    /// Recovery diagnostics from opening, such as an ignored damaged publication slot.
    pub fn recovery_diagnostics(&self) -> &[String] {
        &self.inner.diagnostics
    }

    /// Instrumentation counters and current tree shape.
    pub fn stats(&self) -> Result<StoreStats, StorageError> {
        stats_of(&self.inner)
    }
}

fn stats_of(inner: &Inner) -> Result<StoreStats, StorageError> {
    let c = &inner.counters;
    let get = |a: &std::sync::atomic::AtomicU64| a.load(std::sync::atomic::Ordering::Relaxed);
    Ok(StoreStats {
        pages_written: get(&c.pages_written),
        page_bytes_written: get(&c.page_bytes_written),
        content_bytes_written: get(&c.content_bytes_written),
        node_cache_misses: get(&c.cache_misses),
        node_cache_evictions: get(&c.cache_evictions),
        node_cache_capacity: inner.cache_capacity(),
        tree_height: inner.height()?,
    })
}

/// An admitted commit. Dropping the ticket does not cancel the commit; reconcile its outcome with
/// [`StorageEngine::outcome`] using the attempt identity.
pub struct CommitTicket {
    rx: std::sync::mpsc::Receiver<Result<CommitReceipt, CommitError>>,
}

impl CommitTicket {
    /// Waits for the commit's result.
    pub fn wait(self) -> Result<CommitReceipt, CommitError> {
        self.rx.recv().unwrap_or_else(|_| {
            Err(CommitError::Indeterminate(
                "commit worker stopped before answering; reconcile with outcome".into(),
            ))
        })
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
            ..Options::default()
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
    /// Logical reads made through this view; a commit from it is validated against them.
    observed: Vec<Observation>,
}

impl ReadView for NativeView {
    fn context(&mut self, id: &ContextId) -> Result<Option<ContextRecord>, StorageError> {
        self.observed
            .push(Observation::Point(engine::context_key(id)));
        self.inner.context(&self.snap, id)
    }

    fn relationships(
        &mut self,
        query: &RelationshipQuery,
    ) -> Result<Box<dyn Iterator<Item = Result<Relationship, StorageError>> + '_>, StorageError>
    {
        // Registered before any result is produced, so an empty or partly consumed range is protected.
        self.observed
            .push(Observation::Range(engine::relationship_range(query)));
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
            observed: Vec::new(),
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
        self.submit(attempt, basis, changes)?.wait()
    }

    fn outcome(&self, attempt: &AttemptId) -> Result<AttemptOutcome, StorageError> {
        self.inner.outcome(attempt)
    }

    fn close(self) -> Result<(), StorageError> {
        self.inner.close()
    }
}
