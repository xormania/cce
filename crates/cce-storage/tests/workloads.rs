//! Bounded-resource workloads at the production defaults, through the driver in fresh processes.
//!
//! These are a separate tier: each test is ignored in the ordinary run and is meant to be run in
//! a release build, one test at a time:
//!
//! ```text
//! cargo test --release --locked -p cce-storage --test workloads -- --ignored --test-threads=1 --nocapture
//! ```
//!
//! Each prints one `WORKLOAD {json}` line with what it measured. Sizes are derived from the
//! store's reported default capacities. Every driver runs under an address-space and an open-file
//! limit set by the parent. Timings are recorded, never asserted.

mod common;

use std::collections::BTreeMap;
use std::time::Instant;

use common::{Driver, answered, gen_bytes, hex, proc_usage, sha256, test_dir};

/// The documented largest content segment, which bounds a content stream's buffer.
const SEGMENT: u64 = 65_536;
/// The metadata page size (format document).
const PAGE: u64 = 4096;
/// Limits for every workload driver: 1 GiB of address space, 64 open files.
const LIMITS: &str = "ulimit -v 1048576 -n 64";

fn driver() -> Driver {
    Driver::spawn_with_shell(LIMITS)
}

fn fields(line: &str) -> BTreeMap<String, u64> {
    line.split(' ')
        .filter_map(|f| f.split_once('='))
        .filter_map(|(k, v)| v.parse().ok().map(|v| (k.to_string(), v)))
        .collect()
}

fn stats(d: &mut Driver) -> BTreeMap<String, u64> {
    fields(&answered(d.step("stats"), "stats")[0])
}

fn space(d: &mut Driver) -> BTreeMap<String, u64> {
    fields(&answered(d.step("space"), "space")[0])
}

fn committed(d: &mut Driver, attempt: &str) {
    let got = answered(d.step(&format!("commit {attempt}")), attempt);
    assert!(
        got.len() == 1 && got[0].starts_with("committed "),
        "{attempt}: {got:?}"
    );
}

/// Runs `cmds` in commits of at most `batch` changes each.
fn commit_batches(d: &mut Driver, tag: &str, cmds: &[String], batch: usize) {
    for (n, chunk) in cmds.chunks(batch).enumerate() {
        for c in chunk {
            answered(d.step(c), c);
        }
        committed(d, &format!("{tag}-{n:05}"));
    }
}

fn open(store: &std::path::Path) -> Driver {
    let mut d = driver();
    answered(d.step(&format!("open {}", store.display())), "open");
    d
}

fn close(mut d: Driver) {
    answered(d.step("close"), "close");
    assert_eq!(d.finish().and_then(|s| s.code()), Some(0));
}

fn report(name: &str, values: &[(&str, String)]) {
    let body: Vec<String> = values.iter().map(|(k, v)| format!("\"{k}\":{v}")).collect();
    println!("WORKLOAD {{\"workload\":\"{name}\",{}}}", body.join(","));
}

fn map_json(m: &BTreeMap<String, u64>) -> String {
    let body: Vec<String> = m.iter().map(|(k, v)| format!("\"{k}\":{v}")).collect();
    format!("{{{}}}", body.join(","))
}

