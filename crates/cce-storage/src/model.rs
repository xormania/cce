//! The independent logical model of the storage contract and the harness that runs the engine
//! against it, step by step: the reference for the generated sequences and their crash and
//! I/O-failure campaigns (`model_tests`) and for the operation-sequence fuzz target.
//!
//! The model knows the logical contract only and never calls the engine; the harness executes
//! the request the model resolved for each step and compares every observation, with the
//! retirement audit, the store's space accounting and an independent ownership model after every
//! step. Compiled for tests and with the `fuzz-api` feature only.
#![cfg_attr(not(test), allow(dead_code, irrefutable_let_patterns))]

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use std::io::Read;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};

use cce_core::storage::{
    AttemptId, AttemptOutcome, Change, ChangeSet, CommitError, CommitReceipt, ContentBinding,
    ContentDigest, ContentRef, ContextId, ContextRecord, Direction, Evidence, HistoryEntry,
    HistoryPosition, Precondition, ReadView, Relationship, RelationshipId, RelationshipKind,
    RelationshipPosition, RelationshipQuery, RemovalMode, Revision, StagedContent, StorageEngine,
    StorageError,
};
use sha2::{Digest, Sha256};

use crate::format::{PAGE_SIZE, StoreId};
#[cfg(test)]
use crate::io::FsIo;
use crate::io::{FileId, Io};
use crate::sim::{Persist, SimIo};
#[cfg(test)]
use crate::test_support::resources::TestDir;
#[cfg(test)]
use crate::test_support::{attempt, cid, create_on, open_on};
use crate::{NativeStore, NativeView};

// The shared test support is compiled for unit tests only; the fuzz build defines the same four
// helpers here.
#[cfg(not(test))]
fn create_on(
    io: Box<dyn Io>,
    id: StoreId,
    options: crate::engine::Options,
) -> Result<NativeStore, StorageError> {
    crate::engine::Inner::create(io, id, options).map(|inner| NativeStore { inner })
}

#[cfg(not(test))]
fn open_on(io: Box<dyn Io>, options: crate::engine::Options) -> Result<NativeStore, StorageError> {
    crate::engine::Inner::open(io, options).map(|inner| NativeStore { inner })
}

#[cfg(not(test))]
fn cid(s: &str) -> ContextId {
    ContextId::new(s.as_bytes().to_vec()).expect("a valid context identity")
}

#[cfg(not(test))]
fn attempt(s: &str) -> AttemptId {
    AttemptId::new(s.as_bytes().to_vec()).expect("a valid attempt identity")
}

/// Seed of the routine run. Changing it changes the generated sequences.
pub(crate) const ROUTINE_SEED: u64 = 0x5eed_0001_cce0_0001;
pub(crate) const ROUTINE_CASES: u32 = 32;
pub(crate) const ROUTINE_FS_CASES: u32 = 4;
pub(crate) const MIN_OPS: usize = 64;
pub(crate) const MAX_OPS: usize = 80;

pub(crate) const STORE: StoreId = StoreId(*b"model-test-store");
/// Context identities; "a" and "ab" share a prefix on purpose.
pub(crate) const CONTEXTS: [&str; 10] = ["a", "ab", "b", "c", "d", "e", "f", "g", "h", "i"];
pub(crate) const KINDS: [&str; 3] = ["k", "kk", "x"];
pub(crate) const RELATIONSHIPS: usize = 8;
pub(crate) const MAX_HELD: usize = 3;

// ---------------------------------------------------------------------------------------------
// Independent model. Uses cce-core types and SHA-256 only.
// ---------------------------------------------------------------------------------------------

pub(crate) fn ctx_id(i: u8) -> ContextId {
    cid(CONTEXTS[i as usize % CONTEXTS.len()])
}

pub(crate) fn rel_id(i: u8) -> RelationshipId {
    RelationshipId::new(format!("r{}", i as usize % RELATIONSHIPS).into_bytes()).unwrap()
}

pub(crate) fn kind(i: u8) -> RelationshipKind {
    RelationshipKind::new(KINDS[i as usize % KINDS.len()].as_bytes().to_vec()).unwrap()
}

/// The content a put binds: freshly staged bytes, or the same staging handle as another binding of
/// the request (the `n`-th fresh one, which may come before or after it).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ContentPick {
    Fresh(ContentSpec),
    SameStaging(u8),
    /// A handle staged earlier and still staged (by a stage step or a rejected commit); the
    /// `n`-th one at the time the request is made.
    Staged(u8),
}

/// Content bytes for a generated content choice: mostly small, sometimes several KiB, rarely
/// larger than one streaming segment.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ContentSpec {
    pub(crate) seed: u8,
    pub(crate) size: u8,
}

impl ContentSpec {
    pub(crate) fn bytes(self) -> Vec<u8> {
        let s = self.seed as usize;
        let len = match self.size % 8 {
            0..=4 => 1 + s % 97,
            5 | 6 => 2000 + s * 13,
            _ => 70_000 + s * 101,
        };
        (0..len)
            .map(|i| (i.wrapping_mul(31) + s * 7 + (i >> 8)) as u8)
            .collect()
    }
}

pub(crate) fn content_ref(bytes: &[u8]) -> ContentRef {
    ContentRef {
        digest: ContentDigest(Sha256::digest(bytes).into()),
        len: bytes.len() as u64,
    }
}

#[derive(Clone, Copy, Debug)]
pub(crate) enum ExpectSpec {
    Any,
    Absent,
    /// The record's current revision (or `Absent` when there is none).
    Current,
    /// A revision the record does not have.
    Wrong,
}

#[derive(Clone, Debug)]
pub(crate) enum ChangeSpec {
    PutContext {
        ctx: u8,
        expect: ExpectSpec,
        body: u8,
        content: Option<ContentPick>,
    },
    RemoveContext {
        ctx: u8,
        expect: ExpectSpec,
    },
    PutRelationship {
        rel: u8,
        expect: ExpectSpec,
        kind: u8,
        source: u8,
        target: u8,
    },
    RemoveRelationship {
        rel: u8,
        expect: ExpectSpec,
    },
}

#[derive(Clone, Debug)]
pub(crate) enum ReadSpec {
    Context(u8),
    Relationships {
        from: u8,
        outgoing: bool,
        kind: Option<u8>,
        /// Read this many results, then resume after the last one.
        page: u8,
    },
    /// Read the content of a context's record, if it has one.
    Content(u8),
}

#[derive(Clone, Debug)]
pub(crate) struct RequestSpec {
    pub(crate) changes: Vec<ChangeSpec>,
    pub(crate) evidence: Vec<u8>,
    pub(crate) quiesce: bool,
    pub(crate) reads: Vec<ReadSpec>,
}

