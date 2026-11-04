//! Architecture checks: dependency direction, no borrowed database engine, binaries that use only
//! the logical port, file access only through the I/O adapter, no public physical modules, and a
//! listed public surface. Sources are parsed, not matched line by line: `use` groups, aliases,
//! `self`, leading `::` and `extern crate` renames are resolved, and declarations are read as items.
//! Each check is exercised against deliberately violating fixtures as well as the real workspace.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::process::Command;

use serde_json::{Value, json};

const KERNEL: &str = "cce-core";
const STORAGE: &str = "cce-storage";

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

fn driver_violations(source: &str) -> Vec<String> {
    binary_violations(source, &["NativeStore", "StoreOptions", "NativeReadOnly"])
}

fn surface_violations(lib_rs: &str) -> Vec<String> {
    let file = match syn::parse_file(lib_rs) {
        Ok(f) => f,
        Err(e) => return vec![format!("lib.rs does not parse: {e}")],
    };
    let mut v = Vec::new();
    for item in &file.items {
        match item {
            syn::Item::Mod(m) if is_pub(&m.vis) => {
                v.push(format!("storage exposes internals: pub mod {}", m.ident));
            }
            syn::Item::Use(u) if is_pub(&u.vis) => {
                for (_, full) in use_bindings(&u.tree) {
                    let root = full.first().map(String::as_str).unwrap_or_default();
                    if !EXTERNAL_ROOTS.contains(&root) {
                        v.push(format!(
                            "storage exposes internals: pub use {}",
                            full.join("::")
                        ));
                    }
                }
            }
            _ => {}
        }
    }
    v
}

/// The public items at the root of `cce-storage`. A top-level public item that is not listed fails
/// `storage_public_items_are_exactly_the_listed_ones`, and so does a listed item that no longer
/// exists, so every change to the public surface shows up in review here.
///
/// Caveat: only top-level items of `src/lib.rs` are checked. Public methods on the listed types are
/// covered by those types' ownership and by review, not by this list.
const PUBLIC_ITEMS: &[&str] = &[
    "NativeStore",
    "NativeReadOnly",
    "NativeView",
    "CommitTicket",
    "StoreOptions",
    "ReceiptGuard",
    "StoreStats",
    "VerifyLevel",
    "Finding",
    "Report",
];

/// Public items compiled only with a cargo feature: (feature, item).
const FEATURE_ITEMS: &[(&str, &str)] = &[("fuzz-api", "fuzz_target")];

fn public_items(lib_rs: &str) -> Vec<(String, Option<String>)> {
    let Ok(file) = syn::parse_file(lib_rs) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for item in &file.items {
        let (names, attrs): (Vec<String>, &[syn::Attribute]) = match item {
            syn::Item::Fn(i) if is_pub(&i.vis) => (vec![i.sig.ident.to_string()], &i.attrs),
            syn::Item::Struct(i) if is_pub(&i.vis) => (vec![i.ident.to_string()], &i.attrs),
            syn::Item::Enum(i) if is_pub(&i.vis) => (vec![i.ident.to_string()], &i.attrs),
            syn::Item::Union(i) if is_pub(&i.vis) => (vec![i.ident.to_string()], &i.attrs),
            syn::Item::Trait(i) if is_pub(&i.vis) => (vec![i.ident.to_string()], &i.attrs),
            syn::Item::TraitAlias(i) if is_pub(&i.vis) => (vec![i.ident.to_string()], &i.attrs),
            syn::Item::Type(i) if is_pub(&i.vis) => (vec![i.ident.to_string()], &i.attrs),
            syn::Item::Const(i) if is_pub(&i.vis) => (vec![i.ident.to_string()], &i.attrs),
            syn::Item::Static(i) if is_pub(&i.vis) => (vec![i.ident.to_string()], &i.attrs),
            syn::Item::Use(i) if is_pub(&i.vis) => (
                use_bindings(&i.tree)
                    .into_iter()
                    .map(|(name, _)| name)
                    .collect(),
                &i.attrs,
            ),
            syn::Item::Macro(i) if i.attrs.iter().any(|a| a.path().is_ident("macro_export")) => {
                (i.ident.iter().map(ToString::to_string).collect(), &i.attrs)
            }
            _ => continue,
        };
        let feature = attrs.iter().find_map(cfg_feature);
        out.extend(names.into_iter().map(|n| (n, feature.clone())));
    }
    out
}

