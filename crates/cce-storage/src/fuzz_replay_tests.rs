//! Stable side of fuzzing: the committed seed inputs are exactly what their generator produces and
//! are valid inputs for their targets, and every seed and every saved finding replays through the
//! same harness functions the fuzz targets call.
//!
//! Seeds live in `fuzz/seeds/<target>/` and saved findings (minimized crashing, hanging or
//! oversized inputs) in `fuzz/regressions/<target>/`. Regenerate the seeds after a format change
//! with `cargo test -p cce-storage --lib fuzz_replay_tests::write_seeds -- --ignored`.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use cce_core::storage::{
    AttemptId, Change, ChangeSet, ContentBinding, ContentDigest, ContentRef, ContextId, Evidence,
    Precondition, RelationshipId, RelationshipKind, StorageEngine,
};

use crate::NativeStore;
use crate::codec::key_component;
use crate::engine::{Inner, Options};
use crate::format::{Descriptor, PageKind, PageRef, StoreId, decode_slot, encode_page};
use crate::fuzz_api::{self, TARGETS, join_image, split_image};
use crate::io::FileId;
use crate::model::{
    ByteRun, ChangeSpec, ContentPick, ContentSpec, ExpectSpec, Op, ReadSpec,
    double_binding_sequence, merge_skip_sequence, ops_from_bytes, ops_to_bytes, plain,
};
use crate::records::{AttemptValue, ContextValue, ExtentRef, Manifest, RelValue, encode_history};
use crate::sim::SimIo;
use crate::tree::Node;

const STORE: StoreId = StoreId(*b"fuzz-seed-store!");

fn fuzz_dir(kind: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("fuzz")
        .join(kind)
}

fn cid(s: &str) -> ContextId {
    ContextId::new(s.as_bytes().to_vec()).unwrap()
}

fn put(id: &str, body: &str, content: Option<usize>) -> Change {
    Change::PutContext {
        id: cid(id),
        expect: Precondition::Any,
        body: body.as_bytes().to_vec(),
        content,
    }
}

fn rel(id: &str, source: &str, target: &str) -> Change {
    Change::PutRelationship {
        id: RelationshipId::new(id.as_bytes().to_vec()).unwrap(),
        expect: Precondition::Absent,
        kind: RelationshipKind::new(*b"cites").unwrap(),
        source: cid(source),
        target: cid(target),
    }
}

fn commit(s: &NativeStore, attempt: &str, cs: ChangeSet) {
    s.commit(
        AttemptId::new(attempt.as_bytes().to_vec()).unwrap(),
        s.begin().unwrap(),
        cs,
    )
    .unwrap();
}

/// The durable images of a simulated store, joined as one `image` input.
fn image_of(sim: &SimIo) -> Vec<u8> {
    let durable = |f| sim.file_images_for_test(f).unwrap().0;
    join_image(&durable(FileId::Meta), &durable(FileId::Content))
}

/// A request with content spanning two segments, relationships, evidence and a removal.
fn rich_store() -> (SimIo, ChangeSet) {
    let sim = SimIo::new();
    let s = NativeStore {
        inner: Inner::create(Box::new(sim.clone()), STORE, Options::default()).unwrap(),
    };
    let payload: Vec<u8> = (0..70_000u32).map(|i| (i * 7 + i / 300) as u8).collect();
    let staged = s.stage_content(&mut &payload[..]).unwrap();
    let cs = ChangeSet {
        removal: Default::default(),
        changes: vec![
            put("A", "first", Some(0)),
            put("B", "second", None),
            rel("r1", "A", "B"),
            rel("r2", "B", "A"),
        ],
        content: vec![ContentBinding::from(&staged)],
        evidence: vec![Evidence {
            label: b"why".to_vec(),
            bytes: b"seed".to_vec(),
        }],
    };
    commit(&s, "one", cs.clone());
    commit(
        &s,
        "two",
        ChangeSet {
            changes: vec![
                Change::RemoveRelationship {
                    id: RelationshipId::new(*b"r2").unwrap(),
                    expect: Precondition::Any,
                },
                put("C", "third", None),
            ],
            ..ChangeSet::default()
        },
    );
    drop(s);
    (sim, cs)
}

