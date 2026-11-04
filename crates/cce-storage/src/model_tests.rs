//! Generated operation sequences checked against an independent logical model.
//!
//! The model below knows the logical contract only: records, relationships navigated in both
//! directions, content bytes, retained history, attempt outcomes, read-basis conflicts and view
//! isolation. It never calls the engine. Each generated step is resolved by the model into a
//! concrete request and the result it predicts; the engine (production code on the simulator)
//! executes the same request and every observation is compared.
//!
//! Budget: `CCE_MODEL_CASES` sequences (default 32) of 64 to 80 attempted operations, from the
//! fixed seed `CCE_MODEL_SEED` (default [`ROUTINE_SEED`]). Minimized failures are saved under
//! `proptest-regressions/` and replayed first on later runs.

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

use crate::engine::Inner;
use crate::format::StoreId;
use crate::sim::SimIo;
use crate::{NativeStore, NativeView};

/// Seed of the routine run. Changing it changes the generated sequences.
const ROUTINE_SEED: u64 = 0x5eed_0001_cce0_0001;
const ROUTINE_CASES: u32 = 32;
const MIN_OPS: usize = 64;
const MAX_OPS: usize = 80;

const STORE: StoreId = StoreId(*b"model-test-store");
/// Context identities; "a" and "ab" share a prefix on purpose.
const CONTEXTS: [&str; 6] = ["a", "ab", "b", "c", "d", "e"];
const KINDS: [&str; 3] = ["k", "kk", "x"];
const RELATIONSHIPS: usize = 8;
const MAX_HELD: usize = 3;

// ---------------------------------------------------------------------------------------------
// Independent model. Uses cce-core types and SHA-256 only.
// ---------------------------------------------------------------------------------------------

fn ctx_id(i: u8) -> ContextId {
    ContextId::new(CONTEXTS[i as usize % CONTEXTS.len()].as_bytes().to_vec()).unwrap()
}

fn rel_id(i: u8) -> RelationshipId {
    RelationshipId::new(format!("r{}", i as usize % RELATIONSHIPS).into_bytes()).unwrap()
}

fn kind(i: u8) -> RelationshipKind {
    RelationshipKind::new(KINDS[i as usize % KINDS.len()].as_bytes().to_vec()).unwrap()
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
        content: Option<ContentSpec>,
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
    evidence: Vec<Evidence>,
    removal: RemovalMode,
}