/// One generated step.
#[derive(Clone, Debug)]
pub(crate) enum Op {
    /// Read, then commit from a fresh view.
    Commit(RequestSpec),
    /// Commit from a held (possibly stale) view.
    CommitFromHeld {
        slot: u8,
        request: RequestSpec,
    },
    /// Admit without waiting and poll the outcome until it resolves.
    SubmitAndPoll(RequestSpec),
    /// Retry a committed attempt, identically or with changed evidence.
    Retry {
        pick: u8,
        identical: bool,
    },
    /// Begin a view, read through it and keep it.
    Hold(Vec<ReadSpec>),
    /// Read again through a held view.
    ReadHeld {
        slot: u8,
        reads: Vec<ReadSpec>,
    },
    Release(u8),
    Stage(ContentSpec),
    /// Discard staged content; `refused` picks a handle storage must refuse (committed or from a
    /// previous open).
    Discard {
        pick: u8,
        refused: bool,
    },
    /// Compare the whole logical state, history and outcomes.
    Check,
    Reopen,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Rel {
    pub(crate) revision: u64,
    pub(crate) kind: RelationshipKind,
    pub(crate) source: ContextId,
    pub(crate) target: ContextId,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Record {
    pub(crate) revision: u64,
    pub(crate) body: Vec<u8>,
    pub(crate) content: Option<ContentRef>,
}

#[derive(Clone, Debug, Default)]
pub(crate) struct Logical {
    pub(crate) contexts: BTreeMap<ContextId, Record>,
    pub(crate) rels: BTreeMap<RelationshipId, Rel>,
}

/// A logical key a commit writes and a read can observe.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum WriteKey {
    Context(ContextId),
    /// A directional index entry: (outgoing?, endpoint, kind).
    Index(bool, ContextId, RelationshipKind),
}

#[derive(Clone, Debug)]
pub(crate) enum Observation {
    Point(ContextId),
    Range(bool, ContextId, Option<RelationshipKind>),
}

impl Observation {
    pub(crate) fn covers(&self, k: &WriteKey) -> bool {
        match (self, k) {
            (Observation::Point(a), WriteKey::Context(b)) => a == b,
            (Observation::Range(d, f, kind), WriteKey::Index(d2, f2, k2)) => {
                d == d2 && f == f2 && kind.as_ref().is_none_or(|k| k == k2)
            }
            _ => false,
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) struct HeldModel {
    pub(crate) basis: u64,
    pub(crate) snapshot: Logical,
    pub(crate) observed: Vec<Observation>,
}

/// What a read returns, as predicted.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ReadResult {
    Context(ContextId, Option<ContextRecord>),
    Relationships {
        query: RelationshipQuery,
        page: usize,
        expected: Vec<Relationship>,
    },
    Content(Option<(ContentRef, Arc<Vec<u8>>)>),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum ErrKind {
    Conflict,
    Rejected,
    Mismatch,
    ReadersActive,
}

#[derive(Clone, Debug)]
pub(crate) enum Expected {
    Committed(u64),
    Failed(BTreeSet<ErrKind>),
}

/// A request resolved to concrete values.
#[derive(Clone, Debug)]
pub(crate) struct Request {
    pub(crate) attempt: AttemptId,
    pub(crate) changes: Vec<Change>,
    pub(crate) contents: Vec<Arc<Vec<u8>>>,
    /// Per binding: `Some(i)` binds the same staging handle as binding `i` (missing: none).
    pub(crate) aliases: Vec<Option<usize>>,
    /// Per binding: `Some(k)` binds the `k`-th handle still staged when the request was made
    /// (missing: none). A rejected commit leaves it staged; a committed one consumes it.
    pub(crate) from_staged: Vec<Option<usize>>,
    pub(crate) evidence: Vec<Evidence>,
    pub(crate) removal: RemovalMode,
}

impl Request {
    pub(crate) fn alias(&self, j: usize) -> Option<usize> {
        self.aliases.get(j).copied().flatten()
    }

    pub(crate) fn staged_pick(&self, j: usize) -> Option<usize> {
        self.from_staged.get(j).copied().flatten()
    }

    /// The bytes of each staging the request makes itself, in binding order.
    pub(crate) fn stagings(&self) -> Vec<Arc<Vec<u8>>> {
        (0..self.contents.len())
            .filter(|j| self.alias(*j).is_none() && self.staged_pick(*j).is_none())
            .map(|j| self.contents[j].clone())
            .collect()
    }

    /// The distinct earlier stagings it binds, by their index among the staged handles.
    pub(crate) fn staged_picks(&self) -> BTreeSet<usize> {
        (0..self.contents.len())
            .filter_map(|j| self.staged_pick(j))
            .collect()
    }

    pub(crate) fn refs(&self) -> Vec<ContentRef> {
        self.contents.iter().map(|b| content_ref(b)).collect()
    }
}

/// The concrete step the engine must execute, with the predicted results.
#[derive(Clone, Debug, Default)]
pub(crate) enum Action {
    #[default]
    Nothing,
    Commit {
        reads: Vec<ReadResult>,
        held_slot: Option<usize>,
        request: Request,
        expected: Expected,
        poll: bool,
    },
    Retry {
        request: Request,
        expected: Result<u64, ErrKind>,
    },
    Hold(Vec<ReadResult>),
    ReadHeld(usize, Vec<ReadResult>),
    Release(usize),
    Stage(Arc<Vec<u8>>),
    Discard {
        index: usize,
        refused: bool,
    },
    Check(Box<Snapshot>),
    Reopen,
}

/// The complete expected state for a [`Op::Check`].
#[derive(Clone, Debug)]
pub(crate) struct Snapshot {
    pub(crate) logical: Logical,
    pub(crate) history: Vec<HistoryEntry>,
    pub(crate) outcomes: Vec<(AttemptId, AttemptOutcome)>,
    /// Content history names but no record uses: readable exactly, or no longer retained.
    pub(crate) unreferenced: Vec<(ContentRef, Arc<Vec<u8>>)>,
    pub(crate) bytes: BTreeMap<[u8; 32], Arc<Vec<u8>>>,
}

#[derive(Clone, Debug, Default)]
pub(crate) struct Model {
    pub(crate) state: Logical,
    pub(crate) position: u64,
    pub(crate) history: Vec<HistoryEntry>,
    /// Committed attempts in commit order with their requests (for retries).
    pub(crate) committed: Vec<(Request, u64)>,
    /// Attempt identities used without committing.
    pub(crate) uncommitted: BTreeSet<AttemptId>,
    pub(crate) next_attempt: u64,
    /// Every content byte stream ever committed, by digest.
    pub(crate) bytes: BTreeMap<[u8; 32], Arc<Vec<u8>>>,
    /// Logical keys written by each commit since the last reopen.
    pub(crate) log: Vec<(u64, Vec<WriteKey>)>,
    pub(crate) held: Vec<HeldModel>,
    /// Staged handles that can still be discarded, in the engine's order.
    pub(crate) staged: Vec<Arc<Vec<u8>>>,
    /// Handles storage must refuse to discard (committed, or issued by an earlier open).
    pub(crate) refused: usize,
    pub(crate) action: Action,
}

impl Logical {
    pub(crate) fn context(&self, id: &ContextId) -> Option<ContextRecord> {
        self.contexts.get(id).map(|r| ContextRecord {
            id: id.clone(),
            revision: Revision(r.revision),
            body: r.body.clone(),
            content: r.content,
        })
    }

    /// Relationships in result order: (kind, other endpoint, id).
    pub(crate) fn query(
        &self,
        from: &ContextId,
        outgoing: bool,
        kind: Option<&RelationshipKind>,
    ) -> Vec<Relationship> {
        let mut v: Vec<(RelationshipKind, ContextId, RelationshipId, Relationship)> = self
            .rels
            .iter()
            .filter(|(_, r)| {
                let end = if outgoing { &r.source } else { &r.target };
                end == from && kind.is_none_or(|k| *k == r.kind)
            })
            .map(|(id, r)| {
                let other = if outgoing { &r.target } else { &r.source };
                (
                    r.kind.clone(),
                    other.clone(),
                    id.clone(),
                    Relationship {
                        id: id.clone(),
                        kind: r.kind.clone(),
                        source: r.source.clone(),
                        target: r.target.clone(),
                        revision: Revision(r.revision),
                    },
                )
            })
            .collect();
        v.sort_by(|a, b| (&a.0, &a.1, &a.2).cmp(&(&b.0, &b.1, &b.2)));
        v.into_iter().map(|t| t.3).collect()
    }

    pub(crate) fn revision_of(&self, key: &WriteKey) -> Option<u64> {
        match key {
            WriteKey::Context(id) => self.contexts.get(id).map(|r| r.revision),
            WriteKey::Index(..) => None,
        }
    }
}

impl Model {
    pub(crate) fn read(
        logical: &Logical,
        bytes: &BTreeMap<[u8; 32], Arc<Vec<u8>>>,
        spec: &ReadSpec,
        observed: &mut Vec<Observation>,
    ) -> ReadResult {
        match spec {
            ReadSpec::Context(c) => {
                let id = ctx_id(*c);
                observed.push(Observation::Point(id.clone()));
                ReadResult::Context(id.clone(), logical.context(&id))
            }
            ReadSpec::Relationships {
                from,
                outgoing,
                kind: k,
                page,
            } => {
                let from = ctx_id(*from);
                let k = k.map(kind);
                observed.push(Observation::Range(*outgoing, from.clone(), k.clone()));
                ReadResult::Relationships {
                    expected: logical.query(&from, *outgoing, k.as_ref()),
                    query: RelationshipQuery {
                        from,
                        direction: if *outgoing {
                            Direction::Outgoing
                        } else {
                            Direction::Incoming
                        },
                        kind: k,
                        after: None,
                    },
                    page: *page as usize % 4,
                }
            }
            ReadSpec::Content(c) => ReadResult::Content(
                logical
                    .contexts
                    .get(&ctx_id(*c))
                    .and_then(|r| r.content)
                    .map(|cr| (cr, bytes[&cr.digest.0].clone())),
            ),
        }
    }

    pub(crate) fn resolve_expect(&self, e: ExpectSpec, current: Option<u64>) -> Precondition {
        match e {
            ExpectSpec::Any => Precondition::Any,
            ExpectSpec::Absent => Precondition::Absent,
            ExpectSpec::Current => current.map_or(Precondition::Absent, |r| {
                Precondition::AtRevision(Revision(r))
            }),
            ExpectSpec::Wrong => Precondition::AtRevision(Revision(current.unwrap_or(0) + 1000)),
        }
    }

    pub(crate) fn fresh_attempt(&mut self) -> AttemptId {
        self.next_attempt += 1;
        attempt(&format!("attempt-{}", self.next_attempt))
    }

    pub(crate) fn resolve(&mut self, spec: &RequestSpec) -> Request {
        // Fresh stagings in change order; a same-staging pick names one of them.
        let fresh: Vec<ContentSpec> = spec
            .changes
            .iter()
            .filter_map(|c| match c {
                ChangeSpec::PutContext {
                    content: Some(ContentPick::Fresh(s)),
                    ..
                } => Some(*s),
                _ => None,
            })
            .collect();
        let mut contents = Vec::new();
        let mut aliases: Vec<Option<usize>> = Vec::new();
        let mut from_staged: Vec<Option<usize>> = Vec::new();
        let staged_now = self.staged.clone();
        let mut fresh_at: Vec<Option<usize>> = vec![None; fresh.len()];
        let mut pending: Vec<(usize, usize)> = Vec::new();
        let mut next_fresh = 0;
        let changes = spec
            .changes
            .iter()
            .map(|c| match c {
                ChangeSpec::PutContext {
                    ctx,
                    expect,
                    body,
                    content,
                } => {
                    let id = ctx_id(*ctx);
                    let cur = self.state.contexts.get(&id).map(|r| r.revision);
                    Change::PutContext {
                        expect: self.resolve_expect(*expect, cur),
                        body: body_bytes(*body, &id),
                        content: content.map(|pick| {
                            let j = contents.len();
                            from_staged.push(None);
                            match pick {
                                ContentPick::Staged(k) if !staged_now.is_empty() => {
                                    let k = k as usize % staged_now.len();
                                    contents.push(staged_now[k].clone());
                                    aliases.push(None);
                                    from_staged[j] = Some(k);
                                }
                                ContentPick::Staged(k) => {
                                    let s = ContentSpec {
                                        seed: k % 12,
                                        size: 1,
                                    };
                                    contents.push(Arc::new(s.bytes()));
                                    aliases.push(None);
                                }
                                ContentPick::SameStaging(k) if !fresh.is_empty() => {
                                    let k = k as usize % fresh.len();
                                    contents.push(Arc::new(fresh[k].bytes()));
                                    aliases.push(None);
                                    pending.push((j, k));
                                }
                                ContentPick::SameStaging(k) => {
                                    let s = ContentSpec {
                                        seed: k % 12,
                                        size: 0,
                                    };
                                    contents.push(Arc::new(s.bytes()));
                                    aliases.push(None);
                                }
                                ContentPick::Fresh(s) => {
                                    contents.push(Arc::new(s.bytes()));
                                    aliases.push(None);
                                    fresh_at[next_fresh] = Some(j);
                                    next_fresh += 1;
                                }
                            }
                            j
                        }),
                        id,
                    }
                }
                ChangeSpec::RemoveContext { ctx, expect } => {
                    let id = ctx_id(*ctx);
                    let cur = self.state.contexts.get(&id).map(|r| r.revision);
                    Change::RemoveContext {
                        expect: self.resolve_expect(*expect, cur),
                        id,
                    }
                }
                ChangeSpec::PutRelationship {
                    rel,
                    expect,
                    kind: k,
                    source,
                    target,
                } => {
                    let id = rel_id(*rel);
                    let cur = self.state.rels.get(&id).map(|r| r.revision);
                    Change::PutRelationship {
                        expect: self.resolve_expect(*expect, cur),
                        kind: kind(*k),
                        source: ctx_id(*source),
                        target: ctx_id(*target),
                        id,
                    }
                }
                ChangeSpec::RemoveRelationship { rel, expect } => {
                    let id = rel_id(*rel);
                    let cur = self.state.rels.get(&id).map(|r| r.revision);
                    Change::RemoveRelationship {
                        expect: self.resolve_expect(*expect, cur),
                        id,
                    }
                }
            })
            .collect();
        for (j, k) in pending {
            aliases[j] = fresh_at[k];
        }
        Request {
            attempt: self.fresh_attempt(),
            changes,
            contents,
            aliases,
            from_staged,
            evidence: spec
                .evidence
                .iter()
                .map(|e| Evidence {
                    label: b"decision".to_vec(),
                    bytes: vec![*e; 1 + *e as usize % 5],
                })
                .collect(),
            removal: if spec.quiesce {
                RemovalMode::RequireQuiescence
            } else {
                RemovalMode::PreserveViews
            },
        }
    }

    /// Predicts a new attempt's result against the current state; `held` is the basis view, if
    /// any, with what was read through it.
    pub(crate) fn predict(
        &self,
        req: &Request,
        held: Option<&HeldModel>,
    ) -> (Expected, Vec<WriteKey>) {
        let mut errs = BTreeSet::new();
        if req.removal == RemovalMode::RequireQuiescence && !self.held.is_empty() {
            // Other views are live: refused before anything else is considered.
            return (Expected::Failed([ErrKind::ReadersActive].into()), vec![]);
        }
        if let Some(h) = held {
            let stale = self
                .log
                .iter()
                .filter(|(p, _)| *p > h.basis)
                .flat_map(|(_, ks)| ks)
                .any(|k| h.observed.iter().any(|o| o.covers(k)));
            if stale {
                errs.insert(ErrKind::Conflict);
            }
        }
        let mut seen_ctx = BTreeSet::new();
        let mut seen_rel = BTreeSet::new();
        let mut written = Vec::new();
        let check = |expect: &Precondition, cur: Option<u64>| match (expect, cur) {
            (Precondition::Any, _) | (Precondition::Absent, None) => true,
            (Precondition::AtRevision(r), Some(c)) => r.0 == c,
            _ => false,
        };
        for c in &req.changes {
            match c {
                Change::PutContext { id, expect, .. } | Change::RemoveContext { id, expect } => {
                    if !seen_ctx.insert(id.clone()) {
                        errs.insert(ErrKind::Rejected);
                    }
                    let cur = self.state.revision_of(&WriteKey::Context(id.clone()));
                    if !check(expect, cur) {
                        errs.insert(ErrKind::Conflict);
                    } else if matches!(c, Change::RemoveContext { .. }) && cur.is_none() {
                        errs.insert(ErrKind::Rejected);
                    }
                    written.push(WriteKey::Context(id.clone()));
                }
                Change::PutRelationship {
                    id,
                    expect,
                    kind,
                    source,
                    target,
                } => {
                    if !seen_rel.insert(id.clone()) {
                        errs.insert(ErrKind::Rejected);
                    }
                    let prev = self.state.rels.get(id);
                    if !check(expect, prev.map(|r| r.revision)) {
                        errs.insert(ErrKind::Conflict);
                    }
                    if let Some(p) = prev {
                        written.push(WriteKey::Index(true, p.source.clone(), p.kind.clone()));
                        written.push(WriteKey::Index(false, p.target.clone(), p.kind.clone()));
                    }
                    written.push(WriteKey::Index(true, source.clone(), kind.clone()));
                    written.push(WriteKey::Index(false, target.clone(), kind.clone()));
                }
                Change::RemoveRelationship { id, expect } => {
                    if !seen_rel.insert(id.clone()) {
                        errs.insert(ErrKind::Rejected);
                    }
                    let prev = self.state.rels.get(id);
                    if !check(expect, prev.map(|r| r.revision)) {
                        errs.insert(ErrKind::Conflict);
                    } else if prev.is_none() {
                        errs.insert(ErrKind::Rejected);
                    }
                    if let Some(p) = prev {
                        written.push(WriteKey::Index(true, p.source.clone(), p.kind.clone()));
                        written.push(WriteKey::Index(false, p.target.clone(), p.kind.clone()));
                    }
                }
            }
        }
        if errs.is_empty() {
            (Expected::Committed(self.position + 1), written)
        } else {
            (Expected::Failed(errs), vec![])
        }
    }

    /// Applies a committed request to the model.
    pub(crate) fn apply_commit(&mut self, req: &Request, written: Vec<WriteKey>) {
        // An attempt refused earlier may commit later (after an I/O failure, for example).
        self.uncommitted.remove(&req.attempt);
        self.position += 1;
        let pos = self.position;
        let refs = req.refs();
        for (r, b) in refs.iter().zip(&req.contents) {
            self.bytes.entry(r.digest.0).or_insert_with(|| b.clone());
        }
        for c in &req.changes {
            match c {
                Change::PutContext {
                    id, body, content, ..
                } => {
                    self.state.contexts.insert(
                        id.clone(),
                        Record {
                            revision: pos,
                            body: body.clone(),
                            content: content.map(|i| refs[i]),
                        },
                    );
                }
                Change::RemoveContext { id, .. } => {
                    self.state.contexts.remove(id);
                }
                Change::PutRelationship {
                    id,
                    kind,
                    source,
                    target,
                    ..
                } => {
                    self.state.rels.insert(
                        id.clone(),
                        Rel {
                            revision: pos,
                            kind: kind.clone(),
                            source: source.clone(),
                            target: target.clone(),
                        },
                    );
                }
                Change::RemoveRelationship { id, .. } => {
                    self.state.rels.remove(id);
                }
            }
        }
        self.history.push(HistoryEntry {
            position: HistoryPosition(pos),
            attempt: req.attempt.clone(),
            evidence: req.evidence.clone(),
            changes: req.changes.clone(),
            content: refs,
        });
        self.committed.push((req.clone(), pos));
        self.log.push((pos, written));
    }

    pub(crate) fn snapshot(&self) -> Snapshot {
        let used: BTreeSet<[u8; 32]> = self
            .state
            .contexts
            .values()
            .filter_map(|r| r.content.map(|c| c.digest.0))
            .collect();
        let mut unreferenced: Vec<(ContentRef, Arc<Vec<u8>>)> = self
            .history
            .iter()
            .flat_map(|h| h.content.iter().copied())
            .filter(|c| !used.contains(&c.digest.0))
            .map(|c| (c, self.bytes[&c.digest.0].clone()))
            .collect();
        unreferenced.dedup_by_key(|(c, _)| c.digest.0);
        let mut outcomes: Vec<(AttemptId, AttemptOutcome)> = self
            .committed
            .iter()
            .map(|(r, p)| {
                (
                    r.attempt.clone(),
                    AttemptOutcome::Committed(CommitReceipt {
                        attempt: r.attempt.clone(),
                        position: HistoryPosition(*p),
                    }),
                )
            })
            .collect();
        outcomes.extend(
            self.uncommitted
                .iter()
                .map(|a| (a.clone(), AttemptOutcome::Absent)),
        );
        outcomes.push((attempt("never-submitted"), AttemptOutcome::Absent));
        Snapshot {
            logical: self.state.clone(),
            history: self.history.clone(),
            outcomes,
            unreferenced,
            bytes: self.bytes.clone(),
        }
    }

    pub(crate) fn commit_step(&mut self, spec: &RequestSpec, held_slot: Option<usize>, poll: bool) {
        let held = held_slot.map(|s| self.held.remove(s));
        // A fresh basis is read first. Reads at the latest state never conflict (nothing commits
        // in between), so only a held basis carries observations into the prediction; a held
        // basis was already read when it was taken.
        let mut observed = Vec::new();
        let reads = match held {
            Some(_) => Vec::new(),
            None => spec
                .reads
                .iter()
                .map(|r| Model::read(&self.state, &self.bytes, r, &mut observed))
                .collect(),
        };
        let request = self.resolve(spec);
        let (expected, written) = self.predict(&request, held.as_ref());
        match expected {
            Expected::Committed(_) => {
                self.apply_commit(&request, written);
                // Earlier stagings it bound are consumed: discarding them must now be refused.
                for k in request.staged_picks().into_iter().rev() {
                    self.staged.remove(k);
                }
                self.refused += request.stagings().len() + request.staged_picks().len();
            }
            Expected::Failed(_) => {
                // A rejected commit consumes nothing: earlier stagings it bound stay staged and
                // usable, and its own stagings join them.
                self.uncommitted.insert(request.attempt.clone());
                self.staged.extend(request.stagings());
            }
        }
        self.action = Action::Commit {
            reads,
            held_slot,
            request,
            expected,
            poll,
        };
    }
}

/// A record body: mostly short, sometimes large enough that a few records fill a page, so leaves
/// split and removals shrink a leaf next to a large sibling (the tree's merge paths).
pub(crate) fn body_bytes(body: u8, id: &ContextId) -> Vec<u8> {
    let mut b = format!("body {body} of {id:?}").into_bytes();
    let pad = match body % 4 {
        3 => 1650,
        2 => 700,
        _ => 0,
    };
    b.resize(b.len() + pad, b'.');
    b
}

impl Model {
    /// The model's transition for `op` (also the generator's reference transition).
    pub(crate) fn apply(mut m: Model, op: &Op) -> Model {
        m.action = Action::Nothing;
        match op {
            Op::Commit(spec) => m.commit_step(spec, None, false),
            Op::SubmitAndPoll(spec) => m.commit_step(spec, None, true),
            Op::CommitFromHeld { slot, request } => {
                let slot = (!m.held.is_empty()).then(|| *slot as usize % m.held.len());
                m.commit_step(request, slot, false)
            }
            Op::Retry { pick, identical } => {
                if m.committed.is_empty() {
                    return m;
                }
                let (mut request, pos) = m.committed[*pick as usize % m.committed.len()].clone();
                let expected = if *identical {
                    Ok(pos)
                } else {
                    request.evidence.push(Evidence {
                        label: b"changed".to_vec(),
                        bytes: vec![1],
                    });
                    Err(ErrKind::Mismatch)
                };
                m.action = Action::Retry { request, expected };
            }
            Op::Hold(reads) => {
                if m.held.len() == MAX_HELD {
                    m.held.remove(0);
                }
                let mut h = HeldModel {
                    basis: m.position,
                    snapshot: m.state.clone(),
                    observed: Vec::new(),
                };
                let results = reads
                    .iter()
                    .map(|r| Model::read(&h.snapshot, &m.bytes, r, &mut h.observed))
                    .collect();
                m.held.push(h);
                m.action = Action::Hold(results);
            }
            Op::ReadHeld { slot, reads } => {
                if m.held.is_empty() {
                    return m;
                }
                let s = *slot as usize % m.held.len();
                let bytes = m.bytes.clone();
                let h = &mut m.held[s];
                let results = reads
                    .iter()
                    .map(|r| Model::read(&h.snapshot, &bytes, r, &mut h.observed))
                    .collect();
                m.action = Action::ReadHeld(s, results);
            }
            Op::Release(slot) => {
                if m.held.is_empty() {
                    return m;
                }
                let s = *slot as usize % m.held.len();
                m.held.remove(s);
                m.action = Action::Release(s);
            }
            Op::Stage(spec) => {
                let b = Arc::new(spec.bytes());
                m.staged.push(b.clone());
                m.action = Action::Stage(b);
            }
            Op::Discard { pick, refused } => {
                if *refused && m.refused > 0 {
                    m.refused -= 1;
                    m.action = Action::Discard {
                        index: *pick as usize % (m.refused + 1),
                        refused: true,
                    };
                } else if !m.staged.is_empty() {
                    let index = *pick as usize % m.staged.len();
                    m.staged.remove(index);
                    m.action = Action::Discard {
                        index,
                        refused: false,
                    };
                }
            }
            Op::Check => m.action = Action::Check(Box::new(m.snapshot())),
            Op::Reopen => {
                m.held.clear();
                m.log.clear();
                m.refused += m.staged.len();
                m.staged.clear();
                m.action = Action::Reopen;
            }
        }
        m
    }
}

// ---------------------------------------------------------------------------------------------
// The engine under test, and the comparison.
// ---------------------------------------------------------------------------------------------

// Counts of what the generated sequences exercised (reported by the test).
thread_local! {
    // The runner executes every case on the test's own thread, so each test counts its own.
    pub(crate) static STATS: RefCell<BTreeMap<String, u64>> = const { RefCell::new(BTreeMap::new()) };
}

pub(crate) fn count(what: &str) {
    STATS.with(|s| *s.borrow_mut().entry(what.to_string()).or_default() += 1);
}

pub(crate) fn op_name(op: &Op) -> &'static str {
    match op {
        Op::Commit(_) => "commit",
        Op::CommitFromHeld { .. } => "commit from held",
        Op::SubmitAndPoll(_) => "submit and poll",
        Op::Retry { .. } => "retry",
        Op::Hold(_) => "hold",
        Op::ReadHeld { .. } => "read held",
        Op::Release(_) => "release",
        Op::Stage(_) => "stage",
        Op::Discard { .. } => "discard",
        Op::Check => "check",
        Op::Reopen => "reopen",
    }
}

/// Where a controlled schedule pauses a commit: before its data barrier (everything it will
/// publish is sealed), before its publication record is written, or after the publication sync
/// returned and before the commit applies its result to the running store.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Point {
    Barrier,
    Publish,
    Apply,
    /// Before any content-file write (a staging writing a segment it just took).
    ContentWrite,
}

/// A pause armed at one point: the paused operation signals `reached` and waits for `go`.
pub(crate) struct Gate {
    pub(crate) point: Point,
    pub(crate) reached: std::sync::mpsc::Sender<()>,
    pub(crate) go: std::sync::mpsc::Receiver<()>,
}

/// What the harness observes of the simulator underneath the store, independently of the store's
/// own bookkeeping: the kind of operation an injected failure hit, every content-file write (so a
/// staging's space is known from the bytes it wrote), and an optional pause.
#[derive(Default)]
pub(crate) struct Tap {
    pub(crate) failed: Mutex<Option<&'static str>>,
    pub(crate) writes: Mutex<Vec<(u64, Vec<u8>)>>,
    pub(crate) gate: Mutex<Option<Gate>>,
    /// A publication record was written and its sync has not returned yet.
    pub(crate) publishing: std::sync::atomic::AtomicBool,
}

impl Tap {
    pub(crate) fn pause_at(&self, point: Point) {
        let armed = {
            let mut g = self.gate.lock().unwrap();
            if g.as_ref().is_some_and(|g| g.point == point) {
                g.take()
            } else {
                None
            }
        };
        if let Some(g) = armed {
            let _ = g.reached.send(());
            let _ = g.go.recv();
        }
    }
}

/// The simulator as the store sees it, reporting to the harness's tap (the commit itself runs on
/// the store's worker thread).
pub(crate) struct Watched {
    pub(crate) sim: SimIo,
    pub(crate) tap: Arc<Tap>,
}

impl Watched {
    pub(crate) fn note<T>(
        &self,
        kind: &'static str,
        r: Result<T, StorageError>,
    ) -> Result<T, StorageError> {
        if let Err(StorageError::Io(m)) = &r
            && m.starts_with("simulated I/O error")
        {
            *self.tap.failed.lock().unwrap() = Some(kind);
        }
        r
    }
}

impl Io for Watched {
    fn create(&self) -> Result<(), StorageError> {
        self.sim.create()
    }
    fn open(&self) -> Result<(), StorageError> {
        self.sim.open()
    }
    fn open_read_only(&self) -> Result<(), StorageError> {
        self.sim.open_read_only()
    }
    fn read_at(&self, file: FileId, offset: u64, buf: &mut [u8]) -> Result<(), StorageError> {
        self.sim.read_at(file, offset, buf)
    }
    fn write_at(&self, file: FileId, offset: u64, data: &[u8]) -> Result<(), StorageError> {
        let publication = file == FileId::Meta && offset < 2 * PAGE_SIZE as u64;
        if publication {
            self.tap.pause_at(Point::Publish);
        }
        if file == FileId::Content {
            self.tap.pause_at(Point::ContentWrite);
        }
        let r = self.note("write", self.sim.write_at(file, offset, data));
        if r.is_ok() && file == FileId::Content {
            self.tap
                .writes
                .lock()
                .unwrap()
                .push((offset, data.to_vec()));
        }
        if r.is_ok() && publication {
            self.tap.publishing.store(true, Ordering::SeqCst);
        }
        r
    }
    fn len(&self, file: FileId) -> Result<u64, StorageError> {
        self.sim.len(file)
    }
    fn sync(&self, file: FileId) -> Result<(), StorageError> {
        if file == FileId::Content {
            self.tap.pause_at(Point::Barrier);
        }
        let r = self.note("sync", self.sim.sync(file));
        if r.is_ok() && file == FileId::Meta && self.tap.publishing.swap(false, Ordering::SeqCst) {
            self.tap.pause_at(Point::Apply);
        }
        r
    }
    fn sync_dir(&self) -> Result<(), StorageError> {
        self.note("sync", self.sim.sync_dir())
    }
}

/// Where the store under test lives.
pub(crate) enum Backend {
    Sim(SimIo, Arc<Tap>),
    /// A fresh directory on the real filesystem from the shared directory owner: removed when a
    /// sequence passes, kept (and its path printed) when a failing case unwinds through it.
    #[cfg(test)]
    Files(TestDir),
}

impl Backend {
    pub(crate) fn new(files: bool) -> Self {
        #[cfg(test)]
        if files {
            return Backend::Files(TestDir::new("cce-model"));
        }
        assert!(!files || cfg!(test), "real files need the unit-test build");
        Backend::Sim(SimIo::new(), Arc::default())
    }

