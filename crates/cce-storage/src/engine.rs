//! Engine core: open/create, recovery selection, staging, the commit protocol, and reads.
//!
//! Commit protocol (see `docs/storage-failure-model.md`):
//! 1. validate the basis and the attempt identity against the latest committed state;
//! 2. edit a copy-on-write working copy of the newest committed tree, write the history entry to
//!    the content file and every touched tree node to a fresh metadata page (no page reachable from
//!    a publication record is ever overwritten);
//! 3. data barrier: sync the content file, then the metadata file;
//! 4. write the publication record for generation g+1 into slot (g+1) mod 2;
//! 5. publication barrier: sync the metadata file;
//! 6. expose the new generation to new views and acknowledge.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};
use std::io::{self, Read};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, mpsc};

use cce_core::storage::{
    AttemptId, AttemptOutcome, Change, ChangeSet, CommitError, CommitReceipt, ContentDigest,
    ContentReader, ContentRef, ContextId, ContextRecord, Direction, HistoryEntry, HistoryPosition,
    MAX_ID_LEN, Precondition, Relationship, RelationshipId, RelationshipKind, RelationshipQuery,
    RemovalMode, Revision, StagedContent, StagingToken, StorageError,
};
use sha2::{Digest, Sha256};

use crate::codec::{Dec, Enc, corrupt, key_component, key_components};
use crate::format::{
    Descriptor, FEATURE_HISTORY_MARKERS, FIRST_NODE_PAGE, PAGE_SIZE, PageKind, PageRef, Slot,
    StoreId, decode_page, decode_slot, encode_page,
};
use crate::io::{FileId, Io};
use crate::ledger::{ContentPool, Reuse, overlap};
use crate::records::{
    AttemptValue, ContentEntry, ContextValue, ExtentRef, Manifest, P_ATTEMPT, P_CONTENT, P_CONTEXT,
    P_HISTORY, P_HISTORY_REMOVED, P_IN, P_OUT, P_REL, P_RETIRED, RelValue, Retired, decode_history,
    decode_retired, encode_history, fingerprint, history_key, key, removed_history_key,
    retired_extent_key, retired_page_key,
};
use crate::tree::{self, Cursor, MAX_KEY, Node, NodeSink, NodeSource, TreeTxn};

/// Content streaming buffer. Payloads of any length stream through a buffer of this size, and
/// staging fills one segment per buffer, so it is the format's segment bound.
pub(crate) const CONTENT_CHUNK: usize = crate::records::SEGMENT_MAX as usize;
/// Default segment quota per content stream: 2^20 segments of 64 KiB (64 GiB).
pub(crate) const DEFAULT_MAX_CONTENT_SEGMENTS: usize = 1 << 20;
/// Default number of decoded tree nodes kept in memory.
pub(crate) const DEFAULT_CACHE_NODES: usize = 256;

/// Staged or bound content: its manifest's location and its logical reference.
type BoundContent = (ExtentRef, ContentRef);

/// Total content length of the outstanding stagings in `st`.
fn staged_total(st: &State) -> u64 {
    st.staged.values().map(|(_, c)| c.len).sum()
}

static NEXT_INSTANCE: AtomicU64 = AtomicU64::new(1);

/// Engine tuning that does not affect stored bytes.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Options {
    pub cache_nodes: usize,
    /// Commits whose write sets are retained for validating older read bases.
    pub conflict_window: usize,
    /// Admitted commits that may wait for the commit worker before admission is refused.
    pub commit_queue: usize,
    /// Largest number of segments one content stream may have (bounds staging and manifest memory).
    pub max_content_segments: usize,
    /// Largest length of any variable-length request field (record body, evidence label or bytes)
    /// and of any request list; bounded by the 32-bit length prefixes of the encoding.
    pub max_request_field: u64,
    /// Largest encoded size of a whole request this open writes, which history stores as one
    /// length-prefixed field. Reads are bounded by the format, not by this.
    pub max_request_len: u64,
    /// Largest total content length of the stagings outstanding at once.
    pub max_staged_bytes: u64,
    /// Retained receipt information an open checks before touching the store.
    pub guard: Option<crate::ReceiptGuard>,
}

/// Default number of recent commits whose write sets are kept for conflict validation.
pub(crate) const DEFAULT_CONFLICT_WINDOW: usize = 4096;
/// Default number of admitted commits that may wait for the commit worker.
pub(crate) const DEFAULT_COMMIT_QUEUE: usize = 64;
/// Default largest encoded request a store writes: 64 MiB, well below the format's `u32` limit.
pub(crate) const DEFAULT_MAX_REQUEST_LEN: u64 = 64 << 20;
/// Default largest total content length of outstanding stagings: 64 GiB, the default largest
/// single content stream (2^20 segments of 64 KiB).
pub(crate) const DEFAULT_MAX_STAGED_BYTES: u64 = 64 << 30;

impl Default for Options {
    fn default() -> Self {
        Self {
            cache_nodes: DEFAULT_CACHE_NODES,
            conflict_window: DEFAULT_CONFLICT_WINDOW,
            commit_queue: DEFAULT_COMMIT_QUEUE,
            max_content_segments: DEFAULT_MAX_CONTENT_SEGMENTS,
            max_request_field: u32::MAX as u64,
            max_request_len: DEFAULT_MAX_REQUEST_LEN,
            max_staged_bytes: DEFAULT_MAX_STAGED_BYTES,
            guard: None,
        }
    }
}

/// Counters for instrumentation; they never influence engine decisions.
#[derive(Default)]
pub(crate) struct Counters {
    pub pages_written: AtomicU64,
    pub page_bytes_written: AtomicU64,
    pub content_bytes_written: AtomicU64,
    pub cache_misses: AtomicU64,
    pub cache_evictions: AtomicU64,
    /// Tree pages written over retired pages.
    pub pages_reused: AtomicU64,
    /// Staged content bytes written into retired content extents.
    pub content_bytes_reused: AtomicU64,
    pub pages_read: AtomicU64,
    pub pages_read_at_open: AtomicU64,
    pub entries_visited: AtomicU64,
    pub content_buffer_bytes: AtomicU64,
    pub content_buffer_bytes_peak: AtomicU64,
}

/// The committed state a view reads.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Snapshot {
    pub desc: Descriptor,
}

struct State {
    current: Descriptor,
    /// Staged, not yet committed extents of this open instance: serial → extent.
    staged: HashMap<u64, BoundContent>,
    /// Next staging serial; each staging gets a distinct handle even for zero-length content.
    next_stage: u64,
    /// Set when durability became uncertain; further writes are refused until reopen.
    fenced: Option<Fence>,
    /// A commit requiring quiescence is in progress: views register only after it ends, and so
    /// read the state it publishes.
    quiescing: bool,
}

/// Why writes are fenced, and which attempt (if any) has an unknown publication outcome.
#[derive(Clone)]
struct Fence {
    why: String,
    uncertain: Option<AttemptId>,
}

/// Bounded node cache keyed by location *and* incarnation, evicting in insertion order.
struct NodeCache {
    map: HashMap<PageRef, Arc<Node>>,
    order: VecDeque<PageRef>,
    capacity: usize,
}

pub(crate) struct Inner {
    io: Box<dyn Io>,
    store: StoreId,
    instance: u64,
    state: Recorded<State>,
    /// Serializes commits (one publication order per store).
    writer: Mutex<()>,
    /// Content-file allocation cursor; held while streaming an allocation.
    alloc: Mutex<u64>,
    /// Retired content extents staging may reuse, and those stagings of this instance hold.
    pool: Mutex<ContentPool>,
    /// Inconsistencies in space ownership found during cleanup, where no caller could be told.
    faults: Mutex<Vec<String>>,
    /// Next taker id for the reuse pool (one per staging call).
    next_taker: AtomicU64,
    /// Most rounds any publication of this instance took to settle its page allocation.
    allocation_rounds_max: AtomicUsize,
    cache: Mutex<NodeCache>,
    /// Logical write sets of recent commits, for validating older read bases.
    conflicts: Mutex<ConflictLog>,
    /// Sender to the commit worker.
    worker: Mutex<Option<mpsc::SyncSender<Job>>>,
    /// Attempts admitted to the worker and not yet resolved.
    /// Shared with the worker so it can resolve an attempt after releasing its engine reference.
    pending: Arc<Recorded<HashMap<AttemptId, u32>>>,
    /// Set (under the `pending` lock) once `close` began: new commits are refused from then on.
    closing: AtomicBool,
    /// Live reader pins: committed generation -> number of views and content readers reading it.
    /// Space a pinned generation can reach must not be reused.
    pins: Mutex<BTreeMap<u64, u32>>,
    /// Pins held by views and content readers (not by the engine's own short walks): what a
    /// commit requiring quiescence counts as live readers.
    readers: AtomicU64,
    /// Signalled when a quiescent commit ends.
    quiesced: Condvar,
    #[cfg(test)]
    gate: Mutex<()>,
    /// Makes the next commit panic inside the worker (tests of worker robustness only).
    #[cfg(test)]
    panic_next_commit: std::sync::atomic::AtomicBool,
    /// Runs once inside a commit, after it fixed which retired pages it may reuse and before it
    /// takes any (tests forcing reader registration against that decision).
    #[cfg(test)]
    reuse_hook: Mutex<Option<Box<dyn FnOnce() + Send>>>,
    /// Runs once inside a relocation step, after its copy and before it publishes (tests that
    /// commit in between).
    #[cfg(test)]
    relocate_hook: Mutex<Option<Box<dyn FnOnce() + Send>>>,
    /// The next relocation step applies its moves without checking that each source manifest is
    /// still the one indexed (tests of what the space check alone refuses).
    #[cfg(test)]
    skip_relocation_revalidation: std::sync::atomic::AtomicBool,
    /// The next relocation step panics right after its publication (tests of that outcome).
    #[cfg(test)]
    panic_next_maintenance: std::sync::atomic::AtomicBool,
    /// Runs once inside a commit on its space journal and tree just before the journal is sealed
    /// (tests planting a faulty change of ownership).
    #[cfg(test)]
    journal_hook: Mutex<Option<JournalHook>>,
    /// Runs once inside a commit after its publication and before the pool is updated (tests
    /// planting a conflicting pool change).
    #[cfg(test)]
    apply_hook: Mutex<Option<Box<dyn FnOnce() + Send>>>,
    /// View registrations currently waiting for a quiescent commit (tests of that interleaving).
    #[cfg(test)]
    waiting_registrations: std::sync::atomic::AtomicUsize,
    /// Runs once when `outcome` or `height` has chosen the state to walk, before its first page
    /// read (tests parking a walker while commits proceed).
    #[cfg(test)]
    walk_hook: Mutex<Option<Box<dyn FnOnce() + Send>>>,
    /// Runs once right after a commit is admitted to the worker's queue, on the submitting thread
    /// (tests of what the submitter still holds while the commit runs).
    #[cfg(test)]
    admitted_hook: Mutex<Option<Box<dyn FnOnce() + Send>>>,
    /// Makes the worker pause after answering each job (tests of shutdown ordering only).
    #[cfg(test)]
    slow_job_release: std::sync::atomic::AtomicBool,
    /// Largest number of segments one content stream may have.
    max_segments: usize,
    /// Largest total content length of the stagings outstanding at once.
    max_staged_bytes: u64,
    /// Largest variable-length request field or list (see `Options::max_request_field`).
    max_request_field: u64,
    /// Largest encoded request (see `Options::max_request_len`).
    max_request_len: u64,
    /// Opened for inspection only: no commit worker, no staging, no commits, no syncs.
    read_only: bool,
    pub counters: Counters,
    /// Recovery diagnostics from open (for example an ignored damaged slot).
    pub diagnostics: Vec<String>,
}

mod maintain;

/// Engine locks. `Recorded` keeps its mutex private to this module, so engine code can reach a
/// recorded lock only through `lock()` (a bypass would not compile).
mod locks {
    use std::sync::{Mutex, MutexGuard};

    /// Anything that hands out a mutex guard; a poisoned mutex is still usable (its owner panicked,
    /// and the commit worker fences writes in that case).
    pub(super) trait Lockable<T> {
        fn acquire(&self) -> MutexGuard<'_, T>;
    }

    impl<T> Lockable<T> for Mutex<T> {
        fn acquire(&self) -> MutexGuard<'_, T> {
            self.lock().unwrap_or_else(|p| p.into_inner())
        }
    }

    /// A mutex whose acquisitions are recorded per thread in tests, so the order in which an
    /// operation consults engine state can be checked whatever way the code reaches it.
    pub(super) struct Recorded<T> {
        mutex: Mutex<T>,
        #[cfg_attr(not(test), allow(dead_code))]
        name: &'static str,
    }

    impl<T> Recorded<T> {
        pub(super) fn new(name: &'static str, value: T) -> Self {
            Self {
                mutex: Mutex::new(value),
                name,
            }
        }
    }

    impl<T, L: Lockable<T>> Lockable<T> for std::sync::Arc<L> {
        fn acquire(&self) -> MutexGuard<'_, T> {
            (**self).acquire()
        }
    }

    impl<T> Lockable<T> for Recorded<T> {
        fn acquire(&self) -> MutexGuard<'_, T> {
            #[cfg(test)]
            LOCK_TRACE.with(|t| t.borrow_mut().push(self.name));
            self.mutex.acquire()
        }
    }

    #[cfg(test)]
    thread_local! {
        static LOCK_TRACE: std::cell::RefCell<Vec<&'static str>> = const { std::cell::RefCell::new(Vec::new()) };
    }

    /// Test hook: the recorded mutexes this thread acquired since the last call, in order.
    #[cfg(test)]
    pub(crate) fn take_lock_trace_for_test() -> Vec<&'static str> {
        LOCK_TRACE.with(|t| std::mem::take(&mut *t.borrow_mut()))
    }

    pub(super) fn lock<T>(m: &impl Lockable<T>) -> MutexGuard<'_, T> {
        m.acquire()
    }
}

#[cfg(test)]
pub(crate) use locks::take_lock_trace_for_test;
use locks::{Recorded, lock};

fn sha256(b: &[u8]) -> [u8; 32] {
    Sha256::digest(b).into()
}

/// The content file cannot grow further without wrapping its byte offsets.
fn content_space_exhausted() -> StorageError {
    StorageError::InvalidRequest("content space exhausted".into())
}

/// Resolves one admission of `attempt`: it stops being pending once every admission resolved.
fn finish_pending(pending: &Recorded<HashMap<AttemptId, u32>>, attempt: &AttemptId) {
    let mut p = lock(pending);
    if let Some(n) = p.get_mut(attempt) {
        *n -= 1;
        if *n == 0 {
            p.remove(attempt);
        }
    }
}

fn rejected(e: StorageError) -> CommitError {
    CommitError::Rejected(e)
}

/// Ends a quiescent commit when dropped (after its publication, or on any failure): views may
/// register again.
struct Quiescing<'a> {
    inner: &'a Inner,
}

impl Drop for Quiescing<'_> {
    fn drop(&mut self) {
        lock(&self.inner.state).quiescing = false;
        self.inner.quiesced.notify_all();
    }
}

/// Every change of space ownership one commit makes, sealed before it writes anything.
pub(crate) struct SpaceJournal {
    pub(crate) generation: u64,
    /// Committed pages the new tree no longer refers to, retired at `generation`.
    pub(crate) retired_pages: Vec<PageRef>,
    /// Retired pages the commit writes its new nodes into.
    pub(crate) reused_pages: Vec<u64>,
    /// Content extents retired at `generation`: removed or orphaned content, discarded stagings.
    pub(crate) retired_extents: Vec<(u64, u64)>,
    /// Unused parts of reused retirement records, as `(offset, len, retiring generation)`: they
    /// stay retired at their record's original generation.
    pub(crate) remainders: Vec<(u64, u64, u64)>,
    /// Keys of the retirement records of reserved content the commit consumes (deleted).
    pub(crate) consumed: Vec<Vec<u8>>,
    /// Keys of the retirement records of the pages it reuses (deleted).
    pub(crate) reused_records: Vec<Vec<u8>>,
    /// Extents the new published state still reaches (live manifests and segments of touched
    /// digests; for maintenance, relocation's new extents or reclaim's independent full reach)
    /// that this publication must not free.
    pub(crate) still_reachable: Vec<(u64, u64)>,
    /// Serials of the stagings the commit binds (their reservations are its to consume).
    pub(crate) bound: Vec<u64>,
}