impl Request {
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
        AttemptId::new(format!("attempt-{}", self.next_attempt).into_bytes()).unwrap()
    }

    fn resolve(&mut self, spec: &RequestSpec) -> Request {
        let mut contents = Vec::new();
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
                        body: format!("body {body} of {id:?}").into_bytes(),
                        content: content.map(|s| {
                            contents.push(Arc::new(s.bytes()));
                            contents.len() - 1
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
        Request {
            attempt: self.fresh_attempt(),
            changes,
            contents,
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
        outcomes.push((
            AttemptId::new(*b"never-submitted").unwrap(),
            AttemptOutcome::Absent,
        ));
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
                self.staged.extend(request.contents.iter().cloned());
            }
        }
        if matches!(expected, Expected::Committed(_)) {
            self.refused += request.contents.len();
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

fn content_spec() -> impl Strategy<Value = ContentSpec> {
    (0u8..12, any::<u8>()).prop_map(|(seed, size)| ContentSpec { seed, size })
}

fn change_spec() -> impl Strategy<Value = ChangeSpec> {
    let n = CONTEXTS.len() as u8;
    let r = RELATIONSHIPS as u8;
    let k = KINDS.len() as u8;
    prop_oneof![
        5 => (0..n, expect_spec(), any::<u8>(), proptest::option::weighted(0.4, content_spec()))
            .prop_map(|(ctx, expect, body, content)| ChangeSpec::PutContext { ctx, expect, body, content }),
        1 => (0..n, expect_spec()).prop_map(|(ctx, expect)| ChangeSpec::RemoveContext { ctx, expect }),
        4 => (0..r, expect_spec(), 0..k, 0..n, 0..n)
            .prop_map(|(rel, expect, kind, source, target)| ChangeSpec::PutRelationship { rel, expect, kind, source, target }),
        2 => (0..r, expect_spec()).prop_map(|(rel, expect)| ChangeSpec::RemoveRelationship { rel, expect }),
    ]
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

    fn transitions(_: &Model) -> BoxedStrategy<Op> {
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

/// Counts of what the generated sequences exercised (reported by the test).
static STATS: Mutex<BTreeMap<&'static str, u64>> = Mutex::new(BTreeMap::new());

fn count(what: &'static str) {
    *STATS.lock().unwrap().entry(what).or_default() += 1;
}

struct Engine {
    sim: SimIo,
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
        let s = self.store().stage_content(&mut &bytes[..]).expect("stage");
        assert_eq!(s.content, content_ref(bytes), "staged content description");
        s
    }

    fn change_set(&self, req: &Request) -> (ChangeSet, Vec<StagedContent>) {
        let staged: Vec<StagedContent> = req.contents.iter().map(|b| self.stage(b)).collect();
        let cs = ChangeSet {
            changes: req.changes.clone(),
            content: staged.iter().map(ContentBinding::from).collect(),
            evidence: req.evidence.clone(),
            removal: req.removal,
        };
        (cs, staged)
    }
}

fn read_all(r: &mut dyn Read) -> Vec<u8> {
    let mut v = Vec::new();
    r.read_to_end(&mut v).expect("read content");
    v
}

fn check_read(view: &mut NativeView, expected: &ReadResult) {
    match expected {
        ReadResult::Context(id, e) => {
            assert_eq!(&view.context(id).expect("context"), e, "context {id:?}");
        }
        ReadResult::Relationships {
            query,
            page,
            expected,
        } => {
            let all: Vec<Relationship> = view
                .relationships(query)
                .expect("relationships")
                .map(|r| r.expect("relationship"))
                .collect();
            assert_eq!(&all, expected, "relationships for {query:?}");
            if *page > 0 && !expected.is_empty() {
                let first: Vec<Relationship> = view
                    .relationships(query)
                    .expect("relationships")
                    .take(*page)
                    .map(|r| r.expect("relationship"))
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
                    .expect("relationships")
                    .map(|r| r.expect("relationship"))
                    .collect();
                let joined: Vec<Relationship> = first.into_iter().chain(rest).collect();
                assert_eq!(&joined, expected, "paged relationships for {query:?}");
            }
        }
        ReadResult::Content(e) => {
            if let Some((r, bytes)) = e {
                let got = read_all(&mut view.content(r).expect("content of a live record"));
                assert!(got == **bytes, "content bytes of {r:?}");
            }
        }
    }
}

fn main_check(engine: &Engine, snap: &Snapshot) {
    let store = engine.store();
    let mut v = store.begin().expect("begin");
    for i in 0..CONTEXTS.len() as u8 {
        let id = ctx_id(i);
        assert_eq!(
            v.context(&id).expect("context"),
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
                    .expect("relationships")
                    .map(|r| r.expect("relationship"))
                    .collect();
                assert_eq!(got, snap.logical.query(&id, outgoing, k.as_ref()), "{q:?}");
            }
        }
        if let Some(c) = snap.logical.contexts.get(&id).and_then(|r| r.content) {
            let got = read_all(&mut v.content(&c).expect("content of a live record"));
            assert!(got == *snap.bytes[&c.digest.0], "content of {id:?}");
        }
    }
    let history: Vec<HistoryEntry> = v
        .history(HistoryPosition(0))
        .expect("history")
        .map(|h| h.expect("history entry"))
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
        assert_eq!(&store.outcome(a).expect("outcome"), o, "outcome of {a:?}");
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

struct EngineTest;

/// Opens the store again after the previous handle closed (the simulated owner ended).
fn open(sim: &SimIo) -> NativeStore {
    sim.release();
    NativeStore {
        inner: Inner::open(Box::new(sim.clone()), Default::default()).expect("open"),
    }
}

fn reads_through(view: &mut NativeView, reads: &[ReadResult]) {
    for r in reads {
        check_read(view, r);
    }
}

impl StateMachineTest for EngineTest {
    type SystemUnderTest = Engine;
    type Reference = ModelMachine;

    fn init_test(_: &Model) -> Engine {
        let sim = SimIo::new();
        let store = NativeStore {
            inner: Inner::create(Box::new(sim.clone()), STORE, Default::default()).expect("create"),
        };
        Engine {
            sim,
            store: Some(store),
            held: Vec::new(),
            staged: Vec::new(),
            refused: Vec::new(),
        }
    }

    fn apply(mut e: Engine, m: &Model, _: Op) -> Engine {
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
                    None => e.store().begin().expect("begin"),
                };
                if held_slot.is_none() {
                    reads_through(&mut basis, reads);
                }
                let (cs, staged) = e.change_set(request);
                let got = if *poll {
                    count("submit and poll");
                    let store = e.store();
                    assert_eq!(
                        store.outcome(&request.attempt).expect("outcome"),
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
                        match store.outcome(&request.attempt).expect("outcome") {
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
                let basis = e.store().begin().expect("begin");
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
                let mut v = e.store().begin().expect("begin");
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
                    e.store().discard_staged(h).expect("discard staged content");
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
                e.store.take().expect("store").close().expect("clean close");
                e.store = Some(open(&e.sim));
            }
        }
        e
    }

    fn teardown(mut e: Engine, m: Model) {
        let snap = m.snapshot();
        main_check(&e, &snap);
        e.held.clear();
        e.staged.clear();
        e.refused.clear();
        e.store.take().expect("store").close().expect("clean close");
        e.store = Some(open(&e.sim));
        main_check(&e, &snap);
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

#[test]
fn generated_operation_sequences_match_the_logical_model() {
    let cases: u32 = env_or("CCE_MODEL_CASES", ROUTINE_CASES);
    let seed: u64 = env_or("CCE_MODEL_SEED", ROUTINE_SEED);
    let config = Config {
        cases,
        rng_seed: RngSeed::Fixed(seed),
        failure_persistence: Some(Box::new(FileFailurePersistence::SourceParallel(
            "proptest-regressions",
        ))),
        source_file: Some(file!()),
        ..Config::default()
    };
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
            EngineTest::test_sequential(config.clone(), initial, transitions, seen);
            Ok(())
        },
    );
    let (sequences, ops) = *attempted.lock().unwrap();
    let stats = STATS.lock().unwrap().clone();
    println!(
        "model campaign: seed {seed:#x}, cases {cases}, sequences run {sequences}, \
         attempted operations {ops}, observed {stats:?}"
    );
    if let Err(e) = result {
        panic!("{e}");
    }
}