#[test]
#[ignore = "workload tier: release build, --ignored, one test thread"]
fn metadata_beyond_the_cache_with_a_high_degree_node_pages_correctly() {
    let t0 = Instant::now();
    let dir = test_dir("workload-metadata");
    let store = dir.join("store");
    const HUB: usize = 20_000;
    const SPARSE: usize = 5_000;

    let mut d = driver();
    answered(d.step(&format!("create {}", store.display())), "create");
    let mut cmds = Vec::new();
    let mut expected_hub = Vec::new();
    for i in 0..HUB {
        let (id, target) = (format!("h{i:06}"), format!("N{i:06}"));
        cmds.push(format!("put-rel {id} absent links H {target}"));
        expected_hub.push(format!("page links {target} {id}"));
    }
    for i in 0..SPARSE {
        cmds.push(format!(
            "put-context S{i:05} any {} -",
            hex(&[i as u8; 200])
        ));
        for k in 1..=2 {
            cmds.push(format!(
                "put-rel s{i:05}-{k} absent next S{i:05} S{:05}",
                (i + k) % SPARSE
            ));
        }
    }
    commit_batches(&mut d, "build", &cmds, 500);
    close(d);
    expected_hub.sort();

    // A fresh process at the default cache capacity.
    let mut d = open(&store);
    let before = stats(&mut d);
    let cache = before["cache"];
    let sp = space(&mut d);
    assert!(
        sp["live_pages"] >= 4 * cache,
        "the workload is too small: {} live pages for a {cache}-node cache",
        sp["live_pages"]
    );
    let mut got: Vec<String> = Vec::new();
    loop {
        let after = got
            .last()
            .map(|l| format!(" {}", l.trim_start_matches("page ")))
            .unwrap_or_default();
        let page = answered(d.step(&format!("rels H out links 500{after}")), "rels");
        if page.is_empty() {
            break;
        }
        got.extend(page);
    }
    assert_eq!(got.len(), HUB);
    assert!(
        got == expected_hub,
        "the hub's relationships differ from the expected order"
    );
    assert_eq!(
        answered(d.step("rels N012345 in - 10"), "in"),
        ["page links H h012345"]
    );
    for i in [0usize, 1234, SPARSE - 1] {
        let page = answered(d.step(&format!("rels S{i:05} out - 10")), "sparse");
        let want: Vec<String> = (1..=2)
            .map(|k| format!("page next S{:05} s{i:05}-{k}", (i + k) % SPARSE))
            .collect();
        let mut page_sorted = page.clone();
        page_sorted.sort();
        let mut want_sorted = want.clone();
        want_sorted.sort();
        assert_eq!(page_sorted, want_sorted, "sparse node {i}");
    }
    let after = stats(&mut d);
    assert!(
        after["evictions"] > before["evictions"],
        "the cache never evicted"
    );
    assert!(after["height"] >= 3, "tree height {}", after["height"]);
    let (peak, rss, fds) = proc_usage(d.pid());
    close(d);
    report(
        "metadata",
        &[
            ("hub_edges", HUB.to_string()),
            ("sparse_nodes", SPARSE.to_string()),
            ("cache_nodes", cache.to_string()),
            ("space", map_json(&sp)),
            ("stats", map_json(&after)),
            ("peak_rss_kib", peak.to_string()),
            ("rss_kib", rss.to_string()),
            ("fds", fds.to_string()),
            ("elapsed_ms", t0.elapsed().as_millis().to_string()),
        ],
    );
}

#[test]
#[ignore = "workload tier: release build, --ignored, one test thread"]
fn large_content_streams_through_one_segment_buffer() {
    let t0 = Instant::now();
    let dir = test_dir("workload-content");
    let store = dir.join("store");
    const LEN: u64 = 64 << 20;
    const SEED: u64 = 0x5eed_c0de;
    let expected = sha256(&gen_bytes(SEED, LEN));

    let mut d = driver();
    answered(d.step(&format!("create {}", store.display())), "create");
    answered(d.step(&format!("stage L gen {LEN} {SEED}")), "stage");
    answered(d.step("put-context L absent 4c L"), "put");
    committed(&mut d, "large");
    let staging = stats(&mut d);
    assert_eq!(
        staging["buffer_peak"], SEGMENT,
        "staging held more than one segment"
    );
    let (staging_peak_rss, _, _) = proc_usage(d.pid());
    close(d);

    let mut d = open(&store);
    let line = answered(
        d.step(&format!("read-digest {} {LEN}", hex(&expected))),
        "read",
    );
    assert_eq!(line, [format!("digest {} {LEN}", hex(&expected))]);
    let reading = stats(&mut d);
    assert_eq!(
        reading["buffer_peak"], SEGMENT,
        "reading held more than one segment"
    );
    assert_eq!(
        reading["buffer_bytes"], 0,
        "the reader's buffer was not released"
    );
    let (peak, rss, fds) = proc_usage(d.pid());
    // The process never held a sizeable part of the stream: well under a quarter of it.
    assert!(peak * 1024 < LEN / 4, "peak resident memory {peak} KiB");
    assert!(
        staging_peak_rss * 1024 < LEN / 4,
        "staging peak {staging_peak_rss} KiB"
    );
    close(d);
    report(
        "content",
        &[
            ("content_bytes", LEN.to_string()),
            ("segment_bytes", SEGMENT.to_string()),
            ("buffer_peak_staging", staging["buffer_peak"].to_string()),
            ("buffer_peak_reading", reading["buffer_peak"].to_string()),
            ("peak_rss_kib_staging", staging_peak_rss.to_string()),
            ("peak_rss_kib_reading", peak.to_string()),
            ("rss_kib", rss.to_string()),
            ("fds", fds.to_string()),
            ("elapsed_ms", t0.elapsed().as_millis().to_string()),
        ],
    );
}

