//! Relationship navigation at scale: both directional indexes across many pages, replacement and
//! removal, fresh-process reopen. Expected edge sets come from an independent in-test model.

mod common;

use std::collections::BTreeMap;

use common::{Driver, Step, answered, hex, test_dir};

/// Independent model of committed relationships: id → (kind, source, target, revision).
#[derive(Default)]
struct Model {
    rels: BTreeMap<String, (String, String, String, u64)>,
}

impl Model {
    fn put(&mut self, id: &str, kind: &str, src: &str, dst: &str) -> String {
        let (expect, rev) = match self.rels.get(id) {
            None => ("absent".to_string(), 1),
            Some(r) => (format!("rev:{}", r.3), r.3 + 1),
        };
        self.rels
            .insert(id.into(), (kind.into(), src.into(), dst.into(), rev));
        format!("put-rel {id} {expect} {kind} {src} {dst}")
    }

    fn remove(&mut self, id: &str) -> String {
        let r = self.rels.remove(id).expect("modelled");
        format!("remove-rel {id} rev:{}", r.3)
    }

    /// The exact `rel` lines `dump <node>` must print, ordered by (kind, other endpoint, id).
    fn expected(&self, node: &str) -> Vec<String> {
        let mut out = Vec::new();
        for dir in ["out", "in"] {
            let mut sel = BTreeMap::new();
            for (id, (kind, src, dst, rev)) in &self.rels {
                let (me, other) = if dir == "out" { (src, dst) } else { (dst, src) };
                if me == node {
                    let line = format!(
                        "rel {dir} {node} {} {} {} {} rev:{rev}",
                        hex(id.as_bytes()),
                        hex(kind.as_bytes()),
                        hex(src.as_bytes()),
                        hex(dst.as_bytes())
                    );
                    sel.insert(
                        (
                            kind.as_bytes().to_vec(),
                            other.as_bytes().to_vec(),
                            id.as_bytes().to_vec(),
                        ),
                        line,
                    );
                }
            }
            out.extend(sel.into_values());
        }
        out
    }
}

fn rel_lines(dump: Vec<String>) -> Vec<String> {
    dump.into_iter().filter(|l| l.starts_with("rel ")).collect()
}

/// Commits the commands in batches of `batch` changes.
fn commit_all(d: &mut Driver, cmds: &[String], batch: usize, tag: &str) {
    for (n, chunk) in cmds.chunks(batch).enumerate() {
        for c in chunk {
            answered(d.step(c), c);
        }
        let got = answered(d.step(&format!("commit {tag}-{n:04}")), "commit");
        assert_eq!(got.len(), 1);
        assert!(got[0].starts_with("committed "), "{tag} batch {n}: {got:?}");
    }
}

/// A hub with many outgoing edges of two kinds and many incoming edges, plus sparse nodes.
fn populate(model: &mut Model) -> Vec<String> {
    let mut cmds = Vec::new();
    for i in 0..900 {
        let kind = if i % 3 == 0 { "cites" } else { "depends" };
        cmds.push(model.put(
            &format!("hub-out-{i:05}"),
            kind,
            "HUB",
            &format!("N{:05}", (i * 7919) % 900),
        ));
    }
    for i in 0..600 {
        cmds.push(model.put(
            &format!("hub-in-{i:05}"),
            "cites",
            &format!("M{i:05}"),
            "HUB",
        ));
    }
    for i in 0..300 {
        cmds.push(model.put(
            &format!("sparse-{i:05}"),
            "depends",
            &format!("S{i:05}"),
            &format!("T{i:05}"),
        ));
    }
    cmds
}