/// Enough contexts for a tree of more than one level.
fn branching_store() -> SimIo {
    let sim = SimIo::new();
    let s = NativeStore {
        inner: Inner::create(Box::new(sim.clone()), STORE, Options::default()).unwrap(),
    };
    let changes = (0..300)
        .map(|i| put(&format!("ctx-{i:04}"), &format!("body {i}"), None))
        .collect();
    commit(
        &s,
        "many",
        ChangeSet {
            changes,
            ..ChangeSet::default()
        },
    );
    assert!(s.inner.height().unwrap() >= 2, "seed tree has one level");
    drop(s);
    sim
}

/// Every seed: target, file name, bytes. Deterministic.
fn seeds() -> BTreeMap<(&'static str, String), Vec<u8>> {
    let mut out = BTreeMap::new();
    let mut add = |target: &'static str, name: &str, bytes: Vec<u8>| {
        out.insert((target, name.to_string()), bytes);
    };
    let leaf = Node::Leaf(
        [
            (b"a".to_vec(), b"1".to_vec()),
            (b"b\0".to_vec(), vec![2; 40]),
        ]
        .into_iter()
        .collect(),
    );
    let branch = Node::Branch {
        seps: vec![b"m".to_vec(), b"t".to_vec()],
        children: (2..5)
            .map(|page| PageRef {
                page,
                incarnation: 1,
            })
            .collect(),
    };
    add("node", "leaf", leaf.encode());
    add("node", "branch", branch.encode());
    let at = PageRef {
        page: 2,
        incarnation: 1,
    };
    add(
        "page",
        "leaf",
        encode_page(STORE, PageKind::Node, at, &leaf.encode()).unwrap(),
    );
    add(
        "page",
        "branch",
        encode_page(STORE, PageKind::Node, at, &branch.encode()).unwrap(),
    );

    let desc = |generation| Descriptor {
        store: STORE,
        generation,
        root: at,
        next_page: 3,
        content_len: 0,
        history_pos: 0,
    };
    let mut two = desc(2).encode();
    two.extend(desc(1).encode());
    add("slots", "two-valid", two);
    let mut one = vec![0; crate::format::PAGE_SIZE];
    one.extend(desc(1).encode());
    add("slots", "empty-and-valid", one);

    let segment = |offset, len| ExtentRef {
        offset,
        len,
        digest: [3; 32],
    };
    let manifest = Manifest {
        content: ContentRef {
            digest: ContentDigest([4; 32]),
            len: 100,
        },
        segments: vec![segment(0, 60), segment(60, 40)],
    };
    add("manifest", "two-segments", manifest.encode());

    let (rich, request) = rich_store();
    add(
        "history",
        "rich-request",
        encode_history(1, &AttemptId::new(*b"one").unwrap(), &request),
    );

    let ctx = ContextValue {
        revision: 3,
        body: b"body".to_vec(),
        content: Some(manifest.content),
    };
    let relv = RelValue {
        revision: 2,
        kind: b"cites".to_vec(),
        source: b"A".to_vec(),
        target: b"B".to_vec(),
    };
    let attempt = AttemptValue {
        position: 5,
        fingerprint: [6; 32],
    };
    let with = |selector: u8, bytes: Vec<u8>| [vec![selector], bytes].concat();
    add("records", "context", with(0, ctx.encode()));
    add("records", "relationship", with(1, relv.encode()));
    add("records", "attempt", with(2, attempt.encode()));
    add("records", "extent", with(3, segment(9, 9).encode()));
    let mut key = Vec::new();
    key_component(&mut key, b"x\0y");
    key_component(&mut key, b"z");
    add("records", "key", with(4, key));

    let empty = SimIo::new();
    drop(Inner::create(Box::new(empty.clone()), STORE, Options::default()).unwrap());
    add("image", "empty-store", image_of(&empty));
    add("image", "rich-store", image_of(&rich));
    add("image", "branching-store", image_of(&branching_store()));

    let merge_skip = merge_skip_sequence();
    let double = double_binding_sequence();
    add(
        "ops",
        "merge-skip-regression",
        ops_to_bytes(ByteRun::Complete, &merge_skip),
    );
    add(
        "ops",
        "double-binding-regression",
        ops_to_bytes(ByteRun::Complete, &double),
    );
    add(
        "ops",
        "staging-lifecycle",
        ops_to_bytes(ByteRun::Complete, &staging_lifecycle()),
    );
    add(
        "ops",
        "crash-in-merge-skip",
        ops_to_bytes(ByteRun::Crash { cut: 37 }, &merge_skip),
    );
    let failure = ByteRun::Failure {
        cut: 21,
        lossy: false,
        live: true,
    };
    add(
        "ops",
        "failure-in-merge-skip",
        ops_to_bytes(failure, &merge_skip),
    );
    let after = ByteRun::AfterPublication {
        pick: 0,
        live: true,
    };
    add(
        "ops",
        "failure-after-publication",
        ops_to_bytes(after, &double),
    );
    out
}

