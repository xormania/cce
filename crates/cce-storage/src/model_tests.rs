//! Generated operation sequences checked against an independent logical model.
//!
//! The model below knows the logical contract only: records, relationships navigated in both
//! directions, content bytes, retained history, attempt outcomes, read-basis conflicts and view
//! isolation. It never calls the engine. Each generated step is resolved by the model into a
//! concrete request and the result it predicts; the engine (production code on the simulator,
//! and in a smaller run on real files)
//! executes the same request and every observation is compared.
//!
//! Budget: `CCE_MODEL_CASES` sequences on the simulator (default 32) and `CCE_MODEL_FS_CASES` on
//! real files (default 4), each of 64 to 80 attempted operations, from the fixed seed
//! `CCE_MODEL_SEED` (default [`ROUTINE_SEED`]). Larger campaigns set these variables; each run
//! prints one `model report:` JSON line with its seed, budget, executed counts and the operations
//! and outcomes it exercised. Minimized failures are saved under `proptest-regressions/` and
//! replayed first on later runs.

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
use proptest::prelude::*;
use proptest::test_runner::{Config, FileFailurePersistence, RngSeed, TestRunner};
use proptest_state_machine::{ReferenceStateMachine, StateMachineTest};
use sha2::{Digest, Sha256};

use crate::format::{PAGE_SIZE, StoreId};
use crate::io::{FileId, FsIo, Io};
use crate::sim::{Persist, SimIo};
use crate::test_support::resources::TestDir;
use crate::test_support::{attempt, cid, create_on, open_on};
use crate::{NativeStore, NativeView};

/// Seed of the routine run. Changing it changes the generated sequences.
const ROUTINE_SEED: u64 = 0x5eed_0001_cce0_0001;
const ROUTINE_CASES: u32 = 32;
const ROUTINE_FS_CASES: u32 = 4;
const MIN_OPS: usize = 64;
const MAX_OPS: usize = 80;

const STORE: StoreId = StoreId(*b"model-test-store");
/// Context identities; "a" and "ab" share a prefix on purpose.
const CONTEXTS: [&str; 10] = ["a", "ab", "b", "c", "d", "e", "f", "g", "h", "i"];
const KINDS: [&str; 3] = ["k", "kk", "x"];
const RELATIONSHIPS: usize = 8;
const MAX_HELD: usize = 3;

// ---------------------------------------------------------------------------------------------
// Independent model. Uses cce-core types and SHA-256 only.
// ---------------------------------------------------------------------------------------------

fn ctx_id(i: u8) -> ContextId {
    cid(CONTEXTS[i as usize % CONTEXTS.len()])
}

fn rel_id(i: u8) -> RelationshipId {
    RelationshipId::new(format!("r{}", i as usize % RELATIONSHIPS).into_bytes()).unwrap()
}

fn kind(i: u8) -> RelationshipKind {
    RelationshipKind::new(KINDS[i as usize % KINDS.len()].as_bytes().to_vec()).unwrap()
}

/// The content a put binds: freshly staged bytes, or the same staging handle as another binding of
/// the request (the `n`-th fresh one, which may come before or after it).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ContentPick {
    Fresh(ContentSpec),
    SameStaging(u8),
    /// A handle staged earlier and still staged (by a stage step or a rejected commit); the
    /// `n`-th one at the time the request is made.
    Staged(u8),
}

/// Content bytes for a generated content choice: mostly small, sometimes several KiB, rarely
/// larger than one streaming segment.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ContentSpec {
    seed: u8,
    size: u8,
}