#[test]
#[ignore = "workload tier: release build, --ignored, one test thread"]
fn a_fixed_live_set_churned_reuses_space_and_only_history_grows() {
    let t0 = Instant::now();
    let dir = test_dir("workload-churn");
    let store = dir.join("store");
    const RECORDS: usize = 16;
    const LEN: u64 = 4 * SEGMENT;
    const ROUNDS: usize = 30;
    let live = RECORDS as u64 * LEN;

    let mut d = driver();
    answered(d.step(&format!("create {}", store.display())), "create");
    let mut rounds = Vec::new();
    for r in 0..ROUNDS {
        for i in 0..RECORDS {
            let seed = (r * RECORDS + i) as u64;
            answered(d.step(&format!("stage c{i} gen {LEN} {seed}")), "stage");
            answered(
                d.step(&format!("put-context K{i:02} any {} c{i}", hex(&[r as u8]))),
                "put",
            );
        }
        committed(&mut d, &format!("round-{r:03}"));
        let sp = space(&mut d);
        assert_eq!(sp["live_content"], live, "round {r}: live content");
        assert_eq!(sp["history_entries"], r as u64 + 1, "round {r}: history");
        // Every metadata page past the two publication records is live or recorded as retired:
        // no obsolete page is lost to reuse.
        let pages = sp["meta_file"] / PAGE;
        let accounted =
            2 + sp["live_pages"] + sp["retired_pages_protected"] + sp["retired_pages_reusable"];
        assert_eq!(
            pages, accounted,
            "round {r}: metadata pages unaccounted: {sp:?}"
        );
        rounds.push(sp);
    }
    let st = stats(&mut d);
    let last = rounds.last().unwrap().clone();
    // Far more content was written than the files hold: space is reused.
    assert!(st["content_bytes"] >= ROUNDS as u64 * live);
    assert!(st["content_reused"] > 0, "no content space was reused");
    assert!(
        last["content_file"] <= 4 * live,
        "content file {} bytes for {live} live bytes",
        last["content_file"]
    );
    // After the first rounds, the files stop growing except by history.
    let mid = &rounds[ROUNDS / 3];
    assert!(
        last["content_file"] - mid["content_file"]
            <= last["history_bytes"] - mid["history_bytes"] + LEN,
        "content file grew from {} to {}",
        mid["content_file"],
        last["content_file"]
    );
    // The metadata file grows only as the live tree does (history, receipts and the markers of
    // removed content are retained by design), plus the retired pages the two-generation and
    // reader bounds keep: at most what two commits retire.
    let live_growth = (last["live_pages"] - mid["live_pages"]) * PAGE;
    assert!(
        last["meta_file"] - mid["meta_file"]
            <= live_growth + 2 * last["retired_pages_protected"].max(1) * PAGE,
        "metadata file grew from {} to {} while the live tree grew by {live_growth} bytes",
        mid["meta_file"],
        last["meta_file"]
    );
    let (peak, rss, fds) = proc_usage(d.pid());
    close(d);

    // A fresh process reads the last round exactly.
    let mut d = open(&store);
    for i in [0usize, RECORDS - 1] {
        let seed = ((ROUNDS - 1) * RECORDS + i) as u64;
        let bytes = gen_bytes(seed, LEN);
        let line = answered(
            d.step(&format!("read-digest {} {LEN}", hex(&sha256(&bytes)))),
            "read",
        );
        assert_eq!(line, [format!("digest {} {LEN}", hex(&sha256(&bytes)))]);
    }
    close(d);
    let per_round: Vec<String> = rounds.iter().map(map_json).collect();
    report(
        "churn",
        &[
            ("records", RECORDS.to_string()),
            ("content_bytes_each", LEN.to_string()),
            ("rounds", ROUNDS.to_string()),
            ("live_content_bytes", live.to_string()),
            ("stats", map_json(&st)),
            ("space_per_round", format!("[{}]", per_round.join(","))),
            ("peak_rss_kib", peak.to_string()),
            ("rss_kib", rss.to_string()),
            ("fds", fds.to_string()),
            ("elapsed_ms", t0.elapsed().as_millis().to_string()),
        ],
    );
}

