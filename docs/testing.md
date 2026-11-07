# Storage test support

Run the pinned Rust checks with:

```text
cargo fmt --all -- --check
cargo clippy --locked --workspace --all-targets -- --deny warnings
cargo test --workspace --locked
```

`cce-storage` unit suites share the private `test_support` module. `create_on`
and `open_on` compose the existing I/O adapters and return typed results. Callers
keep their store identities, options, scenario inputs and behavioral assertions.
`cid` and `attempt` construct valid fixture identifiers; invalid-input tests
should call the fallible identifier constructors directly.

Unit and integration tests use one directory owner from
`tests/common/resources.rs`. It atomically creates a fresh directory under
`TMPDIR/cce-e2e`, even for repeated labels. Existing directories are occupied and
are never removed to allocate a new one. Declare the owner before dependent
stores or drivers so they close before it is dropped.

Directories are removed after successful tests. Unwinding retains them and
prints the path. If a failure is captured rather than unwound, call `retain()`
before dropping the owner. Runs that fail on purpose in high volume (for example
fault-injection campaigns) can set `CCE_KEEP_FAILED_TEST_DIRS=0` so that an
unwinding test removes its directory instead; a test that expects its own
failure can call `discard_on_unwind()`, and a test that must keep its directory under any
setting can call `keep_on_unwind()`. Tests create every directory they use
through this owner rather than removing a fixed path at the end, which a
failing assertion would skip. Cleanup errors are reported without replacing the
test's original outcome. CI collects retained directories after a failed run.

Integration fixtures in `tests/common/story.rs` compute expected values from
the inputs. Keep these calculations independent of the observed store results.
The process harness in `tests/common/mod.rs` distinguishes answers, exits and
hangs, owns driver termination and reaps its children. Reuse it for process tests.

Internal fault tests compose `SimIo`, `Traced` or the filesystem injector.
Process exit leaves cached writes available; simulated power loss applies the
selected survival policy. Keep the two case labels distinct. Cut sweeps use
`with_repro` to retain durable/cached images, operation traces and case labels;
see `storage-failure-model.md` for the existing image-reopen procedure.

Use the appropriate existing support owner when adding a test. Shared inputs
and setup belong there; behavior-specific assertions stay in the suite. Direct
tests that need no shared machinery can remain direct. Support stays test-only:
the integration driver uses the logical storage port and physical modules remain
private.

## Qualification tiers

`tools/tiers/run_tiers.py` reruns the checks that qualify one candidate and
records what actually ran:

| Tier | What it runs |
|---|---|
| `format` | `cargo fmt --all -- --check` |
| `lint` | Clippy over the workspace, all targets and all features, warnings denied |
| `routine` | `cargo test --workspace --locked --no-fail-fast` |
| `release-story` | the end-to-end story (`tests/story_release.rs`) in a release build |
| `bounded-resources` | the workload tier (`tests/workloads.rs`): release build, ignored tests, one test thread |
| `tooling` | the Python tooling tests under `tests/ci` |
| `campaigns` (opt-in) | the generated-sequence, crash and failure campaigns and the decoder sweep, in staged batches with recorded seeds and budgets |
| `planted-defects` (opt-in) | the planted-defect catalog against the candidate's own suite |

```text
CARGO_BUILD_JOBS=4 RUST_TEST_THREADS=4 python3 tools/tiers/run_tiers.py --output /tmp/cce-tiers-1
python3 tools/tiers/run_tiers.py --output /tmp/cce-tiers-2 --tier campaigns --tier planted-defects
python3 tools/tiers/run_tiers.py --output /tmp/cce-tiers-3 --fresh-clone
```

Without `--tier` the default tiers run; the opt-in tiers take hours and are
selected explicitly. The output directory must be new. `results.json` holds the
candidate commit (and the digest of every file when the working copy is dirty),
the toolchain, and for each tier its exact commands, environment additions,
timing, exit status, outcome, test counts per test binary, every executed test
as `package:target:name` with its result, and the SHA-256 of each log under
`logs/`, and every `RUST*`, `CARGO*` and `CCE_*` environment variable except
those whose names suggest credentials (containing `TOKEN`, `SECRET`, `PASSWORD`
or `KEY`, in any case), which are listed by name only, never with their values. With
`--timeout`, a command that runs too long is stopped together with every
process it started, test binaries included. Runs that show test output use one
test thread. The runner exits 0 only when every selected tier passed.

A Cargo tier passes only when Cargo succeeded, at least one test ran, no test
failed, and every ignored test is one the runner declares as ignored on purpose:
the workload tests (run by `bounded-resources`) and the two maintenance actions
that rewrite committed fixtures. A tier that ran no test, or that ignored any
other test, does not pass.

`campaigns` runs each batch as its own test run and reads the report line the
test prints. A batch passes only when its test ran and passed and its report
shows the budget it was given: every generated sequence of at least 64
attempted operations, every requested case accepted, and the rejected
generated cases recorded as a measured count (rejections are reported, not
forbidden; when the report states the attempted cases, accepted plus rejected
must equal them); in the crash and failure campaigns, exactly the
configured cases, a reported nonzero number of cut points, every cut or exactly
the sampled cuts per case executed, and recoveries both before and after the
step; in the decoder sweep, every decoder target reported with exactly its
per-target budget. The totals must reach at least 1,000 generated
sequences on the simulator and 10,000 decoder cases. Anything short or missing
is listed as a gap and the tier is `incomplete`, never passed.