    /// An adapter for the next create or open. On the simulator the previous handle's ownership
    /// ends first (its "process" exited); on real files closing the handle released it.
    pub(crate) fn io(&self) -> Box<dyn Io> {
        match self {
            Backend::Sim(sim, tap) => {
                sim.release();
                Box::new(Watched {
                    sim: sim.clone(),
                    tap: tap.clone(),
                })
            }
            #[cfg(test)]
            Backend::Files(dir) => Box::new(FsIo::new(dir)),
        }
    }
}

pub(crate) struct Engine {
    pub(crate) backend: Backend,
    pub(crate) store: Option<NativeStore>,
    pub(crate) held: Vec<NativeView>,
    pub(crate) staged: Vec<Staged>,
    pub(crate) refused: Vec<StagedContent>,
    /// Stagings whose handles the harness dropped without discarding or committing them (a
    /// retry answered from its first commit stages content it never consumes): they own their
    /// space until the store is reopened.
    pub(crate) abandoned: Vec<Staged>,
}

/// A live staging handle and the content-file bytes its staging wrote (observed through the tap,
/// not taken from the store): the space this handle owns until a commit consumes it or it is
/// discarded. Empty on real files.
pub(crate) struct Staged {
    pub(crate) handle: StagedContent,
    pub(crate) owns: Vec<(u64, Vec<u8>)>,
    /// Commits that bound this handle and were rejected.
    pub(crate) rejected: u32,
    /// Bound by a commit whose publication is uncertain: its space belongs to that commit until
    /// reopen (it cannot be discarded).
    pub(crate) uncertain: bool,
}

impl Staged {
    pub(crate) fn ranges(&self) -> impl Iterator<Item = (u64, u64)> + '_ {
        self.owns.iter().map(|(o, b)| (*o, b.len() as u64))
    }
}

impl Engine {
    pub(crate) fn store(&self) -> &NativeStore {
        self.store.as_ref().expect("store open")
    }

    pub(crate) fn tap(&self) -> Option<&Arc<Tap>> {
        match &self.backend {
            Backend::Sim(_, tap) => Some(tap),
            #[cfg(test)]
            Backend::Files(_) => None,
        }
    }

    /// Content-file writes observed since the last call.
    pub(crate) fn take_writes(&self) -> Vec<(u64, Vec<u8>)> {
        self.tap()
            .map(|t| std::mem::take(&mut *t.writes.lock().unwrap()))
            .unwrap_or_default()
    }

    pub(crate) fn stage(&self, bytes: &[u8]) -> Staged {
        self.take_writes();
        let handle = self
            .store()
            .stage_content(&mut &bytes[..])
            .unwrap_or_else(|e| panic!("the store failed: stage: {e:?}"));
        assert_eq!(
            handle.content,
            content_ref(bytes),
            "staged content description"
        );
        Staged {
            handle,
            owns: self.take_writes(),
            rejected: 0,
            uncertain: false,
        }
    }

    /// The live handle a binding of `req` names, if it is staged here with the request's bytes
    /// (after a reopen or in a replay that diverged, the request stages those bytes afresh).
    pub(crate) fn earlier(&self, req: &Request, j: usize) -> Option<usize> {
        req.staged_pick(j).filter(|k| {
            self.staged
                .get(*k)
                .is_some_and(|s| s.handle.content == content_ref(&req.contents[j]))
        })
    }

