//! The `cce-verify` command on real store directories: its exit status, its JSON output, and that
//! it never changes a store.

mod common;

use std::path::Path;
use std::process::{Command, Output};
use std::sync::{Mutex, MutexGuard};

use cce_core::storage::{AttemptId, Change, ChangeSet, ContextId, Precondition, StorageEngine};
use cce_storage::NativeStore;

/// Held for each test's whole body. The store lock belongs to the open file, and a child process
/// keeps a copy of every open file until it starts its program: a child spawned by one test while
/// another test's store is open keeps that store locked for a moment after it closes, so the other
/// test's own reopen could be refused as `Busy`. Tests here therefore never overlap.
fn serial() -> MutexGuard<'static, ()> {
    static SERIAL: Mutex<()> = Mutex::new(());
    SERIAL.lock().unwrap_or_else(|e| e.into_inner())
}

fn run(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_cce-verify"))
        .args(args)
        .output()
        .expect("run cce-verify")
}

fn files(store: &Path) -> Vec<Vec<u8>> {
    ["meta.cce", "content.cce"]
        .map(|f| std::fs::read(store.join(f)).unwrap())
        .to_vec()
}

/// A store with one commit of `n` contexts, closed, and the guard of the directory holding it
/// (removed when the test passes).
fn store(name: &str, n: usize) -> (common::TestDir, std::path::PathBuf) {
    let base = common::test_dir(name);
    let dir = base.join("store");
    let s = NativeStore::create(&dir, *b"verify-cli-store").unwrap();
    let changes = (0..n)
        .map(|i| Change::PutContext {
            id: ContextId::new(format!("ctx-{i:04}").into_bytes()).unwrap(),
            expect: Precondition::Any,
            body: format!("body {i}").into_bytes(),
            content: None,
        })
        .collect();
    s.commit(
        AttemptId::new(*b"bulk").unwrap(),
        s.begin().unwrap(),
        ChangeSet {
            changes,
            ..ChangeSet::default()
        },
    )
    .unwrap();
    s.close().unwrap();
    (base, dir)
}

#[test]
fn a_consistent_store_exits_zero_with_json_lines_and_is_not_changed() {
    let _serial = serial();
    let (_base, dir) = store("verify-ok", 300);
    let before = files(&dir);
    let out = run(&[dir.to_str().unwrap(), "--json"]);
    assert_eq!(
        out.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let text = String::from_utf8(out.stdout).unwrap();
    let lines: Vec<serde_json::Value> = text
        .lines()
        .map(|l| serde_json::from_str(l).expect("each line is one JSON object"))
        .collect();
    assert_eq!(lines[0]["type"], "summary");
    assert_eq!(lines[0]["history_position"], 1);
    assert_eq!(lines.last().unwrap()["type"], "result");
    assert_eq!(lines.last().unwrap()["consistent"], true);
    assert_eq!(files(&dir), before, "verify changed the store");

    let human = run(&[dir.to_str().unwrap()]);
    assert_eq!(human.status.code(), Some(0));
    assert!(String::from_utf8_lossy(&human.stdout).ends_with("consistent\n"));
}

#[test]
fn a_damaged_reachable_page_exits_one_and_names_it() {
    let _serial = serial();
    // One commit of many contexts writes its leaves before its root, so the page after the empty
    // store's root is a reachable leaf; the root itself (checked by open) is untouched.
    let (_base, dir) = store("verify-damaged", 300);
    let meta = dir.join("meta.cce");
    let mut bytes = std::fs::read(&meta).unwrap();
    let leaf = 3 * 4096;
    bytes[leaf + 100] ^= 0xff;
    std::fs::write(&meta, &bytes).unwrap();
    let out = run(&[dir.to_str().unwrap(), "--json"]);
    assert_eq!(
        out.status.code(),
        Some(1),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let text = String::from_utf8(out.stdout).unwrap();
    assert!(
        text.lines()
            .any(|l| l.contains("\"check\":\"page\"") && l.contains("page 3")),
        "{text}"
    );
    assert_eq!(
        std::fs::read(&meta).unwrap(),
        bytes,
        "verify changed the store"
    );
}

#[test]
fn a_store_that_cannot_be_inspected_exits_two() {
    let _serial = serial();
    // Owned by a live process: the read-only open is refused, so nothing is inspected.
    let (_base, dir) = store("verify-owned", 3);
    let before = files(&dir);
    let owner = NativeStore::open(&dir).unwrap();
    let out = run(&[dir.to_str().unwrap()]);
    assert_eq!(out.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&out.stderr).contains("cannot inspect"));
    drop(owner);
    assert_eq!(files(&dir), before);

    let missing_base = common::test_dir("verify-missing");
    let missing = missing_base.join("nothing-here");
    assert_eq!(run(&[missing.to_str().unwrap()]).status.code(), Some(2));
    assert_eq!(run(&[]).status.code(), Some(2), "no store argument");
    assert_eq!(
        run(&["a", "b"]).status.code(),
        Some(2),
        "two store arguments"
    );
}

#[test]
fn a_scrub_reads_content_and_exits_one_on_a_damaged_segment() {
    let _serial = serial();
    use cce_core::storage::ContentBinding;
    let base = common::test_dir("verify-scrub");
    let dir = base.join("store");
    let s = NativeStore::create(&dir, *b"verify-cli-store").unwrap();
    let payload: Vec<u8> = (0..70_000u32).map(|i| (i % 251) as u8).collect();
    let staged = s.stage_content(&mut &payload[..]).unwrap();
    s.commit(
        AttemptId::new(*b"with-content").unwrap(),
        s.begin().unwrap(),
        ChangeSet {
            changes: vec![Change::PutContext {
                id: ContextId::new(*b"A").unwrap(),
                expect: Precondition::Any,
                body: b"a".to_vec(),
                content: Some(0),
            }],
            content: vec![ContentBinding::from(&staged)],
            ..ChangeSet::default()
        },
    )
    .unwrap();
    s.close().unwrap();
    let ok = run(&[dir.to_str().unwrap(), "--scrub", "--json"]);
    assert_eq!(
        ok.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&ok.stderr)
    );
    let summary: serde_json::Value = serde_json::from_str(
        String::from_utf8(ok.stdout)
            .unwrap()
            .lines()
            .next()
            .unwrap(),
    )
    .unwrap();
    assert_eq!(summary["content_bytes"], 70_000);

    // The stream's first segment starts the content file.
    let content = dir.join("content.cce");
    let mut bytes = std::fs::read(&content).unwrap();
    bytes[10] ^= 0xff;
    std::fs::write(&content, &bytes).unwrap();
    assert_eq!(
        run(&[dir.to_str().unwrap()]).status.code(),
        Some(0),
        "structure reads no content"
    );
    let bad = run(&[dir.to_str().unwrap(), "--scrub", "--json"]);
    assert_eq!(bad.status.code(), Some(1));
    let text = String::from_utf8(bad.stdout).unwrap();
    assert!(text.contains("\"check\":\"content\""), "{text}");
    assert_eq!(
        std::fs::read(&content).unwrap(),
        bytes,
        "a scrub changed the store"
    );
}
