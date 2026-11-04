//! End-to-end tests: a parent test process drives the native engine through a separate driver
//! process and checks results against independently computed expectations.

mod common;

use std::time::Duration;

use common::story::first_bundle;
use common::{Driver, Step, answered, test_dir};

#[test]
fn harness_distinguishes_answer_exit_and_hang() {
    let mut ok = Driver::spawn();
    assert_eq!(ok.step("probe ok"), Step::Answered(vec!["probe ok".into()]));
    assert_eq!(ok.finish().and_then(|s| s.code()), Some(0));

    let mut failing = Driver::spawn();
    assert_eq!(
        failing.step("probe fail"),
        Step::Exited {
            lines: vec!["probe failing".into()],
            code: Some(3)
        }
    );

    let mut hanging = Driver::spawn();
    assert_eq!(
        hanging.step_with_deadline("probe hang", Duration::from_millis(300)),
        Step::Hung {
            lines: vec!["probe hanging".into()]
        }
    );
    let status = hanging.kill();
    assert!(
        !status.success(),
        "a killed hanging driver must not report success"
    );
}

#[test]
fn committed_bundle_survives_writer_kill_and_fresh_process_reopen() {
    let dir = test_dir("bundle-kill-reopen");
    let store = dir.join("store");
    let bundle = first_bundle();

    // Writer process: create, stage, submit one complete transaction, then hold without shutdown.
    let mut writer = Driver::spawn();
    answered(
        writer.step(&format!("create {}", store.display())),
        "create",
    );
    for cmd in bundle.commands() {
        answered(writer.step(&cmd), &cmd);
    }
    let receipt = answered(writer.step(&format!("commit {}", bundle.attempt)), "commit");
    assert_eq!(
        receipt,
        vec!["committed 1".to_string()],
        "commit must be acknowledged at position 1"
    );
    assert_eq!(writer.wait_for("hold", "holding"), Step::Answered(vec![]));
    let status = writer.kill();
    assert!(
        status.code().is_none(),
        "writer must die by signal, not exit: {status:?}"
    );

    // Fresh process: acquire the store and observe the complete committed state.
    let mut reader = Driver::spawn();
    answered(reader.step(&format!("open {}", store.display())), "open");
    let dump = answered(reader.step("dump A,B,C"), "dump");
    let expected = bundle.expected_dump(1);
    assert_eq!(dump.len(), expected.len(), "dump line count");
    for (got, want) in dump.iter().zip(&expected) {
        assert_eq!(got, want);
    }
    answered(reader.step("close"), "close");
}

#[test]
fn clean_close_and_reopen_preserve_the_complete_bundle() {
    let dir = test_dir("bundle-clean-reopen");
    let store = dir.join("store");
    let bundle = first_bundle();

    let mut writer = Driver::spawn();
    answered(
        writer.step(&format!("create {}", store.display())),
        "create",
    );
    for cmd in bundle.commands() {
        answered(writer.step(&cmd), &cmd);
    }
    assert_eq!(
        answered(writer.step(&format!("commit {}", bundle.attempt)), "commit"),
        vec!["committed 1"]
    );
    answered(writer.step("close"), "close");
    assert_eq!(
        writer.finish().and_then(|s| s.code()),
        Some(0),
        "writer must exit cleanly after close"
    );

    let mut reader = Driver::spawn();
    answered(reader.step(&format!("open {}", store.display())), "open");
    assert_eq!(
        answered(reader.step("dump A,B,C"), "dump"),
        bundle.expected_dump(1)
    );
}

#[test]
fn staged_content_is_not_accepted_context() {
    let dir = test_dir("staged-not-accepted");
    let store = dir.join("store");
    let staged = b"staged but never committed";
    let digest = common::hex(&common::sha256(staged));

    let mut d = Driver::spawn();
    answered(d.step(&format!("create {}", store.display())), "create");
    answered(
        d.step(&format!("stage s hex {}", common::hex(staged))),
        "stage",
    );
    // Neither the content nor any context referring to it is visible as committed.
    let read = answered(
        d.step(&format!("read-content {digest} {}", staged.len())),
        "read staged",
    );
    assert_eq!(read.len(), 1);
    assert!(
        read[0].starts_with("content-error InvalidRequest"),
        "staged bytes readable: {read:?}"
    );
    assert_eq!(
        answered(d.step("dump A"), "dump"),
        vec!["context A absent".to_string()]
    );

    // Positive neighbour: once committed, the same bytes are readable through the same command.
    answered(
        d.step(&format!("put-context A absent {} s", common::hex(b"a"))),
        "put",
    );
    assert_eq!(
        answered(d.step("commit attempt-staged"), "commit"),
        vec!["committed 1"]
    );
    assert_eq!(
        answered(
            d.step(&format!("read-content {digest} {}", staged.len())),
            "read committed"
        ),
        vec![format!("content {}", common::hex(staged))]
    );
}