`--fresh-clone` clones the candidate commit into the output directory and runs
the tiers there with a new Cargo target directory, so no build output of the
working copy takes part. It refuses a working copy with uncommitted changes,
which a clone would silently leave out. It needs only the repository,
the toolchain pinned in `rust-toolchain.toml`, Python 3 and the crates locked in
`Cargo.lock`.

## Compare suites against planted defects

The comparative runner pins two suite revisions and verifies their production
source files and build pins match the selected production baseline. It runs test
discovery and the healthy workspace suite first, then applies each catalog
defect separately to disposable checkouts and executes its named witness.

```text
python3 tools/testkit/sidebyside.py --original origin/master --candidate HEAD --output /tmp/cce-comparison-1
```

The output directory must be new. Use `--fault short-write` (repeatable) for a
focused campaign, `--production REF` to name the common production baseline,
and `--timeout SECONDS` to bound each command. Full history must be available
locally. The command records resolved revisions, source and catalog hashes,
toolchain observations, inventories, commands, timing, logs and assertion
locations. `results.json` and `comparison.md` are written incrementally; retained
test images are under `temporary/`. The summary supplies a fresh-output rerun
command. The operator checkout is never reset or patched.

The runner builds and runs the healthy workspace suite for both revisions and then
builds and runs once per selected defect, each at Cargo's default parallelism (one
job and one test thread per available core). On a shared or constrained machine,
bound it explicitly, for example:

```text
CARGO_BUILD_JOBS=4 RUST_TEST_THREADS=4 python3 tools/testkit/sidebyside.py --original origin/master --candidate HEAD --output /tmp/cce-comparison-1
```

`tools/testkit/faults.json` defines each literal edit, expected anchor count,
named test and witness file. Anchors must match before the edit is applied.
Test-support declarations in `lib.rs` and the trailing unit-test module in
`io.rs` are excluded from production comparison; ordinary source files and
build pins remain compared. This is specific to the current workspace layout.
Update the comparison rules explicitly if that layout changes.

A detected defect requires execution of the selected test and a failure at a
behavioral assertion, explicit error expectation, declared successful-operation
expectation, or declared refusal witness. A successful-operation expectation
specifies the exact source line and the contract it checks; for example, every
byte must be readable after a write reports success. Unscoped `unwrap` failures
are setup errors. Build/setup failures, empty selections and aborted
or timed-out processes are separate results. The command succeeds only when
both suites detect every selected defect. A survivor in either suite leaves
the campaign failed, with its evidence retained for investigation. The catalog
demonstrates sensitivity to those defects, not universal suite equivalence.

The `testkit` workflow runs this bounded campaign on `test-support/**` branches
and supports manual dispatch with an original-suite ref. It uploads the report,
logs and test images on success or failure. Routine formatting, Clippy, workspace
tests and deterministic drift validation keep their existing independent jobs.

## Performance comparison

`tools/perf/run_perf.py` compares a candidate revision with a base revision:

```text
python3 tools/perf/run_perf.py --base origin/master --candidate HEAD --output /tmp/cce-perf-1
```

Both revisions are checked out as detached worktrees under the output
directory, which must be new. The measurement harness (the `perf` example,
`tests/workloads.rs`, `tests/common/` and the storage driver) is copied from the
candidate into the base checkout, so both sides run the same harness against
their own storage code. A base that does not build with it is `not-comparable`.
Both sides are built in release mode. Each side then runs `--trials` independent
trials (default 5, at least 2) in an interleaved order drawn from `--seed`
(random and recorded when omitted). A trial is one `perf` process over every
scenario, followed by one run of the workload tier, each in a fresh directory.
`--timeout` bounds each command, together with every process it starts.

The `perf` example (`crates/cce-storage/examples/perf.rs`) uses the public
storage API only. It touches no file itself, and its directory is supplied by
the caller. Its scenarios are point reads, relationship paging and lookups,
commits (admission and completion timed separately), and staging and reading
32 MiB of content. Each scenario builds deterministic inputs and checks every
result against expectations computed from those inputs. A wrong result fails
the scenario rather than producing a measurement. Each scenario prints one
`PERF` line, which states the work done, the engine counter deltas over the
measured phase and the elapsed times. The scenario documentation states what
each interval includes. Counters are read with `stats()` only before and after
the measured phase. A scenario's version changes whenever its inputs,
operations or boundaries change.

Engine counters decide the result. Counters and work counts must be identical
across one side's trials, otherwise the run is `invalid`. A cost counter (lower
is better) that is higher for the candidate is a regression, and a lower one is
an improvement. Other counters are reported as changed or unchanged. Timings,
resident memory, descriptor counts and the workloads' `elapsed_ms` vary between
runs. They are reported with their median and range over trials and never
change the result. The OS page cache is not controlled.

`results.json` holds:

- the resolved revisions and the harness digests;
- the toolchain and the observed environment;
- the seed and trial order;
- every command with its exit status, timing and log;
- every trial's parsed output;
- the per-metric comparison and the status.

`summary.md` is the readable report, and `logs/` holds each command's output.
The directory of a failed trial is kept under `work/`. The status is `passed`,
`regressed`, `incomplete` (an expected scenario, workload or metric is
missing), `invalid`, `not-comparable` or `error`. The command exits 0 only for
`passed`.

The `perf` workflow runs this on every branch push. The base is the merge base
with master, or the previous master commit on master. The workflow is separate
from the `ci` gate. It writes the summary to the job summary and keeps the
bundle as an artifact for 30 days.
