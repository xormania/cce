//! Performance scenarios measured directly through the public storage API.
//!
//! ```text
//! cargo run --release --locked -p cce-storage --example perf -- --list
//! cargo run --release --locked -p cce-storage --example perf -- --dir <new-dir> [--scenario <name>]...
//! ```
//!
//! `--dir` names a directory that does not exist yet; each scenario creates its store in a
//! subdirectory of it. The harness touches no file itself: stores are created and opened through
//! `NativeStore` only, and the caller owns the directory and removes it.
//!
//! Each scenario builds its inputs deterministically, checks every result against expectations
//! computed from those inputs, and prints one line:
//!
//! ```text
//! PERF {"scenario":..,"version":..,"work":{..},"cost":{..},"info":{..},"timing":{..}}
//! ```
//!
//! - `work`: the useful work done and checked (operation and byte counts).
//! - `cost`: engine counter deltas where lower is better. They are read with `stats()` before and
//!   after the measured phase, never inside it, while no commit is in flight; for a single-threaded
//!   scenario they are exact and repeat exactly for the same code.
//! - `info`: counters and sizes reported for context, with no direction.
//! - `timing`: elapsed nanoseconds from `Instant`. A per-operation series gives the number of
//!   operations, the nearest-rank p50 and p95, the maximum and the total; a single interval gives
//!   `ns`. Each scenario's documentation states what each interval includes.
//!
//! A scenario whose results differ from the expectations prints an error and the process exits
//! with status 1, without a `PERF` line. A scenario's `version` changes whenever its inputs,
//! operations or measurement boundaries change, so results of different versions are never
//! compared.

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Instant;

use cce_core::storage::{
    AttemptId, Change, ChangeSet, ContentBinding, ContextId, Direction, Evidence, Precondition,
    ReadView, RelationshipId, RelationshipKind, RelationshipPosition, RelationshipQuery,
    StorageEngine,
};
use cce_storage::{NativeStore, StoreStats};
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};

type R<T> = Result<T, String>;

const STORE_ID: [u8; 16] = *b"cce-perf-harness";

struct Scenario {
    name: &'static str,
    version: u64,
    run: fn(&Path) -> R<Report>,
}

const SCENARIOS: &[Scenario] = &[
    Scenario {
        name: "point-read",
        version: 1,
        run: point_read,
    },
    Scenario {
        name: "relationship-page",
        version: 1,
        run: relationship_page,
    },
    Scenario {
        name: "commit",
        version: 1,
        run: commit,
    },
    Scenario {
        name: "content",
        version: 1,
        run: content,
    },
];

#[derive(Default)]
struct Report {
    work: Map<String, Value>,
    cost: Map<String, Value>,
    info: Map<String, Value>,
    timing: Map<String, Value>,
}

impl Report {
    fn work(&mut self, k: &str, v: u64) {
        self.work.insert(k.into(), v.into());
    }
    fn cost(&mut self, k: &str, v: u64) {
        self.cost.insert(k.into(), v.into());
    }
    fn info(&mut self, k: &str, v: u64) {
        self.info.insert(k.into(), v.into());
    }
    fn interval(&mut self, k: &str, ns: u64) {
        self.timing.insert(k.into(), json!({ "ns": ns }));
    }
    fn series(&mut self, k: &str, mut ns: Vec<u64>) {
        ns.sort_unstable();
        let rank = |p: usize| ns[(ns.len() * p).div_ceil(100).max(1) - 1];
        let v = json!({
            "count": ns.len(),
            "p50_ns": rank(50),
            "p95_ns": rank(95),
            "max_ns": ns[ns.len() - 1],
            "total_ns": ns.iter().sum::<u64>(),
        });
        self.timing.insert(k.into(), v);
    }
    /// Counter deltas over a measured phase, as cost metrics named `<prefix>_<counter>`.
    fn deltas(&mut self, prefix: &str, before: &StoreStats, after: &StoreStats) {
        for (name, b, a) in [
            ("pages_read", before.pages_read, after.pages_read),
            (
                "entries_visited",
                before.entries_visited,
                after.entries_visited,
            ),
            (
                "cache_misses",
                before.node_cache_misses,
                after.node_cache_misses,
            ),
            (
                "cache_evictions",
                before.node_cache_evictions,
                after.node_cache_evictions,
            ),
            ("pages_written", before.pages_written, after.pages_written),
            (
                "page_bytes_written",
                before.page_bytes_written,
                after.page_bytes_written,
            ),
            (
                "content_bytes_written",
                before.content_bytes_written,
                after.content_bytes_written,
            ),
        ] {
            self.cost(&format!("{prefix}_{name}"), a - b);
        }
    }
}

