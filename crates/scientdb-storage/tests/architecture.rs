//! Architecture checks: dependency direction, no borrowed database engine, a driver that uses
//! only the logical port, and no public physical modules. Each check is exercised against a
//! deliberately violating fixture as well as the real workspace.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::process::Command;

use serde_json::{Value, json};

const KERNEL: &str = "scientdb-kernel";
const STORAGE: &str = "scientdb-storage";

/// Database/storage engines that must never become the persistence path.
const FORBIDDEN_ENGINES: &[&str] = &[
    "rusqlite",
    "libsqlite3-sys",
    "sqlite",
    "sqlx",
    "diesel",
    "redb",
    "lmdb",
    "lmdb-rkv",
    "lmdb-sys",
    "heed",
    "rocksdb",
    "librocksdb-sys",
    "sled",
    "fjall",
    "persy",
    "jammdb",
    "nebari",
    "sanakirja",
    "tikv-client",
    "surrealdb",
    "polodb_core",
    "native_db",
    "agatedb",
    "speedb",
    "foundationdb",
    "duckdb",
    "limbo",
    "canopydb",
];

/// Violations of the dependency rules in `cargo metadata` output.
fn dependency_violations(meta: &Value) -> Vec<String> {
    let mut names = BTreeMap::new();
    for p in meta["packages"].as_array().into_iter().flatten() {
        names.insert(
            p["id"].as_str().unwrap_or_default(),
            p["name"].as_str().unwrap_or_default(),
        );
    }
    // Normal (non-dev) dependency edges by package name.
    let mut deps: BTreeMap<&str, BTreeSet<&str>> = BTreeMap::new();
    for node in meta["resolve"]["nodes"].as_array().into_iter().flatten() {
        let from = names
            .get(node["id"].as_str().unwrap_or_default())
            .copied()
            .unwrap_or("?");
        let entry = deps.entry(from).or_default();
        for d in node["deps"].as_array().into_iter().flatten() {
            let normal = d["dep_kinds"]
                .as_array()
                .into_iter()
                .flatten()
                .any(|k| k["kind"].is_null() || k["kind"] == "build");
            if normal {
                entry.insert(
                    names
                        .get(d["pkg"].as_str().unwrap_or_default())
                        .copied()
                        .unwrap_or("?"),
                );
            }
        }
    }
    let reach = |root: &str| {
        let mut seen = BTreeSet::new();
        let mut stack = vec![root];
        while let Some(n) = stack.pop() {
            for d in deps.get(n).into_iter().flatten() {
                if seen.insert(*d) {
                    stack.push(d);
                }
            }
        }
        seen
    };
    let mut v = Vec::new();
    let kernel = reach(KERNEL);
    if kernel.contains(STORAGE) {
        v.push("kernel depends on storage".into());
    }
    if !kernel.is_empty() {
        v.push(format!(
            "kernel contract has non-std dependencies: {kernel:?}"
        ));
    }
    let storage = reach(STORAGE);
    if !storage.contains(KERNEL) {
        v.push("storage does not depend on the kernel contract".into());
    }
    for name in names.values() {
        if FORBIDDEN_ENGINES.contains(name) {
            v.push(format!(
                "forbidden storage engine in dependency graph: {name}"
            ));
        }
    }
    v
}

/// Violations of the driver rule: only the logical port and the native-store constructor.
fn driver_violations(source: &str) -> Vec<String> {
    let mut v = Vec::new();
    for line in source.lines().map(str::trim) {
        if let Some(rest) = line.strip_prefix("use scientdb_storage::")
            && rest.trim_end_matches(';') != "NativeStore"
        {
            v.push(format!("driver imports storage internals: {line}"));
        }
        if line.contains("std::fs") || line.contains("File::") || line.contains("OpenOptions") {
            v.push(format!("driver touches files directly: {line}"));
        }
    }
    v
}

/// Violations of the narrow-surface rule: physical modules must stay private.
fn surface_violations(lib_rs: &str) -> Vec<String> {
    lib_rs
        .lines()
        .map(str::trim)
        .filter(|l| l.starts_with("pub mod ") || l.starts_with("pub use crate::"))
        .map(|l| format!("storage exposes internals: {l}"))
        .collect()
}

fn workspace_metadata() -> Value {
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
    let out = Command::new(cargo)
        .args(["metadata", "--format-version", "1", "--locked"])
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .output()
        .expect("run cargo metadata");
    assert!(
        out.status.success(),
        "cargo metadata failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    serde_json::from_slice(&out.stdout).expect("parse cargo metadata")
}

fn read(rel: &str) -> String {
    std::fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join(rel)).expect(rel)
}

fn pkg(id: &str) -> Value {
    json!({ "id": id, "name": id })
}

fn node(id: &str, deps: &[&str]) -> Value {
    json!({ "id": id, "deps": deps.iter().map(|d| json!({ "pkg": d, "dep_kinds": [{ "kind": null }] })).collect::<Vec<_>>() })
}

fn fixture(packages: &[&str], nodes: Vec<Value>) -> Value {
    json!({ "packages": packages.iter().map(|p| pkg(p)).collect::<Vec<_>>(), "resolve": { "nodes": nodes } })
}

