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

use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};
use std::io::{self, Read};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, mpsc};

use cce_core::storage::{
    AttemptId, AttemptOutcome, Change, ChangeSet, CommitError, CommitReceipt, ContentDigest,
    ContentReader, ContentRef, ContextId, ContextRecord, Direction, HistoryEntry, HistoryPosition,
    Precondition, Relationship, RelationshipId, RelationshipKind, RelationshipQuery, RemovalMode,
    Revision, StagedContent, StagingToken, StorageError,
};
use sha2::{Digest, Sha256};

use crate::codec::{Dec, Enc, corrupt, key_component, key_components};
use crate::format::{
    Descriptor, FIRST_NODE_PAGE, PAGE_SIZE, PageKind, PageRef, Slot, StoreId, decode_page,
    decode_slot, encode_page,
};
use crate::io::{FileId, Io};
use crate::records::{
    AttemptValue, ContentEntry, ContextValue, ExtentRef, Manifest, P_ATTEMPT, P_CONTENT, P_CONTEXT,
    P_HISTORY, P_IN, P_OUT, P_REL, P_RETIRED, RelValue, Retired, decode_history, decode_retired,
    encode_history, fingerprint, history_key, key, retired_extent_key, retired_page_key,
};
use crate::tree::{self, Cursor, Node, NodeSink, NodeSource, TreeTxn};

/// Content streaming buffer. Payloads of any length stream through a buffer of this size.
pub(crate) const CONTENT_CHUNK: usize = 64 * 1024;
/// Default segment quota per content stream: 2^20 segments of 64 KiB (64 GiB).
pub(crate) const DEFAULT_MAX_CONTENT_SEGMENTS: usize = 1 << 20;
/// Default number of decoded tree nodes kept in memory.
pub(crate) const DEFAULT_CACHE_NODES: usize = 256;

/// Staged or bound content: its manifest's location and its logical reference.
type BoundContent = (ExtentRef, ContentRef);

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
    /// Largest encoded size of a whole request, which history stores as one length-prefixed field.
    pub max_request_len: u64,
}

/// Default number of recent commits whose write sets are kept for conflict validation.
pub(crate) const DEFAULT_CONFLICT_WINDOW: usize = 4096;
/// Default number of admitted commits that may wait for the commit worker.
pub(crate) const DEFAULT_COMMIT_QUEUE: usize = 64;