#[cfg(test)]
pub(crate) type JournalHook = Box<dyn for<'a> FnOnce(&mut SpaceJournal, &mut TreeTxn<'a>) + Send>;

/// Content-file extents as `(offset, len)`.
type ExtentList = Vec<(u64, u64)>;

/// A retirement record's key in words, for error messages.
fn retired_key_label(k: &[u8]) -> String {
    match decode_retired(k, &[0; 8]) {
        Ok(Retired::Page {
            generation, page, ..
        }) => format!("of page {page} at generation {generation}"),
        Ok(Retired::Extent {
            generation, offset, ..
        }) => format!("of content at offset {offset}, generation {generation}"),
        Err(_) => format!("{k:?}"),
    }
}

/// Remainders of consumed reservations, `(offset, len, retiring generation)`, and the keys of the
/// consumed records.
type Consumed = (Vec<(u64, u64, u64)>, Vec<Vec<u8>>);

/// Content a commit wrote that no publication references yet. If the commit ends before its
/// publication record is written, the extent joins the discarded extents, which the next
/// publication retires with a record: it is never made free without one.
struct Unpublished<'a> {
    inner: &'a Inner,
    extent: Option<(u64, u64)>,
}

impl Drop for Unpublished<'_> {
    fn drop(&mut self) {
        if let Some(x) = self.extent.take() {
            let queued = lock(&self.inner.pool).queue_discarded(x);
            if let Err(why) = queued {
                self.inner.fault(&why);
            }
        }
    }
}

/// Extents one staging call took from the pool; returned to it unless the staging completes.
struct Taken<'a> {
    inner: &'a Inner,
    /// This staging call's taker id in the pool.
    owner: u64,
    taken: Vec<Reuse>,
}

impl Taken<'_> {
    /// Records the extents as held by staging `serial`.
    fn hold(mut self, serial: u64) {
        let taken = std::mem::take(&mut self.taken);
        if !taken.is_empty() {
            let held = lock(&self.inner.pool).hold(self.owner, serial, taken);
            if let Err(taken) = held {
                // A staging reserves once; give the space back rather than lose it.
                self.taken = taken;
                self.inner
                    .fault(&format!("staging {serial} reserved space twice"));
            }
        }
    }
}

impl Drop for Taken<'_> {
    /// Gives the space back. An inconsistency found here cannot be returned to anyone: it is
    /// recorded as a fault and fences writes (never a second panic while unwinding).
    fn drop(&mut self) {
        if !self.taken.is_empty() {
            let taken = std::mem::take(&mut self.taken);
            let given = lock(&self.inner.pool).give_back(self.owner, &taken);
            if let Err(why) = given {
                self.inner.fault(&why);
            }
        }
    }
}

/// Writes new tree nodes for one generation: first into retired pages the commit took for reuse,
/// then at fresh pages.
struct PageWriter<'a> {
    inner: &'a Inner,
    next_page: u64,
    generation: u64,
    reuse: Vec<u64>,
}

impl NodeSink for PageWriter<'_> {
    fn write(&mut self, node: &Node) -> Result<PageRef, StorageError> {
        let (page, next) = match self.reuse.pop() {
            Some(page) => {
                self.inner
                    .counters
                    .pages_reused
                    .fetch_add(1, Ordering::Relaxed);
                (page, self.next_page)
            }
            None => (
                self.next_page,
                self.next_page
                    .checked_add(1)
                    .ok_or_else(|| StorageError::InvalidRequest("page counter exhausted".into()))?,
            ),
        };
        let at = PageRef {
            page,
            incarnation: self.generation,
        };
        let offset = at
            .page
            .checked_mul(PAGE_SIZE as u64)
            .ok_or_else(|| StorageError::InvalidRequest("page counter exhausted".into()))?;
        let page = encode_page(self.inner.store, PageKind::Node, at, &node.encode())?;
        self.inner.io.write_at(FileId::Meta, offset, &page)?;
        self.next_page = next;
        self.inner
            .counters
            .pages_written
            .fetch_add(1, Ordering::Relaxed);
        self.inner
            .counters
            .page_bytes_written
            .fetch_add(page.len() as u64, Ordering::Relaxed);
        Ok(at)
    }
}

impl NodeSource for Inner {
    fn entry_visited(&self) {
        self.counters
            .entries_visited
            .fetch_add(1, Ordering::Relaxed);
    }

    fn load(&self, at: PageRef) -> Result<Arc<Node>, StorageError> {
        let cur = lock(&self.state).current;
        let offset = at.page.checked_mul(PAGE_SIZE as u64);
        if at.page < FIRST_NODE_PAGE
            || at.page >= cur.next_page
            || at.incarnation == 0
            || at.incarnation > cur.generation
            || offset.is_none()
        {
            return Err(corrupt(format!(
                "node reference {}@{} is not committed metadata (pages below {}, generations up to {})",
                at.page, at.incarnation, cur.next_page, cur.generation
            )));
        }
        if let Some(n) = lock(&self.cache).map.get(&at) {
            return Ok(n.clone());
        }
        self.counters.cache_misses.fetch_add(1, Ordering::Relaxed);
        self.counters.pages_read.fetch_add(1, Ordering::Relaxed);
        let mut buf = vec![0; PAGE_SIZE];
        self.io
            .read_at(FileId::Meta, offset.unwrap_or_default(), &mut buf)?;
        let node = Arc::new(Node::decode(&decode_page(
            self.store,
            PageKind::Node,
            at,
            &buf,
        )?)?);
        let mut c = lock(&self.cache);
        if c.capacity > 0 && c.map.insert(at, node.clone()).is_none() {
            c.order.push_back(at);
            while c.map.len() > c.capacity {
                let Some(old) = c.order.pop_front() else {
                    break;
                };
                c.map.remove(&old);
                self.counters
                    .cache_evictions
                    .fetch_add(1, Ordering::Relaxed);
            }
        }
        Ok(node)
    }
}

/// Largest buffer held for a committed extent before its digest is verified. The same size as a
/// content segment today, but a separate bound: it governs read buffering, not segment layout.
const VERIFY_CHUNK: usize = 64 * 1024;

/// Largest history entry the format can hold: its position, the length-prefixed attempt identifier
/// and the length-prefixed encoded request. Reads are bounded by this, not by the open's options,
/// which bound only what the open writes.
pub(crate) const HISTORY_ENTRY_MAX: u64 = 8 + 4 + MAX_ID_LEN as u64 + 4 + u32::MAX as u64;

impl Inner {
    pub fn create(
        io: Box<dyn Io>,
        store: StoreId,
        opts: Options,
    ) -> Result<Arc<Self>, StorageError> {
        io.create()?;
        // Make the new directory entries durable before anything refers to them.
        io.sync_dir()?;
        let root = PageRef {
            page: FIRST_NODE_PAGE,
            incarnation: 1,
        };
        // Slot 0 stays empty (a file hole reads as zeros); generation 1 is published in slot 1.
        let empty = Node::Leaf(tree::Entries::new()).encode();
        io.write_at(
            FileId::Meta,
            root.page * PAGE_SIZE as u64,
            &encode_page(store, PageKind::Node, root, &empty)?,
        )?;
        io.sync(FileId::Content)?;
        io.sync(FileId::Meta)?;
        let desc = Descriptor {
            store,
            generation: 1,
            root,
            next_page: FIRST_NODE_PAGE + 1,
            content_len: 0,
            history_pos: 0,
            features: 0,
        };
        io.write_at(
            FileId::Meta,
            Descriptor::slot(1) * PAGE_SIZE as u64,
            &desc.encode(),
        )?;
        io.sync(FileId::Meta)?;
        let inner = Arc::new(Self::new(io, desc, Vec::new(), opts));
        inner.start_worker(opts.commit_queue)?;
        Ok(inner)
    }

    pub fn open(io: Box<dyn Io>, opts: Options) -> Result<Arc<Self>, StorageError> {
        io.open()?;
        let (desc, diagnostics) = read_publication(&*io)?;
        check_guard(&desc, opts.guard)?;
        // The selected record and everything it references may still be only in the operating
        // system's cache (the previous owner exited before its barrier). Make them durable before
        // any receipt or new commit can rely on them.
        io.sync(FileId::Content)?;
        io.sync(FileId::Meta)?;
        let inner = Arc::new(Self::new(io, desc, diagnostics, opts));
        inner
            .counters
            .pages_read
            .fetch_add(PUBLICATION_SLOTS, Ordering::Relaxed);
        inner.load_content_pool()?;
        inner.opened();
        inner.start_worker(opts.commit_queue)?;
        Ok(inner)
    }

    /// Opens an existing store for inspection: the same publication record selection as `open`,
    /// in memory only. Nothing is written or synced, no commit worker runs, and the handle holds
    /// the store's lock in shared mode, so owners are refused while it lives.
    pub fn open_read_only(io: Box<dyn Io>, opts: Options) -> Result<Arc<Self>, StorageError> {
        io.open_read_only()?;
        let (desc, diagnostics) = read_publication(&*io)?;
        check_guard(&desc, opts.guard)?;
        let mut inner = Self::new(io, desc, diagnostics, opts);
        inner.read_only = true;
        inner
            .counters
            .pages_read
            .fetch_add(PUBLICATION_SLOTS, Ordering::Relaxed);
        inner.opened();
        Ok(Arc::new(inner))
    }

    /// Fixes the count of pages read while opening.
    fn opened(&self) {
        let n = self.counters.pages_read.load(Ordering::Relaxed);
        self.counters.pages_read_at_open.store(n, Ordering::Relaxed);
    }

    /// Notes `n` more bytes held in content stream buffers, keeping the peak.
    fn buffer_held(&self, n: u64) {
        let now = self
            .counters
            .content_buffer_bytes
            .fetch_add(n, Ordering::Relaxed)
            + n;
        self.counters
            .content_buffer_bytes_peak
            .fetch_max(now, Ordering::Relaxed);
    }

    /// Notes `n` bytes of content stream buffers released.
    fn buffer_released(&self, n: u64) {
        self.counters
            .content_buffer_bytes
            .fetch_sub(n, Ordering::Relaxed);
    }

    fn refuse_if_read_only(&self, what: &str) -> Result<(), StorageError> {
        if self.read_only {
            return Err(StorageError::StoreState(format!(
                "{what} refused on a read-only store handle"
            )));
        }
        Ok(())
    }

    fn new(io: Box<dyn Io>, desc: Descriptor, diagnostics: Vec<String>, opts: Options) -> Self {
        Self {
            io,
            store: desc.store,
            instance: NEXT_INSTANCE.fetch_add(1, Ordering::Relaxed),
            alloc: Mutex::new(desc.content_len),
            pool: Mutex::new(ContentPool::default()),
            faults: Mutex::new(Vec::new()),
            next_taker: AtomicU64::new(0),
            allocation_rounds_max: AtomicUsize::new(0),
            state: Recorded::new(
                "state",
                State {
                    current: desc,
                    staged: HashMap::new(),
                    next_stage: 1,
                    fenced: None,
                    quiescing: false,
                },
            ),
            writer: Mutex::new(()),
            cache: Mutex::new(NodeCache {
                map: HashMap::new(),
                order: VecDeque::new(),
                capacity: opts.cache_nodes,
            }),
            worker: Mutex::new(None),
            pending: Arc::new(Recorded::new("pending", HashMap::new())),
            closing: AtomicBool::new(false),
            pins: Mutex::new(BTreeMap::new()),
            readers: AtomicU64::new(0),
            quiesced: Condvar::new(),
            #[cfg(test)]
            gate: Mutex::new(()),
            #[cfg(test)]
            panic_next_commit: std::sync::atomic::AtomicBool::new(false),
            #[cfg(test)]
            reuse_hook: Mutex::new(None),
            #[cfg(test)]
            relocate_hook: Mutex::new(None),
            #[cfg(test)]
            skip_relocation_revalidation: std::sync::atomic::AtomicBool::new(false),
            #[cfg(test)]
            panic_next_maintenance: std::sync::atomic::AtomicBool::new(false),
            #[cfg(test)]
            journal_hook: Mutex::new(None),
            #[cfg(test)]
            apply_hook: Mutex::new(None),
            #[cfg(test)]
            waiting_registrations: std::sync::atomic::AtomicUsize::new(0),
            #[cfg(test)]
            walk_hook: Mutex::new(None),
            #[cfg(test)]
            admitted_hook: Mutex::new(None),
            #[cfg(test)]
            slow_job_release: std::sync::atomic::AtomicBool::new(false),
            conflicts: Mutex::new(ConflictLog {
                floor: desc.history_pos,
                entries: VecDeque::new(),
                capacity: opts.conflict_window.max(1),
            }),
            max_segments: opts.max_content_segments,
            max_request_field: opts.max_request_field,
            max_request_len: opts.max_request_len,
            max_staged_bytes: opts.max_staged_bytes,
            read_only: false,
            counters: Counters::default(),
            diagnostics,
        }
    }

    /// Test hook: makes the next commit panic inside the worker.
    #[cfg(test)]
    pub(crate) fn slow_job_release_for_test(&self) {
        self.slow_job_release.store(true, Ordering::SeqCst);
    }

    /// Most rounds any publication of this instance (a commit or a maintenance step) took to
    /// settle its page allocation (0 before the first).
    #[cfg(test)]
    pub(crate) fn allocation_rounds_max(&self) -> usize {
        self.allocation_rounds_max.load(Ordering::Relaxed)
    }

    /// Settles a publication's page allocation (see [`settle_allocation`]) and records the rounds
    /// it took. Every publication settles through here.
    fn settle(
        &self,
        txn: &mut TreeTxn,
        cur: &Descriptor,
        generation: u64,
        reusable_through: Option<u64>,
        below: Option<u64>,
    ) -> Result<ReusedPages, StorageError> {
        let (pages, rounds) =
            settle_allocation(txn, self, cur, generation, reusable_through, below)?;
        self.allocation_rounds_max
            .fetch_max(rounds, Ordering::Relaxed);
        Ok(pages)
    }

    #[cfg(test)]
    pub(crate) fn panic_next_commit_for_test(&self) {
        self.panic_next_commit.store(true, Ordering::SeqCst);
    }