    /// The change set for `req` and the stagings it made itself (earlier stagings it binds stay
    /// in `self.staged`).
    pub(crate) fn change_set(&self, req: &Request) -> (ChangeSet, Vec<Staged>) {
        // One handle per distinct staging; an aliased binding binds its target's handle again,
        // a binding of an earlier staging binds that live handle.
        let mut handle_of = vec![usize::MAX; req.contents.len()];
        let mut staged: Vec<Staged> = Vec::new();
        for (j, b) in req.contents.iter().enumerate() {
            if req.alias(j).is_none() && self.earlier(req, j).is_none() {
                handle_of[j] = staged.len();
                staged.push(self.stage(b));
            }
        }
        let content = (0..req.contents.len())
            .map(|j| match self.earlier(req, j) {
                Some(k) => ContentBinding::from(&self.staged[k].handle),
                None => ContentBinding::from(&staged[handle_of[req.alias(j).unwrap_or(j)]].handle),
            })
            .collect();
        let cs = ChangeSet {
            changes: req.changes.clone(),
            content,
            evidence: req.evidence.clone(),
            removal: req.removal,
        };
        (cs, staged)
    }
}

pub(crate) fn read_all(r: &mut dyn Read) -> Vec<u8> {
    let mut v = Vec::new();
    r.read_to_end(&mut v)
        .unwrap_or_else(|e| panic!("the store failed: read content: {e:?}"));
    v
}

pub(crate) fn check_read(view: &mut NativeView, expected: &ReadResult) {
    match expected {
        ReadResult::Context(id, e) => {
            assert_eq!(
                &view
                    .context(id)
                    .unwrap_or_else(|e| panic!("the store failed: context: {e:?}")),
                e,
                "context {id:?}"
            );
        }
        ReadResult::Relationships {
            query,
            page,
            expected,
        } => {
            let all: Vec<Relationship> = view
                .relationships(query)
                .unwrap_or_else(|e| panic!("the store failed: relationships: {e:?}"))
                .map(|r| r.unwrap_or_else(|e| panic!("the store failed: relationship: {e:?}")))
                .collect();
            assert_eq!(&all, expected, "relationships for {query:?}");
            if *page > 0 && !expected.is_empty() {
                let first: Vec<Relationship> = view
                    .relationships(query)
                    .unwrap_or_else(|e| panic!("the store failed: relationships: {e:?}"))
                    .take(*page)
                    .map(|r| r.unwrap_or_else(|e| panic!("the store failed: relationship: {e:?}")))
                    .collect();
                let last = first.last().expect("nonempty page");
                let other = if query.direction == Direction::Outgoing {
                    last.target.clone()
                } else {
                    last.source.clone()
                };
                let resumed = RelationshipQuery {
                    after: Some(RelationshipPosition {
                        kind: last.kind.clone(),
                        other,
                        id: last.id.clone(),
                    }),
                    ..query.clone()
                };
                let rest: Vec<Relationship> = view
                    .relationships(&resumed)
                    .unwrap_or_else(|e| panic!("the store failed: relationships: {e:?}"))
                    .map(|r| r.unwrap_or_else(|e| panic!("the store failed: relationship: {e:?}")))
                    .collect();
                let joined: Vec<Relationship> = first.into_iter().chain(rest).collect();
                assert_eq!(&joined, expected, "paged relationships for {query:?}");
            }
        }
        ReadResult::Content(e) => {
            if let Some((r, bytes)) = e {
                let got = read_all(&mut view.content(r).unwrap_or_else(|e| {
                    panic!("the store failed: content of a live record: {e:?}")
                }));
                assert!(got == **bytes, "content bytes of {r:?}");
            }
        }
    }
}

pub(crate) fn main_check(engine: &Engine, snap: &Snapshot) {
    let store = engine.store();
    let mut v = store
        .begin()
        .unwrap_or_else(|e| panic!("the store failed: begin: {e:?}"));
    for i in 0..CONTEXTS.len() as u8 {
        let id = ctx_id(i);
        assert_eq!(
            v.context(&id)
                .unwrap_or_else(|e| panic!("the store failed: context: {e:?}")),
            snap.logical.context(&id),
            "context {id:?}"
        );
        for outgoing in [true, false] {
            let direction = if outgoing {
                Direction::Outgoing
            } else {
                Direction::Incoming
            };
            for k in std::iter::once(None).chain((0..KINDS.len() as u8).map(Some)) {
                let k = k.map(kind);
                let q = RelationshipQuery {
                    from: id.clone(),
                    direction,
                    kind: k.clone(),
                    after: None,
                };
                let got: Vec<Relationship> = v
                    .relationships(&q)
                    .unwrap_or_else(|e| panic!("the store failed: relationships: {e:?}"))
                    .map(|r| r.unwrap_or_else(|e| panic!("the store failed: relationship: {e:?}")))
                    .collect();
                assert_eq!(got, snap.logical.query(&id, outgoing, k.as_ref()), "{q:?}");
            }
        }
        if let Some(c) = snap.logical.contexts.get(&id).and_then(|r| r.content) {
            let got =
                read_all(&mut v.content(&c).unwrap_or_else(|e| {
                    panic!("the store failed: content of a live record: {e:?}")
                }));
            assert!(got == *snap.bytes[&c.digest.0], "content of {id:?}");
        }
    }
    let history: Vec<HistoryEntry> = v
        .history(HistoryPosition(0))
        .unwrap_or_else(|e| panic!("the store failed: history: {e:?}"))
        .map(|h| h.unwrap_or_else(|e| panic!("the store failed: history entry: {e:?}")))
        .collect();
    assert_eq!(history, snap.history, "retained history");
    for (c, bytes) in &snap.unreferenced {
        match v.content(c) {
            Ok(mut r) => assert!(read_all(&mut r) == **bytes, "unreferenced content {c:?}"),
            Err(StorageError::ContentNotRetained) => count("unreferenced content not retained"),
            Err(e) => panic!("unreferenced content {c:?}: {e:?}"),
        }
    }
    drop(v);
    for (a, o) in &snap.outcomes {
        assert_eq!(
            &store
                .outcome(a)
                .unwrap_or_else(|e| panic!("the store failed: outcome: {e:?}")),
            o,
            "outcome of {a:?}"
        );
    }
}

pub(crate) fn err_kind(e: &CommitError) -> ErrKind {
    match e {
        CommitError::Conflict(_) => ErrKind::Conflict,
        CommitError::Rejected(_) => ErrKind::Rejected,
        CommitError::AttemptMismatch => ErrKind::Mismatch,
        CommitError::ReadersActive(_) => ErrKind::ReadersActive,
        CommitError::Indeterminate(m) => panic!("no faults are injected, yet indeterminate: {m}"),
        other => panic!("unexpected commit error {other:?}"),
    }
}

pub(crate) fn check_result(
    got: &Result<CommitReceipt, CommitError>,
    req: &Request,
    expected: &Expected,
) {
    match (got, expected) {
        (Ok(r), Expected::Committed(p)) => assert_eq!(
            r,
            &CommitReceipt {
                attempt: req.attempt.clone(),
                position: HistoryPosition(*p)
            }
        ),
        (Err(e), Expected::Failed(kinds)) => {
            let k = err_kind(e);
            assert!(kinds.contains(&k), "{e:?} is not one of {kinds:?}");
            count(match k {
                ErrKind::Conflict => "commit conflict",
                ErrKind::Rejected => "commit rejected",
                ErrKind::Mismatch => "commit mismatch",
                ErrKind::ReadersActive => "commit readers active",
            });
        }
        (got, expected) => panic!("commit result {got:?}, model expected {expected:?}"),
    }
}

/// The engine on the simulator (`FILES == false`) or on real files.
pub(crate) struct EngineTest<const FILES: bool>;

/// Opens the store again after the previous handle closed.
pub(crate) fn open(backend: &Backend) -> NativeStore {
    open_on(backend.io(), Default::default())
        .unwrap_or_else(|e| panic!("a cleanly closed store must open again: {e:?}"))
}

pub(crate) fn reads_through(view: &mut NativeView, reads: &[ReadResult]) {
    for r in reads {
        check_read(view, r);
    }
}

impl Engine {
    /// A fresh store on the simulator (or, `files`, on real files).
    pub(crate) fn init(files: bool) -> Engine {
        let backend = Backend::new(files);
        let store = create_on(backend.io(), STORE, Default::default())
            .unwrap_or_else(|e| panic!("creating the store under test: {e:?}"));
        Engine {
            backend,
            store: Some(store),
            held: Vec::new(),
            staged: Vec::new(),
            refused: Vec::new(),
            abandoned: Vec::new(),
        }
    }

    /// Runs the request the model resolved for `op` and checks every observation.
    pub(crate) fn apply(mut e: Engine, m: &Model, op: Op) -> Engine {
        count(&format!("op {}", op_name(&op)));
        match &m.action {
            Action::Nothing => count("op without effect"),
            Action::Commit {
                reads,
                held_slot,
                request,
                expected,
                poll,
            } => {
                let mut basis = match held_slot {
                    Some(s) => {
                        count("commit from held view");
                        e.held.remove(*s)
                    }
                    None => e
                        .store()
                        .begin()
                        .unwrap_or_else(|e| panic!("the store failed: begin: {e:?}")),
                };
                if held_slot.is_none() {
                    reads_through(&mut basis, reads);
                }
                let (cs, staged) = e.change_set(request);
                let got = if *poll {
                    count("submit and poll");
                    let store = e.store();
                    assert_eq!(
                        store
                            .outcome(&request.attempt)
                            .unwrap_or_else(|e| panic!("the store failed: outcome: {e:?}")),
                        AttemptOutcome::Absent
                    );
                    let ticket = match store.submit(request.attempt.clone(), basis, cs) {
                        Ok(t) => t,
                        Err(err) => {
                            check_result(&Err(err), request, expected);
                            e.staged.extend(staged);
                            audit(&e, "a refused submission");
                            ownership_check(&e, "a refused submission");
                            return e;
                        }
                    };
                    let mut polls = 0u32;
                    loop {
                        match store
                            .outcome(&request.attempt)
                            .unwrap_or_else(|e| panic!("the store failed: outcome: {e:?}"))
                        {
                            AttemptOutcome::Pending => {}
                            AttemptOutcome::Committed(r) => {
                                assert!(
                                    matches!(expected, Expected::Committed(_)),
                                    "committed {r:?}, model expected {expected:?}"
                                );
                                break;
                            }
                            AttemptOutcome::Absent => {
                                assert!(
                                    matches!(expected, Expected::Failed(_)),
                                    "an admitted attempt that commits was reported absent \
                                     after {polls} polls"
                                );
                                break;
                            }
                            AttemptOutcome::Unknown => panic!("unknown outcome without faults"),
                        }
                        polls += 1;
                        assert!(polls < 10_000_000, "attempt never resolved");
                    }
                    ticket.wait()
                } else {
                    e.store().commit(request.attempt.clone(), basis, cs)
                };
                check_result(&got, request, expected);
                let picks: BTreeSet<usize> = (0..request.contents.len())
                    .filter_map(|j| e.earlier(request, j))
                    .collect();
                if got.is_ok() {
                    count("commit ok");
                    if !picks.is_empty() {
                        count("commit of earlier stagings");
                    }
                    // Its own stagings and the earlier ones it bound are consumed.
                    let mut consumed: Vec<Staged> = staged;
                    for k in picks.into_iter().rev() {
                        let s = e.staged.remove(k);
                        if s.rejected > 0 {
                            count("staging committed after a rejected commit bound it");
                        }
                        consumed.push(s);
                    }
                    let ranges: Vec<(u64, u64)> =
                        consumed.iter().flat_map(Staged::ranges).collect();
                    consumed_check(&e, &ranges, "a commit");
                    e.refused.extend(consumed.into_iter().map(|s| s.handle));
                } else {
                    // Rejected: earlier stagings it bound stay staged and owned, untouched.
                    if !picks.is_empty() {
                        count("rejected commit of earlier stagings");
                    }
                    for k in picks {
                        e.staged[k].rejected += 1;
                    }
                    e.staged.extend(staged);
                }
            }
            Action::Retry { request, expected } => {
                let (cs, staged) = e.change_set(request);
                let basis = e
                    .store()
                    .begin()
                    .unwrap_or_else(|e| panic!("the store failed: begin: {e:?}"));
                let got = e.store().commit(request.attempt.clone(), basis, cs);
                match (&got, expected) {
                    (Ok(r), Ok(p)) => {
                        count("retry returned original receipt");
                        assert_eq!(r.position, HistoryPosition(*p));
                        assert_eq!(r.attempt, request.attempt);
                    }
                    (Err(CommitError::AttemptMismatch), Err(ErrKind::Mismatch)) => {
                        count("retry mismatch")
                    }
                    (got, expected) => panic!("retry {got:?}, model expected {expected:?}"),
                }
                // Answered from the first commit: the content it staged is not consumed.
                e.abandoned.extend(staged);
            }
            Action::Hold(reads) => {
                count("hold view");
                if e.held.len() == MAX_HELD {
                    e.held.remove(0);
                }
                let mut v = e
                    .store()
                    .begin()
                    .unwrap_or_else(|e| panic!("the store failed: begin: {e:?}"));
                reads_through(&mut v, reads);
                e.held.push(v);
            }
            Action::ReadHeld(s, reads) => {
                count("read held view");
                reads_through(&mut e.held[*s], reads);
            }
            Action::Release(s) => {
                e.held.remove(*s);
            }
            Action::Stage(b) => {
                let s = e.stage(b);
                e.staged.push(s);
            }
            Action::Discard { index, refused } if !*refused => {
                let h = e.staged.remove(*index);
                if h.rejected > 0 {
                    count("discard after a rejected commit bound the staging");
                }
                let ranges: Vec<(u64, u64)> = h.ranges().collect();
                e.store()
                    .discard_staged(h.handle)
                    .unwrap_or_else(|e| panic!("the store failed: discard staged content: {e:?}"));
                count("discard ok");
                if let Backend::Sim(sim, _) = &e.backend {
                    let (_, r) = traverse(sim, "a discard");
                    let reached = reached_extents(&r);
                    assert!(
                        !ranges
                            .iter()
                            .any(|a| reached.iter().any(|b| overlaps(*a, *b))),
                        "a discarded staging's space is reachable"
                    );
                }
            }
            Action::Discard { index, refused } => {
                if *refused {
                    let h = e.refused.remove(*index);
                    let got = e.store().discard_staged(h);
                    assert!(
                        got.is_err(),
                        "discarding a committed or foreign handle succeeded"
                    );
                    count("discard refused");
                } else {
                    unreachable!("an ordinary discard is handled above")
                }
            }
            Action::Check(snap) => {
                count("full check");
                main_check(&e, snap);
            }
            Action::Reopen => {
                count("reopen");
                e.held.clear();
                // Handles of the closed instance: discarding them must be refused; they own
                // nothing any more.
                e.refused.extend(e.staged.drain(..).map(|s| s.handle));
                e.abandoned.clear();
                e.store
                    .take()
                    .expect("store")
                    .close()
                    .unwrap_or_else(|e| panic!("the store failed: clean close: {e:?}"));
                e.store = Some(open(&e.backend));
            }
        }
        audit(&e, &format!("step {}", op_name(&op)));
        ownership_check(&e, &format!("step {}", op_name(&op)));
        e
    }