fn nanos(t: Instant) -> u64 {
    u64::try_from(t.elapsed().as_nanos()).unwrap_or(u64::MAX)
}

fn err<E: std::fmt::Debug>(what: &str) -> impl FnOnce(E) -> String + '_ {
    move |e| format!("{what}: {e:?}")
}

fn check(ok: bool, what: impl FnOnce() -> String) -> R<()> {
    if ok { Ok(()) } else { Err(what()) }
}

/// Deterministic bytes for `(seed, i)`: SplitMix64 output, independent of the store.
fn gen_bytes(seed: u64, len: usize) -> Vec<u8> {
    let mut x = seed;
    let mut out = Vec::with_capacity(len + 8);
    while out.len() < len {
        x = x.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = x;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        out.extend_from_slice(&(z ^ (z >> 31)).to_le_bytes());
    }
    out.truncate(len);
    out
}

fn cid(s: &str) -> R<ContextId> {
    ContextId::new(s.as_bytes().to_vec()).map_err(err("context id"))
}

fn attempt(s: &str) -> R<AttemptId> {
    AttemptId::new(s.as_bytes().to_vec()).map_err(err("attempt id"))
}

fn stats(store: &NativeStore) -> R<StoreStats> {
    store.stats().map_err(err("stats"))
}

/// Commits `changes` in change sets of at most `batch`, each acknowledged before the next.
fn commit_all(store: &NativeStore, tag: &str, changes: Vec<Change>, batch: usize) -> R<()> {
    let mut it = changes.into_iter().peekable();
    let mut n = 0;
    while it.peek().is_some() {
        let set = ChangeSet {
            changes: it.by_ref().take(batch).collect(),
            ..ChangeSet::default()
        };
        let basis = store.begin().map_err(err("begin"))?;
        store
            .commit(attempt(&format!("{tag}-{n:05}"))?, basis, set)
            .map_err(err("commit"))?;
        n += 1;
    }
    Ok(())
}

fn create(dir: &Path, name: &str) -> R<(PathBuf, NativeStore)> {
    let path = dir.join(name);
    let store = NativeStore::create(&path, STORE_ID).map_err(err("create"))?;
    Ok((path, store))
}

fn close(store: NativeStore) -> R<()> {
    store.close().map_err(err("close"))
}

/// Point reads of a store of 16,000 records (100-byte bodies), in a fresh handle at the default
/// cache size.
///
/// `open` is `NativeStore::open` until it returns. Each `read_present` / `read_absent` interval is
/// `begin` plus one `context` call until the record (or its absence) is returned; checking the
/// result follows the interval. Present keys are read once each in a fixed permutation of 2,000 of
/// the records; absent keys fall between and around stored keys. Costs are counter deltas over
/// all reads.
fn point_read(dir: &Path) -> R<Report> {
    const N: usize = 16_000;
    const READS: usize = 2_000;
    const ABSENT: usize = 500;
    let key = |i: usize| format!("R{i:06}");
    let body = |i: usize| gen_bytes(0x5eed_0001 + i as u64, 100);

    let (path, store) = create(dir, "point-read")?;
    let puts = (0..N)
        .map(|i| {
            Ok(Change::PutContext {
                id: cid(&key(i))?,
                expect: Precondition::Absent,
                body: body(i),
                content: None,
            })
        })
        .collect::<R<Vec<_>>>()?;
    commit_all(&store, "build", puts, 500)?;
    close(store)?;

    let mut r = Report::default();
    let t = Instant::now();
    let store = NativeStore::open(&path).map_err(err("open"))?;
    r.interval("open", nanos(t));
    let s0 = stats(&store)?;
    r.cost("open_pages_read", s0.pages_read_at_open);
    r.info("tree_height", s0.tree_height as u64);
    r.info("cache_nodes", s0.node_cache_capacity as u64);

    // 7919 is prime and does not divide N, so i * 7919 mod N visits distinct records.
    let order: Vec<usize> = (0..READS).map(|i| (i * 7919 + 13) % N).collect();
    let mut present = Vec::with_capacity(READS);
    let mut absent = Vec::with_capacity(ABSENT);
    let mut got = Vec::with_capacity(READS);
    let before = stats(&store)?;
    for &i in &order {
        let id = cid(&key(i))?;
        let t = Instant::now();
        let mut view = store.begin().map_err(err("begin"))?;
        let rec = view.context(&id).map_err(err("context"))?;
        present.push(nanos(t));
        got.push((i, rec));
    }
    for i in 0..ABSENT {
        // Sorts just after stored key `R{2i:06}` (or after every stored key).
        let id = cid(&format!("{}x", key(i * 37 % (N + 1))))?;
        let t = Instant::now();
        let mut view = store.begin().map_err(err("begin"))?;
        let rec = view.context(&id).map_err(err("context"))?;
        absent.push(nanos(t));
        check(rec.is_none(), || format!("absent key {i} was found"))?;
    }
    let after = stats(&store)?;
    for (i, rec) in got {
        let rec = rec.ok_or_else(|| format!("record {i} is missing"))?;
        check(rec.body == body(i) && rec.content.is_none(), || {
            format!("record {i} differs")
        })?;
    }
    close(store)?;

    r.work("records", N as u64);
    r.work("reads_present", READS as u64);
    r.work("reads_absent", ABSENT as u64);
    r.deltas("read", &before, &after);
    r.series("read_present", present);
    r.series("read_absent", absent);
    Ok(r)
}