#[test]
fn supplied_evidence_is_recorded_byte_for_byte_without_interpretation() {
    let dir = test_dir("evidence-opaque");
    let store = dir.join("store");
    let bundle = first_bundle();
    let mut d = Driver::spawn();
    answered(d.step(&format!("create {}", store.display())), "create");
    for cmd in bundle.commands() {
        answered(d.step(&cmd), &cmd);
    }
    assert_eq!(
        answered(d.step(&format!("commit {}", bundle.attempt)), "commit"),
        vec!["committed 1"]
    );

    // A later attempt relabels its evidence and supplies bytes that read like a refusal.
    // Storage must record them unchanged and must not treat them as a decision.
    let label = b"decision-relabelled";
    let bytes = b"TEST-EVIDENCE: host says DENY \x00\xff (storage must still record this)";
    answered(
        d.step(&format!("put-context D absent {} -", common::hex(b"d"))),
        "put D",
    );
    answered(
        d.step(&format!(
            "evidence {} {}",
            common::hex(label),
            common::hex(bytes)
        )),
        "evidence",
    );
    assert_eq!(
        answered(d.step("commit attempt-0002"), "commit"),
        vec!["committed 2"]
    );

    let dump = answered(d.step("dump D"), "dump");
    let want = format!("evidence 2 0 {} {}", common::hex(label), common::hex(bytes));
    assert!(
        dump.contains(&want),
        "evidence not returned byte-for-byte:\n{dump:#?}"
    );
    // The first attempt's evidence is unchanged.
    for (i, (l, b)) in bundle.evidence.iter().enumerate() {
        assert!(dump.contains(&format!(
            "evidence 1 {i} {} {}",
            common::hex(l),
            common::hex(b)
        )));
    }
}

/// The driver's explicit error for a step that must fail.
#[track_caller]
fn failed(step: Step, what: &str) -> String {
    let lines = match step {
        Step::Answered(lines) | Step::Exited { lines, .. } => lines,
        Step::Hung { lines } => panic!("{what}: hung after {lines:?}"),
    };
    lines
        .iter()
        .find(|l| l.starts_with("error "))
        .cloned()
        .unwrap_or_else(|| panic!("{what}: expected an explicit error, got {lines:?}"))
}

/// Every entry of the store directory with the SHA-256 of its bytes, in name order.
fn directory_image(store: &std::path::Path) -> Vec<(String, String)> {
    use sha2::{Digest, Sha256};
    let mut entries: Vec<(String, String)> = std::fs::read_dir(store)
        .expect("read store directory")
        .map(|e| {
            let e = e.expect("directory entry");
            let bytes = std::fs::read(e.path()).expect("read store file");
            (
                e.file_name().to_string_lossy().into_owned(),
                common::hex(&Sha256::digest(&bytes)),
            )
        })
        .collect();
    entries.sort();
    entries
}