fn public_item_violations(lib_rs: &str) -> Vec<String> {
    if let Err(e) = syn::parse_file(lib_rs) {
        return vec![format!("lib.rs does not parse: {e}")];
    }
    let found = public_items(lib_rs);
    let mut v = Vec::new();
    for (name, feature) in &found {
        let listed = match feature {
            None => PUBLIC_ITEMS.contains(&name.as_str()),
            Some(f) => FEATURE_ITEMS.contains(&(f.as_str(), name.as_str())),
        };
        if !listed {
            v.push(format!(
                "unlisted public item {name}{}",
                feature
                    .as_ref()
                    .map_or(String::new(), |f| format!(" (feature {f})"))
            ));
        }
    }
    let present = |name: &str, feature: Option<&str>| {
        found
            .iter()
            .any(|(n, f)| n == name && f.as_deref() == feature)
    };
    for name in PUBLIC_ITEMS {
        if !present(name, None) {
            v.push(format!(
                "listed public item {name} is not in the crate root"
            ));
        }
    }
    for (feature, name) in FEATURE_ITEMS {
        if !present(name, Some(feature)) {
            v.push(format!(
                "listed public item {name} (feature {feature}) is not in the crate root"
            ));
        }
    }
    v
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
        driver_violations("use cce_storage::NativeStore;\nuse cce_core::storage::Change;")
            .is_empty()
    );
    assert_eq!(driver_violations("use cce_storage::tree::Page;").len(), 1);
    assert_eq!(
        driver_violations("fn f() { let _ = std::fs::read(path); }").len(),
        1
    );
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

#[test]
fn storage_public_items_are_exactly_the_listed_ones() {
    let lib = read("src/lib.rs");
    assert_eq!(public_item_violations(&lib), Vec::<String>::new());
    let found = public_items(&lib);
    assert_eq!(
        found.len(),
        PUBLIC_ITEMS.len() + FEATURE_ITEMS.len(),
        "{found:?}"
    );
}

#[test]
fn the_verify_tool_uses_only_the_public_api_and_never_touches_files() {
    assert_eq!(
        binary_violations(
            &read("src/bin/cce-verify.rs"),
            &["NativeStore", "VerifyLevel"]
        ),
        Vec::<String>::new()
    );
    // Any other storage item, however it is spelled, and any file access are refused.
    for src in [
        "use cce_storage::{NativeStore as S, tree::Node};",
        "use ::cce_storage::fuzz_target;",
        "fn f() { cce_storage::fuzz_target(\"page\", &[]); }",
        "use std::{fs as disk};",
    ] {
        assert!(
            !binary_violations(src, &["NativeStore", "VerifyLevel"]).is_empty(),
            "{src}"
        );
    }
}