/// Pages through the 5,000 outgoing relationships of one node, 100 per page, and looks up the
/// single incoming relationship of 200 of its targets, in a fresh handle.
///
/// Each `page_out` interval is `begin` plus the query and collecting 100 relationships (fewer on
/// the last page); each `lookup_in` interval is `begin` plus the incoming query and collecting its
/// one result. Order and content are checked after each interval.
fn relationship_page(dir: &Path) -> R<Report> {
    const EDGES: usize = 5_000;
    const PAGE: usize = 100;
    const LOOKUPS: usize = 200;
    let target = |i: usize| format!("N{i:05}");
    let rel = |i: usize| format!("h{i:05}");
    let kind = RelationshipKind::new(b"links".to_vec()).map_err(err("kind"))?;

    let (path, store) = create(dir, "relationship-page")?;
    let rels = (0..EDGES)
        .map(|i| {
            Ok(Change::PutRelationship {
                id: RelationshipId::new(rel(i).into_bytes()).map_err(err("rel id"))?,
                expect: Precondition::Absent,
                kind: kind.clone(),
                source: cid("H")?,
                target: cid(&target(i))?,
            })
        })
        .collect::<R<Vec<_>>>()?;
    commit_all(&store, "build", rels, 500)?;
    close(store)?;

    let store = NativeStore::open(&path).map_err(err("open"))?;
    let mut r = Report::default();
    let mut pages = Vec::new();
    let mut lookups = Vec::with_capacity(LOOKUPS);
    let mut seen = 0usize;
    let mut after: Option<RelationshipPosition> = None;
    let before = stats(&store)?;
    loop {
        let q = RelationshipQuery {
            from: cid("H")?,
            direction: Direction::Outgoing,
            kind: None,
            after: after.clone(),
        };
        let t = Instant::now();
        let mut view = store.begin().map_err(err("begin"))?;
        let page = view
            .relationships(&q)
            .map_err(err("relationships"))?
            .take(PAGE)
            .collect::<Result<Vec<_>, _>>()
            .map_err(err("relationship"))?;
        drop(view);
        pages.push(nanos(t));
        let Some(last) = page.last() else { break };
        for (k, got) in page.iter().enumerate() {
            let i = seen + k;
            check(
                got.id.as_bytes() == rel(i).as_bytes()
                    && got.target.as_bytes() == target(i).as_bytes()
                    && got.kind == kind,
                || format!("outgoing relationship {i} differs"),
            )?;
        }
        seen += page.len();
        after = Some(RelationshipPosition {
            kind: last.kind.clone(),
            other: last.target.clone(),
            id: last.id.clone(),
        });
    }
    check(seen == EDGES, || format!("{seen} outgoing relationships"))?;
    for n in 0..LOOKUPS {
        let i = (n * 1543 + 7) % EDGES;
        let q = RelationshipQuery {
            from: cid(&target(i))?,
            direction: Direction::Incoming,
            kind: Some(kind.clone()),
            after: None,
        };
        let t = Instant::now();
        let mut view = store.begin().map_err(err("begin"))?;
        let got = view
            .relationships(&q)
            .map_err(err("relationships"))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(err("relationship"))?;
        drop(view);
        lookups.push(nanos(t));
        check(
            got.len() == 1
                && got[0].id.as_bytes() == rel(i).as_bytes()
                && got[0].source.as_bytes() == b"H",
            || format!("incoming relationship of target {i} differs"),
        )?;
    }
    let after_stats = stats(&store)?;
    close(store)?;

    r.work("relationships", EDGES as u64);
    r.work("pages", pages.len() as u64);
    r.work("lookups", LOOKUPS as u64);
    r.deltas("query", &before, &after_stats);
    r.series("page_out", pages);
    r.series("lookup_in", lookups);
    Ok(r)
}

