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

use crate::format::StoreId;
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
    evidence: Vec<Evidence>,
    removal: RemovalMode,
}

impl Request {
    fn alias(&self, j: usize) -> Option<usize> {
        self.aliases.get(j).copied().flatten()
    }

    /// The bytes of each distinct staging the request binds, in binding order.
    fn stagings(&self) -> Vec<Arc<Vec<u8>>> {
        (0..self.contents.len())
            .filter(|j| self.alias(*j).is_none())
            .map(|j| self.contents[j].clone())
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
                            match pick {
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
            Expected::Committed(_) => self.apply_commit(&request, written),
            Expected::Failed(_) => {
                self.uncommitted.insert(request.attempt.clone());
                self.staged.extend(request.stagings());
            }
        }
        if matches!(expected, Expected::Committed(_)) {
            self.refused += request.stagings().len();
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

/// The kind of mutating operation ("write" or "sync") an injected simulator failure hit.
type FailedOp = Arc<Mutex<Option<&'static str>>>;

/// The simulator as the store sees it, recording which kind of operation an injected failure hit
/// (the commit itself runs on the store's worker thread).
struct Watched {
    sim: SimIo,
    failed: FailedOp,
}

impl Watched {
    fn note<T>(&self, kind: &'static str, r: Result<T, StorageError>) -> Result<T, StorageError> {
        if let Err(StorageError::Io(m)) = &r
            && m.starts_with("simulated I/O error")
        {
            *self.failed.lock().unwrap() = Some(kind);
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
        self.note("write", self.sim.write_at(file, offset, data))
    }
    fn len(&self, file: FileId) -> Result<u64, StorageError> {
        self.sim.len(file)
    }
    fn sync(&self, file: FileId) -> Result<(), StorageError> {
        self.note("sync", self.sim.sync(file))
    }
    fn sync_dir(&self) -> Result<(), StorageError> {
        self.note("sync", self.sim.sync_dir())
    }
}

/// Where the store under test lives.
enum Backend {
    Sim(SimIo, FailedOp),
    /// A fresh directory on the real filesystem from the shared directory owner: removed when a
    /// sequence passes, kept (and its path printed) when a failing case unwinds through it.
    Files(TestDir),
}

impl Backend {
    fn new(files: bool) -> Self {
        if files {
            Backend::Files(TestDir::new("cce-model"))
        } else {
            Backend::Sim(SimIo::new(), FailedOp::default())
        }
    }

    /// An adapter for the next create or open. On the simulator the previous handle's ownership
    /// ends first (its "process" exited); on real files closing the handle released it.
    fn io(&self) -> Box<dyn Io> {
        match self {
            Backend::Sim(sim, failed) => {
                sim.release();
                Box::new(Watched {
                    sim: sim.clone(),
                    failed: failed.clone(),
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
    staged: Vec<StagedContent>,
    refused: Vec<StagedContent>,
}

impl Engine {
    fn store(&self) -> &NativeStore {
        self.store.as_ref().expect("store open")
    }

    fn stage(&self, bytes: &[u8]) -> StagedContent {
        let s = self
            .store()
            .stage_content(&mut &bytes[..])
            .unwrap_or_else(|e| panic!("the store failed: stage: {e:?}"));
        assert_eq!(s.content, content_ref(bytes), "staged content description");
        s
    }

    fn change_set(&self, req: &Request) -> (ChangeSet, Vec<StagedContent>) {
        // One handle per distinct staging; an aliased binding binds its target's handle again.
        let mut handle_of = vec![usize::MAX; req.contents.len()];
        let mut staged: Vec<StagedContent> = Vec::new();
        for (j, b) in req.contents.iter().enumerate() {
            if req.alias(j).is_none() {
                handle_of[j] = staged.len();
                staged.push(self.stage(b));
            }
        }
        let content = (0..req.contents.len())
            .map(|j| ContentBinding::from(&staged[handle_of[req.alias(j).unwrap_or(j)]]))
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
                if got.is_ok() {
                    count("commit ok");
                    e.refused.extend(staged);
                } else {
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
                    let h = e.staged.remove(*index);
                    e.store().discard_staged(h).unwrap_or_else(|e| {
                        panic!("the store failed: discard staged content: {e:?}")
                    });
                    count("discard ok");
                }
            }
            Action::Check(snap) => {
                count("full check");
                main_check(&e, snap);
            }
            Action::Reopen => {
                count("reopen");
                e.held.clear();
                e.refused.append(&mut e.staged);
                e.store
                    .take()
                    .expect("store")
                    .close()
                    .unwrap_or_else(|e| panic!("the store failed: clean close: {e:?}"));
                e.store = Some(open(&e.backend));
            }
        }
        audit(&e, &format!("step {}", op_name(&op)));
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
}

#[test]
fn generated_operation_sequences_match_the_logical_model() {
    run_model::<false>(
        "simulator",
        env_or("CCE_MODEL_CASES", ROUTINE_CASES),
        env_or("CCE_MODEL_SEED", ROUTINE_SEED),
    );
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
        backend: Backend::Sim(sim, FailedOp::default()),
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

fn sim_of(e: &Engine) -> &SimIo {
    match &e.backend {
        Backend::Sim(sim, _) => sim,
        Backend::Files(_) => unreachable!("crash campaigns run on the simulator"),
    }
}

/// The kind of operation the injected failure hit so far, if any.
fn failed_op(e: &Engine) -> Option<&'static str> {
    match &e.backend {
        Backend::Sim(_, failed) => *failed.lock().unwrap(),
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
        if request.alias(j).is_some() {
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
        .map(|j| {
            let h = handles[request.alias(j).unwrap_or(j)].as_ref();
            ContentBinding::from(h.expect("an alias names a staged binding"))
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
            if let Ok(s) = e.store().stage_content(&mut &b[..]) {
                e.staged.push(s);
            }
            (None, None)
        }
        Action::Discard { index, refused } => {
            let h = if *refused {
                e.refused.remove(*index)
            } else {
                e.staged.remove(*index)
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
    let backend = Backend::Sim(SimIo::new(), FailedOp::default());
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
    let (request, _) = in_flight(&mut e, &after.action);
    let allowed = Allowed::around(&before, &after, request);
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
            let k = e.staged.iter().position(|s| s.content == want)?;
            Some((e.staged.remove(k), bytes.clone()))
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