#[test]
fn public_item_check_flags_unlisted_items_and_nothing_else() {
    // Every listed item, as a crate root would declare it.
    let mut listed: String = PUBLIC_ITEMS
        .iter()
        .map(|n| format!("pub struct {n};\n"))
        .collect();
    for (feature, name) in FEATURE_ITEMS {
        listed.push_str(&format!(
            "#[cfg(feature = \"{feature}\")]\n/// Doc.\npub fn {name}() {{}}\n"
        ));
    }
    let listed = listed.as_str();
    assert_eq!(public_item_violations(listed), Vec::<String>::new());
    for decl in [
        "pub fn Sneaky() {}",
        "pub struct Sneaky;",
        "pub enum Sneaky {}",
        "pub trait Sneaky {}",
        "pub type Sneaky = u8;",
        "pub const Sneaky: u8 = 0;",
        "pub static Sneaky: u8 = 0;",
    ] {
        let text = format!("{listed}{decl}\n");
        let v = public_item_violations(&text);
        assert_eq!(v.len(), 1, "{decl}: {v:?}");
        assert!(v[0].contains("Sneaky"), "{v:?}");
    }
    // Methods, crate-visible items and listed items are not violations.
    let inner = format!(
        "{listed}impl NativeStore {{\n    pub fn method(&self) {{}}\n}}\npub(crate) fn helper() {{}}\n"
    );
    assert_eq!(public_item_violations(&inner), Vec::<String>::new());
    // A feature-gated item is listed with its feature only: ungated, or under another feature, it
    // is both unlisted and missing from its listed place.
    let ungated = listed.replace("#[cfg(feature = \"fuzz-api\")]\n", "");
    assert_eq!(
        public_item_violations(&ungated),
        [
            "unlisted public item fuzz_target",
            "listed public item fuzz_target (feature fuzz-api) is not in the crate root"
        ]
    );
    let other = listed.replace("fuzz-api", "other");
    assert_eq!(
        public_item_violations(&other),
        [
            "unlisted public item fuzz_target (feature other)",
            "listed public item fuzz_target (feature fuzz-api) is not in the crate root"
        ]
    );
    // A listed item that disappeared is reported too.
    let gone = listed.replace("pub struct StoreStats;\n", "");
    let v = public_item_violations(&gone);
    assert!(v.len() == 1 && v[0].contains("StoreStats"), "{v:?}");
}

fn io_routing_violations(files: &[(String, String)]) -> Vec<String> {
    let mut v = Vec::new();
    for (name, src) in files {
        if name == "io.rs" || name.starts_with("bin/") || name.ends_with("_tests.rs") {
            continue;
        }
        match resolved_paths(src) {
            Err(e) => v.push(format!("{name}: {e}")),
            Ok(paths) => v.extend(paths.iter().filter(|p| reaches_fs(p)).map(|p| {
                format!(
                    "{name}: filesystem access outside the I/O adapter: {}",
                    p.join("::")
                )
            })),
        }
    }
    v
}

/// Crates a `pub use` may re-export from without exposing internals (still subject to the list of
/// public items).
const EXTERNAL_ROOTS: &[&str] = &["std", "core", "alloc", "cce_core", "crc32fast", "sha2"];

fn is_pub(vis: &syn::Visibility) -> bool {
    matches!(vis, syn::Visibility::Public(_))
}

/// The feature named by a `#[cfg(feature = "...")]` attribute.
fn cfg_feature(attr: &syn::Attribute) -> Option<String> {
    if !attr.path().is_ident("cfg") {
        return None;
    }
    let nv: syn::MetaNameValue = attr.parse_args().ok()?;
    match (&nv.value, nv.path.is_ident("feature")) {
        (
            syn::Expr::Lit(syn::ExprLit {
                lit: syn::Lit::Str(s),
                ..
            }),
            true,
        ) => Some(s.value()),
        _ => None,
    }
}

/// The names a `use` tree binds and the full path each stands for: groups, renames and `self`
/// expanded; a glob binds `*`.
fn use_bindings(tree: &syn::UseTree) -> Vec<(String, Vec<String>)> {
    fn walk(t: &syn::UseTree, prefix: &mut Vec<String>, out: &mut Vec<(String, Vec<String>)>) {
        match t {
            syn::UseTree::Path(p) => {
                prefix.push(p.ident.to_string());
                walk(&p.tree, prefix, out);
                prefix.pop();
            }
            syn::UseTree::Name(n) if n.ident == "self" => {
                out.push((prefix.last().cloned().unwrap_or_default(), prefix.clone()));
            }
            syn::UseTree::Name(n) => {
                let mut full = prefix.clone();
                full.push(n.ident.to_string());
                out.push((n.ident.to_string(), full));
            }
            syn::UseTree::Rename(r) => {
                let mut full = prefix.clone();
                if r.ident != "self" {
                    full.push(r.ident.to_string());
                }
                out.push((r.rename.to_string(), full));
            }
            syn::UseTree::Glob(_) => {
                let mut full = prefix.clone();
                full.push("*".into());
                out.push(("*".into(), full));
            }
            syn::UseTree::Group(g) => {
                for t in &g.items {
                    walk(t, prefix, out);
                }
            }
        }
    }
    let mut out = Vec::new();
    walk(tree, &mut Vec::new(), &mut out);
    out
}