/// 400 commits that each add 8 new records (64-byte bodies) and one 32-byte evidence item, then
/// 200 commits that each replace 8 of those records.
///
/// For each commit, `admit` is `begin` plus `submit` until the ticket is returned, and `complete`
/// is from the same start until `wait` returns the receipt (so it includes admission and any
/// queueing). Receipt positions are checked to follow each other, and a sample of records is read
/// back after the measured phase.
fn commit(dir: &Path) -> R<Report> {
    const INSERTS: usize = 400;
    const UPDATES: usize = 200;
    const WIDTH: usize = 8;
    let key = |c: usize, k: usize| format!("C{c:04}-{k}");
    let body = |round: usize, c: usize, k: usize| {
        gen_bytes(
            0x5eed_0003 ^ ((round * 1_000_000 + c * WIDTH + k) as u64),
            64,
        )
    };

    let (_, store) = create(dir, "commit")?;
    let mut r = Report::default();
    let mut admit = Vec::with_capacity(INSERTS + UPDATES);
    let mut complete = Vec::with_capacity(INSERTS + UPDATES);
    let mut position = None;
    let before = stats(&store)?;
    for n in 0..INSERTS + UPDATES {
        let (round, c, expect) = if n < INSERTS {
            (0, n, Precondition::Absent)
        } else {
            (1, (n - INSERTS) * 2, Precondition::Any)
        };
        let changes = (0..WIDTH)
            .map(|k| {
                Ok(Change::PutContext {
                    id: cid(&key(c, k))?,
                    expect,
                    body: body(round, c, k),
                    content: None,
                })
            })
            .collect::<R<Vec<_>>>()?;
        let set = ChangeSet {
            changes,
            evidence: vec![Evidence {
                label: b"perf".to_vec(),
                bytes: gen_bytes(0x5eed_e000 + n as u64, 32),
            }],
            ..ChangeSet::default()
        };
        let id = attempt(&format!("commit-{n:05}"))?;
        let t = Instant::now();
        let basis = store.begin().map_err(err("begin"))?;
        let ticket = store.submit(id, basis, set).map_err(err("submit"))?;
        admit.push(nanos(t));
        let receipt = ticket.wait().map_err(err("wait"))?;
        complete.push(nanos(t));
        if let Some(p) = position {
            check(receipt.position.0 == p + 1, || {
                format!("commit {n}: position {} after {p}", receipt.position.0)
            })?;
        }
        position = Some(receipt.position.0);
    }
    let after = stats(&store)?;
    let mut view = store.begin().map_err(err("begin"))?;
    for c in [0, 1, 2, INSERTS / 2, INSERTS - 1] {
        for k in [0, WIDTH - 1] {
            let round = usize::from(c % 2 == 0 && c < UPDATES * 2);
            let rec = view
                .context(&cid(&key(c, k))?)
                .map_err(err("context"))?
                .ok_or_else(|| format!("record {c}-{k} is missing"))?;
            check(rec.body == body(round, c, k), || {
                format!("record {c}-{k} differs")
            })?;
        }
    }
    drop(view);
    let space = store.space_report().map_err(err("space report"))?;
    close(store)?;

    r.work("commits", (INSERTS + UPDATES) as u64);
    r.work("records_written", ((INSERTS + UPDATES) * WIDTH) as u64);
    r.deltas("commit", &before, &after);
    r.cost("meta_file_bytes", space.meta_file_bytes);
    r.cost("live_pages", space.live_pages);
    r.info("history_bytes", space.history_bytes);
    r.info("pages_reused", after.pages_reused - before.pages_reused);
    r.series("admit", admit);
    r.series("complete", complete);
    Ok(r)
}