/// Stages, binds earlier stagings, holds and reads views, submits, retries, discards and
/// reopens: every kind of step at least once.
fn staging_lifecycle() -> Vec<Op> {
    let with_staged = |ctx: u8, k: u8| ChangeSpec::PutContext {
        ctx,
        expect: ExpectSpec::Any,
        body: 1,
        content: Some(ContentPick::Staged(k)),
    };
    vec![
        Op::Stage(ContentSpec { seed: 3, size: 5 }),
        Op::Stage(ContentSpec { seed: 4, size: 0 }),
        Op::Commit(plain(vec![
            with_staged(1, 0),
            ChangeSpec::PutRelationship {
                rel: 1,
                expect: ExpectSpec::Absent,
                kind: 0,
                source: 1,
                target: 2,
            },
        ])),
        Op::Hold(vec![
            ReadSpec::Context(1),
            ReadSpec::Relationships {
                from: 1,
                outgoing: true,
                kind: None,
                page: 1,
            },
        ]),
        Op::SubmitAndPoll(plain(vec![crate::model::put(2, 2)])),
        Op::ReadHeld {
            slot: 0,
            reads: vec![ReadSpec::Content(1)],
        },
        Op::CommitFromHeld {
            slot: 0,
            request: plain(vec![crate::model::put(1, 6)]),
        },
        Op::Retry {
            pick: 0,
            identical: true,
        },
        Op::Retry {
            pick: 1,
            identical: false,
        },
        Op::Discard {
            pick: 0,
            refused: true,
        },
        Op::Discard {
            pick: 0,
            refused: false,
        },
        Op::Release(0),
        Op::Reopen,
        Op::Check,
    ]
}

fn files_in(dir: &Path) -> BTreeMap<String, Vec<u8>> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return BTreeMap::new();
    };
    entries
        .map(|e| e.unwrap().path())
        .filter(|p| p.is_file())
        .map(|p| {
            let name = p.file_name().unwrap().to_string_lossy().into_owned();
            (name, std::fs::read(&p).unwrap())
        })
        .collect()
}

#[test]
fn committed_seeds_match_their_generator() {
    let regenerate = "regenerate with `cargo test -p cce-storage --lib fuzz_replay_tests::write_seeds -- --ignored`";
    let expected = seeds();
    for (target, _) in TARGETS {
        let want: BTreeMap<_, _> = expected
            .iter()
            .filter(|((t, _), _)| t == target)
            .map(|((_, n), b)| (n.clone(), b.clone()))
            .collect();
        assert!(!want.is_empty(), "no seed for target {target}");
        let have = files_in(&fuzz_dir("seeds").join(target));
        assert_eq!(
            have.keys().collect::<Vec<_>>(),
            want.keys().collect::<Vec<_>>(),
            "{target}: seed files differ from the generator; {regenerate}"
        );
        for (name, bytes) in &want {
            assert!(
                have[name] == *bytes,
                "{target}/{name} is stale; {regenerate}"
            );
        }
    }
}

#[test]
fn seeds_are_valid_inputs_for_their_targets() {
    for ((target, name), bytes) in seeds() {
        let what = format!("{target}/{name}");
        match target {
            "node" => assert!(Node::decode(&bytes).is_ok(), "{what}"),
            "page" => {
                let at = PageRef {
                    page: 2,
                    incarnation: 1,
                };
                assert!(
                    crate::format::decode_page(STORE, PageKind::Node, at, &bytes).is_ok(),
                    "{what}"
                );
            }
            "slots" => {
                let (a, b) = bytes.split_at(bytes.len() / 2);
                assert!(
                    [a, b]
                        .iter()
                        .any(|s| matches!(decode_slot(s), crate::format::Slot::Valid(_))),
                    "{what}"
                );
            }
            "manifest" => assert!(Manifest::decode(&bytes).is_ok(), "{what}"),
            "history" => assert!(crate::records::decode_history(&bytes).is_ok(), "{what}"),
            "records" => {} // each selector's value round-trips in the harness itself
            "image" => {
                let (meta, content) = split_image(&bytes);
                let opened = Inner::open_read_only(
                    Box::new(SimIo::from_images(meta, content)),
                    Options::default(),
                );
                assert!(opened.is_ok(), "{what}: {:?}", opened.err());
            }
            "ops" => {
                let (run, ops) = ops_from_bytes(&bytes);
                assert!(!ops.is_empty(), "{what}: no steps");
                assert_eq!(ops_to_bytes(run, &ops), bytes, "{what}: not canonical");
            }
            other => panic!("seed for unknown target {other}"),
        }
    }
}