#[test]
fn relationship_indexes_span_many_pages_and_survive_writer_kill() {
    let dir = test_dir("rels-many-pages");
    let store = dir.join("store");
    let mut model = Model::default();
    let cmds = populate(&mut model);

    let mut w = Driver::spawn();
    answered(w.step(&format!("create {}", store.display())), "create");
    commit_all(&mut w, &cmds, 300, "fill");
    assert_eq!(w.wait_for("hold", "holding"), Step::Answered(vec![]));
    assert!(w.kill().code().is_none());

    let mut r = Driver::spawn();
    answered(r.step(&format!("open {}", store.display())), "open");
    for node in ["HUB", "N00007", "M00599", "S00150", "T00299", "nobody"] {
        let got = rel_lines(answered(r.step(&format!("dump {node}")), "dump"));
        assert_eq!(got, model.expected(node), "edges of {node}");
    }
    assert_eq!(model.expected("HUB").len(), 1500);
}

#[test]
fn replaced_and_removed_relationships_leave_no_stale_index_entries() {
    let dir = test_dir("rels-replace-remove");
    let store = dir.join("store");
    let mut model = Model::default();
    let cmds = populate(&mut model);

    let mut w = Driver::spawn();
    answered(w.step(&format!("create {}", store.display())), "create");
    commit_all(&mut w, &cmds, 300, "fill");

    // Remove the first, middle and last hub edges and every third one; retarget others.
    let mut edits = Vec::new();
    for i in (0..900).step_by(3).chain([1, 449, 899]) {
        let id = format!("hub-out-{i:05}");
        if model.rels.contains_key(&id) {
            edits.push(model.remove(&id));
        }
    }
    for i in (1..900).step_by(5) {
        let id = format!("hub-out-{i:05}");
        if model.rels.contains_key(&id) {
            edits.push(model.put(&id, "cites", "HUB", &format!("R{i:05}")));
        }
    }
    for i in 0..600 {
        edits.push(model.remove(&format!("hub-in-{i:05}")));
    }
    commit_all(&mut w, &edits, 250, "edit");
    answered(w.step("close"), "close");
    drop(w);

    let mut r = Driver::spawn();
    answered(r.step(&format!("open {}", store.display())), "open");
    for node in ["HUB", "N00000", "N00449", "R00006", "M00000", "S00000"] {
        let got = rel_lines(answered(r.step(&format!("dump {node}")), "dump"));
        assert_eq!(got, model.expected(node), "edges of {node} after edits");
    }

    // Refill after removing everything a node had.
    let refill: Vec<String> = (0..600)
        .map(|i| {
            model.put(
                &format!("hub-in2-{i:05}"),
                "depends",
                &format!("M{i:05}"),
                "HUB",
            )
        })
        .collect();
    commit_all(&mut r, &refill, 300, "refill");
    let got = rel_lines(answered(r.step("dump HUB"), "dump"));
    assert_eq!(got, model.expected("HUB"));
}

#[test]
fn metadata_grows_past_one_branch_and_traverses_with_a_tiny_cache() {
    let dir = test_dir("rels-height-cache");
    let store = dir.join("store");
    let mut model = Model::default();
    let mut cmds = populate(&mut model);
    for i in 0..4000 {
        cmds.push(model.put(
            &format!("bulk-{i:06}"),
            "depends",
            &format!("B{:05}", i % 97),
            &format!("C{i:06}"),
        ));
    }
    let mut w = Driver::spawn();
    answered(w.step(&format!("create {}", store.display())), "create");
    commit_all(&mut w, &cmds, 500, "fill");
    answered(w.step("close"), "close");
    drop(w);

    // Reopen with a cache far smaller than the tree.
    let mut r = Driver::spawn();
    answered(r.step(&format!("open {} 4", store.display())), "open");
    let got = rel_lines(answered(r.step("dump HUB,B00042"), "dump"));
    let mut want = model.expected("HUB");
    want.extend(model.expected("B00042"));
    assert_eq!(got, want);
    let stats = answered(r.step("stats"), "stats").join(" ");
    let field = |name: &str| -> u64 {
        stats
            .split(' ')
            .find_map(|f| f.strip_prefix(&format!("{name}=")))
            .and_then(|v| v.parse().ok())
            .unwrap_or_else(|| panic!("{name} in {stats}"))
    };
    assert!(field("height") >= 3, "tree height: {stats}");
    assert_eq!(field("cache"), 4);
    assert!(field("evictions") > 0, "a tiny cache must evict: {stats}");
}
