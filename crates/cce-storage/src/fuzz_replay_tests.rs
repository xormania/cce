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
    out
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
            "page", "slots", "node", "manifest", "history", "records", "image"
        ]
    );
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