#[test]
fn every_seed_and_saved_finding_replays_through_its_harness() {
    let mut replayed = 0;
    for (target, _) in TARGETS {
        for kind in ["seeds", "regressions"] {
            for (name, bytes) in files_in(&fuzz_dir(kind).join(target)) {
                // The same dispatch the fuzz targets use.
                let r = std::panic::catch_unwind(|| fuzz_api::run(target, &bytes));
                assert!(r.is_ok(), "{kind}/{target}/{name} fails its harness");
                replayed += 1;
            }
        }
    }
    assert!(replayed >= TARGETS.len(), "nothing replayed");
    assert_eq!(
        TARGETS.iter().map(|(n, _)| *n).collect::<Vec<_>>(),
        [
            "page", "slots", "node", "manifest", "history", "records", "image", "ops"
        ]
    );
}

/// Saved findings the suite must replay (raise when one is added).
const MIN_REGRESSIONS: usize = 1;

/// Checks the layout of a fuzz directory: `seeds/` and `regressions/` both exist, every directory
/// in them is named after a target, those directories hold only files, and nothing else sits at
/// their top level except a `.gitkeep`. Returns the number of saved findings. A misspelled or
/// nested directory would otherwise be skipped by the replay without notice.
fn check_layout(fuzz: &Path) -> Result<usize, String> {
    let names: Vec<&str> = TARGETS.iter().map(|(n, _)| *n).collect();
    let mut findings = 0;
    for kind in ["seeds", "regressions"] {
        let top = fuzz.join(kind);
        let entries = std::fs::read_dir(&top).map_err(|e| format!("{}: {e}", top.display()))?;
        for entry in entries {
            let path = entry.map_err(|e| e.to_string())?.path();
            let name = path.file_name().unwrap().to_string_lossy().into_owned();
            if !path.is_dir() {
                if name == ".gitkeep" {
                    continue;
                }
                return Err(format!("{kind}/{name}: not in a target directory"));
            }
            if !names.contains(&name.as_str()) {
                return Err(format!("{kind}/{name}: not a fuzz target ({names:?})"));
            }
            for inner in std::fs::read_dir(&path).map_err(|e| e.to_string())? {
                let inner = inner.map_err(|e| e.to_string())?.path();
                let file = inner.file_name().unwrap().to_string_lossy().into_owned();
                if inner.is_dir() {
                    return Err(format!("{kind}/{name}/{file}: nested directory"));
                }
                // A .gitkeep only keeps an empty directory in version control.
                if kind == "regressions" && file != ".gitkeep" {
                    findings += 1;
                }
            }
        }
    }
    Ok(findings)
}

#[test]
fn the_fuzz_directories_hold_only_known_targets() {
    let n = check_layout(&fuzz_dir("")).unwrap();
    assert!(n >= MIN_REGRESSIONS, "only {n} saved findings");
}