#[test]
fn read_only_opens_are_refused_while_owned_and_never_change_the_store() {
    let dir = test_dir("read-only-open");
    let store = dir.join("store");
    let open = format!("open {}", store.display());
    let open_ro = format!("open-ro {}", store.display());
    let bundle = first_bundle();

    let mut owner = Driver::spawn();
    answered(owner.step(&format!("create {}", store.display())), "create");
    for cmd in bundle.commands() {
        answered(owner.step(&cmd), &cmd);
    }
    assert_eq!(
        answered(owner.step(&format!("commit {}", bundle.attempt)), "commit"),
        ["committed 1"]
    );
    // While a process owns the store, both kinds of open from another process are busy.
    for cmd in [&open, &open_ro] {
        let mut other = Driver::spawn();
        let err = failed(other.step(cmd), cmd);
        assert!(err.contains("Busy"), "{cmd}: {err}");
    }
    assert_eq!(owner.wait_for("hold", "holding"), Step::Answered(vec![]));
    assert!(owner.kill().code().is_none(), "owner must die by signal");
    let before = directory_image(&store);

    // After the owner died: read-only opens succeed, coexist, and see the committed state.
    let mut ro = Driver::spawn();
    answered(ro.step(&open_ro), "first read-only open");
    let mut ro2 = Driver::spawn();
    answered(ro2.step(&open_ro), "second read-only open");
    for (what, d) in [("first", &mut ro), ("second", &mut ro2)] {
        assert_eq!(
            answered(d.step("dump A,B,C"), "dump"),
            bundle.expected_dump(1),
            "{what} read-only view"
        );
    }
    // A read-only holder keeps writers out.
    let mut writer = Driver::spawn();
    let err = failed(writer.step(&open), "writer under readers");
    assert!(err.contains("Busy"), "{err}");
    answered(ro2.step("close"), "close second");
    assert_eq!(ro2.finish().and_then(|s| s.code()), Some(0));
    // A content reader keeps the read-only hold after its handle is dropped.
    answered(ro.step("keep-reader A"), "keep reader");
    answered(ro.step("drop-store"), "drop read-only handle");
    let mut writer = Driver::spawn();
    let err = failed(writer.step(&open), "writer under a kept reader");
    assert!(err.contains("Busy"), "{err}");
    answered(ro.step("drop-readers"), "drop readers");
    assert_eq!(ro.finish().and_then(|s| s.code()), Some(0));

    // Read-only sessions changed nothing on disk, not even the lock file.
    assert_eq!(
        directory_image(&store),
        before,
        "read-only opens modified the store"
    );

    // A read-write open now acquires ownership and recovers the committed state.
    let mut fresh = Driver::spawn();
    answered(fresh.step(&open), "read-write open after readers");
    assert_eq!(
        answered(fresh.step("dump A,B,C"), "dump"),
        bundle.expected_dump(1)
    );
    answered(fresh.step("close"), "close");
}

#[test]
fn store_files_are_real_and_damage_yields_explicit_errors() {
    let dir = test_dir("files-explicit-errors");
    let store = dir.join("store");
    let bundle = first_bundle();
    let mut d = Driver::spawn();
    answered(d.step(&format!("create {}", store.display())), "create");
    for cmd in bundle.commands() {
        answered(d.step(&cmd), &cmd);
    }
    answered(d.step(&format!("commit {}", bundle.attempt)), "commit");
    answered(d.step("close"), "close");
    drop(d);

    // The store consists of real files with real content.
    let meta = store.join("meta.cce");
    let content = store.join("content.cce");
    assert!(std::fs::metadata(&meta).unwrap().len() >= 3 * 4096);
    assert!(std::fs::metadata(&content).unwrap().len() >= common::story::LARGE_LEN);

    let open_error = |path: &std::path::Path| -> String {
        let mut d = Driver::spawn();
        failed(
            d.step(&format!("open {}", path.display())),
            "open of a damaged store",
        )
    };

    // Creating over an existing store is refused and leaves it intact.
    let before = std::fs::read(&meta).unwrap();
    let mut d = Driver::spawn();
    let err = failed(
        d.step(&format!("create {}", store.display())),
        "create over a store",
    );
    assert!(err.contains("already contains a store"), "{err}");
    assert_eq!(
        std::fs::read(&meta).unwrap(),
        before,
        "refused create modified the store"
    );

    // A missing directory is not silently created.
    let missing = dir.join("no-such-store");
    assert!(open_error(&missing).contains("no store directory"));
    assert!(!missing.exists(), "open created a directory");

    // A missing content file is reported, not recreated.
    let copy = dir.join("copy-missing-content");
    std::fs::create_dir_all(&copy).unwrap();
    std::fs::copy(&meta, copy.join("meta.cce")).unwrap();
    assert!(open_error(&copy).contains("content.cce is missing"));
    assert!(!copy.join("content.cce").exists());

    // Damaged publication records: refuse, never reinitialize, never rewrite.
    let damaged = dir.join("copy-damaged-slots");
    std::fs::create_dir_all(&damaged).unwrap();
    let mut bytes = before.clone();
    for b in &mut bytes[..2 * 4096] {
        *b ^= 0x5a;
    }
    std::fs::write(damaged.join("meta.cce"), &bytes).unwrap();
    std::fs::copy(&content, damaged.join("content.cce")).unwrap();
    assert!(open_error(&damaged).contains("no valid publication record"));
    assert_eq!(
        std::fs::read(damaged.join("meta.cce")).unwrap(),
        bytes,
        "failed open modified the store"
    );

    // A truncated metadata file is an explicit incomplete-initialization error.
    let short = dir.join("copy-truncated");
    std::fs::create_dir_all(&short).unwrap();
    std::fs::write(short.join("meta.cce"), &before[..100]).unwrap();
    std::fs::copy(&content, short.join("content.cce")).unwrap();
    assert!(open_error(&short).contains("initialization never completed"));

    // Positive neighbour: the untouched store still opens with its full state.
    let mut d = Driver::spawn();
    answered(d.step(&format!("open {}", store.display())), "open");
    assert_eq!(
        answered(d.step("dump A,B,C"), "dump"),
        bundle.expected_dump(1)
    );
}