    /// The final checks of a sequence: the model's state, before and after a clean reopen.
    pub(crate) fn teardown(mut e: Engine, m: Model) {
        let snap = m.snapshot();
        main_check(&e, &snap);
        e.held.clear();
        e.staged.clear();
        e.refused.clear();
        e.store
            .take()
            .expect("store")
            .close()
            .unwrap_or_else(|e| panic!("the store failed: clean close: {e:?}"));
        e.store = Some(open(&e.backend));
        main_check(&e, &snap);
        e.store
            .take()
            .expect("store")
            .close()
            .unwrap_or_else(|e| panic!("the store failed: clean close: {e:?}"));
        // A passing real-files sequence's directory is removed when its owner drops with `e`.
    }
}

// ---------------------------------------------------------------------------------------------
// Crash and I/O-failure campaigns: the same generated sequences, interrupted.
//
// A sequence is first run to completion to learn how many mutating I/O operations each step
// performs. For each chosen cut `k`, a fresh simulated store replays the sequence; every step that
// completed before the cut is checked as usual and the step in flight runs without expectations.
//
// Crash: the simulator crashes after `k` mutating operations. The image is recovered with pending
// effects lost, all kept and two seeded torn and reordered subsets; a crash during that recovery
// is recovered again; and the process may instead exit at the cut, be recovered, and lose power.
//
// I/O failure: operation `k + 1` fails (a failed sync keeps or, lossy, drops its pending writes)
// and the process keeps running: what the store reports about the failed attempt, and every
// commit it acknowledges afterwards, must fit the model with that step applied or not applied.
// The process then exits or loses power and the store is recovered.
//
// Every recovered store must be exactly one model state the observations allow, report outcomes
// accordingly, honour a retry of the interrupted request, and commit and reopen afterwards.
// ---------------------------------------------------------------------------------------------

pub(crate) const CRASH_SEED: u64 = 0x5eed_0002_cce0_0002;
/// Pending effects lost, all kept, and two seeded subsets (torn and reordered).
pub(crate) const CRASH_PERSISTS: [Persist; 4] = [
    Persist::None,
    Persist::All,
    Persist::Seeded(1),
    Persist::Seeded(2),
];
/// Commit steps the store runs after an I/O failure before the process ends.
pub(crate) const LIVE_STEPS: usize = 6;

pub(crate) fn sim_engine(sim: SimIo, store: NativeStore) -> Engine {
    Engine {
        backend: Backend::Sim(sim, Arc::default()),
        store: Some(store),
        held: Vec::new(),
        staged: Vec::new(),
        refused: Vec::new(),
        abandoned: Vec::new(),
    }
}

/// The retirement audit on the simulator's image: no retired page or content extent is reachable
/// from the published state, and the newest commit retired exactly what it stopped reaching.
/// Reads only, so it never changes the operation counts replays compare. (A real-files store holds
/// its lock, so the audit runs on the simulator.)
pub(crate) fn audit(e: &Engine, after: &str) {
    // The store's own pool-versus-records oracle (only meaningful while nothing is in flight,
    // which holds between model steps). Its view is compared with the independent ownership
    // model in `ownership_check`.
    if let Some(store) = &e.store {
        count("ledger oracle checks");
        assert_eq!(
            store.inner.ledger_check_for_test(),
            Ok(()),
            "ledger oracle after {after}"
        );
    }
    if let Backend::Sim(sim, _) = &e.backend {
        count("retirement audits");
        assert_eq!(
            crate::audit::check_retirement(sim),
            Ok(()),
            "retirement audit after {after}"
        );
    }
}

/// What the newest publication record reaches and records as retired, by traversal through the
/// audit read path (the store's pool and journal are never consulted).
pub(crate) fn traverse(
    sim: &SimIo,
    after: &str,
) -> (crate::format::Descriptor, crate::audit::Reach) {
    let pubs = crate::audit::published(sim)
        .unwrap_or_else(|x| panic!("{after}: publication records unreadable: {x:?}"));
    let cur = pubs
        .last()
        .cloned()
        .unwrap_or_else(|| panic!("{after}: no valid publication record"));
    let r = crate::audit::reach(sim, &cur)
        .unwrap_or_else(|x| panic!("{after}: the published state cannot be traversed: {x:?}"));
    (cur, r)
}

pub(crate) fn reached_extents(r: &crate::audit::Reach) -> Vec<(u64, u64)> {
    r.manifests
        .iter()
        .chain(&r.content_segments)
        .chain(&r.history_extents)
        .copied()
        .collect()
}

pub(crate) fn overlaps(a: (u64, u64), b: (u64, u64)) -> bool {
    a.0 < b.0 + b.1 && b.0 < a.0 + a.1
}

/// The independent ownership model, after every step: each live staging owns the content bytes
/// its staging wrote (known from the operations the harness issued and the writes it observed).
/// Owned bytes must be intact, no two stagings may own the same byte, and the published state
/// must not reach owned space. Owned space may carry a persisted retirement record (a staging
/// that took retired space before any commit consumed the record); that is counted, not refused.
pub(crate) fn ownership_check(e: &Engine, after: &str) {
    let Backend::Sim(sim, _) = &e.backend else {
        return;
    };
    count("ownership checks");
    let (cur, r) = traverse(sim, after);
    let reached = reached_extents(&r);
    let live: Vec<(usize, (u64, u64))> = e
        .staged
        .iter()
        .chain(&e.abandoned)
        .enumerate()
        .flat_map(|(i, s)| s.ranges().map(move |x| (i, x)))
        .collect();
    for (n, &(i, a)) in live.iter().enumerate() {
        assert!(
            !reached.iter().any(|b| overlaps(a, *b)),
            "ownership after {after}: space {a:?} owned by live staging {i} is reachable from \
             generation {}",
            cur.generation
        );
        for &(j, b) in &live[n + 1..] {
            assert!(
                i == j || !overlaps(a, b),
                "ownership after {after}: live stagings {i} and {j} own the same space ({a:?}, \
                 {b:?})"
            );
        }
        if r.retired_extents
            .iter()
            .any(|(o, (_, l))| overlaps(a, (*o, *l)))
        {
            count("staging owns space with a retirement record");
        }
    }
    // The store's view of the same space, compared with this model: no extent it offers for
    // reuse touches owned space, and the reservations it says stagings hold are exactly the
    // retirement records live stagings' bytes lie in.
    if let Some(store) = &e.store {
        let (_, free) = store
            .inner
            .reusable_for_test()
            .unwrap_or_else(|x| panic!("ownership after {after}: reusable space: {x:?}"));
        for f in &free {
            if let Some((i, a)) = live.iter().find(|(_, a)| overlaps(*a, *f)) {
                panic!(
                    "ownership after {after}: the store offers {f:?} for reuse, but live staging \
                     {i} owns {a:?}"
                );
            }
        }
        let owners: Vec<&Staged> = e.staged.iter().chain(&e.abandoned).collect();
        let in_records = |uncertain: bool| -> u64 {
            r.retired_extents
                .iter()
                .filter(|(o, (_, l))| {
                    owners
                        .iter()
                        .filter(|s| s.uncertain == uncertain)
                        .any(|s| s.ranges().any(|a| overlaps(a, (**o, *l))))
                })
                .map(|(_, (_, l))| *l)
                .sum()
        };
        let (held, uncertain) = store.inner.reserved_bytes_for_test();
        assert_eq!(
            held,
            in_records(false),
            "ownership after {after}: the store holds {held} bytes of reservations for stagings; \
             the stagings it issued own bytes in retirement records of {} bytes",
            in_records(false)
        );
        assert_eq!(
            uncertain,
            in_records(true),
            "ownership after {after}: an uncertain publication owns {uncertain} bytes of \
             reservations; the stagings it bound own bytes in records of {} bytes",
            in_records(true)
        );
        count("ownership model agrees with the ledger");
    }
    for (i, st) in e.staged.iter().enumerate() {
        for (o, bytes) in &st.owns {
            let mut now = vec![0; bytes.len()];
            sim.read_at(FileId::Content, *o, &mut now)
                .unwrap_or_else(|x| {
                    panic!("ownership after {after}: owned bytes unreadable: {x:?}")
                });
            assert!(
                now == *bytes,
                "ownership after {after}: bytes at {o}+{} owned by live staging {i} were \
                 overwritten",
                bytes.len()
            );
        }
    }
}

/// After a commit consumed stagings: each byte they owned is reached by the new state or
/// recorded as retired (a staging the index did not take), never simply lost or left owned.
pub(crate) fn consumed_check(e: &Engine, consumed: &[(u64, u64)], after: &str) {
    let Backend::Sim(sim, _) = &e.backend else {
        return;
    };
    let (cur, r) = traverse(sim, after);
    let mut known = reached_extents(&r);
    known.extend(r.retired_extents.iter().map(|(o, (_, l))| (*o, *l)));
    for a in consumed {
        assert!(
            known.iter().any(|k| k.0 <= a.0 && a.0 + a.1 <= k.0 + k.1),
            "ownership after {after}: consumed staging space {a:?} is neither reached nor \
             retired by generation {}",
            cur.generation
        );
        count("consumed staging space accounted");
    }
}

/// After a recovery no staging owns anything: space owned by stagings that were live before the
/// interruption and that the interrupted step did not bind is unreachable, and the publication
/// the traversal reads is the one the store opened on.
pub(crate) fn recovered_ownership_check(e: &Engine, prior: &[(u64, u64)]) {
    let Backend::Sim(sim, _) = &e.backend else {
        return;
    };
    let (cur, r) = traverse(sim, "recovery");
    assert_eq!(
        cur.history_pos,
        last_position(e.store()),
        "the publication record the traversal reads is not the state the store opened on"
    );
    let reached = reached_extents(&r);
    for a in prior {
        assert!(
            !reached.iter().any(|b| overlaps(*a, *b)),
            "after recovery: space {a:?} owned by an unconsumed staging before the interruption \
             is reachable"
        );
    }
    count("recovered ownership checks");
}

/// The live stagings (by index) the step `action` binds.
pub(crate) fn bound_stagings(e: &Engine, action: &Action) -> BTreeSet<usize> {
    match action {
        Action::Commit { request, .. } | Action::Retry { request, .. } => {
            (0..request.contents.len())
                .filter_map(|j| e.earlier(request, j))
                .collect()
        }
        _ => BTreeSet::new(),
    }
}

/// Space owned by live stagings that the step `action` does not bind or discard.
pub(crate) fn unbound_staged_space(e: &Engine, action: &Action) -> Vec<(u64, u64)> {
    let mut bound = bound_stagings(e, action);
    if let Action::Discard {
        index,
        refused: false,
    } = action
    {
        bound.insert(*index);
    }
    e.staged
        .iter()
        .enumerate()
        .filter(|(i, _)| !bound.contains(i))
        .flat_map(|(_, s)| s.ranges())
        .collect()
}

pub(crate) fn sim_of(e: &Engine) -> &SimIo {
    match &e.backend {
        Backend::Sim(sim, _) => sim,
        #[cfg(test)]
        Backend::Files(_) => unreachable!("crash campaigns run on the simulator"),
    }
}

/// The kind of operation the injected failure hit so far, if any.
pub(crate) fn failed_op(e: &Engine) -> Option<&'static str> {
    match &e.backend {
        Backend::Sim(_, tap) => *tap.failed.lock().unwrap(),
        #[cfg(test)]
        Backend::Files(_) => None,
    }
}

/// Mutating operations performed after store creation, cumulatively after each step.
pub(crate) fn boundaries(transitions: &[Op]) -> Vec<u64> {
    let mut e = Engine::init(false);
    let base = sim_of(&e).ops();
    let mut m = Model::default();
    let mut out = Vec::with_capacity(transitions.len());
    for op in transitions {
        m = Model::apply(m, op);
        e = Engine::apply(e, &m, op.clone());
        out.push(sim_of(&e).ops() - base);
    }
    out
}