    /// Test hook: holds the commit worker before its next job until the guard is dropped.
    #[cfg(test)]
    pub(crate) fn pause_worker_for_test(&self) -> std::sync::MutexGuard<'_, ()> {
        lock(&self.gate)
    }

    /// Test hook: installs (or with `None` removes) the callback a commit runs once after fixing
    /// which retired pages it may reuse. Returns the callback it replaced.
    #[cfg(test)]
    pub(crate) fn set_reuse_hook_for_test(
        &self,
        hook: Option<Box<dyn FnOnce() + Send>>,
    ) -> Option<Box<dyn FnOnce() + Send>> {
        std::mem::replace(&mut *lock(&self.reuse_hook), hook)
    }

    /// Test hook: the callback the next commit runs on its space journal before sealing it.
    #[cfg(test)]
    pub(crate) fn set_journal_hook_for_test(&self, hook: JournalHook) {
        *lock(&self.journal_hook) = Some(hook);
    }

    /// Test hook: the callback the next commit runs after publishing, before updating the pool.
    #[cfg(test)]
    pub(crate) fn set_apply_hook_for_test(&self, hook: Box<dyn FnOnce() + Send>) {
        *lock(&self.apply_hook) = Some(hook);
    }

    /// Test oracle: the published state's retired-content records and the reuse pool's
    /// accounting correspond one to one. Every record (offset, length, retiring generation) is
    /// exactly one of: a free extent, a reservation a staging holds, or a reservation an uncertain
    /// publication owns, with the same offset, length and generation; and every free extent and
    /// reservation is exactly one record. Records never overlap one another. Discarded extents
    /// awaiting retirement have no record yet and overlap neither records, nor each other, nor
    /// anything the published state reaches.
    ///
    /// Only meaningful while nothing is in flight: a staging's takes stay outside the pool's
    /// reservations until it registers, and a commit changes ownership as it publishes. The check
    /// refuses to run otherwise.
    #[cfg(any(test, feature = "fuzz-api"))]
    pub(crate) fn ledger_check_for_test(&self) -> Result<(), String> {
        let e = |x: StorageError| x.to_string();
        let staging = lock(&self.pool).taking().len();
        let committing = lock(&self.pending).len();
        if staging > 0 || committing > 0 {
            return Err(format!(
                "not quiescent: {staging} extent(s) being taken by stagings and {committing} \
                 commit(s) in flight"
            ));
        }
        let cur = lock(&self.state).current;
        let mut records: BTreeMap<u64, (u64, u64)> = BTreeMap::new();
        let mut c = Cursor::new(self, cur.root, &[P_RETIRED], None, cur.next_page).map_err(e)?;
        while let Some((k, v)) = c.next_entry().map_err(e)? {
            if let Retired::Extent {
                generation,
                offset,
                len,
            } = decode_retired(&k, &v).map_err(e)?
                && records.insert(offset, (len, generation)).is_some()
            {
                return Err(format!("two retirement records at offset {offset}"));
            }
        }
        if let Some(w) = records
            .iter()
            .collect::<Vec<_>>()
            .windows(2)
            .find(|w| overlap((*w[0].0, w[0].1.0), (*w[1].0, w[1].1.0)))
        {
            return Err(format!(
                "retirement records at offsets {} and {} overlap",
                w[0].0, w[1].0
            ));
        }
        let reached = crate::audit::reach(&*self.io, &cur).map_err(e)?;
        let live: Vec<(u64, u64)> = reached
            .manifests
            .iter()
            .chain(&reached.content_segments)
            .chain(&reached.history_extents)
            .copied()
            .collect();
        let pool = lock(&self.pool);
        let mut entries: Vec<(u64, u64, u64, String)> = pool
            .free()
            .map(|(o, l, g)| (o, l, g, "free".to_string()))
            .collect();
        for (serial, rs) in pool.held() {
            entries.extend(rs.iter().map(|r| {
                (
                    r.offset,
                    r.record_len,
                    r.retired_at,
                    format!("staging {serial}"),
                )
            }));
        }
        for (serial, attempt, g, rs) in pool.uncertain() {
            entries.extend(rs.iter().map(|r| {
                (
                    r.offset,
                    r.record_len,
                    r.retired_at,
                    format!("staging {serial} under uncertain {attempt:?} at {g}"),
                )
            }));
        }
        let mut seen: BTreeMap<u64, String> = BTreeMap::new();
        for (offset, len, g, owner) in &entries {
            if let Some(other) = seen.insert(*offset, owner.clone()) {
                return Err(format!(
                    "content at offset {offset} accounted twice: {other} and {owner}"
                ));
            }
            match records.get(offset) {
                None => {
                    return Err(format!(
                        "{owner} accounts for content at offset {offset} ({len} bytes, \
                         generation {g}) that no record retires"
                    ));
                }
                Some(r) if *r != (*len, *g) => {
                    return Err(format!(
                        "{owner} accounts for content at offset {offset} as ({len}, {g}), its \
                         record says {r:?}"
                    ));
                }
                Some(_) => {}
            }
        }
        if let Some((offset, (len, g))) = records.iter().find(|(o, _)| !seen.contains_key(o)) {
            return Err(format!(
                "the record retiring content at offset {offset} ({len} bytes, generation {g}) is \
                 neither free nor reserved"
            ));
        }
        let mut queued = pool.discarded().to_vec();
        queued.sort_unstable();
        if let Some(w) = queued.windows(2).find(|w| overlap(w[0], w[1])) {
            return Err(format!(
                "discarded extents {:?} and {:?} overlap",
                w[0], w[1]
            ));
        }
        if let Some(x) = queued
            .iter()
            .find(|x| records.iter().any(|(o, (l, _))| overlap(**x, (*o, *l))))
        {
            return Err(format!("discarded extent {x:?} already has a record"));
        }
        if let Some(x) = queued
            .iter()
            .find(|x| live.iter().any(|l| overlap(**x, *l)))
        {
            return Err(format!(
                "discarded extent {x:?} overlaps content the published state reaches"
            ));
        }
        Ok(())
    }

    /// Test hook: the extents queued for retirement by the next publication.
    #[cfg(test)]
    pub(crate) fn queued_for_test(&self) -> Vec<(u64, u64)> {
        lock(&self.pool).discarded().to_vec()
    }

    /// Test hook: removes the free extent at the lowest offset from the reuse pool.
    #[cfg(test)]
    pub(crate) fn drop_free_for_test(&self) -> Option<(u64, u64)> {
        lock(&self.pool).drop_first_free_for_test()
    }

    /// Test hook: puts an extent in the reuse pool as free.
    #[cfg(test)]
    pub(crate) fn insert_free_for_test(&self, offset: u64, len: u64, retired_at: u64) {
        lock(&self.pool).plant_free_for_test(offset, len, retired_at);
    }

    /// How many view registrations are waiting for a quiescent commit to publish.
    #[cfg(test)]
    pub(crate) fn waiting_registrations_for_test(&self) -> &std::sync::atomic::AtomicUsize {
        &self.waiting_registrations
    }

    /// Test hook: installs (or with `None` removes) the callback the next tree walk of
    /// `outcome` or `height` runs before its first page read. Returns the one it replaced.
    #[cfg(test)]
    pub(crate) fn set_walk_hook_for_test(
        &self,
        hook: Option<Box<dyn FnOnce() + Send>>,
    ) -> Option<Box<dyn FnOnce() + Send>> {
        std::mem::replace(&mut *lock(&self.walk_hook), hook)
    }

    /// Test hook: installs the callback the next admitted commit runs on the submitting thread.
    #[cfg(test)]
    pub(crate) fn set_admitted_hook_for_test(
        &self,
        hook: Option<Box<dyn FnOnce() + Send>>,
    ) -> Option<Box<dyn FnOnce() + Send>> {
        std::mem::replace(&mut *lock(&self.admitted_hook), hook)
    }

    /// How many attempts are admitted and not yet resolved.
    #[cfg(test)]
    pub(crate) fn pending_attempts_for_test(&self) -> usize {
        lock(&self.pending).len()
    }

    /// Runs the walk hook once, if one is installed (no-op outside tests).
    fn run_walk_hook(&self) {
        #[cfg(test)]
        {
            let hook = lock(&self.walk_hook).take();
            if let Some(hook) = hook {
                hook();
            }
        }
    }

    /// The conflict log as (floor, every recorded position with its written keys) (tests that a
    /// refused commit leaves it unchanged).
    #[cfg(test)]
    pub(crate) fn conflict_log_for_test(&self) -> (u64, Vec<ConflictEntry>) {
        let log = lock(&self.conflicts);
        (log.floor, log.entries.iter().cloned().collect())
    }

    /// What the engine would reuse right now under its own rules: retired tree pages the next
    /// commit may take, and retired content extents a staging may take (tests auditing reuse
    /// against every retained root and pin).
    #[cfg(any(test, feature = "fuzz-api"))]
    pub(crate) fn reusable_for_test(&self) -> Result<Reusable, StorageError> {
        let cur = lock(&self.state).current;
        let bound = self.reusable_through(cur.generation.saturating_add(1));
        let (page_bound, content_bound) = (bound, bound);
        let mut pages = Vec::new();
        let mut c = Cursor::new(self, cur.root, &[P_RETIRED], None, cur.next_page)?;
        while let Some((k, v)) = c.next_entry()? {
            if let Retired::Page {
                generation, page, ..
            } = decode_retired(&k, &v)?
                && may_reuse(generation, page_bound)
            {
                pages.push(page);
            }
        }
        let extents = lock(&self.pool)
            .free()
            .filter(|(_, _, retired_at)| may_reuse(*retired_at, content_bound))
            .map(|(offset, len, _)| (offset, len))
            .collect();
        Ok((pages, extents))
    }

    /// The content-file extents `(offset, len)` of a staging issued by this instance: its
    /// manifest and segments (tests of space reuse only).
    #[cfg(test)]
    pub(crate) fn staged_extents_for_test(
        &self,
        staged: &StagedContent,
    ) -> Result<Vec<(u64, u64)>, StorageError> {
        let (_, (manifest, _)) = self.staged_extent(&staged.token, &staged.content)?;
        let m = Manifest::decode(&self.read_written_extent(&manifest)?)?;
        Ok(std::iter::once(manifest)
            .chain(m.segments)
            .map(|x| (x.offset, x.len))
            .collect())
    }

    /// Starts the store's commit worker: the single thread that orders publications. The worker
    /// holds no reference to the engine; each queued job holds a strong one, so admitted work
    /// completes even if every caller handle is dropped, and an idle worker never keeps the store
    /// owned. A job's engine reference is released before its attempt stops being pending. The
    /// worker exits when the engine (and with it the queue's sender) is dropped.
    fn start_worker(self: &Arc<Self>, capacity: usize) -> Result<(), StorageError> {
        let (tx, rx) = mpsc::sync_channel::<Job>(capacity.max(1));
        *lock(&self.worker) = Some(tx);
        std::thread::Builder::new()
            .name("cce-commit".into())
            .spawn(move || {
                for job in rx {
                    let Job {
                        inner,
                        attempt,
                        basis,
                        observed,
                        changes,
                        reply,
                        staged,
                    } = job;
                    #[cfg(test)]
                    drop(lock(&inner.gate));
                    let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        inner.commit(attempt.clone(), basis, &observed, changes)
                    }))
                    .unwrap_or_else(|_| {
                        // A commit that panicked may have written anything up to its
                        // publication: its outcome is unknown, and writes stay fenced until
                        // reopen. Queued jobs keep being answered (refused behind the fence).
                        let why = format!("commit of attempt {attempt:?} failed unexpectedly");
                        let generation = lock(&inner.state).current.generation.saturating_add(1);
                        inner.fence(Fence {
                            why: why.clone(),
                            uncertain: Some(attempt.clone()),
                        });
                        inner.hold_for_uncertain(&attempt, generation, &staged);
                        Err(CommitError::Indeterminate(why))
                    });
                    inner.release_admitted(&staged);
                    #[cfg(test)]
                    let slow = inner.slow_job_release.load(Ordering::SeqCst);
                    // Release this job's hold on the engine before its attempt stops being
                    // pending: once nothing is pending (what `close` waits for), no job keeps the
                    // store owned.
                    let pending = inner.pending.clone();
                    drop(inner);
                    finish_pending(&pending, &attempt);
                    // The caller may have gone away; the commit stands either way.
                    let _ = reply.send(r);
                    #[cfg(test)]
                    if slow {
                        std::thread::sleep(std::time::Duration::from_millis(100));
                    }
                }
            })
            .map_err(|e| StorageError::Io(format!("start commit worker: {e}")))?;
        Ok(())
    }

    /// Admits a commit to the worker's queue, or refuses it explicitly when the queue is full.
    pub fn submit(
        self: &Arc<Self>,
        attempt: AttemptId,
        basis: Snapshot,
        observed: Vec<Observation>,
        changes: ChangeSet,
    ) -> Result<mpsc::Receiver<Result<CommitReceipt, CommitError>>, CommitError> {
        self.refuse_if_read_only("commits are").map_err(rejected)?;
        let (reply, rx) = mpsc::channel();
        let tx = lock(&self.worker).clone().ok_or_else(|| {
            rejected(StorageError::StoreState("commit worker not running".into()))
        })?;
        {
            // Checked under the same lock `close` sets it under, so `close` never misses an
            // admission that raced with it.
            let mut pending = lock(&self.pending);
            if self.closing.load(Ordering::SeqCst) {
                return Err(rejected(StorageError::StoreState(
                    "store is closing; no new commits are admitted".into(),
                )));
            }
            *pending.entry(attempt.clone()).or_default() += 1;
        }
        // Mark the stagings this job binds as admitted, so they cannot be discarded under it.
        let staged: Vec<u64> = {
            let st = lock(&self.state);
            let mut pool = lock(&self.pool);
            changes
                .content
                .iter()
                .filter_map(|b| self.token_serial(&b.token))
                .filter(|serial| st.staged.contains_key(serial))
                .inspect(|serial| pool.admit(*serial))
                .collect()
        };
        let job = Job {
            inner: self.clone(),
            attempt: attempt.clone(),
            basis,
            observed,
            changes,
            reply,
            staged: staged.clone(),
        };
        match tx.try_send(job) {
            Ok(()) => {
                #[cfg(test)]
                {
                    let hook = lock(&self.admitted_hook).take();
                    if let Some(hook) = hook {
                        hook();
                    }
                }
                Ok(rx)
            }
            Err(e) => {
                self.release_admitted(&staged);
                self.finish_pending(&attempt);
                Err(rejected(match e {
                    mpsc::TrySendError::Full(_) => {
                        StorageError::Busy("commit queue is full; retry later".into())
                    }
                    mpsc::TrySendError::Disconnected(_) => {
                        StorageError::StoreState("commit worker stopped".into())
                    }
                }))
            }
        }
    }

    fn finish_pending(&self, attempt: &AttemptId) {
        finish_pending(&self.pending, attempt);
    }

    /// The bytes of one metadata page as stored now, undecoded (inspection: the integrity check
    /// also reads the padding the decoders ignore). Reads only, outside the node cache.
    pub(crate) fn meta_page(&self, page: u64) -> Result<Vec<u8>, StorageError> {
        let offset = page
            .checked_mul(PAGE_SIZE as u64)
            .ok_or_else(|| corrupt(format!("page {page} lies beyond any metadata file")))?;
        let mut buf = vec![0; PAGE_SIZE];
        self.io.read_at(FileId::Meta, offset, &mut buf)?;
        Ok(buf)
    }

    /// The current committed state without a pin (tests only: production reads go through
    /// `pin_current`, so nothing they can reach is reused under them).
    #[cfg(test)]
    pub fn snapshot(&self) -> Result<Snapshot, StorageError> {
        Ok(Snapshot {
            desc: lock(&self.state).current,
        })
    }

    /// The current committed state, pinned: the pin is registered under the same lock that reads
    /// the current state, so no publication can slip between choosing the generation and pinning it.
    /// The pin belongs to a reader (a view and the content readers it opens).
    pub(crate) fn pin_current(self: &Arc<Self>) -> Result<(Snapshot, Pin), StorageError> {
        let (snap, pin, ()) = self.pin_current_then(true, |_| ());
        Ok((snap, pin))
    }

    /// Like `pin_current`, also reading `read` from the same state, under the same lock. A pin
    /// that is not a `reader`'s (the engine's own walk) protects reuse like any other, but a
    /// commit requiring quiescence does not count it as a live reader.
    fn pin_current_then<R>(
        self: &Arc<Self>,
        reader: bool,
        read: impl FnOnce(&State) -> R,
    ) -> (Snapshot, Pin, R) {
        let mut st = lock(&self.state);
        // Never register on a state a quiescent commit is about to replace.
        while st.quiescing {
            #[cfg(test)]
            self.waiting_registrations.fetch_add(1, Ordering::SeqCst);
            st = self
                .quiesced
                .wait(st)
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            #[cfg(test)]
            self.waiting_registrations.fetch_sub(1, Ordering::SeqCst);
        }
        let desc = st.current;
        *lock(&self.pins).entry(desc.generation).or_default() += 1;
        if reader {
            self.readers.fetch_add(1, Ordering::SeqCst);
        }
        let r = read(&st);
        drop(st);
        (
            Snapshot { desc },
            Pin {
                inner: self.clone(),
                generation: desc.generation,
                reader,
            },
            r,
        )
    }

    /// Live pins by generation.
    #[cfg(test)]
    pub(crate) fn pinned_generations(&self) -> BTreeMap<u64, u32> {
        lock(&self.pins).clone()
    }

    /// Moves the page and content allocation cursors (boundary tests only).
    #[cfg(test)]
    pub(crate) fn set_cursors_for_test(&self, next_page: Option<u64>, content: Option<u64>) {
        if let Some(p) = next_page {
            lock(&self.state).current.next_page = p;
        }
        if let Some(c) = content {
            *lock(&self.alloc) = c;
        }
    }

    pub fn height(self: &Arc<Self>) -> Result<usize, StorageError> {
        let (snap, _pin, ()) = self.pin_current_then(false, |_| ());
        let root = snap.desc.root;
        self.run_walk_hook();
        tree::height(&**self, root)
    }

    pub fn cache_capacity(&self) -> usize {
        lock(&self.cache).capacity
    }

    /// Reads and verifies a committed extent of at most `limit` bytes. The recorded length is
    /// checked against `limit` and the committed content before anything is allocated or read.
    pub(crate) fn read_extent(
        &self,
        x: &ExtentRef,
        limit: u64,
        what: &str,
    ) -> Result<Vec<u8>, StorageError> {
        if x.len > limit {
            return Err(corrupt(format!(
                "{what} of {} bytes exceeds the {limit} byte limit",
                x.len
            )));
        }
        let committed = lock(&self.state).current.content_len;
        if x.offset
            .checked_add(x.len)
            .is_none_or(|end| end > committed)
        {
            return Err(corrupt(format!(
                "extent {}+{} lies outside committed content ({committed} bytes)",
                x.offset, x.len
            )));
        }
        let len = usize::try_from(x.len).map_err(|_| corrupt("extent too large"))?;
        let mismatch = || corrupt(format!("content extent at {}: digest mismatch", x.offset));
        // A recorded length can be damaged while still in bounds. Before holding more than one
        // chunk, verify the extent's digest by streaming it through one reused chunk buffer;
        // nothing read in this pass is kept or interpreted.
        if len > VERIFY_CHUNK {
            let mut chunk = vec![0; VERIFY_CHUNK];
            let mut hash = Sha256::new();
            let mut at = x.offset;
            let mut left = len;
            while left > 0 {
                let n = left.min(VERIFY_CHUNK);
                self.io.read_at(FileId::Content, at, &mut chunk[..n])?;
                hash.update(&chunk[..n]);
                at += n as u64;
                left -= n;
            }
            let got: [u8; 32] = hash.finalize().into();
            if got != x.digest {
                return Err(mismatch());
            }
        }
        // The length is now authentic: hold the extent whole and check it again as decoded.
        let mut buf = vec![0; len];
        self.io.read_at(FileId::Content, x.offset, &mut buf)?;
        if sha256(&buf) != x.digest {
            return Err(mismatch());
        }
        Ok(buf)
    }

    // ----- staging -----------------------------------------------------------------------------

    /// Total content length of this instance's outstanding stagings: staged and neither
    /// committed nor discarded (manifests are not counted).
    pub fn staged_bytes(&self) -> u64 {
        staged_total(&lock(&self.state))
    }

    /// Refuses a staging of `len` content bytes that would take the outstanding total past the
    /// staging budget.
    fn check_staging_budget(&self, len: u64) -> Result<(), StorageError> {
        let outstanding = self.staged_bytes();
        if outstanding.saturating_add(len) <= self.max_staged_bytes {
            return Ok(());
        }
        Err(StorageError::ResourceExhausted(format!(
            "staging {len} bytes would exceed the budget of {} bytes for outstanding staged \
             content ({outstanding} bytes outstanding); commit or discard outstanding stagings \
             to release it (stagings bound by a commit whose publication is uncertain are \
             released only by reopening the store)",
            self.max_staged_bytes
        )))
    }

    pub fn stage(&self, bytes: &mut dyn Read) -> Result<StagedContent, StorageError> {
        self.stage_checked(bytes, None)
    }

    /// Stages content, enforcing a declared reference if given: a stream longer than declared is
    /// cut off as soon as it exceeds it, and length and digest are compared before anything is
    /// registered. Nothing is staged on a mismatch.
    pub fn stage_checked(
        &self,
        bytes: &mut dyn Read,
        expected: Option<&ContentRef>,
    ) -> Result<StagedContent, StorageError> {
        self.refuse_if_read_only("staging is")?;
        if let Some(f) = &lock(&self.state).fenced {
            return Err(StorageError::StoreState(format!(
                "writes fenced: {}",
                f.why
            )));
        }
        if let Some(e) = expected {
            self.check_staging_budget(e.len)?;
        }
        let mut cursor = lock(&self.alloc);
        // Retired content this staging may reuse: no publication slot reaches it (retired at
        // most one generation before the current one) and no live reader's generation does.
        let reusable_through = {
            let current = lock(&self.state).current.generation;
            self.reusable_through(current.saturating_add(1))
        };
        let mut taken = Taken {
            inner: self,
            owner: self.next_taker.fetch_add(1, Ordering::Relaxed),
            taken: Vec::new(),
        };
        let start = *cursor;
        let mut at = start;
        let mut total = Sha256::new();
        let mut len = 0u64;
        let mut segments = Vec::new();
        let mut buf = vec![0; CONTENT_CHUNK];
        let _held = BufferHold::new(self, CONTENT_CHUNK as u64);
        loop {
            // Fill one segment (short reads and interruptions are retried), then write it.
            // With a declared length, never ask for more than one byte past it, and reject as
            // soon as it is exceeded.
            let mut filled = 0;
            while filled < buf.len() {
                let want = match expected {
                    Some(e) => {
                        let left = e.len - (len + filled as u64);
                        (buf.len() - filled)
                            .min(usize::try_from(left).map_or(usize::MAX, |l| l.saturating_add(1)))
                    }
                    None => buf.len() - filled,
                };
                match bytes.read(&mut buf[filled..filled + want]) {
                    Ok(0) => break,
                    Ok(n) => filled += n,
                    Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                    Err(e) => return Err(StorageError::Io(format!("staging source: {e}"))),
                }
                if let Some(e) = expected
                    && len + filled as u64 > e.len
                {
                    return Err(StorageError::InvalidRequest(format!(
                        "staged stream is longer than the declared {} bytes",
                        e.len
                    )));
                }
            }
            if filled == 0 {
                break;
            }
            if segments.len() >= self.max_segments {
                return Err(StorageError::InvalidRequest(format!(
                    "staged stream exceeds the limit of {} segments",
                    self.max_segments
                )));
            }
            // Before this segment is written: outstanding stagings (read now, so a commit or a
            // discard meanwhile has released its share) plus this stream so far must fit.
            self.check_staging_budget(len + filled as u64)?;
            let seg = &buf[..filled];
            let place = self.place(&mut at, filled as u64, reusable_through, &mut taken)?;
            self.io.write_at(FileId::Content, place, seg)?;
            self.counters
                .content_bytes_written
                .fetch_add(filled as u64, Ordering::Relaxed);
            segments.push(ExtentRef {
                offset: place,
                len: filled as u64,
                digest: sha256(seg),
            });
            total.update(seg);
            len += filled as u64;
            if filled < buf.len() {
                break;
            }
        }
        let content = ContentRef {
            digest: ContentDigest(total.finalize().into()),
            len,
        };
        if let Some(e) = expected
            && *e != content
        {
            return Err(StorageError::InvalidRequest(format!(
                "staged stream ({} bytes) does not match the declared content ({} bytes) or its digest",
                content.len, e.len
            )));
        }
        let manifest = Manifest { content, segments }.encode();
        let place = self.place(&mut at, manifest.len() as u64, reusable_through, &mut taken)?;
        self.io.write_at(FileId::Content, place, &manifest)?;
        self.counters
            .content_bytes_written
            .fetch_add(manifest.len() as u64, Ordering::Relaxed);
        let mref = ExtentRef {
            offset: place,
            len: manifest.len() as u64,
            digest: sha256(&manifest),
        };
        *cursor = at;
        let serial = {
            let mut st = lock(&self.state);
            let serial = st.next_stage;
            st.next_stage += 1;
            st.staged.insert(serial, (mref, content));
            serial
        };
        taken.hold(serial);
        let mut t = Enc::default();
        t.raw(&self.store.0)
            .u64(self.instance)
            .u64(serial)
            .u64(len)
            .raw(&content.digest.0);
        Ok(StagedContent {
            content,
            token: StagingToken(t.buf),
        })
    }

    /// Resolves a staging token issued by this open instance.
    fn staged_extent(
        &self,
        token: &StagingToken,
        content: &ContentRef,
    ) -> Result<(u64, BoundContent), StorageError> {
        let mut d = Dec::new(&token.0, "staging token");
        let parsed = (|| -> Result<_, StorageError> {
            let s: [u8; 16] = d.array()?;
            let inst = d.u64()?;
            let serial = d.u64()?;
            let len = d.u64()?;
            let dig: [u8; 32] = d.array()?;
            Ok((s, inst, serial, len, dig))
        })();
        let Ok((s, inst, serial, len, dig)) = parsed else {
            return Err(StorageError::ForeignHandle);
        };
        if s != self.store.0 || inst != self.instance {
            return Err(StorageError::ForeignHandle);
        }
        let staged = lock(&self.state).staged.get(&serial).copied();
        match staged {
            Some((m, c)) if c.len == len && c.digest.0 == dig && *content == c => {
                Ok((serial, (m, c)))
            }
            _ => Err(StorageError::ForeignHandle),
        }
    }

    // ----- commit ------------------------------------------------------------------------------

    pub fn commit(
        &self,
        attempt: AttemptId,
        basis: Snapshot,
        observed: &[Observation],
        cs: ChangeSet,
    ) -> Result<CommitReceipt, CommitError> {
        self.refuse_if_read_only("commits are").map_err(rejected)?;
        let _w = lock(&self.writer);
        // Before anything is encoded: no field may overflow its length prefix.
        crate::records::check_request_bounds(&cs, self.max_request_field, self.max_request_len)
            .map_err(rejected)?;
        #[cfg(test)]
        if self.panic_next_commit.swap(false, Ordering::SeqCst) {
            panic!("injected commit panic");
        }
        let (cur, fence) = {
            let st = lock(&self.state);
            (st.current, st.fenced.clone())
        };
        // The attempt whose publication is uncertain stays uncertain; it is never reported as
        // rejected. Other attempts are still answered from the last published state below.
        if let Some(f) = &fence
            && f.uncertain.as_ref() == Some(&attempt)
        {
            return Err(CommitError::Indeterminate(f.why.clone()));
        }
        let mut txn = TreeTxn::new(self, cur.root);

        // Every content index must name a binding before the request is fingerprinted.
        for c in &cs.changes {
            if let Change::PutContext {
                content: Some(i), ..
            } = c
                && *i >= cs.content.len()
            {
                return Err(rejected(StorageError::InvalidRequest(format!(
                    "content index {i} out of range ({} bindings)",
                    cs.content.len()
                ))));
            }
        }

        // Attempt identity: an identical retry returns the original receipt.
        let fp = fingerprint(&attempt, &cs);
        let akey = key(P_ATTEMPT, &[attempt.as_bytes()]);
        if let Some(v) = txn.get(&akey).map_err(rejected)? {
            let prior = AttemptValue::decode(&v).map_err(rejected)?;
            return if prior.fingerprint == fp {
                Ok(CommitReceipt {
                    attempt,
                    position: HistoryPosition(prior.position),
                })
            } else {
                Err(CommitError::AttemptMismatch)
            };
        }
        if let Some(f) = fence {
            return Err(rejected(StorageError::StoreState(format!(
                "writes fenced: {}",
                f.why
            ))));
        }

        // Requiring quiescence: refuse, before writing anything, while any view or content
        // reader is live (the request's own basis view was consumed at admission and is not).
        // Otherwise keep new views from registering until this commit ends.
        let _quiescing = if cs.removal == RemovalMode::RequireQuiescence {
            let mut st = lock(&self.state);
            let live = self.readers.load(Ordering::SeqCst);
            if live > 0 {
                return Err(CommitError::ReadersActive(format!(
                    "{live} view or content reader{} live",
                    if live == 1 { " is" } else { "s are" }
                )));
            }
            st.quiescing = true;
            Some(Quiescing { inner: self })
        } else {
            None
        };

        // The basis must still hold: nothing it observed may have been written since.
        lock(&self.conflicts).validate(basis.desc.history_pos, observed)?;

        let pos = cur.history_pos.checked_add(1).ok_or_else(|| {
            rejected(StorageError::InvalidRequest(
                "history position overflow".into(),
            ))
        })?;
        let bound = self.resolve_content(&cs)?;
        // Changes refer to bindings by position, so `extents` keeps one entry per binding. A
        // staging bound more than once is still one staging: from here on it is settled, consumed
        // and released once.
        let extents: Vec<BoundContent> = bound.iter().map(|(_, x)| *x).collect();
        let content_refs = apply(&mut txn, &cs, &extents, pos)?;
        let mut seen = BTreeSet::new();
        let bound: Vec<(u64, BoundContent)> =
            bound.into_iter().filter(|(s, _)| seen.insert(*s)).collect();
        let stagings: Vec<BoundContent> = bound.iter().map(|(_, x)| *x).collect();
        let written: Vec<Vec<u8>> = txn
            .touched()
            .iter()
            .filter(|k| matches!(k.first(), Some(&P_CONTEXT | &P_REL | &P_OUT | &P_IN)))
            .cloned()
            .collect();

        // History entry → content file; then touched tree nodes → fresh metadata pages.
        let hist = encode_history(pos, &attempt, &cs);
        let hist_ref = {
            let mut cursor = lock(&self.alloc);
            let x = ExtentRef {
                offset: *cursor,
                len: hist.len() as u64,
                digest: sha256(&hist),
            };
            let end = x
                .offset
                .checked_add(x.len)
                .ok_or_else(content_space_exhausted)
                .map_err(rejected)?;
            self.io
                .write_at(FileId::Content, x.offset, &hist)
                .map_err(rejected)?;
            self.counters
                .content_bytes_written
                .fetch_add(x.len, Ordering::Relaxed);
            *cursor = end;
            x
        };
        // Until the publication record is written, nothing references the history entry: a
        // commit that ends earlier hands its bytes to the next publication to retire.
        let mut unpublished = Unpublished {
            inner: self,
            extent: Some((hist_ref.offset, hist_ref.len)),
        };
        txn.put(history_key(pos), hist_ref.encode())
            .map_err(rejected)?;
        txn.put(
            akey,
            AttemptValue {
                position: pos,
                fingerprint: fp,
            }
            .encode(),
        )
        .map_err(rejected)?;

        let Some(generation) = cur.generation.checked_add(1) else {
            return Err(rejected(StorageError::InvalidRequest(
                "generation counter exhausted".into(),
            )));
        };
        let removed_history =
            self.remove_history(&mut txn, &cs, basis.desc.history_pos, generation)?;
        let (mut retired_content, live_manifests) =
            self.settle_content(&mut txn, &stagings, content_refs, generation)?;
        let removed_features = if removed_history.is_empty() {
            0
        } else {
            FEATURE_HISTORY_MARKERS
        };
        retired_content.extend(removed_history);
        let (remainders, consumed) = self.consume_reuse(&mut txn, &bound)?;
        // Discarded stagings' own space becomes retired with this commit. The queue is copied
        // here and only the copied entries are removed from it after publication: a discard
        // that arrives meanwhile only appends, and publications are serialised by the commit
        // worker, so no entry is retired twice or lost.
        let discarded: Vec<(u64, u64)> = lock(&self.pool).discarded().to_vec();
        for (offset, len) in &discarded {
            txn.put(
                retired_extent_key(generation, *offset),
                len.to_le_bytes().to_vec(),
            )
            .map_err(rejected)?;
        }
        // Retired pages this commit may reuse: none that either publication slot can still reach
        // while this commit is unpublished (retired at most two generations before it), and none
        // a live reader's generation can reach (retired no later than the oldest pinned one). A
        // reader registering from now on pins the current generation, which reaches none of them.
        let reusable_through = self.reusable_through(generation);
        #[cfg(test)]
        {
            let hook = lock(&self.reuse_hook).take();
            if let Some(hook) = hook {
                hook();
            }
        }
        let reused = self
            .settle(&mut txn, &cur, generation, reusable_through, None)
            .map_err(rejected)?;
        let reuse: Vec<u64> = reused.iter().map(|(page, _)| *page).collect();
        // Every change of space ownership this commit makes, checked as a whole before anything
        // it writes can become reachable.
        #[allow(unused_mut)]
        let mut journal = SpaceJournal {
            generation,
            retired_pages: txn.replaced().to_vec(),
            reused_pages: reuse.clone(),
            retired_extents: retired_content.iter().chain(&discarded).copied().collect(),
            remainders: remainders.clone(),
            consumed,
            reused_records: reused.into_iter().map(|(_, k)| k).collect(),
            still_reachable: live_manifests,
            bound: bound.iter().map(|(serial, _)| *serial).collect(),
        };
        #[cfg(test)]
        {
            let hook = lock(&self.journal_hook).take();
            if let Some(hook) = hook {
                hook(&mut journal, &mut txn);
            }
        }
        self.seal(&txn, cur.root, &journal).map_err(rejected)?;
        let mut sink = PageWriter {
            inner: self,
            next_page: cur.next_page,
            generation,
            reuse,
        };
        let root = txn.finish(&mut sink).map_err(rejected)?;
        if !sink.reuse.is_empty() {
            return Err(rejected(StorageError::InvalidRequest(format!(
                "{} pages taken for reuse were not written",
                sink.reuse.len()
            ))));
        }
        let next_page = sink.next_page;

        // Data barrier: every byte the new generation references must be durable first. Capture
        // the allocation boundary before it: staging may append past it concurrently, and only
        // bytes the barrier covers may be published.
        let content_len = *lock(&self.alloc);
        if let Err(e) = self
            .io
            .sync(FileId::Content)
            .and_then(|()| self.io.sync(FileId::Meta))
        {
            // Nothing is published, but a failed barrier may have dropped written bytes that
            // staged content or later commits would reference: fence writes until reopen.
            self.fence(Fence {
                why: format!("data barrier failed: {e}"),
                uncertain: None,
            });
            return Err(rejected(e));
        }

        let desc = Descriptor {
            store: self.store,
            generation,
            root,
            next_page,
            content_len,
            history_pos: pos,
            features: cur.features | removed_features,
        };
        // From the record write on the publication may be durable and reference the entry.
        unpublished.extent = None;
        let published = self
            .io
            .write_at(
                FileId::Meta,
                Descriptor::slot(generation) * PAGE_SIZE as u64,
                &desc.encode(),
            )
            .and_then(|()| self.io.sync(FileId::Meta));
        if let Err(e) = published {
            // The record may or may not be durable: fence writes until reconciled.
            let why = format!("publication of generation {generation} uncertain: {e}");
            self.fence(Fence {
                why: why.clone(),
                uncertain: Some(attempt.clone()),
            });
            let serials: Vec<u64> = bound.iter().map(|(serial, _)| *serial).collect();
            self.hold_for_uncertain(&attempt, generation, &serials);
            return Err(CommitError::Indeterminate(why));
        }

        lock(&self.conflicts).record(pos, written);
        let mut st = lock(&self.state);
        st.current = desc;
        for (serial, _) in &bound {
            st.staged.remove(serial);
        }
        drop(st);
        // The published state's retired content, as this commit changed it: what it retired is
        // reusable from the next generation on; what bound stagings held is consumed, apart from
        // unused remainders.
        #[cfg(test)]
        {
            let hook = lock(&self.apply_hook).take();
            if let Some(hook) = hook {
                hook();
            }
        }
        let mut pool = lock(&self.pool);
        for (serial, _) in &bound {
            let held = pool.consume(*serial);
            self.count_reused(&held);
        }
        // Sealed before publication; checked again here as an invariant, since the record is
        // durable and nothing can be rejected any more: an extent that would overlap space the
        // pool already accounts for is left out (unused until reopen) and writes are fenced.
        // The queued extents this commit retired leave the queue first, then become free.
        pool.trim_discarded(&discarded);
        let mut clashes = Vec::new();
        let frees = retired_content
            .iter()
            .map(|(o, l)| (*o, *l, generation))
            .chain(remainders.iter().copied())
            .chain(discarded.iter().map(|(o, l)| (*o, *l, generation)));
        for (offset, len, retired_at) in frees {
            if pool.insert_free(&[(offset, len, retired_at)]).is_err() {
                clashes.push((offset, len));
            }
        }
        drop(pool);
        if !clashes.is_empty() {
            self.fence(Fence {
                why: format!(
                    "space check failed after publishing generation {generation}: {clashes:?} \
                     overlap space the pool already accounts for"
                ),
                uncertain: None,
            });
        }
        Ok(CommitReceipt {
            attempt,
            position: HistoryPosition(pos),
        })
    }

    /// Publishes a maintenance step's `txn` as generation `generation` at history position `pos`
    /// (unchanged: maintenance adds no history): retires the space of discarded stagings, settles
    /// page retirement and reuse, checks every change of space ownership against the step's
    /// journal (as a commit does), writes the tree, and runs the data and publication barriers.
    /// Returns the published record and the discarded extents it retired; the caller makes the
    /// record current.
    fn publish(
        &self,
        cur: &Descriptor,
        txn: TreeTxn<'_>,
        generation: u64,
        pos: u64,
        space: MaintenanceSpace,
    ) -> Result<Published, CommitError> {
        self.publish_below(cur, txn, generation, pos, space, None)?
            .ok_or_else(|| {
                rejected(StorageError::InvalidRequest(
                    "an unrestricted publication found no pages to write".into(),
                ))
            })
    }

    /// [`Inner::publish`], but with `below` every tree page it writes must be a retired page
    /// numbered below it: if there are too few, it writes and publishes nothing and returns `None`.
    fn publish_below(
        &self,
        cur: &Descriptor,
        mut txn: TreeTxn<'_>,
        generation: u64,
        pos: u64,
        space: MaintenanceSpace,
        below: Option<u64>,
    ) -> Result<Option<Published>, CommitError> {
        // Discarded stagings' own space becomes retired with this publication; only the copied
        // entries leave the queue afterwards (see `commit`).
        let discarded: Vec<(u64, u64)> = lock(&self.pool).discarded().to_vec();
        for (offset, len) in &discarded {
            txn.put(
                retired_extent_key(generation, *offset),
                len.to_le_bytes().to_vec(),
            )
            .map_err(rejected)?;
        }
        let reusable_through = self.reusable_through(generation);
        let reused = self
            .settle(&mut txn, cur, generation, reusable_through, below)
            .map_err(rejected)?;
        let reuse: Vec<u64> = reused.iter().map(|(page, _)| *page).collect();
        if below.is_some() && reuse.len() < txn.dirty_pages() {
            return Ok(None);
        }
        // Every change of space ownership the step makes, checked as a whole before anything it
        // writes can become reachable, exactly as a commit's.
        #[allow(unused_mut)]
        let mut journal = SpaceJournal {
            generation,
            retired_pages: txn.replaced().to_vec(),
            reused_pages: reuse.clone(),
            retired_extents: space.retired.iter().chain(&discarded).copied().collect(),
            remainders: space.remainders,
            consumed: space.consumed,
            reused_records: reused.into_iter().map(|(_, k)| k).collect(),
            still_reachable: space.still_reachable,
            bound: space.bound,
        };
        #[cfg(test)]
        {
            let hook = lock(&self.journal_hook).take();
            if let Some(hook) = hook {
                hook(&mut journal, &mut txn);
            }
        }
        // Each content entry a relocation rewrote must name the manifest it copied to.
        for (k, manifest) in &space.entries {
            let names = txn
                .get(k)
                .map_err(rejected)?
                .map(|v| ContentEntry::decode(&v))
                .transpose()
                .map_err(rejected)?;
            if !matches!(names, Some(ContentEntry::Live { manifest: m, .. }) if m == *manifest) {
                return Err(rejected(StorageError::StoreState(format!(
                    "space check failed: a relocated content entry does not name its new manifest \
                     at {}",
                    manifest.offset
                ))));
            }
        }
        self.seal(&txn, cur.root, &journal).map_err(rejected)?;
        let mut sink = PageWriter {
            inner: self,
            next_page: cur.next_page,
            generation,
            reuse,
        };
        let root = txn.finish(&mut sink).map_err(rejected)?;
        if !sink.reuse.is_empty() {
            return Err(rejected(StorageError::InvalidRequest(format!(
                "{} pages taken for reuse were not written",
                sink.reuse.len()
            ))));
        }
        let next_page = sink.next_page;
        // Data barrier, as for a commit.
        let content_len = *lock(&self.alloc);
        if let Err(e) = self
            .io
            .sync(FileId::Content)
            .and_then(|()| self.io.sync(FileId::Meta))
        {
            self.fence(Fence {
                why: format!("data barrier failed: {e}"),
                uncertain: None,
            });
            return Err(rejected(e));
        }
        let desc = Descriptor {
            store: self.store,
            generation,
            root,
            next_page,
            content_len,
            history_pos: pos,
            // Maintenance carries the features of the state it publishes over.
            features: cur.features,
        };
        let published = self
            .io
            .write_at(
                FileId::Meta,
                Descriptor::slot(generation) * PAGE_SIZE as u64,
                &desc.encode(),
            )
            .and_then(|()| self.io.sync(FileId::Meta));
        if let Err(e) = published {
            // The record may or may not be durable: fence writes until reconciled.
            let why = format!("publication of generation {generation} uncertain: {e}");
            self.fence(Fence {
                why: why.clone(),
                uncertain: None,
            });
            return Err(CommitError::Indeterminate(why));
        }
        Ok(Some((desc, discarded)))
    }

    fn resolve_content(&self, cs: &ChangeSet) -> Result<Vec<(u64, BoundContent)>, CommitError> {
        cs.content
            .iter()
            .map(|b| self.staged_extent(&b.token, &b.content).map_err(rejected))
            .collect()
    }

    /// Brings content index entries in line with the records that use them: `refs` is this
    /// commit's change in users per digest. Content gaining its first user is indexed with the
    /// commit's binding for it; content losing its last user becomes a removed entry and its
    /// manifest and segments are retired at `generation`. Bound content the index does not take
    /// (a second staging of committed bytes, or a binding no change uses) is retired too: no
    /// published state ever reaches it. `bound` names each staging once. Returns the extents it
    /// retired and the manifests the new state's entries for the touched digests name.
    fn settle_content(
        &self,
        t: &mut TreeTxn,
        bound: &[BoundContent],
        refs: BTreeMap<[u8; 32], i64>,
        generation: u64,
    ) -> Result<(ExtentList, ExtentList), CommitError> {
        let mut retired = Vec::new();
        let mut live = Vec::new();
        let mut first: BTreeMap<[u8; 32], ExtentRef> = BTreeMap::new();
        let mut orphans: Vec<ExtentRef> = Vec::new();
        for (manifest, c) in bound {
            match first.entry(c.digest.0) {
                std::collections::btree_map::Entry::Occupied(_) => orphans.push(*manifest),
                std::collections::btree_map::Entry::Vacant(e) => {
                    e.insert(*manifest);
                }
            }
        }
        let digests: BTreeSet<[u8; 32]> = refs.keys().chain(first.keys()).copied().collect();
        for d in digests {
            let delta = refs.get(&d).copied().unwrap_or(0);
            let binding = first.get(&d).copied();
            let ck = key(P_CONTENT, &[&d]);
            let entry = t
                .get(&ck)
                .map_err(rejected)?
                .map(|v| ContentEntry::decode(&v))
                .transpose()
                .map_err(rejected)?;
            match entry {
                Some(ContentEntry::Live { manifest, refs: n }) => {
                    let now = i64::try_from(n)
                        .ok()
                        .and_then(|n| n.checked_add(delta))
                        .filter(|n| *n >= 0)
                        .ok_or_else(|| {
                            rejected(corrupt(format!(
                                "content used by {n} records cannot change by {delta}"
                            )))
                        })?;
                    if now == 0 {
                        t.put(ck, ContentEntry::Removed.encode())
                            .map_err(rejected)?;
                        // A committed manifest, named by the stored index: read it like any
                        // reader does, bounded and verified before it is held whole.
                        let m = Manifest::decode(
                            &self
                                .read_extent(
                                    &manifest,
                                    Manifest::encoded_len(u32::MAX as usize),
                                    "content manifest",
                                )
                                .map_err(rejected)?,
                        )
                        .map_err(rejected)?;
                        self.retire_content(t, &manifest, &m, generation, &mut retired)?;
                    } else {
                        live.push((manifest.offset, manifest.len));
                        if now as u64 != n {
                            t.put(
                                ck,
                                ContentEntry::Live {
                                    manifest,
                                    refs: now as u64,
                                }
                                .encode(),
                            )
                            .map_err(rejected)?;
                        }
                    }
                    orphans.extend(binding);
                }
                Some(ContentEntry::Removed) | None => {
                    if delta < 0 {
                        return Err(rejected(corrupt(
                            "a record used content that is not committed",
                        )));
                    }
                    if delta > 0 {
                        let manifest = binding.ok_or_else(|| {
                            rejected(corrupt("new use of content without a binding"))
                        })?;
                        live.push((manifest.offset, manifest.len));
                        t.put(
                            ck,
                            ContentEntry::Live {
                                manifest,
                                refs: delta as u64,
                            }
                            .encode(),
                        )
                        .map_err(rejected)?;
                    } else {
                        orphans.extend(binding);
                    }
                }
            }
        }
        for o in orphans {
            // Staged by this instance in this commit, possibly beyond the committed length.
            let m = Manifest::decode(&self.read_written_extent(&o).map_err(rejected)?)
                .map_err(rejected)?;
            self.retire_content(t, &o, &m, generation, &mut retired)?;
        }
        Ok((retired, live))
    }

    /// Records a content manifest and every segment it names as retired at `generation`, and
    /// lists them in `retired` as `(offset, len)`.
    fn retire_content(
        &self,
        t: &mut TreeTxn,
        manifest: &ExtentRef,
        m: &Manifest,
        generation: u64,
        retired: &mut Vec<(u64, u64)>,
    ) -> Result<(), CommitError> {
        for x in std::iter::once(*manifest).chain(m.segments.iter().copied()) {
            t.put(
                retired_extent_key(generation, x.offset),
                x.len.to_le_bytes().to_vec(),
            )
            .map_err(rejected)?;
            retired.push((x.offset, x.len));
        }
        Ok(())
    }

    /// Removes the history entries `cs` designates: each entry's key is replaced by a marker that
    /// keeps only the attempt it recorded, and its extent is retired at `generation`. Returns the
    /// retired extents. An entry the basis at `basis_pos` does not retain (never written, already
    /// removed, the commit's own, or newer than the basis) rejects the commit; a position named twice
    /// was already refused by `apply`.
    fn remove_history(
        &self,
        t: &mut TreeTxn,
        cs: &ChangeSet,
        basis_pos: u64,
        generation: u64,
    ) -> Result<ExtentList, CommitError> {
        let invalid = |m: String| CommitError::Rejected(StorageError::InvalidRequest(m));
        let mut retired = Vec::new();
        for c in &cs.changes {
            let Change::RemoveHistory { position } = c else {
                continue;
            };
            let k = history_key(position.0);
            let retained = if position.0 <= basis_pos {
                t.get(&k).map_err(rejected)?
            } else {
                None
            };
            let Some(v) = retained else {
                let removed = t
                    .get(&removed_history_key(position.0))
                    .map_err(rejected)?
                    .is_some();
                return Err(invalid(if removed {
                    format!("history position {} is already removed", position.0)
                } else {
                    format!("history position {} is not retained", position.0)
                }));
            };
            let x = ExtentRef::decode(&v).map_err(rejected)?;
            let entry = decode_history(
                &self
                    .read_extent(&x, HISTORY_ENTRY_MAX, "history entry")
                    .map_err(rejected)?,
            )
            .map_err(rejected)?;
            if entry.position != *position {
                return Err(rejected(corrupt(format!(
                    "history entry at position {} records position {}",
                    position.0, entry.position.0
                ))));
            }
            t.delete(&k).map_err(rejected)?;
            t.put(
                removed_history_key(position.0),
                entry.attempt.as_bytes().to_vec(),
            )
            .map_err(rejected)?;
            t.put(
                retired_extent_key(generation, x.offset),
                x.len.to_le_bytes().to_vec(),
            )
            .map_err(rejected)?;
            retired.push((x.offset, x.len));
        }
        Ok(retired)
    }

    /// Reads an extent this store wrote, committed or staged, checking its digest.
    fn read_written_extent(&self, x: &ExtentRef) -> Result<Vec<u8>, StorageError> {
        let len = usize::try_from(x.len).map_err(|_| corrupt("extent too large"))?;
        let mut buf = vec![0; len];
        self.io.read_at(FileId::Content, x.offset, &mut buf)?;
        if sha256(&buf) != x.digest {
            return Err(corrupt(format!(
                "content extent at {}: digest mismatch",
                x.offset
            )));
        }
        Ok(buf)
    }

    /// Where to write `need` bytes of a staging: the smallest retired extent recorded no later
    /// than `reusable_through` that fits (taken from the pool into `taken`), else the append
    /// cursor `at`.
    fn place(
        &self,
        at: &mut u64,
        need: u64,
        reusable_through: Option<u64>,
        taken: &mut Taken,
    ) -> Result<u64, StorageError> {
        if let Some(bound) = reusable_through {
            // The fence is re-checked at every take, under the lock a fence's change of
            // ownership is made under: a staging that passed its entry check takes nothing once
            // writes are fenced.
            let took = lock(&self.pool)
                .take(taken.owner, need, bound, None)
                .map_err(StorageError::StoreState)?;
            if let Some(r) = took {
                taken.taken.push(r);
                return Ok(r.offset);
            }
        }
        let offset = *at;
        *at = at.checked_add(need).ok_or_else(content_space_exhausted)?;
        Ok(offset)
    }

    /// The newest retiring generation whose space may be overwritten by writes that become
    /// visible no earlier than generation `next` (a commit producing it, or staging while the
    /// current generation is `next - 1`): retired at most two generations before `next`, so
    /// neither publication slot reaches it, and no later than the oldest generation a live view
    /// or content reader pins. `None`: nothing may be reused.
    /// See [`may_reuse`] for how the bound is applied.
    fn reusable_through(&self, next: u64) -> Option<u64> {
        let oldest_pin = lock(&self.pins).keys().next().copied();
        next.checked_sub(2)
            .map(|g| oldest_pin.map_or(g, |p| g.min(p)))
    }

    /// Loads the retired content extents of the current state into the reuse pool (at open).
    fn load_content_pool(&self) -> Result<(), StorageError> {
        let cur = lock(&self.state).current;
        let mut c = Cursor::new(self, cur.root, &[P_RETIRED], None, cur.next_page)?;
        let mut pool = lock(&self.pool);
        while let Some((k, v)) = c.next_entry()? {
            if let Retired::Extent {
                generation,
                offset,
                len,
            } = decode_retired(&k, &v)?
            {
                pool.insert_free(&[(offset, len, generation)])
                    .map_err(|why| corrupt(format!("retirement records: {why}")))?;
            }
        }
        Ok(())
    }

    /// Gives up staged content that will not be committed (see the port). Retired content the
    /// staging took returns to the pool at once (its retirement records were never consumed);
    /// the staging's own space is retired by the next commit.
    pub fn discard_staged(&self, staged: StagedContent) -> Result<(), StorageError> {
        self.refuse_if_read_only("discarding staged content is")?;
        let (serial, (manifest, _)) = self.staged_extent(&staged.token, &staged.content)?;
        let m = Manifest::decode(&self.read_written_extent(&manifest)?)?;
        let extents: Vec<(u64, u64)> = std::iter::once(manifest)
            .chain(m.segments)
            .map(|x| (x.offset, x.len))
            .collect();
        let mut st = lock(&self.state);
        if !st.staged.contains_key(&serial) {
            return Err(StorageError::ForeignHandle);
        }
        let mut pool = lock(&self.pool);
        if pool.is_admitted(serial) {
            return Err(StorageError::InvalidRequest(
                "staged content is bound by an admitted commit; discard it only after that \
                 commit resolves"
                    .into(),
            ));
        }
        if pool.is_uncertain(serial) {
            return Err(StorageError::StoreState(
                "staged content is bound by a commit whose publication is uncertain; reopen the \
                 store to resolve it"
                    .into(),
            ));
        }
        pool.discard(serial, &extents)
            .map_err(|why| StorageError::StoreState(format!("space check failed: {why}")))?;
        st.staged.remove(&serial);
        Ok(())
    }

    /// Checks a commit's changes of space ownership as a whole, after its allocation settled and
    /// before it writes anything: no page retired twice or while the new tree still refers to it,
    /// no retired page reused while referenced, the retirement records it wrote at its generation
    /// exactly the retirements checked, no content retired twice or while a touched digest's live
    /// entry names it, and nothing it frees overlapping space the pool already accounts for. A
    /// violation rejects the commit.
    fn seal(
        &self,
        txn: &TreeTxn,
        committed: PageRef,
        j: &SpaceJournal,
    ) -> Result<(), StorageError> {
        let fail = |why: String| {
            Err(StorageError::StoreState(format!(
                "space check failed: {why}"
            )))
        };
        let mut pages = HashSet::new();
        for p in &j.retired_pages {
            if !pages.insert(p.page) {
                return fail(format!("page {} retired twice", p.page));
            }
        }
        let frontier: HashSet<u64> = txn.frontier().iter().map(|p| p.page).collect();
        if let Some(p) = j.retired_pages.iter().find(|p| frontier.contains(&p.page)) {
            return fail(format!("retired page {} is still referenced", p.page));
        }
        if let Some(p) = j.reused_pages.iter().find(|p| frontier.contains(p)) {
            return fail(format!("reused page {p} is still referenced"));
        }
        let mut freed: Vec<(u64, u64)> = j
            .retired_extents
            .iter()
            .copied()
            .chain(j.remainders.iter().map(|(o, l, _)| (*o, *l)))
            .collect();
        freed.sort_unstable();
        if let Some(w) = freed.windows(2).find(|w| overlap(w[0], w[1])) {
            return fail(format!(
                "content {:?} and {:?} retired over each other",
                w[0], w[1]
            ));
        }
        if let Some(x) = freed
            .iter()
            .find(|x| j.still_reachable.iter().any(|m| overlap(**x, *m)))
        {
            return fail(format!("retired content {x:?} is still referenced"));
        }
        {
            let pool = lock(&self.pool);
            if let Some(x) = freed.iter().find(|x| pool.overlaps(**x, &j.bound)) {
                return fail(format!(
                    "content {x:?} would be freed over space the pool accounts for"
                ));
            }
        }
        // Every retirement record the commit touched ends exactly as the journal says: new
        // records at its generation for what it retires, remainder records at their original
        // generation, consumed and reused records deleted; any other touched record unchanged
        // from the committed state (taken for reuse, then given back).
        let mut expected: HashMap<Vec<u8>, Option<Vec<u8>>> = HashMap::new();
        let mut expect = |k: Vec<u8>, v: Option<Vec<u8>>| expected.insert(k, v).is_none();
        let mut distinct = true;
        for p in &j.retired_pages {
            distinct &= expect(
                retired_page_key(j.generation, p.page),
                Some(p.incarnation.to_le_bytes().to_vec()),
            );
        }
        for (offset, len) in &j.retired_extents {
            distinct &= expect(
                retired_extent_key(j.generation, *offset),
                Some(len.to_le_bytes().to_vec()),
            );
        }
        for (offset, len, retired_at) in &j.remainders {
            distinct &= expect(
                retired_extent_key(*retired_at, *offset),
                Some(len.to_le_bytes().to_vec()),
            );
        }
        for k in j.consumed.iter().chain(&j.reused_records) {
            distinct &= expect(k.clone(), None);
        }
        if !distinct {
            return fail("one retirement record is changed twice".into());
        }
        for (k, want) in &expected {
            if txn.get(k)? != *want {
                return fail(format!(
                    "retirement record {} is not as the journal says",
                    retired_key_label(k)
                ));
            }
        }
        for k in txn
            .touched()
            .iter()
            .filter(|k| k.first() == Some(&P_RETIRED) && !expected.contains_key(*k))
        {
            if txn.get(k)? != tree::get(self, committed, k)? {
                return fail(format!(
                    "retirement record {} changed outside the journal",
                    retired_key_label(k)
                ));
            }
        }
        Ok(())
    }

    /// Counts the retired space a publication consumed as reused (reservations returned to the pool
    /// never count).
    fn count_reused(&self, consumed: &[Reuse]) {
        let bytes = consumed.iter().map(|r| r.used).sum::<u64>();
        self.counters
            .content_bytes_reused
            .fetch_add(bytes, Ordering::Relaxed);
    }

    /// Fences writes until reopen. The pool learns of it under its own lock, so no staging
    /// takes reusable space afterwards.
    fn fence(&self, fence: Fence) {
        lock(&self.state).fenced = Some(fence);
        lock(&self.pool).fence();
    }

    /// Records an inconsistency in space ownership found where it cannot be returned to a caller
    /// (cleanup in a destructor), and fences writes. An existing fence, and the uncertain attempt
    /// it may name, is kept; the fault is recorded beside it, never instead of an operation's own
    /// error, and never by panicking.
    fn fault(&self, why: &str) {
        let why = format!("space check failed: {why}");
        lock(&self.faults).push(why.clone());
        {
            let mut st = lock(&self.state);
            if st.fenced.is_none() {
                st.fenced = Some(Fence {
                    why,
                    uncertain: None,
                });
            }
        }
        lock(&self.pool).fence();
    }

    /// Test hook: total length of the retired extents stagings hold, and of those an uncertain
    /// publication owns.
    #[cfg(any(test, feature = "fuzz-api"))]
    pub(crate) fn reserved_bytes_for_test(&self) -> (u64, u64) {
        let pool = lock(&self.pool);
        let sum = |rs: &[Reuse]| rs.iter().map(|r| r.record_len).sum::<u64>();
        (
            pool.held().map(|(_, rs)| sum(rs)).sum(),
            pool.uncertain().map(|(_, _, _, rs)| sum(rs)).sum(),
        )
    }

    /// Test hook: why writes are fenced, if they are.
    #[cfg(test)]
    pub(crate) fn fence_reason(&self) -> Option<String> {
        lock(&self.state).fenced.as_ref().map(|f| f.why.clone())
    }

    /// Test hook: fences writes as a failed publication would, without one.
    #[cfg(test)]
    pub(crate) fn fence_for_test(&self, why: &str) {
        self.fence(Fence {
            why: why.into(),
            uncertain: None,
        });
    }

    /// The publication of `attempt` as generation `generation` is uncertain: it may be durable
    /// and have consumed the reservations of the stagings it binds (`serials`). Those stay owned
    /// by it, out of reach of reuse and discard, until reopen rebuilds the pool from the
    /// publication that won. A hook for any publication that can become uncertain.
    pub(crate) fn hold_for_uncertain(&self, attempt: &AttemptId, generation: u64, serials: &[u64]) {
        lock(&self.pool).hand_to_uncertain(attempt, generation, serials);
    }

    /// Marks the stagings of a resolved job as no longer admitted.
    fn release_admitted(&self, staged: &[u64]) {
        let mut pool = lock(&self.pool);
        for serial in staged {
            pool.resolve_admission(*serial);
        }
    }

    /// The serial of a staging token this open instance issued, if it is one.
    fn token_serial(&self, token: &StagingToken) -> Option<u64> {
        let mut d = Dec::new(&token.0, "staging token");
        let s: [u8; 16] = d.array().ok()?;
        let inst = d.u64().ok()?;
        let serial = d.u64().ok()?;
        (s == self.store.0 && inst == self.instance).then_some(serial)
    }

    /// Space use of the current state (see `SpaceReport`): a walk of a pinned snapshot, holding no
    /// lock across it. Retirements are split by the reuse rule as of that snapshot.
    pub(crate) fn space_report(self: &Arc<Self>) -> Result<crate::SpaceReport, StorageError> {
        let (snap, _pin, ()) = self.pin_current_then(false, |_| ());
        let d = snap.desc;
        let through = self.reusable_through(d.generation.saturating_add(1));
        let mut r = crate::SpaceReport::default();
        // Every tree page the state reaches.
        let mut stack = vec![d.root];
        while let Some(at) = stack.pop() {
            r.live_pages += 1;
            if r.live_pages > d.next_page {
                return Err(corrupt("tree reaches more pages than the store has"));
            }
            if let Node::Branch { children, .. } = &*self.load(at)? {
                stack.extend(children.iter().copied());
            }
        }
        let mut c = Cursor::new(&**self, d.root, &[P_CONTENT], None, d.next_page)?;
        while let Some((_, v)) = c.next_entry()? {
            if let ContentEntry::Live { manifest, .. } = ContentEntry::decode(&v)? {
                let limit = Manifest::encoded_len(u32::MAX as usize);
                r.live_content_bytes +=
                    Manifest::decode(&self.read_extent(&manifest, limit, "content manifest")?)?
                        .content
                        .len;
            }
        }
        let mut c = Cursor::new(&**self, d.root, &[P_HISTORY], None, d.next_page)?;
        while let Some((_, v)) = c.next_entry()? {
            r.history_entries += 1;
            r.history_bytes += ExtentRef::decode(&v)?.len;
        }
        let mut c = Cursor::new(&**self, d.root, &[P_HISTORY_REMOVED], None, d.next_page)?;
        while let Some((k, v)) = c.next_entry()? {
            r.history_entries_removed += 1;
            r.history_marker_bytes += (k.len() + v.len()) as u64;
        }
        let mut c = Cursor::new(&**self, d.root, &[P_ATTEMPT], None, d.next_page)?;
        while let Some((k, v)) = c.next_entry()? {
            r.attempt_receipts += 1;
            r.attempt_receipt_bytes += (k.len() + v.len()) as u64;
        }
        let mut c = Cursor::new(&**self, d.root, &[P_RETIRED], None, d.next_page)?;
        while let Some((k, v)) = c.next_entry()? {
            match decode_retired(&k, &v)? {
                Retired::Page { generation, .. } if may_reuse(generation, through) => {
                    r.retired_pages_reusable += 1;
                }
                Retired::Page { .. } => r.retired_pages_protected += 1,
                Retired::Extent {
                    generation, len, ..
                } if may_reuse(generation, through) => r.retired_content_bytes_reusable += len,
                Retired::Extent { len, .. } => r.retired_content_bytes_protected += len,
            }
        }
        r.meta_file_bytes = self.io.len(FileId::Meta)?;
        r.content_file_bytes = self.io.len(FileId::Content)?;
        Ok(r)
    }

    /// Total length of retired content extents no staging holds.
    pub fn reusable_content_bytes(&self) -> u64 {
        lock(&self.pool).free_bytes()
    }

    /// Consumes, in the transaction, the retirement records of extents held by the stagings this
    /// commit binds; a record longer than what its staging wrote leaves a record for the rest,
    /// still retired at its original generation. Returns those remainders and the keys of the
    /// records it consumed.
    fn consume_reuse(
        &self,
        t: &mut TreeTxn,
        bound: &[(u64, BoundContent)],
    ) -> Result<Consumed, CommitError> {
        let held: Vec<Reuse> = {
            let pool = lock(&self.pool);
            bound
                .iter()
                .flat_map(|(serial, _)| pool.held_by(*serial).to_vec())
                .collect()
        };
        let (mut remainders, mut consumed) = (Vec::new(), Vec::new());
        for r in held {
            let k = retired_extent_key(r.retired_at, r.offset);
            t.delete(&k).map_err(rejected)?;
            consumed.push(k);
            if r.record_len > r.used {
                let (offset, len) = (r.offset + r.used, r.record_len - r.used);
                t.put(
                    retired_extent_key(r.retired_at, offset),
                    len.to_le_bytes().to_vec(),
                )
                .map_err(rejected)?;
                remainders.push((offset, len, r.retired_at));
            }
        }
        Ok((remainders, consumed))
    }

    pub fn outcome(self: &Arc<Self>, attempt: &AttemptId) -> Result<AttemptOutcome, StorageError> {
        // Order matters: an attempt leaves `pending` only after its publication, so checking
        // `pending` first and the published state second can never miss a finishing commit.
        if lock(&self.pending).contains_key(attempt) {
            return Ok(AttemptOutcome::Pending);
        }
        // The walk reads a pinned state: commits meanwhile cannot reuse its pages. Like any new
        // view, this waits while a commit requiring quiescence runs.
        let (snap, _pin, uncertain) = self.pin_current_then(false, |st| {
            st.fenced
                .as_ref()
                .is_some_and(|f| f.uncertain.as_ref() == Some(attempt))
        });
        if uncertain {
            return Ok(AttemptOutcome::Unknown);
        }
        let cur = snap.desc;
        self.run_walk_hook();
        Ok(
            match tree::get(&**self, cur.root, &key(P_ATTEMPT, &[attempt.as_bytes()]))? {
                Some(v) => AttemptOutcome::Committed(CommitReceipt {
                    attempt: attempt.clone(),
                    position: HistoryPosition(AttemptValue::decode(&v)?.position),
                }),
                None => AttemptOutcome::Absent,
            },
        )
    }

    /// Stops admitting commits, waits until every admitted commit has resolved (published,
    /// refused or failed), then returns. Reads stay available on existing handles.
    pub fn close(&self) -> Result<(), StorageError> {
        {
            let _pending = lock(&self.pending);
            self.closing.store(true, Ordering::SeqCst);
        }
        while !lock(&self.pending).is_empty() {
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        let _w = lock(&self.writer);
        Ok(())
    }

    // ----- reads -------------------------------------------------------------------------------

    pub fn context(
        &self,
        s: &Snapshot,
        id: &ContextId,
    ) -> Result<Option<ContextRecord>, StorageError> {
        let Some(v) = tree::get(self, s.desc.root, &key(P_CONTEXT, &[id.as_bytes()]))? else {
            return Ok(None);
        };
        let v = ContextValue::decode(&v)?;
        Ok(Some(ContextRecord {
            id: id.clone(),
            revision: Revision(v.revision),
            body: v.body,
            content: v.content,
        }))
    }

    /// Lazily yields relationships from one direction's index, one leaf at a time.
    pub fn relationships<'a>(
        &'a self,
        s: &Snapshot,
        q: &RelationshipQuery,
    ) -> Result<impl Iterator<Item = Result<Relationship, StorageError>> + 'a, StorageError> {
        let p = match q.direction {
            Direction::Outgoing => P_OUT,
            Direction::Incoming => P_IN,
        };
        let prefix = relationship_range(q);
        let start = match &q.after {
            None => None,
            Some(a) => {
                if q.kind.as_ref().is_some_and(|k| *k != a.kind) {
                    return Err(StorageError::InvalidRequest(
                        "resume position has a different kind than the query".into(),
                    ));
                }
                let mut k = vec![p];
                key_component(&mut k, q.from.as_bytes());
                key_component(&mut k, a.kind.as_bytes());
                key_component(&mut k, a.other.as_bytes());
                key_component(&mut k, a.id.as_bytes());
                // Strictly after the position: the next possible key.
                k.push(0);
                Some(k)
            }
        };
        let root = s.desc.root;
        let cursor = Cursor::new(self, root, &prefix, start.as_deref(), s.desc.next_page)?;
        Ok(cursor.map(move |entry| {
            let (k, _) = entry?;
            let parts = key_components(&k[1..])?;
            let [_, _, _, id] = parts.as_slice() else {
                return Err(corrupt("relationship index key arity"));
            };
            let rv = tree::get(self, root, &key(P_REL, &[id]))?
                .ok_or_else(|| corrupt("relationship index entry without its record"))?;
            let rv = RelValue::decode(&rv)?;
            let ident =
                |b: &[u8]| corrupt(format!("stored identifier {}", String::from_utf8_lossy(b)));
            Ok(Relationship {
                id: RelationshipId::new(id.clone()).map_err(|_| ident(id))?,
                kind: RelationshipKind::new(rv.kind.clone()).map_err(|_| ident(&rv.kind))?,
                source: ContextId::new(rv.source.clone()).map_err(|_| ident(&rv.source))?,
                target: ContextId::new(rv.target.clone()).map_err(|_| ident(&rv.target))?,
                revision: Revision(rv.revision),
            })
        }))
    }

    /// Lazily yields retained history from `start`.
    pub fn history<'a>(
        &'a self,
        s: &Snapshot,
        start: HistoryPosition,
    ) -> Result<impl Iterator<Item = Result<HistoryEntry, StorageError>> + 'a, StorageError> {
        let from = history_key(start.0.max(1));
        let cursor = Cursor::new(
            self,
            s.desc.root,
            &[P_HISTORY],
            Some(&from),
            s.desc.next_page,
        )?;
        Ok(cursor.map(move |entry| {
            let (_, v) = entry?;
            let x = ExtentRef::decode(&v)?;
            decode_history(&self.read_extent(&x, HISTORY_ENTRY_MAX, "history entry")?)
        }))
    }

    /// Physical segments holding committed content, in stream order (inspection for tests and tools).
    /// The committed manifest of `c` in this view, checked against the request.
    fn manifest(&self, s: &Snapshot, c: &ContentRef) -> Result<Manifest, StorageError> {
        let x =
            tree::get(self, s.desc.root, &key(P_CONTENT, &[&c.digest.0]))?.ok_or_else(|| {
                StorageError::InvalidRequest("content is not committed in this view".into())
            })?;
        let x = match ContentEntry::decode(&x)? {
            ContentEntry::Live { manifest, .. } => manifest,
            ContentEntry::Removed => return Err(StorageError::ContentNotRetained),
        };
        // The format's maximum (a u32 segment count): the open's segment limit bounds staging only.
        let limit = Manifest::encoded_len(u32::MAX as usize);
        let m = Manifest::decode(&self.read_extent(&x, limit, "content manifest")?)?;
        if m.content != *c {
            return Err(corrupt(
                "content manifest does not match the requested digest and length",
            ));
        }
        let committed = lock(&self.state).current.content_len;
        if m.segments.iter().any(|x| {
            x.offset
                .checked_add(x.len)
                .is_none_or(|end| end > committed)
        }) {
            return Err(corrupt("content segment lies outside committed content"));
        }
        Ok(m)
    }

    /// Physical segments holding committed content, in stream order (inspection for tests).
    #[cfg(test)]
    pub(crate) fn content_segments(
        &self,
        s: &Snapshot,
        c: &ContentRef,
    ) -> Result<Vec<ExtentRef>, StorageError> {
        Ok(self.manifest(s, c)?.segments)
    }

    pub fn content(
        self: &Arc<Self>,
        s: &Snapshot,
        c: &ContentRef,
    ) -> Result<SegmentReader, StorageError> {
        let m = self.manifest(s, c)?;
        Ok(SegmentReader {
            inner: self.clone(),
            manifest: m,
            next: 0,
            buf: Vec::new(),
            held: 0,
            pos: 0,
            total: Sha256::new(),
            failed: None,
            _pin: None,
        })
    }
}

