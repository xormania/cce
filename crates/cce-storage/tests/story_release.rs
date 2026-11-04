//! The growing end-to-end story, in order, through real files and a fresh process at every reopen:
//! the first bundle, an unclean exit, an identical retry, growth past pages and the cache, an old
//! view held across newer commits, a conflict over an empty range beside a disjoint commit, and
//! removal under both reader modes with actual reuse of the freed space. Meant to run in a release
//! build as well as in the ordinary test run.

mod common;

use common::story::{LARGE_LEN, first_bundle};
use common::{Driver, Step, answered, gen_bytes, hex, sha256, test_dir};

/// Records and relationships only (no history), so states compare across commits.
fn state(dump: &[String]) -> Vec<String> {
    dump.iter()
        .filter(|l| l.starts_with("context ") || l.starts_with("content ") || l.starts_with("rel "))
        .cloned()
        .collect()
}

fn committed(d: &mut Driver, attempt: &str) -> u64 {
    let got = answered(d.step(&format!("commit {attempt}")), attempt);
    got.first()
        .and_then(|l| l.strip_prefix("committed "))
        .and_then(|p| p.parse().ok())
        .unwrap_or_else(|| panic!("{attempt}: {got:?}"))
}

fn run(d: &mut Driver, cmds: &[&str]) {
    for c in cmds {
        answered(d.step(c), c);
    }
}

fn stat(d: &mut Driver, name: &str) -> u64 {
    let s = answered(d.step("stats"), "stats").join(" ");
    s.split(' ')
        .find_map(|f| f.strip_prefix(&format!("{name}=")))
        .and_then(|v| v.parse().ok())
        .unwrap_or_else(|| panic!("{name} in {s}"))
}

fn open(store: &std::path::Path, cache: Option<usize>) -> Driver {
    let mut d = Driver::spawn();
    let cmd = match cache {
        Some(n) => format!("open {} {n}", store.display()),
        None => format!("open {}", store.display()),
    };
    answered(d.step(&cmd), "open");
    d
}

/// Ends a process without any shutdown: it is killed while holding the store.
fn kill(mut d: Driver) {
    assert_eq!(d.wait_for("hold", "holding"), Step::Answered(vec![]));
    assert!(d.kill().code().is_none(), "the writer must die by signal");
}

fn close(mut d: Driver) {
    answered(d.step("close"), "close");
    assert_eq!(d.finish().and_then(|s| s.code()), Some(0));
}

/// The record line `dump` prints for a context without content.
fn plain(id: &str, rev: u64, body: &[u8]) -> String {
    format!("context {id} rev:{rev} {} - -", hex(body))
}

fn read_content(d: &mut Driver, bytes: &[u8]) -> Vec<String> {
    answered(
        d.step(&format!(
            "read-content {} {}",
            hex(&sha256(bytes)),
            bytes.len()
        )),
        "read-content",
    )
}

const HUB_EDGES: usize = 300;