impl Default for Options {
    fn default() -> Self {
        Self {
            cache_nodes: DEFAULT_CACHE_NODES,
            conflict_window: DEFAULT_CONFLICT_WINDOW,
            commit_queue: DEFAULT_COMMIT_QUEUE,
            max_content_segments: DEFAULT_MAX_CONTENT_SEGMENTS,
            max_request_field: u32::MAX as u64,
            max_request_len: u32::MAX as u64,
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

/// One retired content extent a staging took for reuse: it writes `used` bytes at its start.
#[derive(Clone, Copy, Debug)]
struct Reuse {
    offset: u64,
    used: u64,
    record_len: u64,
    retired_at: u64,
}

/// Retired content-file extents of the published state that staging may reuse.
#[derive(Default)]
struct ContentPool {
    /// Not held by any staging: offset -> (length, retiring generation).
    free: BTreeMap<u64, (u64, u64)>,
    /// Held by stagings of this instance, by staging serial, until a commit binding the staging
    /// publishes (consuming them) or the store is reopened (abandoned stagings).
    held: HashMap<u64, Vec<Reuse>>,
    /// Stagings bound by admitted commits (by serial, with how many); they cannot be discarded.
    admitted: HashMap<u64, u32>,
    /// Extents of discarded stagings that no retirement record covers yet; the next commit
    /// retires them.
    discarded: Vec<(u64, u64)>,
}

/// Extents one staging call took from the pool; returned to it unless the staging completes.
struct Taken<'a> {
    pool: &'a Mutex<ContentPool>,
    taken: Vec<Reuse>,
}

impl Taken<'_> {
    /// Records the extents as held by staging `serial`.
    fn hold(mut self, serial: u64) {
        let taken = std::mem::take(&mut self.taken);
        if !taken.is_empty() {
            lock(self.pool).held.insert(serial, taken);
        }
    }
}

impl Drop for Taken<'_> {
    fn drop(&mut self) {
        if !self.taken.is_empty() {
            let mut pool = lock(self.pool);
            for r in self.taken.drain(..) {
                pool.free.insert(r.offset, (r.record_len, r.retired_at));
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
    #[cfg(test)]
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
            .free
            .iter()
            .filter(|(_, (_, retired_at))| may_reuse(*retired_at, content_bound))
            .map(|(offset, (len, _))| (*offset, *len))
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
                        lock(&inner.state).fenced = Some(Fence {
                            why: why.clone(),
                            uncertain: Some(attempt.clone()),
                        });
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
                .inspect(|serial| *pool.admitted.entry(*serial).or_default() += 1)
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

    pub(crate) fn read_extent(&self, x: &ExtentRef) -> Result<Vec<u8>, StorageError> {
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

    // ----- staging -----------------------------------------------------------------------------

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
        let mut cursor = lock(&self.alloc);
        // Retired content this staging may reuse: no publication slot reaches it (retired at
        // most one generation before the current one) and no live reader's generation does.
        let reusable_through = {
            let current = lock(&self.state).current.generation;
            self.reusable_through(current.saturating_add(1))
        };
        let mut taken = Taken {
            pool: &self.pool,
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
        let extents: Vec<BoundContent> = bound.iter().map(|(_, x)| *x).collect();
        let content_refs = apply(&mut txn, &cs, &extents, pos)?;
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
        let retired_content = self.settle_content(&mut txn, &extents, content_refs, generation)?;
        let remainders = self.consume_reuse(&mut txn, &bound)?;
        // Discarded stagings' own space becomes retired with this commit.
        let discarded: Vec<(u64, u64)> = lock(&self.pool).discarded.clone();
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
        let reuse = settle_allocation(&mut txn, self, &cur, generation, reusable_through)
            .map_err(rejected)?;
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
            lock(&self.state).fenced = Some(Fence {
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
            lock(&self.state).fenced = Some(Fence {
                why: why.clone(),
                uncertain: Some(attempt.clone()),
            });
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
        let mut pool = lock(&self.pool);
        for (serial, _) in &bound {
            pool.held.remove(serial);
        }
        for (offset, len) in retired_content {
            pool.free.insert(offset, (len, generation));
        }
        for (offset, len, retired_at) in remainders {
            pool.free.insert(offset, (len, retired_at));
        }
        for (offset, len) in &discarded {
            pool.free.insert(*offset, (*len, generation));
        }
        pool.discarded.retain(|x| !discarded.contains(x));
        drop(pool);
        Ok(CommitReceipt {
            attempt,
            position: HistoryPosition(pos),
        })
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
    /// published state ever reaches it.
    fn settle_content(
        &self,
        t: &mut TreeTxn,
        bound: &[BoundContent],
        refs: BTreeMap<[u8; 32], i64>,
        generation: u64,
    ) -> Result<Vec<(u64, u64)>, CommitError> {
        let mut retired = Vec::new();
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
                        self.retire_content(t, &manifest, generation, &mut retired)?;
                    } else if now as u64 != n {
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
            self.retire_content(t, &o, generation, &mut retired)?;
        }
        Ok(retired)
    }

    /// Records a content manifest and every segment it names as retired at `generation`, and
    /// lists them in `retired` as `(offset, len)`.
    fn retire_content(
        &self,
        t: &mut TreeTxn,
        manifest: &ExtentRef,
        generation: u64,
        retired: &mut Vec<(u64, u64)>,
    ) -> Result<(), CommitError> {
        let m = Manifest::decode(&self.read_written_extent(manifest).map_err(rejected)?)
            .map_err(rejected)?;
        for x in std::iter::once(*manifest).chain(m.segments) {
            t.put(
                retired_extent_key(generation, x.offset),
                x.len.to_le_bytes().to_vec(),
            )
            .map_err(rejected)?;
            retired.push((x.offset, x.len));
        }
        Ok(())
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
            let mut pool = lock(&self.pool);
            let best = pool
                .free
                .iter()
                .filter(|(_, (len, retired_at))| {
                    *len >= need && may_reuse(*retired_at, Some(bound))
                })
                .min_by_key(|(offset, (len, _))| (*len, **offset))
                .map(|(offset, (len, retired_at))| (*offset, *len, *retired_at));
            if let Some((offset, record_len, retired_at)) = best {
                pool.free.remove(&offset);
                self.counters
                    .content_bytes_reused
                    .fetch_add(need, Ordering::Relaxed);
                taken.taken.push(Reuse {
                    offset,
                    used: need,
                    record_len,
                    retired_at,
                });
                return Ok(offset);
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
                pool.free.insert(offset, (len, generation));
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
        if pool.admitted.get(&serial).is_some_and(|n| *n > 0) {
            return Err(StorageError::InvalidRequest(
                "staged content is bound by an admitted commit; discard it only after that \
                 commit resolves"
                    .into(),
            ));
        }
        st.staged.remove(&serial);
        let held = pool.held.remove(&serial).unwrap_or_default();
        for r in &held {
            pool.free.insert(r.offset, (r.record_len, r.retired_at));
        }
        pool.discarded.extend(
            extents
                .into_iter()
                .filter(|(offset, _)| !held.iter().any(|r| r.offset == *offset)),
        );
        Ok(())
    }

    /// Marks the stagings of a resolved job as no longer admitted.
    fn release_admitted(&self, staged: &[u64]) {
        let mut pool = lock(&self.pool);
        for serial in staged {
            if let Some(n) = pool.admitted.get_mut(serial) {
                *n -= 1;
                if *n == 0 {
                    pool.admitted.remove(serial);
                }
            }
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

    /// Total length of retired content extents no staging holds.
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
                r.live_content_bytes +=
                    Manifest::decode(&self.read_extent(&manifest)?)?.content.len;
            }
        }
        let mut c = Cursor::new(&**self, d.root, &[P_HISTORY], None, d.next_page)?;
        while let Some((_, v)) = c.next_entry()? {
            r.history_entries += 1;
            r.history_bytes += ExtentRef::decode(&v)?.len;
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

    pub fn reusable_content_bytes(&self) -> u64 {
        lock(&self.pool).free.values().map(|(len, _)| len).sum()
    }

    /// Consumes, in the transaction, the retirement records of extents held by the stagings this
    /// commit binds; a record longer than what its staging wrote leaves a record for the rest,
    /// still retired at its original generation. Returns those remainders.
    fn consume_reuse(
        &self,
        t: &mut TreeTxn,
        bound: &[(u64, BoundContent)],
    ) -> Result<Vec<(u64, u64, u64)>, CommitError> {
        let held: Vec<Reuse> = {
            let pool = lock(&self.pool);
            bound
                .iter()
                .flat_map(|(serial, _)| pool.held.get(serial).cloned().unwrap_or_default())
                .collect()
        };
        let mut remainders = Vec::new();
        for r in held {
            t.delete(&retired_extent_key(r.retired_at, r.offset))
                .map_err(rejected)?;
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
        Ok(remainders)
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
            decode_history(&self.read_extent(&ExtentRef::decode(&v)?)?)
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
        let limit = Manifest::encoded_len(self.max_segments);
        if x.len > limit {
            return Err(corrupt(format!(
                "content manifest of {} bytes exceeds the {limit} byte limit",
                x.len
            )));
        }
        let m = Manifest::decode(&self.read_extent(&x)?)?;
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
#[cfg(test)]
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

/// A retirement record taken from the committed tree: key, value and the retired page.
type RetiredRecord = (Vec<u8>, Vec<u8>, u64);

/// Upper bound on rounds of settling a commit's page allocation. Each round's edits touch pages
/// already being rewritten (or split from them), so replaced pages and page demand stop changing
/// within a few rounds; a commit that does not settle is rejected.
const ALLOCATION_ROUNDS: usize = 16;

/// Records every committed page the transaction replaces as retired at `generation` and takes,
/// for the pages the transaction will write, retired pages recorded no later than
/// `reusable_through` (deleting their records). Recording, consuming and returning records all
/// edit the tree, which can replace further pages and change how many pages it needs, so this
/// repeats until a round changes nothing. Returns the pages to write first.
fn settle_allocation(
    txn: &mut TreeTxn,
    src: &dyn NodeSource,
    cur: &Descriptor,
    generation: u64,
    reusable_through: Option<u64>,
) -> Result<Vec<u64>, StorageError> {
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
                    if may_reuse(generation, reusable_through) {
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
    for _ in 0..ALLOCATION_ROUNDS {
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
            return Ok(taken.into_iter().map(|(_, _, page)| page).collect());
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
            Slot::Unsupported(v) => {
                return Err(StorageError::Unsupported(format!(
                    "publication record format version {v}"
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
        };
        if !seen.insert(k) {
            return Err(invalid(format!(
                "change set names the same identity twice: {c:?}"
            )));
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
    /// Bytes of `buf` counted as held in the store's content buffer counter.
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
    /// Keeps the store's buffer counter equal to the bytes `buf` now holds.
    fn account(&mut self) {
        let now = self.buf.len() as u64;
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
                self.buf.clear();
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
                self.buf.clear();
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