/// One admitted commit waiting for the worker.
pub(crate) struct Job {
    inner: Arc<Inner>,
    attempt: AttemptId,
    basis: Snapshot,
    observed: Vec<Observation>,
    changes: ChangeSet,
    reply: mpsc::Sender<Result<CommitReceipt, CommitError>>,
    /// Stagings this job binds, marked admitted until it resolves.
    staged: Vec<u64>,
}

/// A logical read a view made, protected until the view's commit is validated.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Observation {
    /// A point read of one record key, including a read that found nothing.
    Point(Vec<u8>),
    /// A range read: every key with this prefix. The whole queried domain is protected, however
    /// much of the range the caller consumed.
    Range(Vec<u8>),
}

impl Observation {
    fn covers(&self, key: &[u8]) -> bool {
        match self {
            Observation::Point(k) => k.as_slice() == key,
            Observation::Range(p) => key.starts_with(p),
        }
    }
}

/// The key a context point read observes.
pub(crate) fn context_key(id: &ContextId) -> Vec<u8> {
    key(P_CONTEXT, &[id.as_bytes()])
}

/// The key range a relationship query observes: one direction, one node, optionally one kind.
pub(crate) fn relationship_range(q: &RelationshipQuery) -> Vec<u8> {
    let mut prefix = vec![match q.direction {
        Direction::Outgoing => P_OUT,
        Direction::Incoming => P_IN,
    }];
    key_component(&mut prefix, q.from.as_bytes());
    if let Some(k) = &q.kind {
        key_component(&mut prefix, k.as_bytes());
    }
    prefix
}

