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
before dropping the owner. Cleanup errors are reported without replacing the
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

`tools/testkit/faults.json` defines each literal edit, expected anchor count,
named test and witness file. Anchors must match before the edit is applied.
Test-support declarations in `lib.rs` and the trailing unit-test module in
`io.rs` are excluded from production comparison; ordinary source files and
build pins remain compared. This is specific to the current workspace layout.
Update the comparison rules explicitly if that layout changes.

A detected defect requires execution of the selected test and a failure at a
behavioral assertion, explicit error expectation, or declared refusal witness.
Build/setup failures, ordinary `unwrap` failures, empty selections and aborted
or timed-out processes are separate results. The command succeeds only when
both suites detect every selected defect. A survivor in either suite leaves
the campaign failed, with its evidence retained for investigation. The catalog
demonstrates sensitivity to those defects, not universal suite equivalence.

The `testkit` workflow runs this bounded campaign on `test-support/**` branches
and supports manual dispatch with an original-suite ref. It uploads the report,
logs and test images on success or failure. Routine formatting, Clippy, workspace
tests and deterministic drift validation keep their existing independent jobs.
