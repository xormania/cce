//! Native storage engine for CCE.
//!
//! Physical structures are private. The public surface is the logical port from
//! `cce-core` plus constructors for a native store. See `docs/storage-format.md`
//! and `docs/storage-failure-model.md` for the implemented format and guarantees.

#[cfg(test)]
mod attempt_tests;
#[cfg(test)]
mod audit;
mod codec;
#[cfg(test)]
mod content_tests;
#[cfg(test)]
mod crash_tests;
#[cfg(test)]
mod decoder_tests;
#[cfg(test)]
mod descriptor_tests;
mod engine;
#[cfg(test)]
mod engine_tests;
mod format;
#[cfg(any(test, feature = "fuzz-api"))]
mod fuzz_api;
#[cfg(test)]
mod fuzz_replay_tests;
#[cfg(test)]
mod guard_tests;
mod io;
#[cfg(test)]
mod oracle_tests;
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
mod reuse_tests;
#[cfg(any(test, feature = "fuzz-api"))]
mod sim;
mod tree;
#[cfg(test)]
mod tree_reopen_tests;
mod verify;
#[cfg(test)]
mod verify_tests;
#[cfg(test)]
mod view_gap_tests;
#[cfg(test)]
mod view_tests;

/// Compile-time probes of the public surface, run as doctests. The listed items are reachable from
/// outside the crate:
///
/// ```
/// use cce_storage::{
///     CommitTicket, Finding, NativeReadOnly, NativeStore, NativeView, Report, StoreOptions,
///     StoreStats, VerifyLevel,
/// };
/// ```
///
/// and no physical module is. Each probe names the module itself, so it compiles exactly when the
/// module is public (an item probe would also fail on the item's own visibility):
///
/// ```compile_fail
/// use cce_storage::tree as _;
/// ```
/// ```compile_fail
/// use cce_storage::format as _;
/// ```
/// ```compile_fail
/// use cce_storage::records as _;
/// ```
/// ```compile_fail
/// use cce_storage::engine as _;
/// ```
/// ```compile_fail
/// use cce_storage::io as _;
/// ```
/// ```compile_fail
/// use cce_storage::codec as _;
/// ```
/// ```compile_fail
/// use cce_storage::verify as _;
/// ```
#[cfg(doctest)]
mod surface_probes {}

use std::io::Read;
use std::path::Path;
use std::sync::Arc;

use cce_core::storage::{
    AttemptId, AttemptOutcome, ChangeSet, CommitError, CommitReceipt, ContentReader, ContentRef,
    ContextId, ContextRecord, HistoryEntry, HistoryPosition, ReadView, Relationship,
    RelationshipQuery, StagedContent, StorageEngine, StorageError,
};

use engine::{Inner, Observation, Options, Pin, Snapshot};

/// Fuzzing entry point, with the `fuzz-api` feature only: runs the named target of the fuzz harness
/// (`page`, `slots`, `node`, `manifest`, `history`, `records` or `image`) on `data`. Not part of the
/// storage API.
#[cfg(feature = "fuzz-api")]
pub fn fuzz_target(name: &str, data: &[u8]) {
    fuzz_api::run(name, data);
}

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

/// How thoroughly [`NativeReadOnly::verify`] inspects a store.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum VerifyLevel {
    /// Both publication slots, every metadata page reachable from the current publication record,
    /// every record in the tree and the relations between records, and the history entries with
    /// their attempt records. Content bytes are not read.
    Structure,
    /// Everything [`VerifyLevel::Structure`] checks, and every committed content stream: each
    /// manifest is read and matches its record, every segment matches its digest, the stream
    /// matches the content's digest and length, and each context's binding matches its manifest.
    Scrub,
}

/// One inconsistency found by [`NativeReadOnly::verify`].
///
/// Findings never contain stored bodies, content, evidence or identifiers: a record is named by its
/// key family and a hash prefix of its key, a page by its number.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Finding {
    /// Stable name of the check that failed (for example `relationship-index`).
    pub check: &'static str,
    pub location: String,
    pub detail: String,
}

/// The result of [`NativeReadOnly::verify`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Report {
    /// The publication record that was inspected.
    pub generation: u64,
    pub history_position: u64,
    /// What each publication slot holds.
    pub slots: [String; 2],
    /// Recovery diagnostics from opening the store.
    pub diagnostics: Vec<String>,
    /// Metadata pages and tree records visited.
    pub pages: u64,
    pub records: u64,
    /// Content bytes read and verified (zero at the structure level).
    pub content_bytes: u64,
    /// Inconsistencies, in a deterministic order.
    pub findings: Vec<Finding>,
}