/// Collects `use` bindings, `extern crate` renames and every path a file names, including paths
/// inside macro invocations whose input parses as expressions.
#[derive(Default)]
struct PathCollector {
    /// (bound name, full path, written with a leading `::`)
    bindings: Vec<(String, Vec<String>, bool)>,
    /// (segments, written with a leading `::`)
    paths: Vec<(Vec<String>, bool)>,
}

impl<'ast> syn::visit::Visit<'ast> for PathCollector {
    fn visit_item_use(&mut self, u: &'ast syn::ItemUse) {
        let absolute = u.leading_colon.is_some();
        for (name, full) in use_bindings(&u.tree) {
            self.bindings.push((name, full, absolute));
        }
    }
    fn visit_item_extern_crate(&mut self, e: &'ast syn::ItemExternCrate) {
        let name = e.ident.to_string();
        let alias = e
            .rename
            .as_ref()
            .map_or(name.clone(), |(_, r)| r.to_string());
        self.bindings.push((alias, vec![name], true));
    }
    fn visit_path(&mut self, p: &'ast syn::Path) {
        let segs = p.segments.iter().map(|s| s.ident.to_string()).collect();
        self.paths.push((segs, p.leading_colon.is_some()));
        syn::visit::visit_path(self, p);
    }
    fn visit_macro(&mut self, m: &'ast syn::Macro) {
        syn::visit::visit_macro(self, m);
        let parser = syn::punctuated::Punctuated::<syn::Expr, syn::Token![,]>::parse_terminated;
        if let Ok(args) = m.parse_body_with(parser) {
            for e in &args {
                self.visit_expr(e);
            }
        }
    }
}

/// Every path a source file names and every path its `use` items bind, fully resolved through the
/// file's own bindings. Scoping is ignored, which only errs towards reporting.
fn resolved_paths(src: &str) -> Result<Vec<Vec<String>>, String> {
    use syn::visit::Visit;
    let file = syn::parse_file(src).map_err(|e| format!("does not parse: {e}"))?;
    let mut c = PathCollector::default();
    c.visit_file(&file);
    let resolve = |segs: &[String], absolute: bool| -> Vec<String> {
        let mut out = segs.to_vec();
        if absolute {
            return out;
        }
        // Follow aliases a few levels deep (`use std as s; use s::fs;`).
        for _ in 0..4 {
            let Some(first) = out.first() else { break };
            let Some((_, full, _)) = c
                .bindings
                .iter()
                .find(|(name, full, _)| name == first && full.first() != Some(first))
            else {
                break;
            };
            out = full
                .iter()
                .cloned()
                .chain(out.into_iter().skip(1))
                .collect();
        }
        out
    };
    let mut out: Vec<Vec<String>> = c
        .bindings
        .iter()
        .map(|(_, full, absolute)| resolve(full, *absolute))
        .collect();
    out.extend(
        c.paths
            .iter()
            .map(|(segs, absolute)| resolve(segs, *absolute)),
    );
    Ok(out)
}

/// A resolved path into the standard library's file system API (including platform extensions and
/// globs that would bring it in).
fn reaches_fs(p: &[String]) -> bool {
    let s: Vec<&str> = p.iter().map(String::as_str).collect();
    matches!(
        s.as_slice(),
        ["std", "fs", ..] | ["std", "*"] | ["std", "os", _, "fs", ..] | ["std", "os", _, "*"]
    )
}

