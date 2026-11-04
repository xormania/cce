//! The `cce-verify` command on real store directories: its exit status, its JSON output, and that
//! it never changes a store.

mod common;

use std::path::Path;
use std::process::{Command, Output};

use cce_core::storage::{AttemptId, Change, ChangeSet, ContextId, Precondition, StorageEngine};
use cce_storage::NativeStore;

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

/// A store with one commit of `n` contexts, closed.
fn store(name: &str, n: usize) -> std::path::PathBuf {
    let dir = common::test_dir(name).join("store");
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
    dir
}

#[test]
fn a_consistent_store_exits_zero_with_json_lines_and_is_not_changed() {
    let dir = store("verify-ok", 300);
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
    // One commit of many contexts writes its leaves before its root, so the page after the empty
    // store's root is a reachable leaf; the root itself (checked by open) is untouched.
    let dir = store("verify-damaged", 300);
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
    // Owned by a live process: the read-only open is refused, so nothing is inspected.
    let dir = store("verify-owned", 3);
    let before = files(&dir);
    let owner = NativeStore::open(&dir).unwrap();
    let out = run(&[dir.to_str().unwrap()]);
    assert_eq!(out.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&out.stderr).contains("cannot inspect"));
    drop(owner);
    assert_eq!(files(&dir), before);

    let missing = common::test_dir("verify-missing").join("nothing-here");
    assert_eq!(run(&[missing.to_str().unwrap()]).status.code(), Some(2));
    assert_eq!(run(&[]).status.code(), Some(2), "no store argument");
    assert_eq!(
        run(&["a", "b"]).status.code(),
        Some(2),
        "two store arguments"
    );
}
