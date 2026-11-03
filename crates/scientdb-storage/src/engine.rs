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
use std::sync::{Arc, Mutex, MutexGuard};

use scientdb_kernel::storage::{
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
    AttemptValue, ContextValue, ExtentRef, P_ATTEMPT, P_CONTENT, P_CONTEXT, P_HISTORY, P_IN, P_OUT,
    P_REL, RelValue, decode_history, encode_history, fingerprint, history_key, key,
};
use crate::tree::{self, Cursor, Node, NodeSink, NodeSource, TreeTxn};

/// Content streaming buffer. Payloads of any length stream through a buffer of this size.
pub(crate) const CONTENT_CHUNK: usize = 64 * 1024;
/// Default number of decoded tree nodes kept in memory.
pub(crate) const DEFAULT_CACHE_NODES: usize = 256;

static NEXT_INSTANCE: AtomicU64 = AtomicU64::new(1);

/// Engine tuning that does not affect stored bytes.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Options {
    pub cache_nodes: usize,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            cache_nodes: DEFAULT_CACHE_NODES,
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
    staged: HashMap<u64, ExtentRef>,
    /// Next staging serial; each staging gets a distinct handle even for zero-length content.
    next_stage: u64,
    /// Set when a publication outcome is uncertain; further writes are refused.
    fenced: Option<String>,
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
        let page = encode_page(self.inner.store, PageKind::Node, at, &node.encode())?;
        self.inner
            .io
            .write_at(FileId::Meta, at.page * PAGE_SIZE as u64, &page)?;
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
        Ok(Arc::new(Self::new(io, desc, Vec::new(), opts)))
    }

    pub fn open(io: Box<dyn Io>, opts: Options) -> Result<Arc<Self>, StorageError> {
        io.open()?;
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
        validate_cursors(&*io, &desc)?;
        // The selected record and everything it references may still be only in the operating
        // system's cache (the previous owner exited before its barrier). Make them durable before
        // any receipt or new commit can rely on them.
        io.sync(FileId::Content)?;
        io.sync(FileId::Meta)?;
        Ok(Arc::new(Self::new(io, desc, diagnostics, opts)))
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
            counters: Counters::default(),
            diagnostics,
        }
    }

    pub fn snapshot(&self) -> Result<Snapshot, StorageError> {
        Ok(Snapshot {
            desc: lock(&self.state).current,
        })
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
        if let Some(f) = &lock(&self.state).fenced {
            return Err(StorageError::StoreState(format!("writes fenced: {f}")));
        }
        let mut cursor = lock(&self.alloc);
        let offset = *cursor;
        let mut hasher = Sha256::new();
        let mut buf = vec![0; CONTENT_CHUNK];
        let mut len = 0u64;
        loop {
            let n = match bytes.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => n,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(StorageError::Io(format!("staging source: {e}"))),
            };
            self.io.write_at(FileId::Content, offset + len, &buf[..n])?;
            self.counters
                .content_bytes_written
                .fetch_add(n as u64, Ordering::Relaxed);
            hasher.update(&buf[..n]);
            len += n as u64;
        }
        *cursor = offset + len;
        let digest: [u8; 32] = hasher.finalize().into();
        let serial = {
            let mut st = lock(&self.state);
            let serial = st.next_stage;
            st.next_stage += 1;
            st.staged.insert(
                serial,
                ExtentRef {
                    offset,
                    len,
                    digest,
                },
            );
            serial
        };
        let mut t = Enc::default();
        t.raw(&self.store.0)
            .u64(self.instance)
            .u64(serial)
            .u64(len)
            .raw(&digest);
        Ok(StagedContent {
            content: ContentRef {
                digest: ContentDigest(digest),
                len,
            },
            token: StagingToken(t.buf),
        })
    }

    /// Resolves a staging token issued by this open instance.
    fn staged_extent(
        &self,
        token: &StagingToken,
        content: &ContentRef,
    ) -> Result<(u64, ExtentRef), StorageError> {
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
            Some(x)
                if x.len == len
                    && x.digest == dig
                    && content.len == len
                    && content.digest.0 == dig =>
            {
                Ok((serial, x))
            }
            _ => Err(StorageError::ForeignHandle),
        }
    }

    // ----- commit ------------------------------------------------------------------------------

    pub fn commit(
        &self,
        attempt: AttemptId,
        _basis: Snapshot,
        cs: ChangeSet,
    ) -> Result<CommitReceipt, CommitError> {
        let _w = lock(&self.writer);
        let cur = {
            let st = lock(&self.state);
            if let Some(f) = &st.fenced {
                return Err(rejected(StorageError::StoreState(format!(
                    "writes fenced: {f}"
                ))));
            }
            st.current
        };
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

        let pos = cur.history_pos.checked_add(1).ok_or_else(|| {
            rejected(StorageError::InvalidRequest(
                "history position overflow".into(),
            ))
        })?;
        let bound = self.resolve_content(&cs)?;
        let extents: Vec<ExtentRef> = bound.iter().map(|(_, x)| *x).collect();
        apply(&mut txn, &cs, &extents, pos)?;

        // History entry → content file; then touched tree nodes → fresh metadata pages.
        let hist = encode_history(pos, &attempt, &cs);
        let hist_ref = {
            let mut cursor = lock(&self.alloc);
            let x = ExtentRef {
                offset: *cursor,
                len: hist.len() as u64,
                digest: sha256(&hist),
            };
            self.io
                .write_at(FileId::Content, x.offset, &hist)
                .map_err(rejected)?;
            self.counters
                .content_bytes_written
                .fetch_add(x.len, Ordering::Relaxed);
            *cursor += x.len;
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

        // Data barrier: every byte the new generation references must be durable first.
        self.io.sync(FileId::Content).map_err(rejected)?;
        self.io.sync(FileId::Meta).map_err(rejected)?;

        let desc = Descriptor {
            store: self.store,
            generation,
            root,
            next_page,
            content_len: *lock(&self.alloc),
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
            lock(&self.state).fenced = Some(why.clone());
            return Err(CommitError::Indeterminate(why));
        }

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

    fn resolve_content(&self, cs: &ChangeSet) -> Result<Vec<(u64, ExtentRef)>, CommitError> {
        cs.content
            .iter()
            .map(|b| self.staged_extent(&b.token, &b.content).map_err(rejected))
            .collect()
    }

    pub fn outcome(&self, attempt: &AttemptId) -> Result<AttemptOutcome, StorageError> {
        let st = lock(&self.state);
        if st.fenced.is_some() {
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
            content: v.content.map(|x| x.content_ref()),
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
        let mut prefix = vec![p];
        key_component(&mut prefix, q.from.as_bytes());
        if let Some(k) = &q.kind {
            key_component(&mut prefix, k.as_bytes());
        }
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

    pub fn content(
        self: &Arc<Self>,
        s: &Snapshot,
        c: &ContentRef,
    ) -> Result<ExtentReader, StorageError> {
        let x =
            tree::get(&**self, s.desc.root, &key(P_CONTENT, &[&c.digest.0]))?.ok_or_else(|| {
                StorageError::InvalidRequest("content is not committed in this view".into())
            })?;
        let x = ExtentRef::decode(&x)?;
        if x.len != c.len || x.digest != c.digest.0 {
            return Err(corrupt(
                "content record does not match the requested digest and length",
            ));
        }
        let committed = lock(&self.state).current.content_len;
        if x.offset
            .checked_add(x.len)
            .is_none_or(|end| end > committed)
        {
            return Err(corrupt("content extent lies outside committed content"));
        }
        Ok(ExtentReader {
            inner: self.clone(),
            extent: x,
            pos: 0,
            hasher: Sha256::new(),
            failed: false,
        })
    }
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
    bound: &[ExtentRef],
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
                if let Some(x) = content {
                    let ck = key(P_CONTENT, &[&x.digest]);
                    if t.get(&ck).map_err(rejected)?.is_none() {
                        t.put(ck, x.encode()).map_err(rejected)?;
                    }
                }
                let revision = pos;
                t.put(
                    k,
                    ContextValue {
                        revision,
                        body: body.clone(),
                        content,
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

/// Streams committed content in bounded chunks and verifies its digest before reporting EOF.
///
/// Integrity contract: the digest covers the whole extent and is checked when the stream ends.
/// Bytes delivered before a mismatch are not verified; a mismatch makes the read fail instead of
/// reporting a complete stream.
pub(crate) struct ExtentReader {
    inner: Arc<Inner>,
    extent: ExtentRef,
    pos: u64,
    hasher: Sha256,
    failed: bool,
}

impl Read for ExtentReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.failed {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "content integrity check failed",
            ));
        }
        let left = self.extent.len - self.pos;
        if left == 0 {
            let got: [u8; 32] = self.hasher.clone().finalize().into();
            if got != self.extent.digest {
                self.failed = true;
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("content at {}: digest mismatch", self.extent.offset),
                ));
            }
            return Ok(0);
        }
        let n = buf.len().min(CONTENT_CHUNK).min(left as usize);
        self.inner
            .io
            .read_at(
                FileId::Content,
                self.extent.offset + self.pos,
                &mut buf[..n],
            )
            .map_err(|e| io::Error::other(e.to_string()))?;
        self.hasher.update(&buf[..n]);
        self.pos += n as u64;
        Ok(n)
    }
}

impl ContentReader for ExtentReader {}