/// Stages a request's content and commits it from `basis` (a fresh view when `None`), with no
/// expectations. `prestaged` binds an already staged handle as the request's last content.
pub(crate) fn submit_tolerant(
    e: &mut Engine,
    basis: Option<NativeView>,
    request: &Request,
    prestaged: Option<StagedContent>,
) -> Option<Result<CommitReceipt, CommitError>> {
    let store = e.store.as_ref()?;
    let basis = match basis {
        Some(b) => b,
        None => store.begin().ok()?,
    };
    // One handle per distinct staging (the last one may be prestaged); aliases bind it again.
    let n = request.contents.len();
    let mut prestaged = prestaged;
    let mut handles: Vec<Option<StagedContent>> = (0..n).map(|_| None).collect();
    for (j, slot) in handles.iter_mut().enumerate() {
        if request.alias(j).is_some() || e.earlier(request, j).is_some() {
            continue;
        }
        *slot = Some(match (j + 1 == n, prestaged.take()) {
            (true, Some(h)) => h,
            (_, other) => {
                prestaged = other;
                store.stage_content(&mut &request.contents[j][..]).ok()?
            }
        });
    }
    let bindings = (0..n)
        .map(|j| match e.earlier(request, j) {
            Some(k) => ContentBinding::from(&e.staged[k].handle),
            None => {
                let h = handles[request.alias(j).unwrap_or(j)].as_ref();
                ContentBinding::from(h.expect("an alias names a staged binding"))
            }
        })
        .collect();
    let cs = ChangeSet {
        changes: request.changes.clone(),
        content: bindings,
        evidence: request.evidence.clone(),
        removal: request.removal,
    };
    Some(store.commit(request.attempt.clone(), basis, cs))
}

/// Executes a step while the store may be failing underneath it: no expectations, no panics on
/// errors. Returns the request it submitted, if any, and the commit's result if it got one.
pub(crate) fn in_flight(
    e: &mut Engine,
    action: &Action,
) -> (Option<Request>, Option<Result<CommitReceipt, CommitError>>) {
    match action {
        Action::Commit {
            held_slot, request, ..
        } => {
            let basis = held_slot.map(|s| e.held.remove(s));
            let got = submit_tolerant(e, basis, request, None);
            (Some(request.clone()), got)
        }
        Action::Retry { request, .. } => {
            let got = submit_tolerant(e, None, request, None);
            (Some(request.clone()), got)
        }
        Action::Stage(b) => {
            e.take_writes();
            if let Ok(handle) = e.store().stage_content(&mut &b[..]) {
                let owns = e.take_writes();
                e.staged.push(Staged {
                    handle,
                    owns,
                    rejected: 0,
                    uncertain: false,
                });
            }
            (None, None)
        }
        Action::Discard { index, refused } => {
            let h = if *refused {
                e.refused.remove(*index)
            } else {
                e.staged.remove(*index).handle
            };
            let _ = e.store().discard_staged(h);
            (None, None)
        }
        Action::Reopen => {
            e.held.clear();
            e.staged.clear();
            if let Some(s) = e.store.take() {
                let _ = s.close();
            }
            e.store = open_on(e.backend.io(), Default::default()).ok();
            (None, None)
        }
        // Steps without mutating I/O never contain a cut.
        _ => (None, None),
    }
}

pub(crate) fn last_position(store: &NativeStore) -> u64 {
    let mut v = store
        .begin()
        .unwrap_or_else(|e| panic!("the store failed: begin: {e:?}"));
    v.history(HistoryPosition(0))
        .unwrap_or_else(|e| panic!("the store failed: history: {e:?}"))
        .map(|h| {
            h.unwrap_or_else(|e| panic!("the store failed: history entry: {e:?}"))
                .position
                .0
        })
        .last()
        .unwrap_or(0)
}

/// The model states a recovered store may be in, around one interrupted step.
#[derive(Clone)]
pub(crate) struct Allowed {
    /// The interrupted step's effects absent (`None`: observations ruled this out).
    pub(crate) without: Option<Model>,
    /// The interrupted step committed at `position` (`None`: it cannot have committed).
    pub(crate) with: Option<(Model, u64)>,
    /// The interrupted request and, when that step was a retry, what the retry must return.
    pub(crate) request: Option<Request>,
    pub(crate) retry: Option<Result<u64, ErrKind>>,
    /// Space owned by stagings live before the interruption that nothing since bound: a
    /// recovered state must not reach it.
    pub(crate) prior: Vec<(u64, u64)>,
}

impl Allowed {
    pub(crate) fn around(before: &Model, after: &Model, request: Option<Request>) -> Self {
        let retry = match &after.action {
            Action::Retry { expected, .. } => Some(*expected),
            _ => None,
        };
        Allowed {
            without: Some(before.clone()),
            with: (after.position == before.position + 1).then(|| (after.clone(), after.position)),
            request,
            retry,
            prior: Vec::new(),
        }
    }

    /// The state a recovered store with history ending at `last` must equal, and whether the
    /// interrupted step is in it.
    pub(crate) fn select(&self, last: u64) -> (Model, bool) {
        let m = match (&self.without, &self.with) {
            (Some(w), _) if w.position == last => (w.clone(), false),
            (_, Some((m, _))) if m.position == last => (m.clone(), true),
            _ => panic!(
                "recovered history ends at {last}; the model allows {:?} without the step or {:?} \
                 with it",
                self.without.as_ref().map(|m| m.position),
                self.with.as_ref().map(|(m, _)| m.position),
            ),
        };
        let mut model = m.0;
        model.held.clear();
        model.log.clear();
        model.staged.clear();
        (model, m.1)
    }
}

pub(crate) fn open_image(img: &SimIo, what: &str) -> Engine {
    let store = open_on(Box::new(img.clone()), Default::default())
        .unwrap_or_else(|e| panic!("{what}: recovery must open the store: {e:?}"));
    sim_engine(img.clone(), store)
}

/// Recovers one image and checks it against the allowed states, the outcome of the interrupted
/// attempt, a retry of it, a further commit and a reopen.
pub(crate) fn check_recovery(img: SimIo, allowed: &Allowed) {
    let mut e = open_image(&img, "after the interruption");
    let (mut m, applied) = allowed.select(last_position(e.store()));
    count(if applied {
        "recovered with the step"
    } else {
        "recovered without the step"
    });
    main_check(&e, &m.snapshot());
    audit(&e, "recovery");
    recovered_ownership_check(&e, &allowed.prior);
    let Some(req) = &allowed.request else {
        return;
    };
    // The selected state says whether the attempt committed (a later retry may have committed it
    // even when the interrupted step itself did not); outcomes were compared by the full check.
    if !m.committed.iter().any(|(c, _)| c.attempt == req.attempt) {
        assert_eq!(
            e.store()
                .outcome(&req.attempt)
                .unwrap_or_else(|e| panic!("the store failed: outcome: {e:?}")),
            AttemptOutcome::Absent,
            "an attempt whose effects did not survive must be absent"
        );
    }
    // Retry contract: the same request, from a fresh view, after recovery.
    let (cs, _staged) = e.change_set(req);
    let basis = e
        .store()
        .begin()
        .unwrap_or_else(|e| panic!("the store failed: begin: {e:?}"));
    let got = e.store().commit(req.attempt.clone(), basis, cs);
    if let Some(expected) = allowed.retry {
        // The interrupted step was itself a retry of a committed attempt; nothing changes.
        match (&got, expected) {
            (Ok(r), Ok(p)) => assert_eq!(r.position, HistoryPosition(p)),
            (Err(CommitError::AttemptMismatch), Err(ErrKind::Mismatch)) => {}
            (got, expected) => panic!("retry after recovery {got:?}, expected {expected:?}"),
        }
    } else {
        let (expected, written) = branch_expect(&m, req);
        let known = m.committed.iter().any(|(c, _)| c.attempt == req.attempt);
        count(if known {
            "retry after recovery returned the receipt"
        } else {
            "retry after recovery decided afresh"
        });
        check_result(&got, req, &expected);
        if !known {
            match expected {
                Expected::Committed(_) => m.apply_commit(req, written),
                Expected::Failed(_) => {
                    m.uncommitted.insert(req.attempt.clone());
                }
            }
        }
    }
    let snap = m.snapshot();
    main_check(&e, &snap);
    e.store
        .take()
        .expect("store")
        .close()
        .unwrap_or_else(|e| panic!("the store failed: clean close: {e:?}"));
    e.store = Some(open(&e.backend));
    main_check(&e, &snap);
}

/// The process exits (the operating system keeps its unsynced writes), the store is recovered,
/// and then the power fails before anything else happens: what the recovery reported must have
/// been made durable by the recovery itself.
pub(crate) fn check_exit_then_power_loss(sim: &SimIo, allowed: &Allowed) {
    let img = sim.after_process_exit();
    let e = open_image(&img, "after a process exit");
    let reported = last_position(e.store());
    let (m, _) = allowed.select(reported);
    let snap = m.snapshot();
    main_check(&e, &snap);
    drop(e);
    let e = open_image(&img.after_crash(Persist::None), "power loss after recovery");
    assert_eq!(
        last_position(e.store()),
        reported,
        "a power loss right after recovery lost the state recovery reported"
    );
    main_check(&e, &snap);
    count("process exit, recovery, power loss");
}

/// Recovery itself is interrupted: a crash after each of its mutating operations, and a failure
/// of each, then a fresh recovery.
pub(crate) fn check_interrupted_recovery(sim: &SimIo, persist: Persist, allowed: &Allowed) {
    let probe = sim.after_crash(persist);
    let base = probe.ops();
    drop(open_image(&probe, "measuring recovery"));
    let n = probe.ops() - base;
    for j in 0..n {
        let img = sim.after_crash(persist);
        img.crash_after(j);
        assert!(
            open_on(Box::new(img.clone()), Default::default()).is_err(),
            "recovery reported success although it crashed after {j} of {n} operations"
        );
        count("crash during recovery");
        check_recovery(img.after_crash(Persist::None), allowed);

        let img = sim.after_crash(persist);
        img.fail_nth_op(j + 1);
        assert!(
            open_on(Box::new(img.clone()), Default::default()).is_err(),
            "recovery reported success although operation {} of {n} failed",
            j + 1
        );
        img.release();
        count("failure during recovery, then recovery again");
        check_recovery(img, allowed);
    }
}

pub(crate) fn replay_until(
    transitions: &[Op],
    bounds: &[u64],
    i: usize,
    e: &mut Option<Engine>,
) -> Model {
    let base = 0;
    let mut m = Model::default();
    for (j, op) in transitions[..i].iter().enumerate() {
        m = Model::apply(m, op);
        let eng = e.take().expect("engine");
        let eng = Engine::apply(eng, &m, op.clone());
        assert_eq!(
            sim_of(&eng).ops() - base,
            bounds[j],
            "replaying step {j} performed different I/O than the first run"
        );
        *e = Some(eng);
    }
    m
}

/// A fresh store whose simulator counts operations from zero after creation.
pub(crate) fn fresh_engine() -> Engine {
    let backend = Backend::Sim(SimIo::new(), Arc::default());
    let store = create_on(backend.io(), STORE, Default::default())
        .unwrap_or_else(|e| panic!("creating the store under test: {e:?}"));
    let e = Engine {
        backend,
        store: Some(store),
        held: Vec::new(),
        staged: Vec::new(),
        refused: Vec::new(),
        abandoned: Vec::new(),
    };
    // Normalize so that replays compare operation counts directly.
    let ops = sim_of(&e).ops();
    assert_eq!(
        ops,
        CREATE_OPS.with(|c| *c.get_or_init(|| ops)),
        "store creation I/O changed"
    );
    e
}

thread_local! {
    pub(crate) static CREATE_OPS: std::cell::OnceCell<u64> = const { std::cell::OnceCell::new() };
}

/// Replays `transitions` with a crash after `cut` mutating operations and checks every recovery.
pub(crate) fn crash_at(transitions: &[Op], bounds: &[u64], cut: u64) {
    let Some(i) = bounds.iter().position(|b| *b > cut) else {
        return;
    };
    let mut e = Some(fresh_engine());
    let created = sim_of(e.as_ref().unwrap()).ops();
    sim_of(e.as_ref().unwrap()).crash_after(cut);
    let shifted: Vec<u64> = bounds.iter().map(|b| b + created).collect();
    let before = replay_until(transitions, &shifted, i, &mut e);
    let mut e = e.unwrap();
    let after = Model::apply(before.clone(), &transitions[i]);
    count(&format!("in flight: {}", op_name(&transitions[i])));
    let prior = unbound_staged_space(&e, &after.action);
    let (request, _) = in_flight(&mut e, &after.action);
    let mut allowed = Allowed::around(&before, &after, request);
    allowed.prior = prior;
    let sim = sim_of(&e).clone();
    drop(e);
    for p in CRASH_PERSISTS {
        count("recoveries");
        check_recovery(sim.after_crash(p), &allowed);
    }
    check_interrupted_recovery(&sim, Persist::All, &allowed);
    check_exit_then_power_loss(&sim, &allowed);
}

/// What a branch of the model expects from `req` now.
pub(crate) fn branch_expect(m: &Model, req: &Request) -> (Expected, Vec<WriteKey>) {
    if let Some((prior, p)) = m.committed.iter().find(|(r, _)| r.attempt == req.attempt) {
        let same = prior.changes == req.changes
            && prior.evidence == req.evidence
            && prior.removal == req.removal
            && prior.refs() == req.refs();
        return if same {
            (Expected::Committed(*p), Vec::new())
        } else {
            (Expected::Failed([ErrKind::Mismatch].into()), Vec::new())
        };
    }
    let mut free = m.clone();
    free.held.clear();
    free.predict(req, None)
}

/// What the live store may answer after the failure, as established by its own reports: only a
/// failure the store reported from a commit (a failed barrier or an uncertain publication) may
/// fence later writes, and only the attempt it reported indeterminate may stay indeterminate.
#[derive(Clone, Default)]
pub(crate) struct Live {
    pub(crate) fence_allowed: bool,
    pub(crate) uncertain: Option<AttemptId>,
}

