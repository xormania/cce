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
