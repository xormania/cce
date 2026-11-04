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
| `lint` | Clippy over the workspace and all targets, warnings denied |
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
`logs/`. The runner exits 0 only when every selected tier passed.

A Cargo tier passes only when Cargo succeeded, at least one test ran, no test
failed, and every ignored test is one the runner declares as ignored on purpose:
the workload tests (run by `bounded-resources`) and the two maintenance actions
that rewrite committed fixtures. A tier that ran no test, or that ignored any
other test, does not pass.

`campaigns` runs each batch as its own test run and reads the report line the
test prints. A batch passes only when its test ran and passed and its report
shows the budget it was given: every generated sequence of at least 64
attempted operations and no rejected generated case; every cut, or exactly the
sampled cuts, executed in the crash and failure campaigns, with recoveries both
before and after the step. The totals must reach at least 1,000 generated
sequences on the simulator and 10,000 decoder cases. Anything short or missing
is listed as a gap and the tier is `incomplete`, never passed.

`--fresh-clone` clones the candidate commit into the output directory and runs
the tiers there with a new Cargo target directory, so neither build output nor
uncommitted files of the working copy take part. It needs only the repository,
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