/// The logical kind of a refusal the model predicts; `None` for anything else (an integrity or
/// I/O error, a fenced store, an indeterminate outcome).
pub(crate) fn logical_kind(e: &CommitError) -> Option<ErrKind> {
    match e {
        CommitError::Conflict(_) => Some(ErrKind::Conflict),
        CommitError::AttemptMismatch => Some(ErrKind::Mismatch),
        CommitError::ReadersActive(_) => Some(ErrKind::ReadersActive),
        CommitError::Rejected(StorageError::InvalidRequest(_)) => Some(ErrKind::Rejected),
        _ => None,
    }
}

pub(crate) fn is_fenced_refusal(e: &CommitError) -> bool {
    matches!(e, CommitError::Rejected(StorageError::StoreState(m)) if m.starts_with("writes fenced"))
}

/// Whether a refusal fits a model branch that predicts `expected` (`committed`: the branch has
/// the attempt committed already, so the store must answer from its record, fence or not).
pub(crate) fn refusal_fits(
    e: &CommitError,
    expected: &Expected,
    committed: bool,
    req: &Request,
    live: &Live,
) -> bool {
    match e {
        CommitError::Indeterminate(_) => live.uncertain.as_ref() == Some(&req.attempt),
        e if is_fenced_refusal(e) => live.fence_allowed && !committed,
        e => match (logical_kind(e), expected) {
            (Some(k), Expected::Failed(kinds)) => kinds.contains(&k),
            _ => false,
        },
    }
}

/// Feeds one live commit result to the model branches: an acknowledged commit must fit a branch,
/// and so must a refusal (a predicted refusal of its kind, or a fence the failure allows); the
/// branches it does not fit are dropped. A refusal changes nothing.
pub(crate) fn live_result(
    allowed: &mut Allowed,
    req: &Request,
    got: &Result<CommitReceipt, CommitError>,
    live: &Live,
) {
    let fits = |m: &Model| -> Option<Model> {
        let (expected, written) = branch_expect(m, req);
        let committed = m.committed.iter().any(|(c, _)| c.attempt == req.attempt);
        let mut m = m.clone();
        match (got, expected) {
            (Ok(r), Expected::Committed(p)) if r.position.0 == p => {
                if !committed {
                    m.apply_commit(req, written);
                }
                Some(m)
            }
            (Ok(_), _) => None,
            (Err(e), expected) => {
                if !refusal_fits(e, &expected, committed, req, live) {
                    return None;
                }
                if !committed {
                    m.uncommitted.insert(req.attempt.clone());
                }
                Some(m)
            }
        }
    };
    let without = allowed.without.as_ref().and_then(fits);
    let with = allowed
        .with
        .as_ref()
        .and_then(|(m, p)| fits(m).map(|m| (m, *p)));
    assert!(
        without.is_some() || with.is_some(),
        "the store answered {got:?} for {:?} after the failure, which fits no model state \
         (fence allowed: {}, uncertain: {:?})",
        req.attempt,
        live.fence_allowed,
        live.uncertain
    );
    count(match got {
        Ok(_) => "acknowledged after the failure",
        Err(e) if is_fenced_refusal(e) => "refused behind the fence after the failure",
        Err(CommitError::Indeterminate(_)) => "indeterminate after the failure",
        Err(_) => "refused as predicted after the failure",
    });
    allowed.without = without;
    allowed.with = with;
}

/// Replays `transitions` with operation `cut + 1` failing, keeps the process running for a few
/// commits, and checks what it reported and what recovery finds. `post`: `cut` is where a
/// committing step ends; that step runs to completion and the failing operation is a staging
/// write right after its publication.
pub(crate) fn fail_at(
    transitions: &[Op],
    bounds: &[u64],
    cut: u64,
    lossy: bool,
    continue_live: bool,
    post: bool,
) {
    let i = if post {
        match bounds.iter().position(|b| *b >= cut) {
            Some(i) if bounds[i] == cut => i,
            _ => return,
        }
    } else {
        match bounds.iter().position(|b| *b > cut) {
            Some(i) => i,
            None => return,
        }
    };
    let mut e = Some(fresh_engine());
    let created = sim_of(e.as_ref().unwrap()).ops();
    if lossy {
        sim_of(e.as_ref().unwrap()).lossy_sync_failures();
    }
    sim_of(e.as_ref().unwrap()).fail_nth_op(cut + 1);
    let shifted: Vec<u64> = bounds.iter().map(|b| b + created).collect();
    let before = replay_until(transitions, &shifted, i, &mut e);
    let mut e = e.unwrap();
    let after = Model::apply(before.clone(), &transitions[i]);
    count(&format!(
        "failure {}: {}",
        if post { "after" } else { "in" },
        op_name(&transitions[i])
    ));
    let prior = unbound_staged_space(&e, &after.action);
    let bound = bound_stagings(&e, &after.action);
    let (request, got) = in_flight(&mut e, &after.action);
    // The stagings the step bound are consumed, owned by an uncertain publication or still
    // staged; nothing after the failure may bind them again.
    for k in bound.into_iter().rev() {
        e.staged.remove(k);
    }
    if post {
        let staged = e
            .store
            .as_ref()
            .map(|s| s.stage_content(&mut &b"staged after a publication"[..]));
        match staged {
            Some(Err(StorageError::Io(_))) => count("staging failed after a publication"),
            other => panic!(
                "the operation after step {i} was to fail: staging returned {:?}",
                other.map(|r| r.map(|s| s.content))
            ),
        }
    }
    let mut allowed = Allowed::around(&before, &after, request.clone());
    if !continue_live {
        // Live commits after the failure may bind those stagings.
        allowed.prior = prior;
    }

    // Only a commit that reported a failed sync (its data barrier) or an uncertain publication
    // may fence later writes. A failed page or data write rejects that commit, and the store
    // keeps committing.
    let failed = failed_op(&e);
    count(&format!(
        "failed operation: {}",
        failed.unwrap_or("none reached")
    ));
    let mut live = Live::default();
    if let (Some(req), Some(Err(err))) = (&request, &got) {
        match err {
            CommitError::Indeterminate(_) => {
                live.fence_allowed = true;
                live.uncertain = Some(req.attempt.clone());
            }
            CommitError::Rejected(StorageError::Io(m)) => {
                assert!(
                    failed.is_some(),
                    "the step was refused with an I/O error ({m}), but no injected failure was hit"
                );
                live.fence_allowed = failed == Some("sync");
            }
            _ => {}
        }
    }

    // What the live store says about the failed attempt narrows the allowed states.
    if let (Some(req), Some(got), None) = (&request, &got, allowed.retry) {
        count(match got {
            Ok(_) => "failed step acknowledged",
            Err(CommitError::Indeterminate(_)) => "failed step indeterminate",
            Err(_) => "failed step refused",
        });
        match got {
            Ok(r) => {
                let (_, p) = allowed.with.as_ref().unwrap_or_else(|| {
                    panic!("acknowledged {r:?}, but the model expects no commit")
                });
                assert_eq!(r.position.0, *p, "receipt position");
                allowed.without = None;
            }
            Err(CommitError::Indeterminate(_)) => {}
            Err(err) => {
                // A definitive refusal: the I/O error of the failure itself, or one the model
                // predicts for this action (from its held basis, when it had one). Nothing of the
                // attempt may ever appear.
                let expected = match &after.action {
                    Action::Commit { expected, .. } => expected.clone(),
                    _ => branch_expect(&before, req).0,
                };
                assert!(
                    matches!(err, CommitError::Rejected(StorageError::Io(_)))
                        || refusal_fits(err, &expected, false, req, &Live::default()),
                    "the failed step was refused with {err:?}; the model predicts {expected:?}"
                );
                allowed.with = None;
            }
        }
        match e
            .store()
            .outcome(&req.attempt)
            .unwrap_or_else(|e| panic!("the store failed: outcome: {e:?}"))
        {
            AttemptOutcome::Absent => {
                count("live outcome absent");
                allowed.with = None;
            }
            AttemptOutcome::Committed(r) => {
                count("live outcome committed");
                let (_, p) = allowed
                    .with
                    .as_ref()
                    .unwrap_or_else(|| panic!("live outcome {r:?}, but it cannot have committed"));
                assert_eq!(r.position.0, *p);
                allowed.without = None;
            }
            AttemptOutcome::Unknown => count("live outcome unknown"),
            AttemptOutcome::Pending => panic!("a resolved attempt is reported pending"),
        }
        assert!(
            allowed.without.is_some() || allowed.with.is_some(),
            "the live answers for {:?} contradict each other ({got:?})",
            req.attempt
        );
    }

    if continue_live {
        // The live commits are predicted without held views (`branch_expect`): release them.
        e.held.clear();
        // Content staged before the failure is committed first: a failed barrier may have lost it.
        // The handle is the one whose content is the model's last staged bytes.
        let prestaged = before.staged.last().and_then(|bytes| {
            let want = content_ref(bytes);
            let k = e.staged.iter().position(|s| s.handle.content == want)?;
            Some((e.staged.remove(k).handle, bytes.clone()))
        });
        if let Some((handle, bytes)) = prestaged {
            let probe = Request {
                attempt: attempt("after-failure-probe"),
                changes: vec![Change::PutContext {
                    id: ctx_id(0),
                    expect: Precondition::Any,
                    body: b"probe after an I/O failure".to_vec(),
                    content: Some(0),
                }],
                contents: vec![bytes],
                aliases: Vec::new(),
                from_staged: Vec::new(),
                evidence: Vec::new(),
                removal: RemovalMode::PreserveViews,
            };
            if let Some(got) = submit_tolerant(&mut e, None, &probe, Some(handle)) {
                count("probe of content staged before the failure");
                live_result(&mut allowed, &probe, &got, &live);
                if got.is_ok() {
                    audit(&e, "the probe commit after the failure");
                }
            }
        }
        let mut m = after.clone();
        let mut steps = 0;
        for op in &transitions[i + 1..] {
            m = Model::apply(m, op);
            let req = match &m.action {
                Action::Commit { request, .. } | Action::Retry { request, .. } => request.clone(),
                _ => continue,
            };
            let bound = bound_stagings(&e, &m.action);
            if let Some(got) = submit_tolerant(&mut e, None, &req, None) {
                if got.is_ok() {
                    for k in bound.into_iter().rev() {
                        e.staged.remove(k);
                    }
                }
                live_result(&mut allowed, &req, &got, &live);
                if got.is_ok() {
                    audit(&e, "a live commit after the failure");
                }
            }
            steps += 1;
            if steps == LIVE_STEPS {
                break;
            }
        }
    }
    let sim = sim_of(&e).clone();
    e.held.clear();
    e.staged.clear();
    drop(e);
    check_recovery(sim.after_process_exit(), &allowed);
    for p in [Persist::None, Persist::All, Persist::Seeded(1)] {
        count("recoveries");
        check_recovery(sim.after_crash(p), &allowed);
    }
}

/// The operation spans `(start, end)` of the steps that commit (and of reopens, when
/// `reopen`) and perform mutating I/O.
pub(crate) fn step_spans(transitions: &[Op], bounds: &[u64], reopen: bool) -> Vec<(u64, u64)> {
    transitions
        .iter()
        .enumerate()
        .filter(|(_, op)| match op {
            Op::Commit(_) | Op::CommitFromHeld { .. } | Op::SubmitAndPoll(_) | Op::Retry { .. } => {
                true
            }
            Op::Reopen => reopen,
            _ => false,
        })
        .map(|(i, _)| (if i == 0 { 0 } else { bounds[i - 1] }, bounds[i]))
        .filter(|(a, b)| b > a)
        .collect()
}

/// Ends of committing steps after whose publication an operation fails: all of them, or `n / 2`
/// spread over the sequence when sampling `n` cuts.
pub(crate) fn publication_ends(
    transitions: &[Op],
    bounds: &[u64],
    sampled: Option<u64>,
) -> Vec<u64> {
    let ends: Vec<u64> = step_spans(transitions, bounds, false)
        .into_iter()
        .map(|(_, b)| b)
        .collect();
    match sampled {
        None => ends,
        Some(n) => {
            let k = (n / 2).max(1) as usize;
            let stride = ends.len().div_ceil(k).max(1);
            ends.into_iter().step_by(stride).collect()
        }
    }
}

/// Seeded cuts: half uniform over the sequence, half among the last three operations of a
/// randomly chosen step that commits.
pub(crate) fn sampled_cuts(transitions: &[Op], bounds: &[u64], n: u64, seed: u64) -> Vec<u64> {
    let total = bounds.last().copied().unwrap_or(0);
    let mut r = seed ^ total.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
    let mut next = || {
        r ^= r << 13;
        r ^= r >> 7;
        r ^= r << 17;
        r
    };
    let tails = step_spans(transitions, bounds, true);
    (0..n)
        .map(|k| {
            if k % 2 == 1 && !tails.is_empty() {
                let (a, b) = tails[(next() % tails.len() as u64) as usize];
                b - 1 - (next() % (b - a).min(3))
            } else {
                next() % total.max(1)
            }
        })
        .collect()
}

// ---------------------------------------------------------------------------------------------
// Operation sequences from bytes: the `ops` fuzz target.
//
// The first byte selects how the sequence is run: to completion, or interrupted once like the
// crash and I/O-failure campaigns (a crash after a mutating operation, a failing operation with
// sync failures lossy or not, or a failing staging write right after a publication), with the
// operation chosen by the next two bytes. The rest is a sequence of at most `BYTE_OPS` steps: a
// tag byte picks the step with the weights of the mixed generator, and its fields follow, each one
// byte drawn from the same ranges. Exhausted input reads as zeros and ends the sequence at the
// next tag. Every step runs the same checks as the generated sequences.
// ---------------------------------------------------------------------------------------------

/// Most steps one input runs.
pub(crate) const BYTE_OPS: usize = 64;

/// How an input's sequence is run.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ByteRun {
    Complete,
    /// A crash after the selected mutating operation (modulo the sequence's total).
    Crash {
        cut: u16,
    },
    /// The operation after the selected one fails, then the store keeps running (`live`).
    Failure {
        cut: u16,
        lossy: bool,
        live: bool,
    },
    /// A staging write right after the selected publication fails.
    AfterPublication {
        pick: u16,
        live: bool,
    },
}