/// Builds a store of `n` records and one node with 20 relationships; returns the store path.
fn scaled_store(dir: &std::path::Path, n: usize) -> std::path::PathBuf {
    let store = dir.join(format!("store-{n}"));
    let mut d = driver();
    answered(d.step(&format!("create {}", store.display())), "create");
    let mut cmds: Vec<String> = (0..n)
        .map(|i| format!("put-context R{i:06} any {} -", hex(&[i as u8; 100])))
        .collect();
    cmds.extend((0..20).map(|k| format!("put-rel f{k:02} absent links F R{k:06}")));
    commit_batches(&mut d, "build", &cmds, 500);
    close(d);
    store
}

/// Opens `store` in a fresh process and measures opening, one point read and one range read.
fn measure(store: &std::path::Path) -> BTreeMap<String, u64> {
    let mut d = open(store);
    let s0 = stats(&mut d);
    // A second statistics call: the height walk is cached now, so it reads nothing more.
    let s1 = stats(&mut d);
    assert_eq!(s1["pages_read"], s0["pages_read"]);
    let got = answered(d.step("get R000007"), "get");
    assert!(got[0].starts_with("context R000007 rev:"), "{got:?}");
    let s2 = stats(&mut d);
    let rels = answered(d.step("rels F out - 100"), "rels");
    assert_eq!(rels.len(), 20);
    let s3 = stats(&mut d);
    close(d);
    BTreeMap::from([
        ("height".to_string(), s0["height"]),
        ("pages_read_at_open".to_string(), s0["pages_read_at_open"]),
        (
            "point_pages_read".to_string(),
            s2["pages_read"] - s1["pages_read"],
        ),
        (
            "point_entries".to_string(),
            s2["entries_visited"] - s1["entries_visited"],
        ),
        (
            "range_pages_read".to_string(),
            s3["pages_read"] - s2["pages_read"],
        ),
        (
            "range_entries".to_string(),
            s3["entries_visited"] - s2["entries_visited"],
        ),
    ])
}

#[test]
#[ignore = "workload tier: release build, --ignored, one test thread"]
fn opening_and_point_and_range_reads_do_not_grow_with_the_store() {
    let t0 = Instant::now();
    let dir = test_dir("workload-scale");
    const N: usize = 2_000;
    let small = measure(&scaled_store(&dir, N));
    let large = measure(&scaled_store(&dir, 8 * N));
    // Work bounded by the tree's height and the result size, not by the number of records.
    let extra_levels = large["height"] - small["height"];
    assert!(
        large["pages_read_at_open"] <= small["pages_read_at_open"] + 2 * extra_levels,
        "opening read {} pages at 8N vs {} at N",
        large["pages_read_at_open"],
        small["pages_read_at_open"]
    );
    assert_eq!(small["point_entries"], 1);
    assert_eq!(large["point_entries"], 1);
    assert!(large["point_pages_read"] <= large["height"]);
    // Each relationship: its index entry and its record.
    assert_eq!(small["range_entries"], 40);
    assert_eq!(large["range_entries"], 40);
    assert!(large["range_pages_read"] <= small["range_pages_read"] + 2 * (extra_levels + 1));
    report(
        "scale",
        &[
            ("n", N.to_string()),
            ("n_large", (8 * N).to_string()),
            ("at_n", map_json(&small)),
            ("at_8n", map_json(&large)),
            ("elapsed_ms", t0.elapsed().as_millis().to_string()),
        ],
    );
}