/// Logical write sets of recent commits. `floor` is the newest history position whose successors'
/// writes are all retained; a basis older than that cannot be validated.
/// Reusable space as (tree pages, content extents `(offset, len)`) (tests only).
#[cfg(any(test, feature = "fuzz-api"))]
pub(crate) type Reusable = (Vec<u64>, Vec<(u64, u64)>);

/// One recorded commit: its history position and the logical keys it wrote.
type ConflictEntry = (u64, Vec<Vec<u8>>);

struct ConflictLog {
    floor: u64,
    entries: VecDeque<ConflictEntry>,
    capacity: usize,
}

fn describe(key: &[u8]) -> String {
    let family = match key.first() {
        Some(&P_CONTEXT) => "context",
        Some(&P_REL) => "relationship",
        Some(&P_OUT) => "outgoing relationship index",
        Some(&P_IN) => "incoming relationship index",
        _ => "record",
    };
    let parts = key_components(key.get(1..).unwrap_or_default()).unwrap_or_default();
    let parts: Vec<String> = parts
        .iter()
        .map(|p| format!("{:?}", String::from_utf8_lossy(p)))
        .collect();
    format!("{family} {}", parts.join("/"))
}

impl ConflictLog {
    fn validate(&self, basis: u64, observed: &[Observation]) -> Result<(), CommitError> {
        if observed.is_empty() {
            return Ok(());
        }
        if basis < self.floor {
            return Err(CommitError::Conflict(format!(
                "read basis at history position {basis} is older than the retained conflict history (from {}); prepare again",
                self.floor
            )));
        }
        for (pos, keys) in self.entries.iter().filter(|(pos, _)| *pos > basis) {
            for k in keys {
                if let Some(o) = observed.iter().find(|o| o.covers(k)) {
                    let what = match o {
                        Observation::Point(_) => "a record read",
                        Observation::Range(_) => "a range read",
                    };
                    return Err(CommitError::Conflict(format!(
                        "{} was written at history position {pos}, after {what} at {basis}",
                        describe(k)
                    )));
                }
            }
        }
        Ok(())
    }