#[test]
fn workspace_dependencies_point_toward_the_contract_without_a_borrowed_engine() {
    assert_eq!(
        dependency_violations(&workspace_metadata()),
        Vec::<String>::new()
    );
}

#[test]
fn dependency_check_rejects_inverted_direction_and_borrowed_engines() {
    let good = fixture(
        &[KERNEL, STORAGE],
        vec![node(KERNEL, &[]), node(STORAGE, &[KERNEL])],
    );
    assert_eq!(dependency_violations(&good), Vec::<String>::new());

    let inverted = fixture(
        &[KERNEL, STORAGE],
        vec![node(KERNEL, &[STORAGE]), node(STORAGE, &[KERNEL])],
    );
    assert!(
        dependency_violations(&inverted)
            .iter()
            .any(|v| v == "kernel depends on storage")
    );

    let detached = fixture(
        &[KERNEL, STORAGE],
        vec![node(KERNEL, &[]), node(STORAGE, &[])],
    );
    assert!(
        dependency_violations(&detached)
            .iter()
            .any(|v| v.contains("does not depend on the kernel"))
    );

    let borrowed = fixture(
        &[KERNEL, STORAGE, "helper", "redb"],
        vec![
            node(KERNEL, &[]),
            node(STORAGE, &[KERNEL, "helper"]),
            node("helper", &["redb"]),
            node("redb", &[]),
        ],
    );
    assert!(
        dependency_violations(&borrowed)
            .iter()
            .any(|v| v.ends_with("redb"))
    );
}

#[test]
fn driver_uses_only_the_logical_port() {
    assert_eq!(
        driver_violations(&read("src/bin/storage-driver.rs")),
        Vec::<String>::new()
    );
}

#[test]
fn driver_check_rejects_internal_imports_and_direct_file_access() {
    assert!(
        driver_violations(
            "use scientdb_storage::NativeStore;\nuse scientdb_kernel::storage::Change;"
        )
        .is_empty()
    );
    assert_eq!(
        driver_violations("use scientdb_storage::tree::Page;").len(),
        1
    );
    assert_eq!(driver_violations("let f = std::fs::read(path);").len(), 1);
}

#[test]
fn storage_exposes_no_physical_modules() {
    assert_eq!(
        surface_violations(&read("src/lib.rs")),
        Vec::<String>::new()
    );
}

#[test]
fn surface_check_rejects_public_physical_modules() {
    assert!(surface_violations("mod tree;\npub struct NativeStore;").is_empty());
    assert_eq!(surface_violations("pub mod tree;").len(), 1);
}

/// True if `code` mentions `pat` as a whole path segment (`SimFile::` does not mention `File::`).
fn mentions(code: &str, pat: &str) -> bool {
    code.match_indices(pat).any(|(i, _)| {
        code[..i]
            .chars()
            .next_back()
            .is_none_or(|c| !(c.is_alphanumeric() || c == '_'))
    })
}

/// Violations of the I/O-seam rule: only the I/O adapter module may touch the filesystem.
fn io_routing_violations(files: &[(String, String)]) -> Vec<String> {
    const FS: &[&str] = &["std::fs", "File::", "OpenOptions", "FileExt", "fs::"];
    let mut v = Vec::new();
    for (name, src) in files {
        if name == "io.rs" || name.starts_with("bin/") || name.ends_with("_tests.rs") {
            continue;
        }
        for (i, line) in src.lines().enumerate() {
            let code = line.split("//").next().unwrap_or("");
            if FS.iter().any(|p| mentions(code, p)) {
                v.push(format!(
                    "{name}:{}: filesystem access outside the I/O adapter",
                    i + 1
                ));
            }
        }
    }
    v
}

fn source_files() -> Vec<(String, String)> {
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut out = Vec::new();
    let mut stack = vec![src.clone()];
    while let Some(dir) = stack.pop() {
        for e in std::fs::read_dir(&dir).unwrap() {
            let p = e.unwrap().path();
            if p.is_dir() {
                stack.push(p);
            } else if p.extension().is_some_and(|x| x == "rs") {
                let rel = p.strip_prefix(&src).unwrap().to_string_lossy().into_owned();
                out.push((rel, std::fs::read_to_string(&p).unwrap()));
            }
        }
    }
    out
}

#[test]
fn production_storage_io_goes_through_the_adapter() {
    let files = source_files();
    assert!(
        files.iter().any(|(n, _)| n == "engine.rs"),
        "engine source not found"
    );
    assert_eq!(io_routing_violations(&files), Vec::<String>::new());
}

#[test]
fn io_routing_check_rejects_a_bypass() {
    let ok = vec![
        ("io.rs".to_string(), "use std::fs::File;".to_string()),
        (
            "engine.rs".to_string(),
            "self.io.write_at(f, 0, b)".to_string(),
        ),
    ];
    assert!(io_routing_violations(&ok).is_empty());
    let bypass = vec![(
        "engine.rs".to_string(),
        "let f = std::fs::OpenOptions::new();".to_string(),
    )];
    assert_eq!(io_routing_violations(&bypass).len(), 1);
}
