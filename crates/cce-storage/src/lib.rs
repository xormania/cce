//! Native storage engine for CCE.
//!
//! Physical structures are private. The public surface is the logical port from
//! `cce-core` plus constructors for a native store. See `docs/storage-format.md`
//! and `docs/storage-failure-model.md` for the implemented format and guarantees.

#[cfg(test)]
mod attempt_tests;
#[cfg(test)]
mod audit;
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
#[cfg(test)]
mod fs_seam_tests;
mod io;
#[cfg(test)]
mod key_bounds_tests;
#[cfg(test)]
mod maintain_tests;
#[cfg(test)]
mod quiescence_tests;
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
mod resource_tests;
#[cfg(test)]
mod reuse_tests;
#[cfg(test)]
mod sim;
#[cfg(test)]
mod split_sweep_tests;
#[cfg(test)]
mod test_support;
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

use engine::{Inner, Observation, Options, Pin, Snapshot};

/// A native store owned by this process.
///
/// Ownership of the store directory is exclusive and lasts until this handle and every view or
/// content reader derived from it are dropped.
pub struct NativeStore {
    inner: Arc<Inner>,
}

/// A physical maintenance task for [`NativeStore::maintain`]. Maintenance changes only where and
/// how committed information is stored, never what it is: it writes no logical record, adds no
/// history, and causes no conflict with logical commits.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum MaintenanceTask {
    /// Retire content-file space below the committed content length that no published structure
    /// reaches and no retirement record covers (left by stagings that were covered by a later
    /// commit and then abandoned or discarded across a restart), so later staging can reuse it.
    ReclaimContentHoles,
    /// Rebuild both relationship indexes from the relationship records.
    RebuildRelationshipIndexes,
    /// Move committed content and metadata pages from the end of their files into retired space
    /// nearer the start.
    Relocate,
}

/// How much one [`NativeStore::maintain`] call may do before it publishes and returns.
/// [`MaintenanceTask::RebuildRelationshipIndexes`] is not bounded by it: it rebuilds both indexes
/// in one publication.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MaintenanceBudget {
    /// Most tree pages a [`MaintenanceTask::Relocate`] step moves.
    pub max_pages: u64,
    /// Most content bytes a step moves ([`MaintenanceTask::Relocate`]) or retires
    /// ([`MaintenanceTask::ReclaimContentHoles`]).
    pub max_content_bytes: u64,
}

impl Default for MaintenanceBudget {
    fn default() -> Self {
        Self {
            max_pages: 1024,
            max_content_bytes: 64 << 20,
        }
    }
}

/// What one [`NativeStore::maintain`] call did.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct MaintenanceReport {
    /// The generation current after the call (unchanged if there was nothing to do).
    pub generation: u64,
    /// Tree pages rewritten into retired pages.
    pub pages_moved: u64,
    /// Committed content bytes copied into retired space (manifests and segments).
    pub content_bytes_moved: u64,
    /// Content-file bytes retired as unreachable.
    pub content_bytes_reclaimed: u64,
    /// Relationship index entries written by an index rebuild.
    pub index_entries_written: u64,
    /// Content streams a relocation skipped because each alone is larger than the budget's
    /// `max_content_bytes`: no call with this budget can move them, a call with a larger one can.
    pub content_streams_over_budget: u64,
    /// False if work remains: the budget ran out before the task was done, so another call
    /// continues, or a stream is over the budget (see `content_streams_over_budget`), which only a
    /// larger budget moves. False for that reason alone, it stays false across identical calls.
    pub complete: bool,
}

impl NativeStore {
    /// Runs one bounded step of physical maintenance and publishes its result as a new physical
    /// generation, ordered with commits. Returns at once with nothing done if there is nothing to do.
    /// Never run implicitly: opening a store does no maintenance.
    ///
    /// Refused with [`CommitError::Rejected`], writing nothing, while writes are fenced. A step
    /// publishes through the same data barrier as a commit, and a failure there is reported and
    /// fences writes as it does for a commit; a crash at any point recovers the state before or
    /// after the step.
    pub fn maintain(
        &self,
        task: MaintenanceTask,
        budget: MaintenanceBudget,
    ) -> Result<MaintenanceReport, CommitError> {
        self.inner.maintain(task, budget)
    }

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
        // The basis view is consumed here: release its pin before the commit can run, so a commit
        // requiring quiescence never counts its own basis as a live reader.
        let NativeView {
            snap,
            observed,
            pin,
            ..
        } = basis;
        drop(pin);
        Ok(CommitTicket {
            rx: self.inner.submit(attempt, snap, observed, changes)?,
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
        NativeView::new(&self.inner)
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

fn stats_of(inner: &Arc<Inner>) -> Result<StoreStats, StorageError> {
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
        pages_reused: get(&c.pages_reused),
        content_bytes_reused: get(&c.content_bytes_reused),
        reusable_content_bytes: inner.reusable_content_bytes(),
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
    /// Tree pages written over retired pages instead of at new pages.
    pub pages_reused: u64,
    /// Content bytes written into retired content extents instead of at the end, counted when a
    /// publication consumes those extents (space a staging or relocation took and gave back again
    /// does not count).
    pub content_bytes_reused: u64,
    /// Retired content bytes currently available for reuse (not held by a staging).
    pub reusable_content_bytes: u64,
}

/// A consistent view of one committed state of a [`NativeStore`].
pub struct NativeView {
    inner: Arc<Inner>,
    snap: Snapshot,
    /// Logical reads made through this view; a commit from it is validated against them.
    observed: Vec<Observation>,
    /// Keeps the generation this view reads from being reused while the view lives.
    pin: Pin,
}

impl NativeView {
    fn new(inner: &Arc<Inner>) -> Result<Self, StorageError> {
        let (snap, pin) = inner.pin_current()?;
        Ok(Self {
            inner: inner.clone(),
            snap,
            observed: Vec::new(),
            pin,
        })
    }
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
        Ok(Box::new(
            self.inner
                .content(&self.snap, content)?
                .pinned(self.pin.clone()),
        ))
    }
}

impl StorageEngine for NativeStore {
    type View = NativeView;

    fn begin(&self) -> Result<NativeView, StorageError> {
        NativeView::new(&self.inner)
    }

    fn stage_content(&self, bytes: &mut dyn Read) -> Result<StagedContent, StorageError> {
        self.inner.stage(bytes)
    }

    fn discard_staged(&self, staged: StagedContent) -> Result<(), StorageError> {
        self.inner.discard_staged(staged)
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
