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

use std::collections::{BTreeSet, HashMap, VecDeque};
use std::io::{self, Read};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, mpsc};

use cce_core::storage::{
    AttemptId, AttemptOutcome, Change, ChangeSet, CommitError, CommitReceipt, ContentDigest,
    ContentReader, ContentRef, ContextId, ContextRecord, Direction, HistoryEntry, HistoryPosition,
    Precondition, Relationship, RelationshipId, RelationshipKind, RelationshipQuery, Revision,
    StagedContent, StagingToken, StorageError,
};
use sha2::{Digest, Sha256};

use crate::codec::{Dec, Enc, corrupt, key_component, key_components};
use crate::format::{
    Descriptor, FIRST_NODE_PAGE, PAGE_SIZE, PageKind, PageRef, Slot, StoreId, decode_page,
    decode_slot, encode_page,
};
use crate::io::{FileId, Io};
use crate::records::{
    AttemptValue, ContextValue, ExtentRef, Manifest, P_ATTEMPT, P_CONTENT, P_CONTEXT, P_HISTORY,
    P_IN, P_OUT, P_REL, RelValue, decode_history, encode_history, fingerprint, history_key, key,
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
    state: Mutex<State>,
    /// Serializes commits (one publication order per store).
    writer: Mutex<()>,
    /// Content-file allocation cursor; held while streaming an allocation.
    alloc: Mutex<u64>,
    cache: Mutex<NodeCache>,
    /// Logical write sets of recent commits, for validating older read bases.
    conflicts: Mutex<ConflictLog>,
    /// Sender to the commit worker.
    worker: Mutex<Option<mpsc::SyncSender<Job>>>,
    /// Attempts admitted to the worker and not yet resolved.
    pending: Mutex<HashMap<AttemptId, u32>>,
    #[cfg(test)]
    gate: Mutex<()>,
    /// Makes the next commit panic inside the worker (tests of worker robustness only).
    #[cfg(test)]
    panic_next_commit: std::sync::atomic::AtomicBool,
    /// Runs once at the start of `outcome`, before it looks at pending attempts or the published
    /// state (tests of answer ordering only).
    #[cfg(test)]
    outcome_hook: Mutex<Option<Box<dyn FnOnce() + Send>>>,
    /// Largest number of segments one content stream may have.
    max_segments: usize,
    /// Largest variable-length request field or list (see `Options::max_request_field`).
    max_request_field: u64,
    /// Opened for inspection only: no commit worker, no staging, no commits, no syncs.
    read_only: bool,
    pub counters: Counters,
    /// Recovery diagnostics from open (for example an ignored damaged slot).
    pub diagnostics: Vec<String>,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

fn sha256(b: &[u8]) -> [u8; 32] {
    Sha256::digest(b).into()
}

/// The content file cannot grow further without wrapping its byte offsets.
fn content_space_exhausted() -> StorageError {
    StorageError::InvalidRequest("content space exhausted".into())
}

fn rejected(e: StorageError) -> CommitError {
    CommitError::Rejected(e)
}

/// Writes new tree nodes at fresh pages for one generation.
struct PageWriter<'a> {
    inner: &'a Inner,
    next_page: u64,
    generation: u64,
}