/// Stages a 32 MiB stream from memory, commits it, and reads it back through a new view in a fresh
/// handle.
///
/// `stage` is `stage_content` over the in-memory bytes until the staging is returned (generating
/// the bytes is outside it); `commit` is `begin` plus `commit` until the receipt; `read` is `begin`,
/// opening the reader and reading every byte into a preallocated buffer. The digest and the bytes
/// read are checked after the intervals.
fn content(dir: &Path) -> R<Report> {
    const LEN: usize = 32 << 20;
    let bytes = gen_bytes(0x5eed_0004, LEN);
    let digest: [u8; 32] = Sha256::digest(&bytes).into();

    let (path, store) = create(dir, "content")?;
    let mut r = Report::default();
    let before = stats(&store)?;
    let t = Instant::now();
    let staged = store.stage_content(&mut &bytes[..]).map_err(err("stage"))?;
    r.interval("stage", nanos(t));
    check(
        staged.content.digest.0 == digest && staged.content.len == LEN as u64,
        || "staged content differs from its input".into(),
    )?;
    let content_ref = staged.content;
    let set = ChangeSet {
        changes: vec![Change::PutContext {
            id: cid("L")?,
            expect: Precondition::Absent,
            body: Vec::new(),
            content: Some(0),
        }],
        content: vec![ContentBinding::from(&staged)],
        ..ChangeSet::default()
    };
    let t = Instant::now();
    let basis = store.begin().map_err(err("begin"))?;
    store
        .commit(attempt("content")?, basis, set)
        .map_err(err("commit"))?;
    r.interval("commit", nanos(t));
    let written = stats(&store)?;
    close(store)?;

    let store = NativeStore::open(&path).map_err(err("open"))?;
    let opened = stats(&store)?;
    let mut buf = vec![0u8; LEN + 1];
    let mut n = 0;
    let t = Instant::now();
    let mut view = store.begin().map_err(err("begin"))?;
    let mut reader = view.content(&content_ref).map_err(err("content"))?;
    loop {
        let k = std::io::Read::read(&mut reader, &mut buf[n..]).map_err(err("read"))?;
        if k == 0 {
            break;
        }
        n += k;
    }
    r.interval("read", nanos(t));
    drop(reader);
    drop(view);
    let read = stats(&store)?;
    close(store)?;
    check(n == LEN && buf[..n] == bytes[..], || {
        format!("read {n} bytes that differ from the {LEN} committed")
    })?;

    r.work("content_bytes", LEN as u64);
    r.deltas("write", &before, &written);
    r.deltas("read", &opened, &read);
    r.cost("write_buffer_peak", written.content_buffer_bytes_peak);
    r.cost("read_buffer_peak", read.content_buffer_bytes_peak);
    Ok(r)
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut dir = None;
    let mut selected = Vec::new();
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match (a.as_str(), it.clone().next()) {
            ("--list", _) => {
                for s in SCENARIOS {
                    println!("{} {}", s.name, s.version);
                }
                return ExitCode::SUCCESS;
            }
            ("--dir", Some(v)) => {
                dir = Some(PathBuf::from(v));
                it.next();
            }
            ("--scenario", Some(v)) => {
                selected.push(v.clone());
                it.next();
            }
            _ => {
                eprintln!("usage: perf --list | perf --dir <new-dir> [--scenario <name>]...");
                return ExitCode::from(2);
            }
        }
    }
    let Some(dir) = dir else {
        eprintln!("perf: --dir is required");
        return ExitCode::from(2);
    };
    if let Some(unknown) = selected
        .iter()
        .find(|n| !SCENARIOS.iter().any(|s| s.name == n.as_str()))
    {
        eprintln!("perf: unknown scenario {unknown}");
        return ExitCode::from(2);
    }
    for s in SCENARIOS {
        if !selected.is_empty() && !selected.iter().any(|n| n == s.name) {
            continue;
        }
        match (s.run)(&dir) {
            Ok(r) => {
                let line = json!({
                    "scenario": s.name,
                    "version": s.version,
                    "work": r.work,
                    "cost": r.cost,
                    "info": r.info,
                    "timing": r.timing,
                });
                println!("PERF {line}");
            }
            Err(e) => {
                eprintln!("perf: scenario {} failed: {e}", s.name);
                return ExitCode::FAILURE;
            }
        }
    }
    ExitCode::SUCCESS
}
