//! Engine core: open/create, recovery selection, staging, the commit protocol, and reads.
//!
//! Commit protocol (see `docs/storage-failure-model.md`):
//! 1. validate the basis and the attempt identity against the latest committed state;
//! 2. build the new tree from the newest committed tree, write the history entry to the content file and the
//!    new tree node to a fresh metadata page (no live page is ever overwritten);
//! 3. data barrier: sync the content file, then the metadata file;
//! 4. write the publication record for generation g+1 into slot (g+1) mod 2;
//! 5. publication barrier: sync the metadata file;
//! 6. expose the new generation to new views and acknowledge.

use std::collections::{BTreeSet, HashMap};
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
use crate::tree::{Entries, decode_leaf, encode_leaf, prefix_range};

/// Content streaming buffer. Payloads of any length stream through a buffer of this size.
pub(crate) const CONTENT_CHUNK: usize = 64 * 1024;
/// Decoded tree nodes kept in memory, keyed by location *and* incarnation.
const CACHE_NODES: usize = 64;

static NEXT_INSTANCE: AtomicU64 = AtomicU64::new(1);

/// The committed state a view reads.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Snapshot {
    pub desc: Descriptor,
}

struct State {
    current: Descriptor,
    /// Staged, not yet committed extents of this open instance: offset → (len, digest).
    staged: HashMap<u64, (u64, [u8; 32])>,
    /// Set when a publication outcome is uncertain; further writes are refused.
    fenced: Option<String>,
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
    cache: Mutex<HashMap<PageRef, Arc<Entries>>>,
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

impl Inner {
    pub fn create(io: Box<dyn Io>, store: StoreId) -> Result<Arc<Self>, StorageError> {
        io.create()?;
        // Make the new directory entries durable before anything refers to them.
        io.sync_dir()?;
        let root = PageRef {
            page: FIRST_NODE_PAGE,
            incarnation: 1,
        };
        // Slot 0 stays empty; generation 1 is published in slot 1.
        io.write_at(FileId::Meta, 0, &vec![0; PAGE_SIZE])?;
        io.write_at(
            FileId::Meta,
            root.page * PAGE_SIZE as u64,
            &encode_page(store, PageKind::Leaf, root, &encode_leaf(&Entries::new()))?,
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
        Ok(Arc::new(Self::new(io, desc, Vec::new())))
    }

    pub fn open(io: Box<dyn Io>) -> Result<Arc<Self>, StorageError> {
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
        Ok(Arc::new(Self::new(io, desc, diagnostics)))
    }

    fn new(io: Box<dyn Io>, desc: Descriptor, diagnostics: Vec<String>) -> Self {
        Self {
            io,
            store: desc.store,
            instance: NEXT_INSTANCE.fetch_add(1, Ordering::Relaxed),
            alloc: Mutex::new(desc.content_len),
            state: Mutex::new(State {
                current: desc,
                staged: HashMap::new(),
                fenced: None,
            }),
            writer: Mutex::new(()),
            cache: Mutex::new(HashMap::new()),
            diagnostics,
        }
    }

    pub fn snapshot(&self) -> Result<Snapshot, StorageError> {
        Ok(Snapshot {
            desc: lock(&self.state).current,
        })
    }

    fn node(&self, at: PageRef) -> Result<Arc<Entries>, StorageError> {
        if let Some(e) = lock(&self.cache).get(&at) {
            return Ok(e.clone());
        }
        let mut buf = vec![0; PAGE_SIZE];
        self.io
            .read_at(FileId::Meta, at.page * PAGE_SIZE as u64, &mut buf)?;
        let entries = Arc::new(decode_leaf(&decode_page(
            self.store,
            PageKind::Leaf,
            at,
            &buf,
        )?)?);
        let mut cache = lock(&self.cache);
        if cache.len() >= CACHE_NODES {
            cache.clear();
        }
        cache.insert(at, entries.clone());
        Ok(entries)
    }

    fn read_extent(&self, x: &ExtentRef) -> Result<Vec<u8>, StorageError> {
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
            hasher.update(&buf[..n]);
            len += n as u64;
        }
        *cursor = offset + len;
        let digest: [u8; 32] = hasher.finalize().into();
        lock(&self.state).staged.insert(offset, (len, digest));
        let mut t = Enc::default();
        t.raw(&self.store.0)
            .u64(self.instance)
            .u64(offset)
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
    ) -> Result<ExtentRef, StorageError> {
        let mut d = Dec::new(&token.0, "staging token");
        let parsed = (|| -> Result<_, StorageError> {
            let s: [u8; 16] = d.array()?;
            let inst = d.u64()?;
            let off = d.u64()?;
            let len = d.u64()?;
            let dig: [u8; 32] = d.array()?;
            Ok((s, inst, off, len, dig))
        })();
        let Ok((s, inst, off, len, dig)) = parsed else {
            return Err(StorageError::ForeignHandle);
        };
        if s != self.store.0 || inst != self.instance {
            return Err(StorageError::ForeignHandle);
        }
        let staged = lock(&self.state).staged.get(&off).copied();
        match staged {
            Some((l, g))
                if l == len && g == dig && content.len == len && content.digest.0 == dig =>
            {
                Ok(ExtentRef {
                    offset: off,
                    len,
                    digest: dig,
                })
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
        let mut entries: Entries = (*self.node(cur.root).map_err(rejected)?).clone();

        // Attempt identity: an identical retry returns the original receipt.
        let fp = fingerprint(&attempt, &cs);
        let akey = key(P_ATTEMPT, &[attempt.as_bytes()]);
        if let Some(v) = entries.get(&akey) {
            let prior = AttemptValue::decode(v).map_err(rejected)?;
            return if prior.fingerprint == fp {
                Ok(CommitReceipt {
                    attempt,
                    position: HistoryPosition(prior.position),
                })
            } else {
                Err(CommitError::AttemptMismatch)
            };
        }

        let bound = self.resolve_content(&cs)?;
        apply(&mut entries, &cs, &bound)?;

        // History entry → content file; then the new tree node → a fresh metadata page.
        let pos = cur.history_pos.checked_add(1).ok_or_else(|| {
            rejected(StorageError::InvalidRequest(
                "history position overflow".into(),
            ))
        })?;
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
            *cursor += x.len;
            x
        };
        entries.insert(history_key(pos), hist_ref.encode());
        entries.insert(
            akey,
            AttemptValue {
                position: pos,
                fingerprint: fp,
            }
            .encode(),
        );

        let generation = cur.generation + 1;
        let root = PageRef {
            page: cur.next_page,
            incarnation: generation,
        };
        let page = encode_page(self.store, PageKind::Leaf, root, &encode_leaf(&entries))
            .map_err(rejected)?;
        self.io
            .write_at(FileId::Meta, root.page * PAGE_SIZE as u64, &page)
            .map_err(rejected)?;

        // Data barrier: every byte the new generation references must be durable first.
        self.io.sync(FileId::Content).map_err(rejected)?;
        self.io.sync(FileId::Meta).map_err(rejected)?;

        let desc = Descriptor {
            store: self.store,
            generation,
            root,
            next_page: cur.next_page + 1,
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
        for b in &bound {
            st.staged.remove(&b.offset);
        }
        Ok(CommitReceipt {
            attempt,
            position: HistoryPosition(pos),
        })
    }

    fn resolve_content(&self, cs: &ChangeSet) -> Result<Vec<ExtentRef>, CommitError> {
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
        let entries = self.node(cur.root)?;
        Ok(match entries.get(&key(P_ATTEMPT, &[attempt.as_bytes()])) {
            Some(v) => AttemptOutcome::Committed(CommitReceipt {
                attempt: attempt.clone(),
                position: HistoryPosition(AttemptValue::decode(v)?.position),
            }),
            None => AttemptOutcome::Absent,
        })
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
        let entries = self.node(s.desc.root)?;
        let Some(v) = entries.get(&key(P_CONTEXT, &[id.as_bytes()])) else {
            return Ok(None);
        };
        let v = ContextValue::decode(v)?;
        Ok(Some(ContextRecord {
            id: id.clone(),
            revision: Revision(v.revision),
            body: v.body,
            content: v.content.map(|x| x.content_ref()),
        }))
    }

    pub fn relationships(
        &self,
        s: &Snapshot,
        q: &RelationshipQuery,
    ) -> Result<Vec<Relationship>, StorageError> {
        let entries = self.node(s.desc.root)?;
        let p = match q.direction {
            Direction::Outgoing => P_OUT,
            Direction::Incoming => P_IN,
        };
        let mut prefix = vec![p];
        key_component(&mut prefix, q.from.as_bytes());
        if let Some(k) = &q.kind {
            key_component(&mut prefix, k.as_bytes());
        }
        let mut out = Vec::new();
        for (k, _) in prefix_range(&entries, &prefix, None) {
            let parts = key_components(&k[1..])?;
            let [_, _, _, id] = parts.as_slice() else {
                return Err(corrupt("relationship index key arity"));
            };
            let rv = entries
                .get(&key(P_REL, &[id]))
                .ok_or_else(|| corrupt("relationship index entry without its record"))?;
            let rv = RelValue::decode(rv)?;
            let ident =
                |b: &[u8]| corrupt(format!("stored identifier {}", String::from_utf8_lossy(b)));
            out.push(Relationship {
                id: RelationshipId::new(id.clone()).map_err(|_| ident(id))?,
                kind: RelationshipKind::new(rv.kind.clone()).map_err(|_| ident(&rv.kind))?,
                source: ContextId::new(rv.source.clone()).map_err(|_| ident(&rv.source))?,
                target: ContextId::new(rv.target.clone()).map_err(|_| ident(&rv.target))?,
                revision: Revision(rv.revision),
            });
        }
        Ok(out)
    }

    pub fn history(
        self: &Arc<Self>,
        s: &Snapshot,
        start: HistoryPosition,
    ) -> Result<impl Iterator<Item = Result<HistoryEntry, StorageError>> + use<>, StorageError>
    {
        let entries = self.node(s.desc.root)?;
        let from = history_key(start.0.max(1));
        let refs: Vec<ExtentRef> = prefix_range(&entries, &[P_HISTORY], Some(&from))
            .map(|(_, v)| ExtentRef::decode(v))
            .collect::<Result<_, _>>()?;
        let me = self.clone();
        Ok(refs
            .into_iter()
            .map(move |x| decode_history(&me.read_extent(&x)?)))
    }

    pub fn content(
        self: &Arc<Self>,
        s: &Snapshot,
        c: &ContentRef,
    ) -> Result<ExtentReader, StorageError> {
        let entries = self.node(s.desc.root)?;
        let x = entries
            .get(&key(P_CONTENT, &[&c.digest.0]))
            .ok_or_else(|| {
                StorageError::InvalidRequest("content is not committed in this view".into())
            })?;
        let x = ExtentRef::decode(x)?;
        if x.len != c.len {
            return Err(corrupt("content length disagrees with its record"));
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

/// Applies a change set to the latest entries, checking preconditions.
fn apply(entries: &mut Entries, cs: &ChangeSet, bound: &[ExtentRef]) -> Result<(), CommitError> {
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
                let prev = entries
                    .get(&k)
                    .map(|v| ContextValue::decode(v))
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
                    entries
                        .entry(key(P_CONTENT, &[&x.digest]))
                        .or_insert_with(|| x.encode());
                }
                let revision = prev.map_or(1, |p| p.revision + 1);
                entries.insert(
                    k,
                    ContextValue {
                        revision,
                        body: body.clone(),
                        content,
                    }
                    .encode(),
                );
            }
            Change::RemoveContext { id, expect } => {
                let k = key(P_CONTEXT, &[id.as_bytes()]);
                let prev = entries
                    .get(&k)
                    .map(|v| ContextValue::decode(v))
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
                entries.remove(&k);
            }
            Change::PutRelationship {
                id,
                expect,
                kind,
                source,
                target,
            } => {
                let k = key(P_REL, &[id.as_bytes()]);
                let prev = entries
                    .get(&k)
                    .map(|v| RelValue::decode(v))
                    .transpose()
                    .map_err(rejected)?;
                check(
                    &format!("{id:?}"),
                    prev.as_ref().map(|p| p.revision),
                    expect,
                )?;
                if let Some(p) = &prev {
                    entries.remove(&p.out_key(id.as_bytes()));
                    entries.remove(&p.in_key(id.as_bytes()));
                }
                let v = RelValue {
                    revision: prev.map_or(1, |p| p.revision + 1),
                    kind: kind.as_bytes().to_vec(),
                    source: source.as_bytes().to_vec(),
                    target: target.as_bytes().to_vec(),
                };
                entries.insert(v.out_key(id.as_bytes()), Vec::new());
                entries.insert(v.in_key(id.as_bytes()), Vec::new());
                entries.insert(k, v.encode());
            }
            Change::RemoveRelationship { id, expect } => {
                let k = key(P_REL, &[id.as_bytes()]);
                let prev = entries
                    .get(&k)
                    .map(|v| RelValue::decode(v))
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
                entries.remove(&p.out_key(id.as_bytes()));
                entries.remove(&p.in_key(id.as_bytes()));
                entries.remove(&k);
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