impl NodeSink for PageWriter<'_> {
    fn write(&mut self, node: &Node) -> Result<PageRef, StorageError> {
        let at = PageRef {
            page: self.next_page,
            incarnation: self.generation,
        };
        let next = self
            .next_page
            .checked_add(1)
            .ok_or_else(|| StorageError::InvalidRequest("page counter exhausted".into()))?;
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
        Ok(Arc::new(inner))
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
            state: Mutex::new(State {
                current: desc,
                staged: HashMap::new(),
                next_stage: 1,
                fenced: None,
            }),
            writer: Mutex::new(()),
            cache: Mutex::new(NodeCache {
                map: HashMap::new(),
                order: VecDeque::new(),
                capacity: opts.cache_nodes,
            }),
            worker: Mutex::new(None),
            pending: Mutex::new(HashMap::new()),
            #[cfg(test)]
            gate: Mutex::new(()),
            #[cfg(test)]
            panic_next_commit: std::sync::atomic::AtomicBool::new(false),
            #[cfg(test)]
            outcome_hook: Mutex::new(None),
            conflicts: Mutex::new(ConflictLog {
                floor: desc.history_pos,
                entries: VecDeque::new(),
                capacity: opts.conflict_window.max(1),
            }),
            max_segments: opts.max_content_segments,
            max_request_field: opts.max_request_field,
            read_only: false,
            counters: Counters::default(),
            diagnostics,
        }
    }

    /// Test hook: makes the next commit panic inside the worker.
    #[cfg(test)]
    pub(crate) fn panic_next_commit_for_test(&self) {
        self.panic_next_commit.store(true, Ordering::SeqCst);
    }

    /// Test hook: holds the commit worker before its next job until the guard is dropped.
    #[cfg(test)]
    pub(crate) fn pause_worker_for_test(&self) -> MutexGuard<'_, ()> {
        lock(&self.gate)
    }

    /// Test hook: installs (or with `None` removes) the callback `outcome` runs once at its start,
    /// before it looks at pending attempts or the published state. Returns the callback it replaced.
    #[cfg(test)]
    pub(crate) fn set_outcome_hook_for_test(
        &self,
        hook: Option<Box<dyn FnOnce() + Send>>,
    ) -> Option<Box<dyn FnOnce() + Send>> {
        std::mem::replace(&mut *lock(&self.outcome_hook), hook)
    }

    /// Starts the store's commit worker: the single thread that orders publications. The worker
    /// holds no reference to the engine; each queued job holds a strong one, so admitted work
    /// completes even if every caller handle is dropped, and an idle worker never keeps the store
    /// owned. The worker exits when the engine (and with it the queue's sender) is dropped.
    fn start_worker(self: &Arc<Self>, capacity: usize) -> Result<(), StorageError> {
        let (tx, rx) = mpsc::sync_channel::<Job>(capacity.max(1));
        *lock(&self.worker) = Some(tx);
        std::thread::Builder::new()
            .name("cce-commit".into())
            .spawn(move || {
                for job in rx {
                    let inner = job.inner.clone();
                    #[cfg(test)]
                    drop(lock(&inner.gate));
                    let attempt = job.attempt.clone();
                    let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        inner.commit(attempt.clone(), job.basis, &job.observed, job.changes)
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
                    inner.finish_pending(&job.attempt);
                    // The caller may have gone away; the commit stands either way.
                    let _ = job.reply.send(r);
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
        *lock(&self.pending).entry(attempt.clone()).or_default() += 1;
        let job = Job {
            inner: self.clone(),
            attempt: attempt.clone(),
            basis,
            observed,
            changes,
            reply,
        };
        match tx.try_send(job) {
            Ok(()) => Ok(rx),
            Err(e) => {
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
        let mut p = lock(&self.pending);
        if let Some(n) = p.get_mut(attempt) {
            *n -= 1;
            if *n == 0 {
                p.remove(attempt);
            }
        }
    }

    pub fn snapshot(&self) -> Result<Snapshot, StorageError> {
        Ok(Snapshot {
            desc: lock(&self.state).current,
        })
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

    pub fn height(&self) -> Result<usize, StorageError> {
        let root = lock(&self.state).current.root;
        tree::height(self, root)
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
        let start = *cursor;
        let mut at = start;
        let mut total = Sha256::new();
        let mut len = 0u64;
        let mut segments = Vec::new();
        let mut buf = vec![0; CONTENT_CHUNK];
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
            let end = at
                .checked_add(filled as u64)
                .ok_or_else(content_space_exhausted)?;
            self.io.write_at(FileId::Content, at, seg)?;
            self.counters
                .content_bytes_written
                .fetch_add(filled as u64, Ordering::Relaxed);
            segments.push(ExtentRef {
                offset: at,
                len: filled as u64,
                digest: sha256(seg),
            });
            total.update(seg);
            at = end;
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
        let end = at
            .checked_add(manifest.len() as u64)
            .ok_or_else(content_space_exhausted)?;
        self.io.write_at(FileId::Content, at, &manifest)?;
        self.counters
            .content_bytes_written
            .fetch_add(manifest.len() as u64, Ordering::Relaxed);
        let mref = ExtentRef {
            offset: at,
            len: manifest.len() as u64,
            digest: sha256(&manifest),
        };
        *cursor = end;
        let serial = {
            let mut st = lock(&self.state);
            let serial = st.next_stage;
            st.next_stage += 1;
            st.staged.insert(serial, (mref, content));
            serial
        };
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
        crate::records::check_request_bounds(&cs, self.max_request_field).map_err(rejected)?;
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

        // The basis must still hold: nothing it observed may have been written since.
        lock(&self.conflicts).validate(basis.desc.history_pos, observed)?;

        let pos = cur.history_pos.checked_add(1).ok_or_else(|| {
            rejected(StorageError::InvalidRequest(
                "history position overflow".into(),
            ))
        })?;
        let bound = self.resolve_content(&cs)?;
        let extents: Vec<BoundContent> = bound.iter().map(|(_, x)| *x).collect();
        apply(&mut txn, &cs, &extents, pos)?;
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
        let mut sink = PageWriter {
            inner: self,
            next_page: cur.next_page,
            generation,
        };
        let root = txn.finish(&mut sink).map_err(rejected)?;
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

    pub fn outcome(&self, attempt: &AttemptId) -> Result<AttemptOutcome, StorageError> {
        // Test seam: lets a concurrent commit finish after this call began, before it looks at
        // anything (tests of answer ordering only).
        #[cfg(test)]
        {
            let hook = lock(&self.outcome_hook).take();
            if let Some(hook) = hook {
                hook();
            }
        }
        if lock(&self.pending).contains_key(attempt) {
            return Ok(AttemptOutcome::Pending);
        }
        let st = lock(&self.state);
        if st
            .fenced
            .as_ref()
            .is_some_and(|f| f.uncertain.as_ref() == Some(attempt))
        {
            return Ok(AttemptOutcome::Unknown);
        }
        let cur = st.current;
        drop(st);
        Ok(
            match tree::get(self, cur.root, &key(P_ATTEMPT, &[attempt.as_bytes()]))? {
                Some(v) => AttemptOutcome::Committed(CommitReceipt {
                    attempt: attempt.clone(),
                    position: HistoryPosition(AttemptValue::decode(&v)?.position),
                }),
                None => AttemptOutcome::Absent,
            },
        )
    }

    pub fn close(&self) -> Result<(), StorageError> {
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
        let x = ExtentRef::decode(&x)?;
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
            pos: 0,
            total: Sha256::new(),
            failed: None,
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
struct ConflictLog {
    floor: u64,
    entries: VecDeque<(u64, Vec<Vec<u8>>)>,
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
    for s in 0..2u64 {
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
) -> Result<(), CommitError> {
    let invalid = |m: String| CommitError::Rejected(StorageError::InvalidRequest(m));
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
                if let Some((manifest, c)) = content {
                    let ck = key(P_CONTENT, &[&c.digest.0]);
                    if t.get(&ck).map_err(rejected)?.is_none() {
                        t.put(ck, manifest.encode()).map_err(rejected)?;
                    }
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
                if prev.is_none() {
                    return Err(invalid(format!("remove of absent {id:?}")));
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
    Ok(())
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
    pos: usize,
    total: Sha256,
    failed: Option<String>,
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
            if let Err(e) = self
                .inner
                .io
                .read_at(FileId::Content, x.offset, &mut self.buf)
            {
                // Nothing of a segment that was not read and verified may stay readable.
                let why = format!("content segment {} at offset {}: {e}", self.next, x.offset);
                self.failed = Some(why.clone());
                self.buf.clear();
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