struct ByteReader<'a>(&'a [u8]);

impl ByteReader<'_> {
    fn next(&mut self) -> Option<u8> {
        let (b, rest) = self.0.split_first()?;
        self.0 = rest;
        Some(*b)
    }

    fn byte(&mut self) -> u8 {
        self.next().unwrap_or(0)
    }

    fn u16(&mut self) -> u16 {
        u16::from_le_bytes([self.byte(), self.byte()])
    }

    fn below(&mut self, n: usize) -> u8 {
        (self.byte() as usize % n) as u8
    }

    fn expect(&mut self) -> ExpectSpec {
        match self.byte() % 12 {
            0..=5 => ExpectSpec::Any,
            6 | 7 => ExpectSpec::Absent,
            8..=10 => ExpectSpec::Current,
            _ => ExpectSpec::Wrong,
        }
    }

    fn content_spec(&mut self) -> ContentSpec {
        ContentSpec {
            seed: self.below(12),
            size: self.byte(),
        }
    }

    fn content_pick(&mut self) -> ContentPick {
        match self.byte() % 7 {
            0..=3 => ContentPick::Fresh(self.content_spec()),
            4 => ContentPick::SameStaging(self.byte()),
            _ => ContentPick::Staged(self.byte()),
        }
    }

    fn change(&mut self) -> ChangeSpec {
        let n = CONTEXTS.len();
        match self.byte() % 13 {
            0..=4 => ChangeSpec::PutContext {
                ctx: self.below(n),
                expect: self.expect(),
                body: self.byte(),
                content: (self.byte() % 10 < 4).then(|| self.content_pick()),
            },
            5 | 6 => ChangeSpec::RemoveContext {
                ctx: self.below(n),
                expect: self.expect(),
            },
            7..=10 => ChangeSpec::PutRelationship {
                rel: self.below(RELATIONSHIPS),
                expect: self.expect(),
                kind: self.below(KINDS.len()),
                source: self.below(n),
                target: self.below(n),
            },
            _ => ChangeSpec::RemoveRelationship {
                rel: self.below(RELATIONSHIPS),
                expect: self.expect(),
            },
        }
    }

    fn read(&mut self) -> ReadSpec {
        let n = CONTEXTS.len();
        match self.byte() % 6 {
            0 | 1 => ReadSpec::Context(self.below(n)),
            2..=4 => ReadSpec::Relationships {
                from: self.below(n),
                outgoing: self.byte() & 1 == 1,
                kind: match self.byte() as usize % (KINDS.len() + 1) {
                    0 => None,
                    k => Some((k - 1) as u8),
                },
                page: self.below(4),
            },
            _ => ReadSpec::Content(self.below(n)),
        }
    }

    fn reads(&mut self, most: usize) -> Vec<ReadSpec> {
        (0..self.byte() as usize % (most + 1))
            .map(|_| self.read())
            .collect()
    }

    fn request(&mut self) -> RequestSpec {
        RequestSpec {
            changes: (0..1 + self.byte() as usize % 3)
                .map(|_| self.change())
                .collect(),
            evidence: (0..self.byte() as usize % 3).map(|_| self.byte()).collect(),
            quiesce: self.byte().is_multiple_of(10),
            reads: self.reads(2),
        }
    }

    fn op(&mut self, tag: u8) -> Op {
        match tag % OP_WEIGHTS {
            0..=11 => Op::Commit(self.request()),
            12..=16 => Op::CommitFromHeld {
                slot: self.byte(),
                request: self.request(),
            },
            17 | 18 => Op::SubmitAndPoll(self.request()),
            19 | 20 => Op::Retry {
                pick: self.byte(),
                identical: self.byte() & 1 == 1,
            },
            21..=24 => Op::Hold(self.reads_nonempty(3)),
            25 | 26 => Op::ReadHeld {
                slot: self.byte(),
                reads: self.reads_nonempty(2),
            },
            27 => Op::Release(self.byte()),
            28 | 29 => Op::Stage(self.content_spec()),
            30 | 31 => Op::Discard {
                pick: self.byte(),
                refused: self.byte() % 10 < 3,
            },
            32..=34 => Op::Check,
            _ => Op::Reopen,
        }
    }

    fn reads_nonempty(&mut self, most: usize) -> Vec<ReadSpec> {
        (0..1 + self.byte() as usize % most)
            .map(|_| self.read())
            .collect()
    }
}

/// The sum of the step weights (the tag ranges in `ByteReader::op`).
const OP_WEIGHTS: u8 = 36;

/// Decodes an input into how to run it and its steps.
pub(crate) fn ops_from_bytes(data: &[u8]) -> (ByteRun, Vec<Op>) {
    let mut r = ByteReader(data);
    let mode = r.byte();
    let live = mode & 8 != 0;
    let run = match mode % 8 {
        0..=3 => ByteRun::Complete,
        4 => ByteRun::Crash { cut: r.u16() },
        5 | 6 => ByteRun::Failure {
            cut: r.u16(),
            lossy: mode % 8 == 6,
            live,
        },
        _ => ByteRun::AfterPublication {
            pick: r.u16(),
            live,
        },
    };
    let mut ops = Vec::new();
    while ops.len() < BYTE_OPS {
        let Some(tag) = r.next() else { break };
        ops.push(r.op(tag));
    }
    (run, ops)
}

/// Encodes a run and steps as an input that `ops_from_bytes` decodes to the same (for seeds).
pub(crate) fn ops_to_bytes(run: ByteRun, ops: &[Op]) -> Vec<u8> {
    let mut out = Vec::new();
    let live = |l: bool| if l { 8 } else { 0 };
    match run {
        ByteRun::Complete => out.push(0),
        ByteRun::Crash { cut } => {
            out.push(4);
            out.extend(cut.to_le_bytes());
        }
        ByteRun::Failure {
            cut,
            lossy,
            live: l,
        } => {
            out.push(if lossy { 6 } else { 5 } | live(l));
            out.extend(cut.to_le_bytes());
        }
        ByteRun::AfterPublication { pick, live: l } => {
            out.push(7 | live(l));
            out.extend(pick.to_le_bytes());
        }
    }
    for op in ops.iter().take(BYTE_OPS) {
        put_op(&mut out, op);
    }
    out
}

fn put_expect(out: &mut Vec<u8>, e: ExpectSpec) {
    out.push(match e {
        ExpectSpec::Any => 0,
        ExpectSpec::Absent => 6,
        ExpectSpec::Current => 8,
        ExpectSpec::Wrong => 11,
    });
}

fn put_content_spec(out: &mut Vec<u8>, c: ContentSpec) {
    out.extend([c.seed, c.size]);
}

fn put_change(out: &mut Vec<u8>, c: &ChangeSpec) {
    match c {
        ChangeSpec::PutContext {
            ctx,
            expect,
            body,
            content,
        } => {
            out.push(0);
            out.push(*ctx);
            put_expect(out, *expect);
            out.push(*body);
            match content {
                None => out.push(9),
                Some(p) => {
                    out.push(0);
                    match p {
                        ContentPick::Fresh(s) => {
                            out.push(0);
                            put_content_spec(out, *s);
                        }
                        ContentPick::SameStaging(k) => out.extend([4, *k]),
                        ContentPick::Staged(k) => out.extend([5, *k]),
                    }
                }
            }
        }
        ChangeSpec::RemoveContext { ctx, expect } => {
            out.extend([5, *ctx]);
            put_expect(out, *expect);
        }
        ChangeSpec::PutRelationship {
            rel,
            expect,
            kind,
            source,
            target,
        } => {
            out.extend([7, *rel]);
            put_expect(out, *expect);
            out.extend([*kind, *source, *target]);
        }
        ChangeSpec::RemoveRelationship { rel, expect } => {
            out.extend([11, *rel]);
            put_expect(out, *expect);
        }
    }
}

fn put_read(out: &mut Vec<u8>, r: &ReadSpec) {
    match r {
        ReadSpec::Context(c) => out.extend([0, *c]),
        ReadSpec::Relationships {
            from,
            outgoing,
            kind,
            page,
        } => out.extend([
            2,
            *from,
            u8::from(*outgoing),
            kind.map_or(0, |k| k + 1),
            *page,
        ]),
        ReadSpec::Content(c) => out.extend([5, *c]),
    }
}

fn put_request(out: &mut Vec<u8>, r: &RequestSpec) {
    out.push(r.changes.len().clamp(1, 3) as u8 - 1);
    for c in r.changes.iter().take(3) {
        put_change(out, c);
    }
    out.push(r.evidence.len().min(2) as u8);
    out.extend(r.evidence.iter().take(2));
    out.push(if r.quiesce { 0 } else { 1 });
    out.push(r.reads.len().min(2) as u8);
    for x in r.reads.iter().take(2) {
        put_read(out, x);
    }
}

fn put_reads_nonempty(out: &mut Vec<u8>, reads: &[ReadSpec], most: usize) {
    let n = reads.len().clamp(1, most);
    out.push(n as u8 - 1);
    for x in reads.iter().take(n) {
        put_read(out, x);
    }
    // An empty list cannot be encoded: it reads as one context read.
    if reads.is_empty() {
        put_read(out, &ReadSpec::Context(0));
    }
}

fn put_op(out: &mut Vec<u8>, op: &Op) {
    match op {
        Op::Commit(r) => {
            out.push(0);
            put_request(out, r);
        }
        Op::CommitFromHeld { slot, request } => {
            out.extend([12, *slot]);
            put_request(out, request);
        }
        Op::SubmitAndPoll(r) => {
            out.push(17);
            put_request(out, r);
        }
        Op::Retry { pick, identical } => out.extend([19, *pick, u8::from(*identical)]),
        Op::Hold(reads) => {
            out.push(21);
            put_reads_nonempty(out, reads, 3);
        }
        Op::ReadHeld { slot, reads } => {
            out.extend([25, *slot]);
            put_reads_nonempty(out, reads, 2);
        }
        Op::Release(s) => out.extend([27, *s]),
        Op::Stage(c) => {
            out.push(28);
            put_content_spec(out, *c);
        }
        Op::Discard { pick, refused } => out.extend([30, *pick, if *refused { 0 } else { 9 }]),
        Op::Check => out.push(32),
        Op::Reopen => out.push(35),
    }
}

/// The `ops` fuzz target: decodes `data` and runs its sequence against the model, completely or
/// interrupted once; any disagreement panics.
pub(crate) fn run_bytes(data: &[u8]) {
    let (run, ops) = ops_from_bytes(data);
    if run == ByteRun::Complete {
        let mut m = Model::default();
        let mut e = Engine::init(false);
        for op in ops {
            m = Model::apply(m, &op);
            e = Engine::apply(e, &m, op);
        }
        Engine::teardown(e, m);
        return;
    }
    let bounds = boundaries(&ops);
    let total = bounds.last().copied().unwrap_or(0);
    if total == 0 {
        return;
    }
    match run {
        ByteRun::Complete => unreachable!("handled above"),
        ByteRun::Crash { cut } => crash_at(&ops, &bounds, u64::from(cut) % total),
        ByteRun::Failure { cut, lossy, live } => {
            fail_at(&ops, &bounds, u64::from(cut) % total, lossy, live, false)
        }
        ByteRun::AfterPublication { pick, live } => {
            let ends = publication_ends(&ops, &bounds, None);
            if let Some(&cut) = ends.get(usize::from(pick) % ends.len().max(1)) {
                fail_at(&ops, &bounds, cut, false, live, true);
            }
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Exact regressions: shrunk counterexamples, replayed by the model tests and saved as seeds of
// the `ops` fuzz target.
// ---------------------------------------------------------------------------------------------

pub(crate) fn put(ctx: u8, body: u8) -> ChangeSpec {
    ChangeSpec::PutContext {
        ctx,
        expect: ExpectSpec::Any,
        body,
        content: None,
    }
}

pub(crate) fn remove(ctx: u8) -> ChangeSpec {
    ChangeSpec::RemoveContext {
        ctx,
        expect: ExpectSpec::Any,
    }
}

pub(crate) fn plain(changes: Vec<ChangeSpec>) -> RequestSpec {
    RequestSpec {
        changes,
        evidence: Vec::new(),
        quiesce: false,
        reads: Vec::new(),
    }
}

/// A removal leaves a small leaf beside a sibling too large to merge with.
pub(crate) fn merge_skip_sequence() -> Vec<Op> {
    vec![
        Op::Commit(plain(vec![put(2, 3), put(6, 3), put(8, 0), put(9, 3)])),
        Op::Commit(plain(vec![
            put(2, 0),
            ChangeSpec::PutRelationship {
                rel: 0,
                expect: ExpectSpec::Any,
                kind: 0,
                source: 0,
                target: 3,
            },
        ])),
        Op::CommitFromHeld {
            slot: 126,
            request: plain(vec![put(0, 227), remove(2), put(4, 114), put(7, 151)]),
        },
        Op::Commit(plain(vec![put(4, 239)])),
        Op::Commit(plain(vec![put(2, 142)])),
        Op::CommitFromHeld {
            slot: 118,
            request: plain(vec![remove(0), put(1, 75), remove(7)]),
        },
    ]
}

/// One commit binds the same staging twice (two records, one handle).
pub(crate) fn double_binding_sequence() -> Vec<Op> {
    vec![Op::Commit(plain(vec![
        ChangeSpec::PutContext {
            ctx: 0,
            expect: ExpectSpec::Any,
            body: 0,
            content: Some(ContentPick::Fresh(ContentSpec { seed: 0, size: 0 })),
        },
        ChangeSpec::PutContext {
            ctx: 1,
            expect: ExpectSpec::Any,
            body: 0,
            content: Some(ContentPick::SameStaging(0)),
        },
    ]))]
}