impl Report {
    /// True if no check failed.
    pub fn is_consistent(&self) -> bool {
        self.findings.is_empty()
    }

    /// The report as JSON objects, one per line, in a deterministic order: a summary, the two
    /// slots, the diagnostics, the findings, and a result line.
    pub fn json_lines(&self) -> Vec<String> {
        let q = |s: &str| verify::json_string(s);
        let mut out = vec![format!(
            "{{\"type\":\"summary\",\"generation\":{},\"history_position\":{},\"pages\":{},\"records\":{},\"content_bytes\":{}}}",
            self.generation, self.history_position, self.pages, self.records, self.content_bytes
        )];
        for (i, s) in self.slots.iter().enumerate() {
            out.push(format!(
                "{{\"type\":\"slot\",\"slot\":{i},\"state\":{}}}",
                q(s)
            ));
        }
        for d in &self.diagnostics {
            out.push(format!("{{\"type\":\"diagnostic\",\"text\":{}}}", q(d)));
        }
        for f in &self.findings {
            out.push(format!(
                "{{\"type\":\"finding\",\"check\":{},\"location\":{},\"detail\":{}}}",
                q(f.check),
                q(&f.location),
                q(&f.detail)
            ));
        }
        out.push(format!(
            "{{\"type\":\"result\",\"consistent\":{},\"findings\":{}}}",
            self.is_consistent(),
            self.findings.len()
        ));
        out
    }
}

impl NativeReadOnly {
    /// Inspects the opened store's structure without changing it. Returns `Err` only when the
    /// store cannot be inspected; inconsistencies are findings in the report. Because a read-only
    /// open is refused while another process owns the store, this never runs against a live owner.
    pub fn verify(&self, level: VerifyLevel) -> Result<Report, StorageError> {
        verify::run(&self.inner, level)
    }

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

/// Options that never change stored bytes. Construct with `StoreOptions::default()` and set
/// fields; new fields may be added.
#[derive(Clone, Copy, Debug)]
#[non_exhaustive]
pub struct StoreOptions {
    /// Decoded metadata nodes kept in memory. Larger stores still work with a small cache; they
    /// read more pages.
    pub cache_nodes: usize,
    /// Largest encoded request this handle writes, 64 MiB by default; a larger request is refused
    /// whole, never split. Any value up to the format's limit (`u32::MAX` bytes) may be set, and
    /// lowering it never makes data written under a larger value unreadable: reads are bounded by
    /// the format, not by this.
    pub max_request_len: u64,
    /// Checked when the store is opened, read-write or read-only; `None` opens without it.
    pub guard: Option<ReceiptGuard>,
}

/// What a caller retained independently of a store, checked before an open touches its data.
///
/// An open with a guard is refused with [`StorageError::GuardRefused`], and leaves the store's
/// data untouched (a read-write open may have created the store's lock file), when the store has
/// another identity or its published logical history position is below `min_history_position`.
/// This is a rollback floor only: a position at or above the minimum does not prove that the
/// history is the one the receipts came from. Construct it with [`ReceiptGuard::new`]; fields may
/// be added.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct ReceiptGuard {
    /// The identity the store was created with.
    pub store_id: [u8; 16],
    /// The highest history position of a receipt the caller retained (`HistoryPosition(0)` if
    /// none): the published position must be at least this.
    pub min_history_position: cce_core::storage::HistoryPosition,
}

impl ReceiptGuard {
    /// A guard for the store created with `store_id`, whose published history position must be
    /// at least `min_history_position`.
    pub fn new(
        store_id: [u8; 16],
        min_history_position: cce_core::storage::HistoryPosition,
    ) -> Self {
        Self {
            store_id,
            min_history_position,
        }
    }
}

impl Default for StoreOptions {
    fn default() -> Self {
        Self {
            cache_nodes: engine::DEFAULT_CACHE_NODES,
            max_request_len: engine::DEFAULT_MAX_REQUEST_LEN,
            guard: None,
        }
    }
}

impl From<StoreOptions> for Options {
    fn from(o: StoreOptions) -> Self {
        Options {
            cache_nodes: o.cache_nodes,
            max_request_len: o.max_request_len,
            guard: o.guard,
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
    /// Staged content bytes written into retired content extents instead of at the end.
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