    fn record(&mut self, pos: u64, keys: Vec<Vec<u8>>) {
        self.entries.push_back((pos, keys));
        while self.entries.len() > self.capacity {
            if let Some((old, _)) = self.entries.pop_front() {
                self.floor = old;
            }
        }
    }
}

/// Refuses an open whose selected record contradicts the caller's retained receipt information,
/// before anything is synced or written: another store identity, or a published logical history
/// position below the retained minimum. Generations are not compared: a store may publish
/// generations that do not move the history position.
fn check_guard(d: &Descriptor, guard: Option<crate::ReceiptGuard>) -> Result<(), StorageError> {
    let Some(g) = guard else {
        return Ok(());
    };
    if d.store.0 != g.store_id {
        return Err(StorageError::GuardRefused(
            "the store's identity differs from the guarded identity".into(),
        ));
    }
    if d.history_pos < g.min_history_position.0 {
        return Err(StorageError::GuardRefused(format!(
            "published history position {} is below the guarded minimum {}",
            d.history_pos, g.min_history_position.0
        )));
    }
    Ok(())
}

/// A retirement record taken from the committed tree: key, value and the retired page.
type RetiredRecord = (Vec<u8>, Vec<u8>, u64);

/// Retired pages a commit reuses, each with the key of the retirement record it consumed.
type ReusedPages = Vec<(u64, Vec<u8>)>;