#[test]
fn a_store_has_one_owning_process_until_every_guard_is_gone() {
    let dir = test_dir("ownership");
    let store = dir.join("store");
    let bundle = first_bundle();
    let mut owner = Driver::spawn();
    answered(owner.step(&format!("create {}", store.display())), "create");
    for cmd in bundle.commands() {
        answered(owner.step(&cmd), &cmd);
    }
    answered(owner.step(&format!("commit {}", bundle.attempt)), "commit");

    let busy = |what: &str| {
        let mut other = Driver::spawn();
        let err = failed(other.step(&format!("open {}", store.display())), what);
        assert!(err.contains("Busy"), "{what}: {err}");
    };
    busy("second opener while owned");

    // Dropping the store handle does not release ownership while a content reader remains.
    answered(owner.step("keep-reader A"), "keep reader");
    answered(owner.step("drop-store"), "drop store");
    busy("second opener while a reader keeps the engine");
    answered(owner.step("drop-readers"), "drop readers");
    let mut next = Driver::spawn();
    answered(
        next.step(&format!("open {}", store.display())),
        "open after release",
    );
    drop(next);

    // After the owner process dies, a fresh process acquires ownership and recovers.
    let mut owner = Driver::spawn();
    answered(owner.step(&format!("open {}", store.display())), "reopen");
    assert_eq!(owner.wait_for("hold", "holding"), Step::Answered(vec![]));
    busy("second opener while a holding process lives");
    assert!(owner.kill().code().is_none());
    let mut fresh = Driver::spawn();
    answered(
        fresh.step(&format!("open {}", store.display())),
        "open after owner death",
    );
    assert_eq!(
        answered(fresh.step("dump A,B,C"), "dump"),
        bundle.expected_dump(1)
    );
}

#[test]
fn an_identical_retry_from_a_fresh_process_returns_the_original_receipt() {
    let dir = test_dir("retry-fresh-process");
    let store = dir.join("store");
    let bundle = first_bundle();
    let commit = format!("commit {}", bundle.attempt);
    let outcome = format!("outcome {}", bundle.attempt);

    // Process A commits, is acknowledged, and is killed before any shutdown.
    let mut a = Driver::spawn();
    answered(a.step(&format!("create {}", store.display())), "create");
    for cmd in bundle.commands() {
        answered(a.step(&cmd), &cmd);
    }
    assert_eq!(answered(a.step(&commit), "commit"), ["committed 1"]);
    assert_eq!(a.wait_for("hold", "holding"), Step::Answered(vec![]));
    let status = a.kill();
    assert!(status.code().is_none(), "A must die by signal: {status:?}");

    // Process B: the outcome is already known, and re-staging the identical bytes under new
    // handles and retrying the identical request returns the original receipt without a new
    // history entry.
    let mut b = Driver::spawn();
    answered(b.step(&format!("open {}", store.display())), "open");
    assert_eq!(
        answered(b.step(&outcome), "outcome"),
        ["outcome committed 1"]
    );
    for cmd in bundle.commands() {
        answered(b.step(&cmd), &cmd);
    }
    assert_eq!(answered(b.step(&commit), "retry"), ["committed 1"]);
    assert_eq!(
        answered(b.step("dump A,B,C"), "dump"),
        bundle.expected_dump(1)
    );
    answered(b.step("close"), "close");
    assert_eq!(b.finish().and_then(|s| s.code()), Some(0));

    // Process C: the same attempt with one more piece of evidence is a different request.
    let mut c = Driver::spawn();
    answered(c.step(&format!("open {}", store.display())), "open");
    for cmd in bundle.commands() {
        answered(c.step(&cmd), &cmd);
    }
    answered(c.step("evidence 6578747261 6d6f7265"), "evidence");
    assert_eq!(
        answered(c.step(&commit), "mismatched retry"),
        ["commit-error AttemptMismatch"]
    );
    assert_eq!(
        answered(c.step("dump A,B,C"), "dump"),
        bundle.expected_dump(1)
    );
    assert_eq!(
        answered(c.step(&outcome), "outcome"),
        ["outcome committed 1"]
    );
    answered(c.step("close"), "close");
}