/// A fuzz directory with one seed directory and one saved finding for `node`, changed by `build`,
/// checked, and removed.
fn layout_case(name: &str, build: impl FnOnce(&Path)) -> Result<usize, String> {
    let dir = std::env::temp_dir().join(format!("cce-fuzz-layout-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    for kind in ["seeds", "regressions"] {
        std::fs::create_dir_all(dir.join(kind).join("node")).unwrap();
    }
    std::fs::write(dir.join("regressions/node/finding"), b"").unwrap();
    build(&dir);
    let r = check_layout(&dir);
    std::fs::remove_dir_all(&dir).unwrap();
    r
}

#[test]
fn a_valid_fuzz_layout_counts_its_saved_findings() {
    assert_eq!(layout_case("valid", |_| {}), Ok(1));
}

#[test]
fn a_gitkeep_inside_a_target_directory_is_not_a_saved_finding() {
    let r = layout_case("gitkeep", |d| {
        std::fs::write(d.join("regressions/node/.gitkeep"), b"").unwrap();
        std::fs::create_dir_all(d.join("regressions/page")).unwrap();
        std::fs::write(d.join("regressions/page/.gitkeep"), b"").unwrap();
    });
    assert_eq!(r, Ok(1), "only the one real finding counts");
}

#[test]
fn a_misspelled_target_directory_is_refused() {
    let r = layout_case("misspelled", |d| {
        std::fs::create_dir_all(d.join("regressions/nodes")).unwrap();
        std::fs::write(d.join("regressions/nodes/finding"), b"").unwrap();
    });
    assert!(matches!(&r, Err(m) if m.contains("nodes")), "{r:?}");
}

#[test]
fn a_nested_directory_is_refused() {
    let r = layout_case("nested", |d| {
        std::fs::create_dir_all(d.join("seeds/node/deeper")).unwrap()
    });
    assert!(matches!(&r, Err(m) if m.contains("deeper")), "{r:?}");
}

#[test]
fn a_missing_regressions_directory_is_refused() {
    let r = layout_case("missing", |d| {
        std::fs::remove_dir_all(d.join("regressions")).unwrap()
    });
    assert!(matches!(&r, Err(m) if m.contains("regressions")), "{r:?}");
}

#[test]
fn a_loose_file_beside_the_target_directories_is_refused() {
    let r = layout_case("stray", |d| {
        std::fs::write(d.join("seeds/loose-input"), b"").unwrap()
    });
    assert!(matches!(&r, Err(m) if m.contains("loose-input")), "{r:?}");
}

#[test]
#[ignore = "writes the seed files; run after a format change"]
fn write_seeds() {
    let root = fuzz_dir("seeds");
    let _ = std::fs::remove_dir_all(&root);
    for ((target, name), bytes) in seeds() {
        let dir = root.join(target);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(name), bytes).unwrap();
    }
}

/// Seeded pseudo-random inputs (xorshift), so the `ops` decoder and harness are exercised on
/// stable without a fuzzing toolchain.
fn random_inputs(seed: u64, n: usize, most: usize) -> Vec<Vec<u8>> {
    let mut r = seed | 1;
    let mut next = move || {
        r ^= r << 13;
        r ^= r >> 7;
        r ^= r << 17;
        r
    };
    (0..n)
        .map(|_| {
            let len = (next() % (most as u64 + 1)) as usize;
            (0..len).map(|_| next() as u8).collect()
        })
        .collect()
}

#[test]
fn ops_inputs_decode_to_steps_their_encoding_reproduces() {
    for data in random_inputs(0x0b5e_ed00_0000_0001, 2_000, 400) {
        let (run, ops) = ops_from_bytes(&data);
        let again = ops_to_bytes(run, &ops);
        let (run2, ops2) = ops_from_bytes(&again);
        assert_eq!(run, run2, "{data:?}");
        assert_eq!(format!("{ops:?}"), format!("{ops2:?}"), "{data:?}");
        assert_eq!(ops_to_bytes(run2, &ops2), again, "{data:?}");
    }
}

/// Budget: `CCE_OPS_SWEEP_CASES` inputs (default 200) from the seed `CCE_OPS_SWEEP_SEED`.
#[test]
fn seeded_random_operation_inputs_run_through_the_ops_harness() {
    let env = |name: &str, default: u64| {
        std::env::var(name)
            .ok()
            .map(|v| {
                v.parse()
                    .unwrap_or_else(|_| panic!("{name} must be a number"))
            })
            .unwrap_or(default)
    };
    let cases = env("CCE_OPS_SWEEP_CASES", 200) as usize;
    let seed = env("CCE_OPS_SWEEP_SEED", 0x0b5e_ed00_0000_0002);
    let mut steps = 0;
    for data in random_inputs(seed, cases, 300) {
        steps += ops_from_bytes(&data).1.len();
        fuzz_api::run("ops", &data);
    }
    println!("ops sweep: {{\"cases\":{cases},\"seed\":\"{seed:#x}\",\"steps\":{steps}}}");
    assert!(steps > cases, "the sweep ran almost no steps");
}