/// A published record and the discarded staging extents its publication retired.
type Published = (Descriptor, Vec<(u64, u64)>);

/// What a maintenance step changes in content space, for its journal (pages are found by the
/// publication itself).
#[derive(Default)]
pub(crate) struct MaintenanceSpace {
    /// Content extents the step retires at its generation.
    pub(crate) retired: Vec<(u64, u64)>,
    /// Unused parts of the retired records it consumes, at their original generation.
    pub(crate) remainders: Vec<(u64, u64, u64)>,
    /// Keys of the retirement records it consumes.
    pub(crate) consumed: Vec<Vec<u8>>,
    /// Extents the new state reaches that this publication must not free.
    pub(crate) still_reachable: Vec<(u64, u64)>,
    /// The reservation serials the step consumes (its own).
    pub(crate) bound: Vec<u64>,
    /// Content entries the step rewrites, by key, with the manifest each must name.
    pub(crate) entries: Vec<(Vec<u8>, ExtentRef)>,
}

/// Upper bound on rounds of settling a commit's page allocation. Each round's edits touch pages
/// already being rewritten (or split from them), so replaced pages and page demand stop changing
/// within a few rounds; a publication that does not settle is rejected. Across the test suites a
/// commit settles in at most 4 rounds and a maintenance step in at most 3, counting the last round,
/// which changes nothing; the reuse, crash-cut sweep and relocation tests require at most 6.
const ALLOCATION_ROUNDS: usize = 16;

/// Records every committed page the transaction replaces as retired at `generation` and takes,
/// for the pages the transaction will write, retired pages recorded no later than
/// `reusable_through` (deleting their records), and with `below` only pages numbered below it.
/// Recording, consuming and returning records all edit the tree, which can replace further pages
/// and change how many pages it needs, so this repeats until a round changes nothing. Returns the
/// pages to write first, with their records' keys, and the number of rounds taken, counting the
/// last one, which changed nothing.
fn settle_allocation(
    txn: &mut TreeTxn,
    src: &dyn NodeSource,
    cur: &Descriptor,
    generation: u64,
    reusable_through: Option<u64>,
    below: Option<u64>,
) -> Result<(ReusedPages, usize), StorageError> {
    // Candidates in record order (oldest retirement first), from the committed tree.
    let mut candidates = match reusable_through {
        Some(_) => Some(Cursor::new(
            src,
            cur.root,
            &[P_RETIRED],
            None,
            cur.next_page,
        )?),
        None => None,
    };
    let next_candidate = |c: &mut Option<Cursor>| -> Result<Option<RetiredRecord>, StorageError> {
        let Some(cursor) = c else { return Ok(None) };
        // Records sort by retiring generation; content extents are not reused by tree pages.
        while let Some((k, v)) = cursor.next_entry()? {
            let retired_at = match decode_retired(&k, &v)? {
                Retired::Page {
                    generation, page, ..
                } => {
                    if may_reuse(generation, reusable_through) && below.is_none_or(|b| page < b) {
                        return Ok(Some((k, v, page)));
                    }
                    generation
                }
                Retired::Extent { generation, .. } => generation,
            };
            if !may_reuse(retired_at, reusable_through) {
                break;
            }
        }
        *c = None;
        Ok(None)
    };
    let mut taken: Vec<RetiredRecord> = Vec::new();
    let mut returned: Vec<RetiredRecord> = Vec::new();
    let mut recorded = 0;
    for round in 1..=ALLOCATION_ROUNDS {
        let mut changed = false;
        let fresh: Vec<PageRef> = txn.replaced()[recorded..].to_vec();
        recorded += fresh.len();
        for p in fresh {
            changed = true;
            txn.put(
                retired_page_key(generation, p.page),
                p.incarnation.to_le_bytes().to_vec(),
            )?;
        }
        let need = txn.dirty_pages();
        while taken.len() < need {
            let next = match returned.pop() {
                Some(x) => Some(x),
                None => next_candidate(&mut candidates)?,
            };
            let Some((k, v, page)) = next else { break };
            txn.delete(&k)?;
            taken.push((k, v, page));
            changed = true;
        }
        while taken.len() > need {
            let Some((k, v, page)) = taken.pop() else {
                break;
            };
            txn.put(k.clone(), v.clone())?;
            returned.push((k, v, page));
            changed = true;
        }
        if !changed {
            let pages = taken.into_iter().map(|(k, _, page)| (page, k)).collect();
            return Ok((pages, round));
        }
    }
    Err(StorageError::InvalidRequest(format!(
        "page allocation did not settle within {ALLOCATION_ROUNDS} rounds"
    )))
}