impl ContentSpec {
    fn bytes(self) -> Vec<u8> {
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

fn content_ref(bytes: &[u8]) -> ContentRef {
    ContentRef {
        digest: ContentDigest(Sha256::digest(bytes).into()),
        len: bytes.len() as u64,
    }
}

#[derive(Clone, Copy, Debug)]
enum ExpectSpec {
    Any,
    Absent,
    /// The record's current revision (or `Absent` when there is none).
    Current,
    /// A revision the record does not have.
    Wrong,
}

#[derive(Clone, Debug)]
enum ChangeSpec {
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
enum ReadSpec {
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
struct RequestSpec {
    changes: Vec<ChangeSpec>,
    evidence: Vec<u8>,
    quiesce: bool,
    reads: Vec<ReadSpec>,
}

/// One generated step.
#[derive(Clone, Debug)]
enum Op {
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
struct Rel {
    revision: u64,
    kind: RelationshipKind,
    source: ContextId,
    target: ContextId,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Record {
    revision: u64,
    body: Vec<u8>,
    content: Option<ContentRef>,
}

#[derive(Clone, Debug, Default)]
struct Logical {
    contexts: BTreeMap<ContextId, Record>,
    rels: BTreeMap<RelationshipId, Rel>,
}

/// A logical key a commit writes and a read can observe.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum WriteKey {
    Context(ContextId),
    /// A directional index entry: (outgoing?, endpoint, kind).
    Index(bool, ContextId, RelationshipKind),
}

#[derive(Clone, Debug)]
enum Observation {
    Point(ContextId),
    Range(bool, ContextId, Option<RelationshipKind>),
}

impl Observation {
    fn covers(&self, k: &WriteKey) -> bool {
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
struct HeldModel {
    basis: u64,
    snapshot: Logical,
    observed: Vec<Observation>,
}

/// What a read returns, as predicted.
#[derive(Clone, Debug, PartialEq, Eq)]
enum ReadResult {
    Context(ContextId, Option<ContextRecord>),
    Relationships {
        query: RelationshipQuery,
        page: usize,
        expected: Vec<Relationship>,
    },
    Content(Option<(ContentRef, Arc<Vec<u8>>)>),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum ErrKind {
    Conflict,
    Rejected,
    Mismatch,
    ReadersActive,
}

#[derive(Clone, Debug)]
enum Expected {
    Committed(u64),
    Failed(BTreeSet<ErrKind>),
}

/// A request resolved to concrete values.
#[derive(Clone, Debug)]
struct Request {
    attempt: AttemptId,
    changes: Vec<Change>,
    contents: Vec<Arc<Vec<u8>>>,
    /// Per binding: `Some(i)` binds the same staging handle as binding `i` (missing: none).
    aliases: Vec<Option<usize>>,
    /// Per binding: `Some(k)` binds the `k`-th handle still staged when the request was made
    /// (missing: none). A rejected commit leaves it staged; a committed one consumes it.
    from_staged: Vec<Option<usize>>,
    evidence: Vec<Evidence>,
    removal: RemovalMode,
}

impl Request {
    fn alias(&self, j: usize) -> Option<usize> {
        self.aliases.get(j).copied().flatten()
    }

    fn staged_pick(&self, j: usize) -> Option<usize> {
        self.from_staged.get(j).copied().flatten()
    }

    /// The bytes of each staging the request makes itself, in binding order.
    fn stagings(&self) -> Vec<Arc<Vec<u8>>> {
        (0..self.contents.len())
            .filter(|j| self.alias(*j).is_none() && self.staged_pick(*j).is_none())
            .map(|j| self.contents[j].clone())
            .collect()
    }

    /// The distinct earlier stagings it binds, by their index among the staged handles.
    fn staged_picks(&self) -> BTreeSet<usize> {
        (0..self.contents.len())
            .filter_map(|j| self.staged_pick(j))
            .collect()
    }

    fn refs(&self) -> Vec<ContentRef> {
        self.contents.iter().map(|b| content_ref(b)).collect()
    }
}

/// The concrete step the engine must execute, with the predicted results.
#[derive(Clone, Debug, Default)]
enum Action {
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
struct Snapshot {
    logical: Logical,
    history: Vec<HistoryEntry>,
    outcomes: Vec<(AttemptId, AttemptOutcome)>,
    /// Content history names but no record uses: readable exactly, or no longer retained.
    unreferenced: Vec<(ContentRef, Arc<Vec<u8>>)>,
    bytes: BTreeMap<[u8; 32], Arc<Vec<u8>>>,
}

#[derive(Clone, Debug, Default)]
struct Model {
    state: Logical,
    position: u64,
    history: Vec<HistoryEntry>,
    /// Committed attempts in commit order with their requests (for retries).
    committed: Vec<(Request, u64)>,
    /// Attempt identities used without committing.
    uncommitted: BTreeSet<AttemptId>,
    next_attempt: u64,
    /// Every content byte stream ever committed, by digest.
    bytes: BTreeMap<[u8; 32], Arc<Vec<u8>>>,
    /// Logical keys written by each commit since the last reopen.
    log: Vec<(u64, Vec<WriteKey>)>,
    held: Vec<HeldModel>,
    /// Staged handles that can still be discarded, in the engine's order.
    staged: Vec<Arc<Vec<u8>>>,
    /// Handles storage must refuse to discard (committed, or issued by an earlier open).
    refused: usize,
    action: Action,
}

impl Logical {
    fn context(&self, id: &ContextId) -> Option<ContextRecord> {
        self.contexts.get(id).map(|r| ContextRecord {
            id: id.clone(),
            revision: Revision(r.revision),
            body: r.body.clone(),
            content: r.content,
        })
    }

    /// Relationships in result order: (kind, other endpoint, id).
    fn query(
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

    fn revision_of(&self, key: &WriteKey) -> Option<u64> {
        match key {
            WriteKey::Context(id) => self.contexts.get(id).map(|r| r.revision),
            WriteKey::Index(..) => None,
        }
    }
}

impl Model {
    fn read(
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

    fn resolve_expect(&self, e: ExpectSpec, current: Option<u64>) -> Precondition {
        match e {
            ExpectSpec::Any => Precondition::Any,
            ExpectSpec::Absent => Precondition::Absent,
            ExpectSpec::Current => current.map_or(Precondition::Absent, |r| {
                Precondition::AtRevision(Revision(r))
            }),
            ExpectSpec::Wrong => Precondition::AtRevision(Revision(current.unwrap_or(0) + 1000)),
        }
    }

    fn fresh_attempt(&mut self) -> AttemptId {
        self.next_attempt += 1;
        attempt(&format!("attempt-{}", self.next_attempt))
    }

    fn resolve(&mut self, spec: &RequestSpec) -> Request {
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
    fn predict(&self, req: &Request, held: Option<&HeldModel>) -> (Expected, Vec<WriteKey>) {
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
    fn apply_commit(&mut self, req: &Request, written: Vec<WriteKey>) {
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

    fn snapshot(&self) -> Snapshot {
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

    fn commit_step(&mut self, spec: &RequestSpec, held_slot: Option<usize>, poll: bool) {
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

struct ModelMachine;

fn expect_spec() -> impl Strategy<Value = ExpectSpec> {
    prop_oneof![
        6 => Just(ExpectSpec::Any),
        2 => Just(ExpectSpec::Absent),
        3 => Just(ExpectSpec::Current),
        1 => Just(ExpectSpec::Wrong),
    ]
}

/// A record body: mostly short, sometimes large enough that a few records fill a page, so leaves
/// split and removals shrink a leaf next to a large sibling (the tree's merge paths).
fn body_bytes(body: u8, id: &ContextId) -> Vec<u8> {
    let mut b = format!("body {body} of {id:?}").into_bytes();
    let pad = match body % 4 {
        3 => 1650,
        2 => 700,
        _ => 0,
    };
    b.resize(b.len() + pad, b'.');
    b
}

fn content_pick() -> impl Strategy<Value = ContentPick> {
    prop_oneof![
        4 => content_spec().prop_map(ContentPick::Fresh),
        1 => any::<u8>().prop_map(ContentPick::SameStaging),
        2 => any::<u8>().prop_map(ContentPick::Staged),
    ]
}

fn content_spec() -> impl Strategy<Value = ContentSpec> {
    (0u8..12, any::<u8>()).prop_map(|(seed, size)| ContentSpec { seed, size })
}

fn change_spec() -> impl Strategy<Value = ChangeSpec> {
    let n = CONTEXTS.len() as u8;
    let r = RELATIONSHIPS as u8;
    let k = KINDS.len() as u8;
    prop_oneof![
        5 => (0..n, expect_spec(), any::<u8>(), proptest::option::weighted(0.4, content_pick()))
            .prop_map(|(ctx, expect, body, content)| ChangeSpec::PutContext { ctx, expect, body, content }),
        2 => (0..n, expect_spec()).prop_map(|(ctx, expect)| ChangeSpec::RemoveContext { ctx, expect }),
        4 => (0..r, expect_spec(), 0..k, 0..n, 0..n)
            .prop_map(|(rel, expect, kind, source, target)| ChangeSpec::PutRelationship { rel, expect, kind, source, target }),
        2 => (0..r, expect_spec()).prop_map(|(rel, expect)| ChangeSpec::RemoveRelationship { rel, expect }),
    ]
}

/// Which transitions the generator draws: the mixed workload, or one that reshapes the tree.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Profile {
    Mixed,
    /// Batches of puts with tiny or page-filling bodies and frequent removals: leaves fill, split
    /// and empty again, so a removal often shrinks a leaf next to a sibling too large to merge
    /// with (the tree's merge paths), between plain reads and reopens.
    TreeShapes,
}

thread_local! {
    // Set by the test that runs the generator; transitions are drawn on that thread.
    static PROFILE: std::cell::Cell<Profile> = const { std::cell::Cell::new(Profile::Mixed) };
}

/// One reshaping request against the current state: distinct contexts (a repeated one would
/// reject the request), removals only of present ones, puts large, medium or tiny (`body_bytes`:
/// a body choice ending in 3, 2 or 0), sometimes one relationship change.
fn shape_request_spec(m: &Model) -> BoxedStrategy<RequestSpec> {
    let present: BTreeSet<u8> = (0..CONTEXTS.len() as u8)
        .filter(|i| m.state.contexts.contains_key(&ctx_id(*i)))
        .collect();
    let all: Vec<u8> = (0..CONTEXTS.len() as u8).collect();
    (
        proptest::sample::subsequence(all, 1..5),
        proptest::collection::vec((any::<u8>(), 0u8..10, any::<bool>()), 5),
        proptest::option::weighted(0.2, change_spec()),
    )
        .prop_map(move |(ctxs, picks, extra)| {
            let mut changes: Vec<ChangeSpec> = ctxs
                .iter()
                .zip(picks)
                .map(|(&ctx, (b, size, remove))| {
                    if remove && present.contains(&ctx) {
                        ChangeSpec::RemoveContext {
                            ctx,
                            expect: ExpectSpec::Any,
                        }
                    } else {
                        ChangeSpec::PutContext {
                            ctx,
                            expect: ExpectSpec::Any,
                            // Large half the time, medium or tiny otherwise.
                            body: (b & !3)
                                | match size {
                                    0..=4 => 3,
                                    5..=7 => 2,
                                    _ => 0,
                                },
                            content: None,
                        }
                    }
                })
                .collect();
            changes.extend(extra.filter(|c| {
                matches!(
                    c,
                    ChangeSpec::PutRelationship { .. } | ChangeSpec::RemoveRelationship { .. }
                )
            }));
            RequestSpec {
                changes,
                evidence: Vec::new(),
                quiesce: false,
                reads: Vec::new(),
            }
        })
        .boxed()
}

fn read_spec() -> impl Strategy<Value = ReadSpec> {
    let n = CONTEXTS.len() as u8;
    let k = KINDS.len() as u8;
    prop_oneof![
        2 => (0..n).prop_map(ReadSpec::Context),
        3 => (0..n, any::<bool>(), proptest::option::of(0..k), 0u8..4)
            .prop_map(|(from, outgoing, kind, page)| ReadSpec::Relationships { from, outgoing, kind, page }),
        1 => (0..n).prop_map(ReadSpec::Content),
    ]
}

fn request_spec() -> impl Strategy<Value = RequestSpec> {
    (
        proptest::collection::vec(change_spec(), 1..4),
        proptest::collection::vec(any::<u8>(), 0..3),
        proptest::bool::weighted(0.1),
        proptest::collection::vec(read_spec(), 0..3),
    )
        .prop_map(|(changes, evidence, quiesce, reads)| RequestSpec {
            changes,
            evidence,
            quiesce,
            reads,
        })
}

impl ReferenceStateMachine for ModelMachine {
    type State = Model;
    type Transition = Op;

    fn init_state() -> BoxedStrategy<Model> {
        Just(Model::default()).boxed()
    }

    fn transitions(m: &Model) -> BoxedStrategy<Op> {
        if PROFILE.with(|p| p.get()) == Profile::TreeShapes {
            return prop_oneof![
                10 => shape_request_spec(m).prop_map(Op::Commit),
                2 => (any::<u8>(), shape_request_spec(m))
                    .prop_map(|(slot, request)| Op::CommitFromHeld { slot, request }),
                2 => proptest::collection::vec(read_spec(), 1..3).prop_map(Op::Hold),
                1 => any::<u8>().prop_map(Op::Release),
                1 => Just(Op::Check),
                1 => Just(Op::Reopen),
            ]
            .boxed();
        }
        prop_oneof![
            12 => request_spec().prop_map(Op::Commit),
            5 => (any::<u8>(), request_spec()).prop_map(|(slot, request)| Op::CommitFromHeld { slot, request }),
            2 => request_spec().prop_map(Op::SubmitAndPoll),
            2 => (any::<u8>(), any::<bool>()).prop_map(|(pick, identical)| Op::Retry { pick, identical }),
            4 => proptest::collection::vec(read_spec(), 1..4).prop_map(Op::Hold),
            2 => (any::<u8>(), proptest::collection::vec(read_spec(), 1..3))
                .prop_map(|(slot, reads)| Op::ReadHeld { slot, reads }),
            1 => any::<u8>().prop_map(Op::Release),
            2 => content_spec().prop_map(Op::Stage),
            2 => (any::<u8>(), proptest::bool::weighted(0.3)).prop_map(|(pick, refused)| Op::Discard { pick, refused }),
            3 => Just(Op::Check),
            1 => Just(Op::Reopen),
        ]
        .boxed()
    }

    fn apply(mut m: Model, op: &Op) -> Model {
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
    static STATS: RefCell<BTreeMap<String, u64>> = const { RefCell::new(BTreeMap::new()) };
}

fn count(what: &str) {
    STATS.with(|s| *s.borrow_mut().entry(what.to_string()).or_default() += 1);
}

fn op_name(op: &Op) -> &'static str {
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
enum Point {
    Barrier,
    Publish,
    Apply,
}

/// A pause armed at one point: the paused operation signals `reached` and waits for `go`.
struct Gate {
    point: Point,
    reached: std::sync::mpsc::Sender<()>,
    go: std::sync::mpsc::Receiver<()>,
}

/// What the harness observes of the simulator underneath the store, independently of the store's
/// own bookkeeping: the kind of operation an injected failure hit, every content-file write (so a
/// staging's space is known from the bytes it wrote), and an optional pause.
#[derive(Default)]
struct Tap {
    failed: Mutex<Option<&'static str>>,
    writes: Mutex<Vec<(u64, Vec<u8>)>>,
    gate: Mutex<Option<Gate>>,
    /// A publication record was written and its sync has not returned yet.
    publishing: std::sync::atomic::AtomicBool,
}

impl Tap {
    fn pause_at(&self, point: Point) {
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
struct Watched {
    sim: SimIo,
    tap: Arc<Tap>,
}

impl Watched {
    fn note<T>(&self, kind: &'static str, r: Result<T, StorageError>) -> Result<T, StorageError> {
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
enum Backend {
    Sim(SimIo, Arc<Tap>),
    /// A fresh directory on the real filesystem from the shared directory owner: removed when a
    /// sequence passes, kept (and its path printed) when a failing case unwinds through it.
    Files(TestDir),
}

impl Backend {
    fn new(files: bool) -> Self {
        if files {
            Backend::Files(TestDir::new("cce-model"))
        } else {
            Backend::Sim(SimIo::new(), Arc::default())
        }
    }

    /// An adapter for the next create or open. On the simulator the previous handle's ownership
    /// ends first (its "process" exited); on real files closing the handle released it.
    fn io(&self) -> Box<dyn Io> {
        match self {
            Backend::Sim(sim, tap) => {
                sim.release();
                Box::new(Watched {
                    sim: sim.clone(),
                    tap: tap.clone(),
                })
            }
            Backend::Files(dir) => Box::new(FsIo::new(dir)),
        }
    }
}

struct Engine {
    backend: Backend,
    store: Option<NativeStore>,
    held: Vec<NativeView>,
    staged: Vec<Staged>,
    refused: Vec<StagedContent>,
}

/// A live staging handle and the content-file bytes its staging wrote (observed through the tap,
/// not taken from the store): the space this handle owns until a commit consumes it or it is
/// discarded. Empty on real files.
struct Staged {
    handle: StagedContent,
    owns: Vec<(u64, Vec<u8>)>,
    /// Commits that bound this handle and were rejected.
    rejected: u32,
}

impl Staged {
    fn ranges(&self) -> impl Iterator<Item = (u64, u64)> + '_ {
        self.owns.iter().map(|(o, b)| (*o, b.len() as u64))
    }
}

impl Engine {
    fn store(&self) -> &NativeStore {
        self.store.as_ref().expect("store open")
    }

    fn tap(&self) -> Option<&Arc<Tap>> {
        match &self.backend {
            Backend::Sim(_, tap) => Some(tap),
            Backend::Files(_) => None,
        }
    }

    /// Content-file writes observed since the last call.
    fn take_writes(&self) -> Vec<(u64, Vec<u8>)> {
        self.tap()
            .map(|t| std::mem::take(&mut *t.writes.lock().unwrap()))
            .unwrap_or_default()
    }

    fn stage(&self, bytes: &[u8]) -> Staged {
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
        }
    }

    /// The live handle a binding of `req` names, if it is staged here with the request's bytes
    /// (after a reopen or in a replay that diverged, the request stages those bytes afresh).
    fn earlier(&self, req: &Request, j: usize) -> Option<usize> {
        req.staged_pick(j).filter(|k| {
            self.staged
                .get(*k)
                .is_some_and(|s| s.handle.content == content_ref(&req.contents[j]))
        })
    }

    /// The change set for `req` and the stagings it made itself (earlier stagings it binds stay
    /// in `self.staged`).
    fn change_set(&self, req: &Request) -> (ChangeSet, Vec<Staged>) {
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

fn read_all(r: &mut dyn Read) -> Vec<u8> {
    let mut v = Vec::new();
    r.read_to_end(&mut v)
        .unwrap_or_else(|e| panic!("the store failed: read content: {e:?}"));
    v
}

fn check_read(view: &mut NativeView, expected: &ReadResult) {
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

fn main_check(engine: &Engine, snap: &Snapshot) {
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

fn err_kind(e: &CommitError) -> ErrKind {
    match e {
        CommitError::Conflict(_) => ErrKind::Conflict,
        CommitError::Rejected(_) => ErrKind::Rejected,
        CommitError::AttemptMismatch => ErrKind::Mismatch,
        CommitError::ReadersActive(_) => ErrKind::ReadersActive,
        CommitError::Indeterminate(m) => panic!("no faults are injected, yet indeterminate: {m}"),
        other => panic!("unexpected commit error {other:?}"),
    }
}

fn check_result(got: &Result<CommitReceipt, CommitError>, req: &Request, expected: &Expected) {
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
struct EngineTest<const FILES: bool>;

/// Opens the store again after the previous handle closed.
fn open(backend: &Backend) -> NativeStore {
    open_on(backend.io(), Default::default())
        .unwrap_or_else(|e| panic!("a cleanly closed store must open again: {e:?}"))
}

fn reads_through(view: &mut NativeView, reads: &[ReadResult]) {
    for r in reads {
        check_read(view, r);
    }
}

impl<const FILES: bool> StateMachineTest for EngineTest<FILES> {
    type SystemUnderTest = Engine;
    type Reference = ModelMachine;

    fn init_test(_: &Model) -> Engine {
        let backend = Backend::new(FILES);
        let store = create_on(backend.io(), STORE, Default::default())
            .unwrap_or_else(|e| panic!("creating the store under test: {e:?}"));
        Engine {
            backend,
            store: Some(store),
            held: Vec::new(),
            staged: Vec::new(),
            refused: Vec::new(),
        }
    }

    fn apply(mut e: Engine, m: &Model, op: Op) -> Engine {
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
                let (cs, _staged) = e.change_set(request);
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

    fn teardown(mut e: Engine, m: Model) {
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

fn env_or<T: std::str::FromStr>(name: &str, default: T) -> T {
    std::env::var(name)
        .ok()
        .map(|v| {
            v.parse()
                .unwrap_or_else(|_| panic!("{name} must be a number"))
        })
        .unwrap_or(default)
}

/// Runs one budget of generated sequences and prints its report.
fn run_model<const FILES: bool>(backend: &str, cases: u32, seed: u64) {
    run_profile::<FILES>(backend, Profile::Mixed, cases, seed)
}

fn run_profile<const FILES: bool>(backend: &str, profile: Profile, cases: u32, seed: u64) {
    PROFILE.with(|p| p.set(profile));
    let config = Config {
        cases,
        rng_seed: RngSeed::Fixed(seed),
        failure_persistence: Some(Box::new(FileFailurePersistence::SourceParallel(
            "proptest-regressions",
        ))),
        source_file: Some(file!()),
        ..Config::default()
    };
    STATS.with(|s| s.borrow_mut().clear());
    let mut runner = TestRunner::new(config.clone());
    let attempted = Arc::new(Mutex::new((0u64, 0u64)));
    let counter = attempted.clone();
    let result = runner.run(
        &ModelMachine::sequential_strategy(MIN_OPS..=MAX_OPS),
        move |(initial, transitions, seen)| {
            {
                let mut c = counter.lock().unwrap();
                c.0 += 1;
                c.1 += transitions.len() as u64;
            }
            EngineTest::<FILES>::test_sequential(config.clone(), initial, transitions, seen);
            Ok(())
        },
    );
    let (sequences, ops) = *attempted.lock().unwrap();
    let report = serde_json::json!({
        "backend": backend,
        "profile": match profile {
            Profile::Mixed => "mixed",
            Profile::TreeShapes => "tree shapes",
        },
        "seed": format!("{seed:#x}"),
        "cases": cases,
        "operations_per_sequence": [MIN_OPS, MAX_OPS],
        // Includes shrinking attempts when a case failed.
        "sequences_run": sequences,
        "attempted_operations": ops,
        "passed": result.is_ok(),
        "observed": STATS.with(|s| s.borrow().clone()),
    });
    PROFILE.with(|p| p.set(Profile::Mixed));
    println!("model report: {report}");
    if let Err(e) = result {
        panic!("{e}");
    }
    if !FILES && profile == Profile::Mixed {
        // The staging lifecycle must actually be exercised: a staging a rejected commit bound is
        // committed later, another is discarded, and stagings own space that still carries a
        // retirement record.
        let observed = STATS.with(|s| s.borrow().clone());
        for what in [
            "staging committed after a rejected commit bound it",
            "discard after a rejected commit bound the staging",
            "staging owns space with a retirement record",
        ] {
            assert!(
                observed.get(what).copied().unwrap_or(0) > 0,
                "the model never observed \"{what}\" (budget too small?)"
            );
        }
    }
}

#[test]
fn generated_operation_sequences_match_the_logical_model() {
    run_model::<false>(
        "simulator",
        env_or("CCE_MODEL_CASES", ROUTINE_CASES),
        env_or("CCE_MODEL_SEED", ROUTINE_SEED),
    );
}

// Exact regressions: shrunk counterexamples the generated tests found, replayed step by step
// through the same model, oracle, retirement audit and ownership checks. They are written in the
// generator's terms, so they stay exact only while CONTEXTS and `body_bytes` stay as they are.

fn replay(ops: Vec<Op>) {
    STATS.with(|s| s.borrow_mut().clear());
    let mut m = Model::default();
    let mut e = EngineTest::<false>::init_test(&m);
    for op in ops {
        m = ModelMachine::apply(m, &op);
        e = EngineTest::<false>::apply(e, &m, op);
    }
    EngineTest::<false>::teardown(e, m);
}

fn put(ctx: u8, body: u8) -> ChangeSpec {
    ChangeSpec::PutContext {
        ctx,
        expect: ExpectSpec::Any,
        body,
        content: None,
    }
}

fn remove(ctx: u8) -> ChangeSpec {
    ChangeSpec::RemoveContext {
        ctx,
        expect: ExpectSpec::Any,
    }
}

fn plain(changes: Vec<ChangeSpec>) -> RequestSpec {
    RequestSpec {
        changes,
        evidence: Vec::new(),
        quiesce: false,
        reads: Vec::new(),
    }
}

/// A removal leaves a small leaf beside a sibling too large to merge with: the sibling must not
/// be retired while the published tree still reaches it. Bodies ending in 3 are 1650 bytes
/// longer, in 2 700 bytes, in 0 short (`body_bytes`); "held" commits without a held view run
/// from a fresh one.
#[test]
fn a_removal_beside_a_sibling_too_large_to_merge_retires_nothing_reachable() {
    replay(vec![
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
    ]);
}

/// One commit binds the same staging twice (two records, one handle): its content must be
/// installed once and neither binding's space retired while the records reach it.
#[test]
fn one_staging_bound_twice_in_a_commit_is_installed_once_and_never_retired() {
    replay(vec![Op::Commit(plain(vec![
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
    ]))]);
}

/// Routine tier of the tree-reshaping profile; the retirement audit after every step is what
/// catches space retired while still reachable.
#[test]
fn generated_tree_reshaping_sequences_match_the_logical_model() {
    run_profile::<false>(
        "simulator",
        Profile::TreeShapes,
        env_or("CCE_MODEL_SHAPE_CASES", ROUTINE_CASES),
        env_or("CCE_MODEL_SEED", ROUTINE_SEED),
    );
}

#[test]
fn generated_operation_sequences_on_real_files_match_the_logical_model() {
    run_model::<true>(
        "files",
        env_or("CCE_MODEL_FS_CASES", ROUTINE_FS_CASES),
        env_or("CCE_MODEL_SEED", ROUTINE_SEED),
    );
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

const CRASH_SEED: u64 = 0x5eed_0002_cce0_0002;
/// Pending effects lost, all kept, and two seeded subsets (torn and reordered).
const CRASH_PERSISTS: [Persist; 4] = [
    Persist::None,
    Persist::All,
    Persist::Seeded(1),
    Persist::Seeded(2),
];
/// Commit steps the store runs after an I/O failure before the process ends.
const LIVE_STEPS: usize = 6;

fn sim_engine(sim: SimIo, store: NativeStore) -> Engine {
    Engine {
        backend: Backend::Sim(sim, Arc::default()),
        store: Some(store),
        held: Vec::new(),
        staged: Vec::new(),
        refused: Vec::new(),
    }
}

/// The retirement audit on the simulator's image: no retired page or content extent is reachable
/// from the published state, and the newest commit retired exactly what it stopped reaching.
/// Reads only, so it never changes the operation counts replays compare. (A real-files store holds
/// its lock, so the audit runs on the simulator.)
fn audit(e: &Engine, after: &str) {
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
fn traverse(sim: &SimIo, after: &str) -> (crate::format::Descriptor, crate::audit::Reach) {
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

fn reached_extents(r: &crate::audit::Reach) -> Vec<(u64, u64)> {
    r.manifests
        .iter()
        .chain(&r.content_segments)
        .chain(&r.history_extents)
        .copied()
        .collect()
}

fn overlaps(a: (u64, u64), b: (u64, u64)) -> bool {
    a.0 < b.0 + b.1 && b.0 < a.0 + a.1
}

/// The independent ownership model, after every step: each live staging owns the content bytes
/// its staging wrote (known from the operations the harness issued and the writes it observed).
/// Owned bytes must be intact, no two stagings may own the same byte, and the published state
/// must not reach owned space. Owned space may carry a persisted retirement record (a staging
/// that took retired space before any commit consumed the record); that is counted, not refused.
fn ownership_check(e: &Engine, after: &str) {
    let Backend::Sim(sim, _) = &e.backend else {
        return;
    };
    count("ownership checks");
    let (cur, r) = traverse(sim, after);
    let reached = reached_extents(&r);
    let live: Vec<(usize, (u64, u64))> = e
        .staged
        .iter()
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
fn consumed_check(e: &Engine, consumed: &[(u64, u64)], after: &str) {
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
fn recovered_ownership_check(e: &Engine, prior: &[(u64, u64)]) {
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

/// Space owned by live stagings that the step `action` does not bind.
fn unbound_staged_space(e: &Engine, action: &Action) -> Vec<(u64, u64)> {
    let bound: BTreeSet<usize> = match action {
        Action::Commit { request, .. } | Action::Retry { request, .. } => {
            (0..request.contents.len())
                .filter_map(|j| e.earlier(request, j))
                .collect()
        }
        Action::Discard {
            index,
            refused: false,
        } => [*index].into(),
        _ => BTreeSet::new(),
    };
    e.staged
        .iter()
        .enumerate()
        .filter(|(i, _)| !bound.contains(i))
        .flat_map(|(_, s)| s.ranges())
        .collect()
}

fn sim_of(e: &Engine) -> &SimIo {
    match &e.backend {
        Backend::Sim(sim, _) => sim,
        Backend::Files(_) => unreachable!("crash campaigns run on the simulator"),
    }
}

/// The kind of operation the injected failure hit so far, if any.
fn failed_op(e: &Engine) -> Option<&'static str> {
    match &e.backend {
        Backend::Sim(_, tap) => *tap.failed.lock().unwrap(),
        Backend::Files(_) => None,
    }
}

/// Mutating operations performed after store creation, cumulatively after each step.
fn boundaries(transitions: &[Op]) -> Vec<u64> {
    let mut e = EngineTest::<false>::init_test(&Model::default());
    let base = sim_of(&e).ops();
    let mut m = Model::default();
    let mut out = Vec::with_capacity(transitions.len());
    for op in transitions {
        m = ModelMachine::apply(m, op);
        e = EngineTest::<false>::apply(e, &m, op.clone());
        out.push(sim_of(&e).ops() - base);
    }
    out
}

/// Stages a request's content and commits it from `basis` (a fresh view when `None`), with no
/// expectations. `prestaged` binds an already staged handle as the request's last content.
fn submit_tolerant(
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
fn in_flight(
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

fn last_position(store: &NativeStore) -> u64 {
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
struct Allowed {
    /// The interrupted step's effects absent (`None`: observations ruled this out).
    without: Option<Model>,
    /// The interrupted step committed at `position` (`None`: it cannot have committed).
    with: Option<(Model, u64)>,
    /// The interrupted request and, when that step was a retry, what the retry must return.
    request: Option<Request>,
    retry: Option<Result<u64, ErrKind>>,
    /// Space owned by stagings live before the interruption that nothing since bound: a
    /// recovered state must not reach it.
    prior: Vec<(u64, u64)>,
}

impl Allowed {
    fn around(before: &Model, after: &Model, request: Option<Request>) -> Self {
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
    fn select(&self, last: u64) -> (Model, bool) {
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

fn open_image(img: &SimIo, what: &str) -> Engine {
    let store = open_on(Box::new(img.clone()), Default::default())
        .unwrap_or_else(|e| panic!("{what}: recovery must open the store: {e:?}"));
    sim_engine(img.clone(), store)
}

/// Recovers one image and checks it against the allowed states, the outcome of the interrupted
/// attempt, a retry of it, a further commit and a reopen.
fn check_recovery(img: SimIo, allowed: &Allowed) {
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
fn check_exit_then_power_loss(sim: &SimIo, allowed: &Allowed) {
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
fn check_interrupted_recovery(sim: &SimIo, persist: Persist, allowed: &Allowed) {
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

fn replay_until(transitions: &[Op], bounds: &[u64], i: usize, e: &mut Option<Engine>) -> Model {
    let base = 0;
    let mut m = Model::default();
    for (j, op) in transitions[..i].iter().enumerate() {
        m = ModelMachine::apply(m, op);
        let eng = e.take().expect("engine");
        let eng = EngineTest::<false>::apply(eng, &m, op.clone());
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
fn fresh_engine() -> Engine {
    let backend = Backend::Sim(SimIo::new(), Arc::default());
    let store = create_on(backend.io(), STORE, Default::default())
        .unwrap_or_else(|e| panic!("creating the store under test: {e:?}"));
    let e = Engine {
        backend,
        store: Some(store),
        held: Vec::new(),
        staged: Vec::new(),
        refused: Vec::new(),
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
    static CREATE_OPS: std::cell::OnceCell<u64> = const { std::cell::OnceCell::new() };
}

/// Replays `transitions` with a crash after `cut` mutating operations and checks every recovery.
fn crash_at(transitions: &[Op], bounds: &[u64], cut: u64) {
    let Some(i) = bounds.iter().position(|b| *b > cut) else {
        return;
    };
    let mut e = Some(fresh_engine());
    let created = sim_of(e.as_ref().unwrap()).ops();
    sim_of(e.as_ref().unwrap()).crash_after(cut);
    let shifted: Vec<u64> = bounds.iter().map(|b| b + created).collect();
    let before = replay_until(transitions, &shifted, i, &mut e);
    let mut e = e.unwrap();
    let after = ModelMachine::apply(before.clone(), &transitions[i]);
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
fn branch_expect(m: &Model, req: &Request) -> (Expected, Vec<WriteKey>) {
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
struct Live {
    fence_allowed: bool,
    uncertain: Option<AttemptId>,
}

/// The logical kind of a refusal the model predicts; `None` for anything else (an integrity or
/// I/O error, a fenced store, an indeterminate outcome).
fn logical_kind(e: &CommitError) -> Option<ErrKind> {
    match e {
        CommitError::Conflict(_) => Some(ErrKind::Conflict),
        CommitError::AttemptMismatch => Some(ErrKind::Mismatch),
        CommitError::ReadersActive(_) => Some(ErrKind::ReadersActive),
        CommitError::Rejected(StorageError::InvalidRequest(_)) => Some(ErrKind::Rejected),
        _ => None,
    }
}

fn is_fenced_refusal(e: &CommitError) -> bool {
    matches!(e, CommitError::Rejected(StorageError::StoreState(m)) if m.starts_with("writes fenced"))
}

/// Whether a refusal fits a model branch that predicts `expected` (`committed`: the branch has
/// the attempt committed already, so the store must answer from its record, fence or not).
fn refusal_fits(
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
fn live_result(
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
fn fail_at(
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
    let after = ModelMachine::apply(before.clone(), &transitions[i]);
    count(&format!(
        "failure {}: {}",
        if post { "after" } else { "in" },
        op_name(&transitions[i])
    ));
    let prior = unbound_staged_space(&e, &after.action);
    let (request, got) = in_flight(&mut e, &after.action);
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
            m = ModelMachine::apply(m, op);
            let req = match &m.action {
                Action::Commit { request, .. } | Action::Retry { request, .. } => request.clone(),
                _ => continue,
            };
            if let Some(got) = submit_tolerant(&mut e, None, &req, None) {
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

#[derive(Clone, Copy, PartialEq, Eq)]
enum Interruption {
    Crash,
    Failure,
}

struct CrashBudget {
    name: &'static str,
    interruption: Interruption,
    ops: std::ops::RangeInclusive<usize>,
    cases: u32,
    /// `None`: every cut; otherwise this many seeded cuts per sequence, half of them among the
    /// last operations of commit steps (where publication happens).
    sampled: Option<u64>,
}

/// The operation spans `(start, end)` of the steps that commit (and of reopens, when
/// `reopen`) and perform mutating I/O.
fn step_spans(transitions: &[Op], bounds: &[u64], reopen: bool) -> Vec<(u64, u64)> {
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
fn publication_ends(transitions: &[Op], bounds: &[u64], sampled: Option<u64>) -> Vec<u64> {
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
fn sampled_cuts(transitions: &[Op], bounds: &[u64], n: u64, seed: u64) -> Vec<u64> {
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

fn run_crash_campaign(b: CrashBudget, seed: u64) {
    let config = Config {
        cases: b.cases,
        rng_seed: RngSeed::Fixed(seed),
        failure_persistence: Some(Box::new(FileFailurePersistence::SourceParallel(
            "proptest-regressions",
        ))),
        source_file: Some(file!()),
        ..Config::default()
    };
    STATS.with(|s| s.borrow_mut().clear());
    let totals = Arc::new(Mutex::new([0u64; 5]));
    let t = totals.clone();
    let sampled = b.sampled;
    let interruption = b.interruption;
    let mut runner = TestRunner::new(config);
    let result = runner.run(
        &ModelMachine::sequential_strategy(b.ops.clone()),
        move |(_, transitions, _)| {
            let bounds = boundaries(&transitions);
            let total = bounds.last().copied().unwrap_or(0);
            let cuts: Vec<u64> = match sampled {
                None => (0..total).collect(),
                Some(n) => sampled_cuts(&transitions, &bounds, n, seed),
            };
            {
                let mut t = t.lock().unwrap();
                t[0] += 1;
                t[1] += transitions.len() as u64;
                t[2] += total;
                t[3] += cuts.len() as u64;
            }
            for cut in cuts {
                match interruption {
                    Interruption::Crash => crash_at(&transitions, &bounds, cut),
                    Interruption::Failure => {
                        for lossy in [false, true] {
                            for continue_live in [false, true] {
                                fail_at(&transitions, &bounds, cut, lossy, continue_live, false);
                            }
                        }
                    }
                }
            }
            if interruption == Interruption::Failure {
                let ends = publication_ends(&transitions, &bounds, sampled);
                t.lock().unwrap()[4] += ends.len() as u64;
                for cut in ends {
                    for continue_live in [false, true] {
                        fail_at(&transitions, &bounds, cut, false, continue_live, true);
                    }
                }
            }
            Ok(())
        },
    );
    let [sequences, ops, cut_points, cuts, post_cuts] = *totals.lock().unwrap();
    let observed = STATS.with(|s| s.borrow().clone());
    let report = serde_json::json!({
        "campaign": b.name,
        "seed": format!("{seed:#x}"),
        "cases": b.cases,
        "operations_per_sequence": [b.ops.start(), b.ops.end()],
        "cut_selection": match b.sampled {
            None => "every cut".to_string(),
            Some(n) => format!("{n} seeded cuts per sequence, half at commit tails"),
        },
        "interruption": match b.interruption {
            Interruption::Crash => "crash: persist none, all, seeded 1, seeded 2; crash and failure during recovery; process exit then power loss",
            Interruption::Failure => "one failed operation, sync failures lossy and not, plus a failed staging write right after each (sampled) publication; stop or continue up to 6 commits; then process exit or persist none, all, seeded 1",
        },
        // Includes shrinking attempts when a case failed.
        "sequences_run": sequences,
        "attempted_operations": ops,
        "cut_points_available": cut_points,
        "cuts_executed": cuts,
        "recovered_without_step": observed.get("recovered without the step").copied().unwrap_or(0),
        "recovered_with_step": observed.get("recovered with the step").copied().unwrap_or(0),
        "failures_after_publication": post_cuts,
        "passed": result.is_ok(),
        "observed": observed,
    });
    println!("crash report: {report}");
    if let Err(e) = result {
        panic!("{e}");
    }
    if b.interruption == Interruption::Failure {
        // The narrowing by an acknowledgement and by a committed live outcome must run, and a
        // store that refuses everything after a failure must not pass.
        for what in [
            "failed step acknowledged",
            "live outcome committed",
            "acknowledged after the failure",
        ] {
            assert!(
                observed.get(what).copied().unwrap_or(0) > 0,
                "the failure campaign never observed \"{what}\""
            );
        }
    }
}

#[test]
fn every_cut_of_short_generated_sequences_recovers_a_model_state() {
    run_crash_campaign(
        CrashBudget {
            name: "crash at every cut, short sequences",
            interruption: Interruption::Crash,
            ops: 8..=16,
            cases: env_or("CCE_CRASH_CASES", 4),
            sampled: None,
        },
        env_or("CCE_CRASH_SEED", CRASH_SEED),
    );
}

#[test]
fn sampled_cuts_of_long_generated_sequences_recover_a_model_state() {
    run_crash_campaign(
        CrashBudget {
            name: "crash at sampled cuts, long sequences",
            interruption: Interruption::Crash,
            ops: MIN_OPS..=MAX_OPS,
            cases: env_or("CCE_CRASH_LONG_CASES", 2),
            sampled: Some(env_or("CCE_CRASH_SAMPLED_CUTS", 12)),
        },
        env_or("CCE_CRASH_SEED", CRASH_SEED),
    );
}

// Budget floor for the failure tiers: each requires an acknowledged failed step, a committed live
// outcome and a live acknowledgement (`run_crash_campaign`), so a budget can be too small to pass
// even when the store is right (no sequence at all always fails). Measured with this generator,
// one sequence per tier already satisfies them at seeds 0 to 8. The defaults (4 short sequences;
// 2 long sequences with 12 sampled cuts) are set by detection instead: the short tier needs 4 at
// the default seed to catch a data barrier that does not fence. Overrides may raise the budget;
// below the defaults detection is not established.
#[test]
fn an_io_failure_at_every_cut_of_short_generated_sequences_is_reported_and_recovered() {
    run_crash_campaign(
        CrashBudget {
            name: "I/O failure at every cut, short sequences",
            interruption: Interruption::Failure,
            ops: 8..=16,
            cases: env_or("CCE_FAILURE_CASES", 4),
            sampled: None,
        },
        env_or("CCE_CRASH_SEED", CRASH_SEED),
    );
}

#[test]
fn an_io_failure_at_sampled_cuts_of_long_generated_sequences_is_reported_and_recovered() {
    run_crash_campaign(
        CrashBudget {
            name: "I/O failure at sampled cuts, long sequences",
            interruption: Interruption::Failure,
            ops: MIN_OPS..=MAX_OPS,
            cases: env_or("CCE_FAILURE_LONG_CASES", 2),
            sampled: Some(env_or("CCE_FAILURE_SAMPLED_CUTS", 12)),
        },
        env_or("CCE_CRASH_SEED", CRASH_SEED),
    );
}

// ---------------------------------------------------------------------------------------------
// Controlled schedules: staging and discard interleaved with a commit paused at its seal
// (before the data barrier), its publication (before the record is written) or its application
// (after the record's sync returned, before the running store applies the result). The pause is
// the harness's I/O tap; no store test hook is involved. A concurrent operation may finish while
// the commit is paused or wait for it; both are legal progress, never finishing is not.
// ---------------------------------------------------------------------------------------------

const POINTS: [Point; 3] = [Point::Barrier, Point::Publish, Point::Apply];

/// Did the concurrent operation finish while the commit was paused?
#[derive(Debug, PartialEq, Eq)]
enum Progress {
    DuringPause,
    AfterRelease,
}

/// Runs `commit` on its own thread paused at `point`; while it is paused runs `during` on another
/// thread, then releases the commit. Returns the commit's result, `during`'s and its progress.
fn across_pause<T: Send>(
    e: &Engine,
    point: Point,
    commit: impl FnOnce(&NativeStore) -> Result<CommitReceipt, CommitError> + Send,
    during: impl FnOnce(&NativeStore) -> T + Send,
) -> (Result<CommitReceipt, CommitError>, T, Progress) {
    let deadline = std::time::Duration::from_secs(20);
    let tap = e.tap().expect("schedules run on the simulator");
    let (reached_tx, reached) = std::sync::mpsc::channel();
    let (go, go_rx) = std::sync::mpsc::channel();
    *tap.gate.lock().unwrap() = Some(Gate {
        point,
        reached: reached_tx,
        go: go_rx,
    });
    let store = e.store();
    std::thread::scope(|s| {
        let c = s.spawn(move || commit(store));
        reached
            .recv_timeout(deadline)
            .unwrap_or_else(|_| panic!("the commit never reached {point:?}"));
        let (done_tx, done) = std::sync::mpsc::channel();
        let d = s.spawn(move || {
            let r = during(store);
            let _ = done_tx.send(());
            r
        });
        let progress = match done.recv_timeout(std::time::Duration::from_millis(300)) {
            Ok(()) => Progress::DuringPause,
            Err(_) => Progress::AfterRelease,
        };
        go.send(()).expect("release the commit");
        let got = c.join().expect("commit thread");
        let r = d.join().expect("concurrent thread");
        (got, r, progress)
    })
}

fn record_content(e: &Engine, name: &str) -> Option<Vec<u8>> {
    let mut v = e
        .store()
        .begin()
        .unwrap_or_else(|x| panic!("the store failed: begin: {x:?}"));
    let rec = v
        .context(&cid(name))
        .unwrap_or_else(|x| panic!("the store failed: context {name}: {x:?}"))?;
    let c = rec.content?;
    Some(read_all(&mut v.content(&c).unwrap_or_else(|x| {
        panic!("the store failed: content of {name}: {x:?}")
    })))
}

fn bytes_of(seed: u8, len: usize) -> Vec<u8> {
    (0..len)
        .map(|i| (i.wrapping_mul(29) + seed as usize * 11 + (i >> 7)) as u8)
        .collect()
}

/// Commits `name` binding `handle` (or no content) from a fresh view.
fn put_record(
    store: &NativeStore,
    attempt_name: &str,
    name: &str,
    handle: Option<&StagedContent>,
) -> Result<CommitReceipt, CommitError> {
    let basis = store
        .begin()
        .unwrap_or_else(|x| panic!("the store failed: begin: {x:?}"));
    store.commit(
        attempt(attempt_name),
        basis,
        ChangeSet {
            changes: vec![Change::PutContext {
                id: cid(name),
                expect: Precondition::Any,
                body: format!("record {name}").into_bytes(),
                content: handle.map(|_| 0),
            }],
            content: handle.map(ContentBinding::from).into_iter().collect(),
            ..ChangeSet::default()
        },
    )
}

/// A store with retired content that is already reusable, and two live stagings: `x` (never
/// bound) and `y` (to be bound by the paused commit).
fn schedule_store() -> (Engine, Vec<u8>, Vec<u8>) {
    let mut e = fresh_engine();
    // Four records with their own content, then all removed in one commit: four retired
    // content extents, reusable once two more generations have passed.
    for i in 0..4u8 {
        let s = e.stage(&bytes_of(i + 1, 6000));
        put_record(
            e.store(),
            &format!("setup-{i}"),
            &format!("old-{i}"),
            Some(&s.handle),
        )
        .unwrap_or_else(|x| panic!("setup commit: {x:?}"));
    }
    let basis = e.store().begin().expect("begin");
    e.store()
        .commit(
            attempt("setup-remove"),
            basis,
            ChangeSet {
                changes: (0..4)
                    .map(|i| Change::RemoveContext {
                        id: cid(&format!("old-{i}")),
                        expect: Precondition::Any,
                    })
                    .collect(),
                ..ChangeSet::default()
            },
        )
        .unwrap_or_else(|x| panic!("setup removal: {x:?}"));
    for i in 0..3 {
        put_record(e.store(), &format!("advance-{i}"), "tick", None)
            .unwrap_or_else(|x| panic!("setup commit: {x:?}"));
    }
    let (xb, yb) = (bytes_of(7, 3000), bytes_of(8, 3000));
    let x = e.stage(&xb);
    let y = e.stage(&yb);
    e.staged.push(x);
    e.staged.push(y);
    audit(&e, "schedule setup");
    ownership_check(&e, "schedule setup");
    (e, xb, yb)
}

#[test]
fn staging_and_discard_across_a_commits_seal_publication_and_application_make_legal_progress() {
    STATS.with(|s| s.borrow_mut().clear());
    for point in POINTS {
        let (mut e, _xb, yb) = schedule_store();
        // Small enough for the retired extent the setup leaves reusable.
        let zb = bytes_of(9, 2500);
        let y = e.staged.pop().expect("y");
        let x = e.staged.pop().expect("x");
        let x_ranges: Vec<(u64, u64)> = x.ranges().collect();
        let tap = e.tap().expect("tap").clone();
        let y_handle = &y.handle;
        let (got, (z, discard), progress) = across_pause(
            &e,
            point,
            move |store| put_record(store, "paused", "p", Some(y_handle)),
            |store| {
                tap.writes.lock().unwrap().clear();
                let z = store.stage_content(&mut &zb[..]);
                let owns = std::mem::take(&mut *tap.writes.lock().unwrap());
                let discard = store.discard_staged(x.handle);
                (
                    z.map(|handle| Staged {
                        handle,
                        owns,
                        rejected: 0,
                    }),
                    discard,
                )
            },
        );
        count(&format!("{point:?}: concurrent staging {progress:?}"));
        if let Ok(z) = &z {
            let (cur, r) = traverse(sim_of(&e), "the paused commit");
            if z.ranges().any(|a| {
                a.0 < cur.content_len
                    || r.retired_extents
                        .iter()
                        .any(|(o, (_, l))| overlaps(a, (*o, *l)))
            }) {
                count(&format!(
                    "{point:?}: concurrent staging reused retired space"
                ));
            }
        }
        let receipt = got.unwrap_or_else(|x| panic!("{point:?}: the paused commit failed: {x:?}"));
        let z = z.unwrap_or_else(|x| panic!("{point:?}: staging across the commit failed: {x:?}"));
        discard.unwrap_or_else(|x| panic!("{point:?}: discard across the commit failed: {x:?}"));
        assert_eq!(
            record_content(&e, "p"),
            Some(yb.clone()),
            "{point:?}: committed content"
        );
        // y is consumed, x discarded, z live and owning its bytes.
        let y_ranges: Vec<(u64, u64)> = y.ranges().collect();
        consumed_check(&e, &y_ranges, &format!("the commit paused at {point:?}"));
        e.staged.push(z);
        audit(&e, &format!("the commit paused at {point:?}"));
        ownership_check(&e, &format!("the commit paused at {point:?}"));
        let (_, r) = traverse(sim_of(&e), "after the schedule");
        let reached = reached_extents(&r);
        assert!(
            !x_ranges
                .iter()
                .any(|a| reached.iter().any(|b| overlaps(*a, *b))),
            "{point:?}: the discarded staging's space is reachable"
        );
        // The staging made across the pause commits and reads back, before and after a reopen.
        let z = e.staged.pop().expect("z");
        put_record(e.store(), "after", "z", Some(&z.handle))
            .unwrap_or_else(|x| panic!("{point:?}: committing the staging made across: {x:?}"));
        assert!(receipt.position.0 > 0);
        assert_eq!(
            record_content(&e, "z"),
            Some(zb.clone()),
            "{point:?}: z content"
        );
        audit(&e, "the commit of z");
        ownership_check(&e, "the commit of z");
        e.store.take().expect("store").close().expect("clean close");
        e.store = Some(open(&e.backend));
        assert_eq!(
            record_content(&e, "p"),
            Some(yb),
            "{point:?}: p after reopen"
        );
        assert_eq!(
            record_content(&e, "z"),
            Some(zb),
            "{point:?}: z after reopen"
        );
        audit(&e, "reopen after the schedule");
    }
    let observed = STATS.with(|s| s.borrow().clone());
    println!("schedule report: {}", serde_json::json!(observed));
    for point in POINTS {
        assert!(
            observed.contains_key(&format!(
                "{point:?}: concurrent staging reused retired space"
            )),
            "{point:?}: the staging across the pause did not reuse retired space, so the schedule \
             tested nothing about reuse"
        );
    }
}

#[test]
fn discarding_a_staging_a_paused_commit_binds_is_refused_and_the_commit_completes() {
    for point in POINTS {
        let (mut e, _xb, yb) = schedule_store();
        let y = e.staged.pop().expect("y");
        let y_handle = &y.handle;
        let y_again = StagedContent {
            content: y.handle.content,
            token: y.handle.token.clone(),
        };
        let (got, discard, _) = across_pause(
            &e,
            point,
            move |store| put_record(store, "paused", "p", Some(y_handle)),
            move |store| store.discard_staged(y_again),
        );
        assert!(
            discard.is_err(),
            "{point:?}: a staging bound by an admitted commit was discarded"
        );
        got.unwrap_or_else(|x| panic!("{point:?}: the paused commit failed: {x:?}"));
        assert_eq!(
            record_content(&e, "p"),
            Some(yb),
            "{point:?}: committed content"
        );
        let again = StagedContent {
            content: y.handle.content,
            token: y.handle.token.clone(),
        };
        assert!(
            e.store().discard_staged(again).is_err(),
            "{point:?}: a consumed staging was discarded"
        );
        audit(&e, &format!("the commit paused at {point:?}"));
        ownership_check(&e, &format!("the commit paused at {point:?}"));
    }
}

/// The publication record's write fails while a staging is made across the pause: the commit's
/// outcome is unknown and the store is fenced; the staging still owns intact bytes; after a
/// reopen the store runs on the publication actually selected, nothing is owned, and the
/// interrupted request decides afresh and commits.
#[test]
fn a_failed_publication_with_a_staging_across_it_recovers_on_the_selected_publication() {
    let (mut e, _xb, yb) = schedule_store();
    let zb = bytes_of(10, 2500);
    let y = e.staged.pop().expect("y");
    let y_handle = &y.handle;
    let tap = e.tap().expect("tap").clone();
    let sim = sim_of(&e).clone();
    let (got, z, progress) = across_pause(
        &e,
        Point::Publish,
        move |store| put_record(store, "paused", "p", Some(y_handle)),
        |store| {
            tap.writes.lock().unwrap().clear();
            let z = store.stage_content(&mut &zb[..]);
            let owns = std::mem::take(&mut *tap.writes.lock().unwrap());
            // The next mutating operation is the paused publication write.
            sim.fail_nth_op(1);
            z.map(|handle| Staged {
                handle,
                owns,
                rejected: 0,
            })
        },
    );
    assert_eq!(
        progress,
        Progress::DuringPause,
        "staging waited for the paused publication"
    );
    assert!(
        matches!(got, Err(CommitError::Indeterminate(_))),
        "a failed publication write: {got:?}"
    );
    let z = z.unwrap_or_else(|x| panic!("staging across the publication: {x:?}"));
    assert_eq!(
        e.store().outcome(&attempt("paused")).expect("outcome"),
        AttemptOutcome::Unknown
    );
    e.staged.push(z);
    ownership_check(&e, "a failed publication");
    let prior: Vec<(u64, u64)> = e.staged.iter().flat_map(Staged::ranges).collect();
    e.staged.clear();
    e.held.clear();
    drop(e.store.take());
    let e = open_image(&sim.after_process_exit(), "after the failed publication");
    recovered_ownership_check(&e, &prior);
    audit(&e, "recovery after the failed publication");
    // The write failed, so the previous publication was selected and the attempt is absent.
    assert_eq!(
        e.store().outcome(&attempt("paused")).expect("outcome"),
        AttemptOutcome::Absent
    );
    assert_eq!(record_content(&e, "p"), None);
    let y2 = e.stage(&yb);
    put_record(e.store(), "paused", "p", Some(&y2.handle))
        .unwrap_or_else(|x| panic!("the retried request after recovery: {x:?}"));
    assert_eq!(record_content(&e, "p"), Some(yb));
    audit(&e, "the retry after recovery");
}