#[test]
fn a_short_write_at_the_file_size_limit_fails_the_commit_explicitly() {
    let dir = test_dir("short-write");
    let store = dir.join("store");
    let content = store.join("content.cce");
    let meta = store.join("meta.cce");
    // Evidence is recorded only in the history entry, so it sets that write's size.
    let evidence = format!("evidence {} {}", common::hex(b"big"), "ab".repeat(30_000));

    let mut setup = Driver::spawn();
    answered(setup.step(&format!("create {}", store.display())), "create");
    // Enough committed content that the content file outgrows the metadata file: the limit below
    // then constrains only the history write, not the metadata pages after it.
    answered(setup.step("stage big gen 100000 7"), "stage");
    answered(setup.step("put-context A absent 61 big"), "put A");
    assert_eq!(
        answered(setup.step("commit a1"), "commit a1"),
        ["committed 1"]
    );
    answered(setup.step("close"), "close");
    assert_eq!(setup.finish().and_then(|s| s.code()), Some(0));

    // A file size limit 1-2 KiB past the committed content: the next history entry (about
    // 30 KB) is written partly, then the kernel refuses the rest. SIGXFSZ is ignored so the
    // refusal arrives as an error, as it would for a full disk.
    let committed = std::fs::metadata(&content).unwrap().len();
    let limit_kib = committed / 1024 + 2;
    let limit = limit_kib * 1024;
    assert!(limit - committed > 1024 && limit - committed <= 2048);
    let meta_len = std::fs::metadata(&meta).unwrap().len();
    assert!(
        meta_len + 64 * 1024 < limit,
        "metadata ({meta_len} bytes) must stay well below the limit ({limit})"
    );
    let mut limited = Driver::spawn_with_shell(&format!("trap '' XFSZ; ulimit -f {limit_kib}"));
    answered(limited.step(&format!("open {}", store.display())), "open");
    answered(limited.step("put-context B absent 62 -"), "put B");
    answered(limited.step(&evidence), "evidence");
    let r = answered(limited.step("commit a2"), "commit a2");
    assert!(
        r.len() == 1 && r[0].starts_with("commit-error Rejected(Io("),
        "a partially written commit must fail explicitly: {r:?}"
    );
    assert_eq!(
        std::fs::metadata(&content).unwrap().len(),
        limit,
        "the history write did not stop short at the limit"
    );
    assert_eq!(
        answered(limited.step("outcome a2"), "outcome"),
        ["outcome absent"]
    );
    let dump = answered(limited.step("dump A,B"), "dump");
    assert!(dump.iter().any(|l| l == "context B absent"), "{dump:?}");
    drop(limited);

    // A fresh process without the limit sees only the first commit, and the retry commits.
    let mut fresh = Driver::spawn();
    answered(fresh.step(&format!("open {}", store.display())), "open");
    assert_eq!(
        answered(fresh.step("outcome a2"), "outcome"),
        ["outcome absent"]
    );
    let dump = answered(fresh.step("dump A,B"), "dump");
    assert!(dump.iter().any(|l| l == "context B absent"), "{dump:?}");
    assert_eq!(
        dump.iter().filter(|l| l.starts_with("history ")).count(),
        1,
        "{dump:?}"
    );
    answered(fresh.step("put-context B absent 62 -"), "put B");
    answered(fresh.step(&evidence), "evidence");
    assert_eq!(answered(fresh.step("commit a2"), "retry"), ["committed 2"]);
    answered(fresh.step("close"), "close");
}