/// The reuse rule, in one place: space retired at generation `retired_at` may be taken when it is
/// no later than `through` (from `Inner::reusable_through`: two generations before the one being
/// written, and no later than the oldest generation a live view or content reader pins). `None`:
/// nothing may be taken. Space this forbids is what the space report calls protected.
pub(crate) fn may_reuse(retired_at: u64, through: Option<u64>) -> bool {
    through.is_some_and(|b| retired_at <= b)
}

/// Publication records, one per slot page at the start of the metadata file.
const PUBLICATION_SLOTS: u64 = 2;

/// Reads both publication slots and selects the record to recover from, checking its cursors
/// against the files. Reads only.
fn read_publication(io: &dyn Io) -> Result<(Descriptor, Vec<String>), StorageError> {
    let len = io.len(FileId::Meta)?;
    if len < 2 * PAGE_SIZE as u64 {
        return Err(StorageError::Integrity(format!(
            "metadata file is {len} bytes; store initialization never completed (not reinitializing)"
        )));
    }
    let mut slots = Vec::new();
    for s in 0..PUBLICATION_SLOTS {
        let mut buf = vec![0; PAGE_SIZE];
        io.read_at(FileId::Meta, s * PAGE_SIZE as u64, &mut buf)?;
        slots.push(decode_slot(&buf));
    }
    let (desc, diagnostics) = select(&slots)?;
    validate_cursors(io, &desc)?;
    Ok((desc, diagnostics))
}

/// Checks the selected record's cursors against the files before anything is written.
fn validate_cursors(io: &dyn Io, d: &Descriptor) -> Result<(), StorageError> {
    let meta = io.len(FileId::Meta)?;
    let content = io.len(FileId::Content)?;
    let pages_end = d.next_page.checked_mul(PAGE_SIZE as u64);
    let ok = d.root.page >= FIRST_NODE_PAGE
        && d.root.page < d.next_page
        && pages_end.is_some_and(|end| end <= meta)
        && d.content_len <= content;
    if !ok {
        return Err(corrupt(format!(
            "publication record generation {} contradicts the store files (root page {}, next page {}, content {} of {content} bytes, metadata {meta} bytes)",
            d.generation, d.root.page, d.next_page, d.content_len
        )));
    }
    Ok(())
}

/// Chooses the publication record to recover from. Never initializes.
fn select(slots: &[Slot]) -> Result<(Descriptor, Vec<String>), StorageError> {
    let mut diags = Vec::new();
    let mut valid = Vec::new();
    for (i, s) in slots.iter().enumerate() {
        match s {
            Slot::Valid(d) if Descriptor::slot(d.generation) == i as u64 => valid.push(*d),
            Slot::Valid(d) => diags.push(format!(
                "slot {i}: generation {} in wrong slot; ignored",
                d.generation
            )),
            Slot::Empty => {}
            Slot::Invalid(why) => diags.push(format!("slot {i}: damaged or torn ({why}); ignored")),
            // Never the other slot instead: an older record does not make a store this build cannot
            // read readable.
            Slot::Unsupported(why) => {
                return Err(StorageError::Unsupported(format!(
                    "publication record of {why}"
                )));
            }
        }
    }
    if let [a, b] = valid.as_slice()
        && a.store != b.store
    {
        return Err(corrupt("publication slots belong to different stores"));
    }
    let best = valid
        .into_iter()
        .max_by_key(|d| d.generation)
        .ok_or_else(|| {
            corrupt("no valid publication record; refusing to initialize over existing files")
        })?;
    Ok((best, diags))
}

/// Applies a change set to the newest tree, checking preconditions.
/// Each changed record's revision becomes `pos`, the history position of this commit, so a
/// revision is never reused for an identity, even after removal and recreation.
fn apply(
    t: &mut TreeTxn,
    cs: &ChangeSet,
    bound: &[BoundContent],
    pos: u64,
) -> Result<BTreeMap<[u8; 32], i64>, CommitError> {
    let invalid = |m: String| CommitError::Rejected(StorageError::InvalidRequest(m));
    // Change in the number of records using each content digest.
    let mut content_refs: BTreeMap<[u8; 32], i64> = BTreeMap::new();
    // Each logical identity may appear at most once per change set.
    let mut seen = BTreeSet::new();
    for c in &cs.changes {
        let k = match c {
            Change::PutContext { id, .. } | Change::RemoveContext { id, .. } => {
                key(P_CONTEXT, &[id.as_bytes()])
            }
            Change::PutRelationship { id, .. } | Change::RemoveRelationship { id, .. } => {
                key(P_REL, &[id.as_bytes()])
            }
            Change::RemoveHistory { position } => history_key(position.0),
            _ => return Err(invalid(format!("unsupported change {c:?}"))),
        };
        if !seen.insert(k) {
            return Err(invalid(format!(
                "change set names the same identity twice: {c:?}"
            )));
        }
        // A relationship's index keys hold all four of its identifiers; refuse one whose keys
        // would exceed the key limit before anything is applied.
        if let Change::PutRelationship {
            id,
            kind,
            source,
            target,
            ..
        } = c
        {
            let len = key(
                P_OUT,
                &[
                    source.as_bytes(),
                    kind.as_bytes(),
                    target.as_bytes(),
                    id.as_bytes(),
                ],
            )
            .len();
            if len > MAX_KEY {
                return Err(invalid(format!(
                    "relationship {id:?}: its index keys would be {len} bytes, over the \
                     {MAX_KEY}-byte key limit (source, kind, target and id may total at most {} \
                     bytes, each zero byte counting twice)",
                    MAX_KEY - 9
                )));
            }
        }
    }
    let check =
        |what: &str, current: Option<u64>, expect: &Precondition| -> Result<(), CommitError> {
            let ok = match (expect, current) {
                (Precondition::Any, _) => true,
                (Precondition::Absent, None) => true,
                (Precondition::AtRevision(r), Some(c)) => r.0 == c,
                _ => false,
            };
            if ok {
                Ok(())
            } else {
                Err(CommitError::Conflict(format!(
                    "{what}: expected {expect:?}, found revision {current:?}"
                )))
            }
        };
    for c in &cs.changes {
        match c {
            Change::PutContext {
                id,
                expect,
                body,
                content,
            } => {
                let k = key(P_CONTEXT, &[id.as_bytes()]);
                let prev = t
                    .get(&k)
                    .map_err(rejected)?
                    .map(|v| ContextValue::decode(&v))
                    .transpose()
                    .map_err(rejected)?;
                check(
                    &format!("{id:?}"),
                    prev.as_ref().map(|p| p.revision),
                    expect,
                )?;
                let content = match content {
                    None => None,
                    Some(i) => Some(
                        *bound
                            .get(*i)
                            .ok_or_else(|| invalid(format!("content index {i} out of range")))?,
                    ),
                };
                if let Some(p) = prev.as_ref().and_then(|p| p.content) {
                    *content_refs.entry(p.digest.0).or_default() -= 1;
                }
                if let Some((_, c)) = content {
                    *content_refs.entry(c.digest.0).or_default() += 1;
                }
                let revision = pos;
                t.put(
                    k,
                    ContextValue {
                        revision,
                        body: body.clone(),
                        content: content.map(|(_, c)| c),
                    }
                    .encode(),
                )
                .map_err(rejected)?;
            }
            Change::RemoveContext { id, expect } => {
                let k = key(P_CONTEXT, &[id.as_bytes()]);
                let prev = t
                    .get(&k)
                    .map_err(rejected)?
                    .map(|v| ContextValue::decode(&v))
                    .transpose()
                    .map_err(rejected)?;
                check(
                    &format!("{id:?}"),
                    prev.as_ref().map(|p| p.revision),
                    expect,
                )?;
                let Some(prev) = prev else {
                    return Err(invalid(format!("remove of absent {id:?}")));
                };
                if let Some(p) = prev.content {
                    *content_refs.entry(p.digest.0).or_default() -= 1;
                }
                t.delete(&k).map_err(rejected)?;
            }
            Change::PutRelationship {
                id,
                expect,
                kind,
                source,
                target,
            } => {
                let k = key(P_REL, &[id.as_bytes()]);
                let prev = t
                    .get(&k)
                    .map_err(rejected)?
                    .map(|v| RelValue::decode(&v))
                    .transpose()
                    .map_err(rejected)?;
                check(
                    &format!("{id:?}"),
                    prev.as_ref().map(|p| p.revision),
                    expect,
                )?;
                if let Some(p) = &prev {
                    t.delete(&p.out_key(id.as_bytes())).map_err(rejected)?;
                    t.delete(&p.in_key(id.as_bytes())).map_err(rejected)?;
                }
                let v = RelValue {
                    revision: pos,
                    kind: kind.as_bytes().to_vec(),
                    source: source.as_bytes().to_vec(),
                    target: target.as_bytes().to_vec(),
                };
                t.put(v.out_key(id.as_bytes()), Vec::new())
                    .map_err(rejected)?;
                t.put(v.in_key(id.as_bytes()), Vec::new())
                    .map_err(rejected)?;
                t.put(k, v.encode()).map_err(rejected)?;
            }
            Change::RemoveRelationship { id, expect } => {
                let k = key(P_REL, &[id.as_bytes()]);
                let prev = t
                    .get(&k)
                    .map_err(rejected)?
                    .map(|v| RelValue::decode(&v))
                    .transpose()
                    .map_err(rejected)?;
                check(
                    &format!("{id:?}"),
                    prev.as_ref().map(|p| p.revision),
                    expect,
                )?;
                let Some(p) = prev else {
                    return Err(invalid(format!("remove of absent {id:?}")));
                };
                t.delete(&p.out_key(id.as_bytes())).map_err(rejected)?;
                t.delete(&p.in_key(id.as_bytes())).map_err(rejected)?;
                t.delete(&k).map_err(rejected)?;
            }
            // History removal reads the entry it removes; the commit does it (`remove_history`).
            Change::RemoveHistory { .. } => {}
            _ => return Err(invalid(format!("unsupported change {c:?}"))),
        }
    }
    Ok(content_refs)
}

/// Streams committed content one segment at a time.
///
/// Integrity contract: each segment (at most one streaming buffer) is read whole and checked against
/// its digest before any of its bytes are delivered; the digest of the complete stream is checked
/// before end of stream is reported. After a failed check or a failed segment read every further
/// read fails, naming the segment; a new reader starts over.
pub(crate) struct SegmentReader {
    inner: Arc<Inner>,
    manifest: Manifest,
    next: usize,
    buf: Vec<u8>,
    /// Bytes of `buf`'s allocation counted as held in the store's content buffer counter.
    held: u64,
    pos: usize,
    total: Sha256,
    failed: Option<String>,
    /// Keeps the generation this reader's content was found in pinned for the reader's life.
    _pin: Option<Pin>,
}

/// Counts a content stream buffer as held for the guard's life.
struct BufferHold<'a> {
    inner: &'a Inner,
    n: u64,
}

impl<'a> BufferHold<'a> {
    fn new(inner: &'a Inner, n: u64) -> Self {
        inner.buffer_held(n);
        Self { inner, n }
    }
}

impl Drop for BufferHold<'_> {
    fn drop(&mut self) {
        self.inner.buffer_released(self.n);
    }
}

impl Drop for SegmentReader {
    fn drop(&mut self) {
        self.inner.buffer_released(self.held);
    }
}

impl SegmentReader {
    /// Keeps the store's buffer counter equal to the bytes `buf` has allocated. A shorter segment
    /// reuses the allocation, so its capacity, not its length, is what the reader holds.
    fn account(&mut self) {
        let now = self.buf.capacity() as u64;
        if now > self.held {
            self.inner.buffer_held(now - self.held);
        } else {
            self.inner.buffer_released(self.held - now);
        }
        self.held = now;
    }

    /// Holds `pin` (normally a clone of the issuing view's pin) for the reader's life.
    pub(crate) fn pinned(mut self, pin: Pin) -> Self {
        self._pin = Some(pin);
        self
    }
}

/// One live reader's hold on a committed generation. Cloning pins the same generation again;
/// dropping releases it.
pub(crate) struct Pin {
    inner: Arc<Inner>,
    generation: u64,
    /// Held by a view or content reader rather than by one of the engine's own walks.
    reader: bool,
}

impl Clone for Pin {
    fn clone(&self) -> Self {
        *lock(&self.inner.pins).entry(self.generation).or_default() += 1;
        if self.reader {
            self.inner.readers.fetch_add(1, Ordering::SeqCst);
        }
        Self {
            inner: self.inner.clone(),
            generation: self.generation,
            reader: self.reader,
        }
    }
}

impl Drop for Pin {
    fn drop(&mut self) {
        if self.reader {
            self.inner.readers.fetch_sub(1, Ordering::SeqCst);
        }
        let mut pins = lock(&self.inner.pins);
        if let Some(n) = pins.get_mut(&self.generation) {
            *n -= 1;
            if *n == 0 {
                pins.remove(&self.generation);
            }
        }
    }
}

impl Read for SegmentReader {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        if let Some(f) = &self.failed {
            return Err(io::Error::new(io::ErrorKind::InvalidData, f.clone()));
        }
        if self.pos == self.buf.len() {
            let Some(x) = self.manifest.segments.get(self.next).copied() else {
                let got: [u8; 32] = self.total.clone().finalize().into();
                if got != self.manifest.content.digest.0 {
                    let why = "content stream digest mismatch".to_string();
                    self.failed = Some(why.clone());
                    // A failed reader holds no buffer.
                    self.buf = Vec::new();
                    self.account();
                    self.pos = 0;
                    return Err(io::Error::new(io::ErrorKind::InvalidData, why));
                }
                return Ok(0);
            };
            self.buf.resize(x.len as usize, 0);
            self.account();
            if let Err(e) = self
                .inner
                .io
                .read_at(FileId::Content, x.offset, &mut self.buf)
            {
                // Nothing of a segment that was not read and verified may stay readable.
                let why = format!("content segment {} at offset {}: {e}", self.next, x.offset);
                self.failed = Some(why.clone());
                // Free the allocation: a failed reader holds no buffer.
                self.buf = Vec::new();
                self.account();
                self.pos = 0;
                return Err(io::Error::other(why));
            }
            if sha256(&self.buf) != x.digest {
                let why = format!(
                    "content segment {} at offset {}: digest mismatch",
                    self.next, x.offset
                );
                self.failed = Some(why.clone());
                // Free the allocation: a failed reader holds no buffer.
                self.buf = Vec::new();
                self.account();
                self.pos = 0;
                return Err(io::Error::new(io::ErrorKind::InvalidData, why));
            }
            self.total.update(&self.buf);
            self.next += 1;
            self.pos = 0;
        }
        let n = out.len().min(self.buf.len() - self.pos);
        out[..n].copy_from_slice(&self.buf[self.pos..self.pos + n]);
        self.pos += n;
        Ok(n)
    }
}

impl ContentReader for SegmentReader {}