/// Violations of the binary rule: a binary may name only the listed `cce_storage` items (and their
/// associated items), and never the file system API.
fn binary_violations(source: &str, allowed: &[&str]) -> Vec<String> {
    let paths = match resolved_paths(source) {
        Ok(p) => p,
        Err(e) => return vec![e],
    };
    let mut v = Vec::new();
    for p in paths {
        if p.first().map(String::as_str) == Some("cce_storage")
            && !p
                .get(1)
                .is_some_and(|item| allowed.contains(&item.as_str()))
        {
            v.push(format!("imports storage internals: {}", p.join("::")));
        }
        if reaches_fs(&p) {
            v.push(format!("touches files directly: {}", p.join("::")));
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
            "fn f() { self.io.write_at(f, 0, b) }".to_string(),
        ),
    ];
    assert!(io_routing_violations(&ok).is_empty());
    let bypass = vec![(
        "engine.rs".to_string(),
        "fn f() { let _ = std::fs::OpenOptions::new(); }".to_string(),
    )];
    assert_eq!(io_routing_violations(&bypass).len(), 1);
}

/// The I/O rule on one production source file.
fn io_violations_in(src: &str) -> usize {
    io_routing_violations(&[("engine.rs".to_string(), src.to_string())]).len()
}

#[test]
fn io_check_sees_through_grouped_aliased_and_qualified_paths() {
    let bypasses = [
        "use std::{fs as disk};\nfn f(p: &Path) { let _ = disk::read(p); }",
        "use std::{io, fs::{self}};",
        "use ::std::fs;",
        "use std::fs\n    ::File;",
        "use std::{os::unix::fs::FileExt as Positional};",
        "use std as s;\nfn f() { let _ = s::fs::remove_file(\"x\"); }",
        "extern crate std as stdlib;\nfn f() { let _ = stdlib::fs::read(\"x\"); }",
        "fn f() { let _ = std::fs::read(\"x\"); }",
        "fn f() { let _ = ::std::fs::File::open(\"x\"); }",
        "struct S { f: std::fs::File }",
    ];
    for src in bypasses {
        assert!(io_violations_in(src) > 0, "not flagged:\n{src}");
    }
}

#[test]
fn io_check_ignores_names_that_only_look_like_file_access() {
    let fine = [
        // A module of this crate named `fs`, and a variable named `fs`.
        "mod fs { pub fn read() {} }\nfn f() { fs::read(); let fs = 3; let _ = fs + 1; }",
        // Text in strings and comments.
        "fn f() { let _ = \"std::fs::read\"; } // std::fs::File::open",
        // Crate types whose names contain File.
        "fn f() { let _ = SimFile::default(); let _ = FileId::Meta; }",
    ];
    for src in fine {
        assert_eq!(io_violations_in(src), 0, "flagged:\n{src}");
    }
}

#[test]
fn surface_check_sees_multiline_modules_and_local_re_exports() {
    let exposed = [
        "pub\nmod tree;",
        "pub use self::tree::Node;",
        "pub use tree::Node;",
        "pub use crate::tree::Node;",
        "pub use crate::{tree::Node, format::Slot};",
    ];
    for src in exposed {
        assert!(!surface_violations(src).is_empty(), "not flagged:\n{src}");
    }
    assert!(surface_violations("pub(crate) mod tree;\nmod format;").is_empty());
}

#[test]
fn public_item_check_reads_items_not_lines() {
    let mut listed: String = PUBLIC_ITEMS
        .iter()
        .map(|n| format!("pub struct {n};\n"))
        .collect();
    // An attribute on the same line as its item.
    listed.push_str("#[cfg(feature = \"fuzz-api\")] pub fn fuzz_target() {}\n");
    assert_eq!(public_item_violations(&listed), Vec::<String>::new());
    for extra in [
        "pub\nfn sneaky() {}",
        "pub use cce_core::storage::ContextId;",
        "pub use cce_core::storage::ContextId as Id;",
        "pub union U { a: u8 }",
    ] {
        let v = public_item_violations(&format!("{listed}{extra}\n"));
        assert_eq!(v.len(), 1, "{extra}: {v:?}");
    }
}
