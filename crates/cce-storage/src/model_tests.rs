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

use std::collections::BTreeSet;
use std::sync::{Arc, Mutex};

use cce_core::storage::{
    AttemptOutcome, Change, ChangeSet, CommitError, CommitReceipt, ContentBinding, Precondition,
    ReadView, StagedContent, StorageEngine, StorageError,
};
use proptest::prelude::*;
use proptest::test_runner::{Config, FileFailurePersistence, RngSeed, TestRunner};
use proptest_state_machine::{ReferenceStateMachine, StateMachineTest};

use crate::NativeStore;
use crate::test_support::{attempt, cid};

use crate::model::*;
pub(crate) struct ModelMachine;

fn expect_spec() -> impl Strategy<Value = ExpectSpec> {
    prop_oneof![
        6 => Just(ExpectSpec::Any),
        2 => Just(ExpectSpec::Absent),
        3 => Just(ExpectSpec::Current),
        1 => Just(ExpectSpec::Wrong),
    ]
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

    fn apply(m: Model, op: &Op) -> Model {
        Model::apply(m, op)
    }
}

impl<const FILES: bool> StateMachineTest for EngineTest<FILES> {
    type SystemUnderTest = Engine;
    type Reference = ModelMachine;

    fn init_test(_: &Model) -> Engine {
        Engine::init(FILES)
    }

    fn apply(e: Engine, m: &Model, op: Op) -> Engine {
        Engine::apply(e, m, op)
    }

    fn teardown(e: Engine, m: Model) {
        Engine::teardown(e, m)
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
                        uncertain: false,
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
                uncertain: false,
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
    // The uncertain commit owns the space of the staging it bound until reopen: discarding that
    // staging is refused, and the staging made across the pause owns none of that space.
    let y_again = StagedContent {
        content: y.handle.content,
        token: y.handle.token.clone(),
    };
    assert!(
        e.store().discard_staged(y_again).is_err(),
        "a staging bound by an uncertain publication was discarded"
    );
    let mut y = y;
    y.uncertain = true;
    e.staged.push(y);
    e.staged.push(z);
    ownership_check(&e, "a failed publication");
    // Fenced: a new staging is refused at entry (and so takes nothing from the reuse pool,
    // nothing the uncertain commit owns).
    let wb = bytes_of(11, 2000);
    match e.store().stage_content(&mut &wb[..]) {
        Err(StorageError::StoreState(m)) if m.contains("fenced") => {
            count("staging refused while fenced")
        }
        other => panic!(
            "staging after an uncertain publication: {:?}",
            other.map(|h| h.content)
        ),
    }
    ownership_check(&e, "staging after a failed publication");
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

/// A staging that took space before a commit's publication became uncertain resumes after it:
/// the staging the uncertain commit binds cannot be discarded, and the resumed staging takes no
/// reusable space (none of what the uncertain commit owns), so the bytes that commit's record
/// may reference stay intact; reopen settles which record won.
#[test]
fn a_staging_in_progress_across_an_uncertain_publication_takes_none_of_its_space() {
    use crate::engine::CONTENT_CHUNK;
    let deadline = std::time::Duration::from_secs(20);
    let mut e = fresh_engine();
    // Three retired chunk-sized contents, reusable.
    let len = CONTENT_CHUNK + 300;
    for i in 0..3u8 {
        let s = e.stage(&bytes_of(20 + i, len));
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
                changes: (0..3)
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
    let yb = bytes_of(30, len);
    let y = e.stage(&yb);
    let (cur, r) = traverse(sim_of(&e), "setup");
    assert!(
        y.ranges().any(|a| a.0 < cur.content_len
            || r.retired_extents
                .iter()
                .any(|(o, (_, l))| overlaps(a, (*o, *l)))),
        "the bound staging did not reuse retired space; the schedule would test nothing"
    );
    let wb = bytes_of(31, 2 * CONTENT_CHUNK + 300);
    let tap = e.tap().expect("tap").clone();
    let sim = sim_of(&e).clone();
    let store = e.store();
    let y_handle = &y.handle;
    let (got, discard, w, w_owns) = std::thread::scope(|s| {
        let arm = |point| {
            let (reached_tx, reached) = std::sync::mpsc::channel();
            let (go, go_rx) = std::sync::mpsc::channel();
            *tap.gate.lock().unwrap() = Some(Gate {
                point,
                reached: reached_tx,
                go: go_rx,
            });
            (reached, go)
        };
        let (p_reached, p_go) = arm(Point::Publish);
        let p = s.spawn(move || put_record(store, "paused", "p", Some(y_handle)));
        p_reached
            .recv_timeout(deadline)
            .expect("the commit reaches its publication");
        tap.writes.lock().unwrap().clear();
        let (w_reached, w_go) = arm(Point::ContentWrite);
        let w = s.spawn(|| store.stage_content(&mut &wb[..]));
        w_reached
            .recv_timeout(deadline)
            .expect("the staging takes space and reaches its first write");
        // The next mutating operation is the paused publication record's write.
        sim.fail_nth_op(1);
        p_go.send(()).expect("release the commit");
        let got = p.join().expect("commit thread");
        let y_again = StagedContent {
            content: y_handle.content,
            token: y_handle.token.clone(),
        };
        let discard = store.discard_staged(y_again);
        w_go.send(()).expect("release the staging");
        let w = w.join().expect("staging thread");
        let owns = std::mem::take(&mut *tap.writes.lock().unwrap());
        (got, discard, w, owns)
    });
    assert!(
        matches!(got, Err(CommitError::Indeterminate(_))),
        "the failed publication: {got:?}"
    );
    assert!(
        discard.is_err(),
        "a staging bound by an uncertain publication was discarded"
    );
    let y_ranges: Vec<(u64, u64)> = y.ranges().collect();
    let (_, r) = traverse(sim_of(&e), "the uncertain publication");
    match w {
        Ok(handle) => {
            count("staging in progress completed across the fence");
            let w = Staged {
                handle,
                owns: w_owns,
                rejected: 0,
                uncertain: false,
            };
            // Only its first take preceded the fence.
            for a in w.ranges().skip(1) {
                assert!(
                    !r.retired_extents
                        .iter()
                        .any(|(o, (_, l))| overlaps(a, (*o, *l))),
                    "the staging took reusable space {a:?} after the fence"
                );
            }
            for a in w.ranges() {
                assert!(
                    !y_ranges.iter().any(|b| overlaps(a, *b)),
                    "the staging took the uncertain commit's space {a:?}"
                );
            }
            e.staged.push(w);
        }
        Err(x) => {
            count("staging in progress refused at the fence");
            assert!(
                matches!(&x, StorageError::StoreState(m) if m.contains("fenced")),
                "the staging across the fence failed otherwise: {x:?}"
            );
        }
    }
    let mut y = y;
    y.uncertain = true;
    e.staged.push(y);
    ownership_check(&e, "a staging across an uncertain publication");
    let prior: Vec<(u64, u64)> = e.staged.iter().flat_map(Staged::ranges).collect();
    e.staged.clear();
    drop(e.store.take());
    let e = open_image(&sim.after_process_exit(), "after the uncertain publication");
    recovered_ownership_check(&e, &prior);
    audit(&e, "recovery after the uncertain publication");
    assert_eq!(record_content(&e, "p"), None, "the record write failed");
}