#[test]
fn the_complete_story_in_order_through_fresh_processes() {
    let dir = test_dir("story-release");
    let store = dir.join("store");
    let content_file = store.join("content.cce");
    let bundle = first_bundle();
    let large = &bundle.contents[1].bytes;

    // 1–3. Staged content is not context; one transaction commits the whole bundle exactly.
    let mut d = Driver::spawn();
    answered(d.step(&format!("create {}", store.display())), "create");
    for cmd in bundle.commands() {
        answered(d.step(&cmd), &cmd);
    }
    assert_eq!(
        state(&answered(d.step("dump A,B,C"), "dump")),
        ["context A absent", "context B absent", "context C absent"]
    );
    assert_eq!(
        answered(d.step(&format!("commit {}", bundle.attempt)), "commit"),
        ["committed 1"]
    );
    assert_eq!(
        answered(d.step("dump A,B,C"), "dump"),
        bundle.expected_dump(1)
    );
    // 4. No graceful shutdown.
    kill(d);

    // 4–5. A fresh process sees the whole bundle; the identical retry returns the original
    // receipt and adds nothing.
    let mut d = open(&store, None);
    assert_eq!(
        answered(d.step("dump A,B,C"), "dump"),
        bundle.expected_dump(1)
    );
    for cmd in bundle.commands() {
        answered(d.step(&cmd), &cmd);
    }
    assert_eq!(
        answered(d.step(&format!("commit {}", bundle.attempt)), "retry"),
        ["committed 1"]
    );
    assert_eq!(
        answered(d.step("dump A,B,C"), "dump"),
        bundle.expected_dump(1)
    );

    // 6. Grow past one leaf and one branch: a hub with many outgoing relationships.
    let mut expected_hub: Vec<String> = Vec::new();
    for batch in 0..HUB_EDGES / 50 {
        for i in batch * 50..(batch + 1) * 50 {
            let (id, target) = (format!("hub-{i:04}"), format!("N{i:04}"));
            answered(
                d.step(&format!("put-rel {id} absent depends H {target}")),
                "put-rel",
            );
            expected_hub.push(format!("page depends {target} {id}"));
        }
        committed(&mut d, &format!("hub-{batch}"));
    }
    expected_hub.sort();
    close(d);

    // A fresh process with a tiny node cache pages through the hub in both directions.
    let mut d = open(&store, Some(8));
    let mut got: Vec<String> = Vec::new();
    loop {
        let after = got
            .last()
            .map(|l| format!(" {}", l.trim_start_matches("page ")))
            .unwrap_or_default();
        let page = answered(d.step(&format!("rels H out depends 37{after}")), "rels");
        if page.is_empty() {
            break;
        }
        got.extend(page);
    }
    assert_eq!(
        got, expected_hub,
        "the hub's outgoing relationships, in order"
    );
    assert_eq!(
        answered(d.step("rels N0123 in - 10"), "rels in"),
        ["page depends H hub-0123"]
    );
    assert!(stat(&mut d, "height") >= 2, "the tree did not grow a level");
    assert!(
        stat(&mut d, "evictions") > 0,
        "the tiny cache never evicted"
    );
    close(d);

    // 7. An old view keeps reading its state across newer commits, and the space it can still
    // reach is not reused while it lives.
    let mut d = open(&store, None);
    run(&mut d, &["view old"]);
    let old = state(&answered(d.step("dump-in old A,B,C"), "dump old"));
    run(&mut d, &["put-context A rev:1 412076322062 -"]);
    // A record's revision is the history position of the commit that last wrote it.
    let rev_a = committed(&mut d, "a-v2");
    run(&mut d, &["remove-context B any"]);
    committed(&mut d, "b-removed");
    let now = state(&answered(d.step("dump A,B"), "dump new"));
    assert_eq!(now[0], plain("A", rev_a, b"A v2 b"));
    assert!(now.contains(&"context B absent".to_string()), "{now:?}");
    let reused_before = stat(&mut d, "content_reused");
    let y = gen_bytes(77, LARGE_LEN);
    run(
        &mut d,
        &[
            &format!("stage Y gen {LARGE_LEN} 77"),
            "put-context D absent 44 Y",
        ],
    );
    committed(&mut d, "d");
    assert_eq!(
        stat(&mut d, "content_reused"),
        reused_before,
        "space the old view can reach was reused"
    );
    assert_eq!(
        state(&answered(d.step("dump-in old A,B,C"), "dump old again")),
        old,
        "the old view's state changed"
    );
    run(&mut d, &["drop-view old"]);
    close(d);

    // A fresh process: the removed large content is no longer retained, the new content is.
    let mut d = open(&store, None);
    assert_eq!(
        read_content(&mut d, large),
        ["content-error ContentNotRetained"]
    );
    assert_eq!(read_content(&mut d, &y), [format!("content {}", hex(&y))]);

    // 8. A read of an empty range conflicts with a commit inserting into it; a commit disjoint
    // from what its view read commits without losing its neighbour's change.
    run(&mut d, &["view v1"]);
    assert_eq!(
        state(&answered(d.step("dump-in v1 X"), "dump X")),
        ["context X absent"]
    );
    run(&mut d, &["put-rel r9 absent cites A X"]);
    let rev_r9 = committed(&mut d, "r9");
    run(&mut d, &["put-context F absent 46 -"]);
    let r = answered(d.step("commit-from v1 f-stale"), "stale commit");
    assert!(
        r.len() == 1 && r[0].starts_with("commit-error Conflict("),
        "{r:?}"
    );
    assert_eq!(
        answered(d.step("outcome f-stale"), "outcome"),
        ["outcome absent"]
    );
    run(&mut d, &["view v2"]);
    assert_eq!(
        state(&answered(d.step("dump-in v2 F"), "dump F")),
        ["context F absent"]
    );
    run(&mut d, &["put-context C rev:1 432076322062 -"]);
    let rev_c = committed(&mut d, "c-v2");
    run(&mut d, &["put-context F absent 46 -"]);
    let rev_f = committed_from(&mut d, "v2", "f");
    kill(d);

    let mut d = open(&store, None);
    let s = state(&answered(d.step("dump C,F,X"), "dump C,F,X"));
    assert!(
        s.contains(&plain("C", rev_c, b"C v2 b")),
        "neighbour lost: {s:?}"
    );
    assert!(s.contains(&plain("F", rev_f, b"F")), "{s:?}");
    let r9 = format!(
        "rel in X {} {} {} {} rev:{rev_r9}",
        hex(b"r9"),
        hex(b"cites"),
        hex(b"A"),
        hex(b"X")
    );
    assert!(s.contains(&r9), "{s:?}");

    // 9. A quiescent removal waits for readers; after it, the freed space is actually reused.
    run(
        &mut d,
        &["keep-reader D", "removal quiescent", "remove-context D any"],
    );
    let r = answered(d.step("commit q"), "quiescent commit");
    assert!(
        r.len() == 1 && r[0].starts_with("commit-error ReadersActive(\"1 "),
        "{r:?}"
    );
    assert_eq!(answered(d.step("outcome q"), "outcome"), ["outcome absent"]);
    run(
        &mut d,
        &["drop-readers", "removal quiescent", "remove-context D any"],
    );
    committed(&mut d, "q");
    for k in 0..2 {
        run(&mut d, &["put-context advance any - -"]);
        committed(&mut d, &format!("advance-{k}"));
    }
    let size = std::fs::metadata(&content_file).unwrap().len();
    let reused_before = stat(&mut d, "content_reused");
    let w = gen_bytes(78, LARGE_LEN);
    run(
        &mut d,
        &[
            &format!("stage W gen {LARGE_LEN} 78"),
            "put-context G absent 47 W",
        ],
    );
    let rev_g = committed(&mut d, "g");
    assert!(
        stat(&mut d, "content_reused") > reused_before,
        "the freed space was not reused"
    );
    let grown = std::fs::metadata(&content_file).unwrap().len() - size;
    assert!(grown < LARGE_LEN, "content file grew by {grown} bytes");
    close(d);

    let mut d = open(&store, None);
    assert_eq!(
        read_content(&mut d, &y),
        ["content-error ContentNotRetained"]
    );
    assert_eq!(read_content(&mut d, &w), [format!("content {}", hex(&w))]);
    let s = state(&answered(d.step("dump A,B,C,D,F,G"), "final dump"));
    assert_eq!(
        s.iter()
            .filter(|l| l.starts_with("context "))
            .cloned()
            .collect::<Vec<_>>(),
        [
            plain("A", rev_a, b"A v2 b"),
            "context B absent".to_string(),
            plain("C", rev_c, b"C v2 b"),
            "context D absent".to_string(),
            plain("F", rev_f, b"F"),
            format!(
                "context G rev:{rev_g} {} {} {}",
                hex(b"G"),
                hex(&sha256(&w)),
                w.len()
            ),
        ]
    );
    close(d);
}

fn committed_from(d: &mut Driver, view: &str, attempt: &str) -> u64 {
    let got = answered(d.step(&format!("commit-from {view} {attempt}")), attempt);
    got.first()
        .and_then(|l| l.strip_prefix("committed "))
        .and_then(|p| p.parse().ok())
        .unwrap_or_else(|| panic!("{attempt}: {got:?}"))
}
