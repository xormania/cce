//! Golden fixtures: store directories committed to the repository, written by the same workload.
//! `fixtures/format-4` was written by the format-4 engine and must be refused, unchanged;
//! `fixtures/format-5` was written by this engine, must open with exactly the workload's contents,
//! and is what this engine still writes, byte for byte.

mod common;

use std::io::Read;
use std::path::{Path, PathBuf};

use cce_core::storage::{
    AttemptId, Change, ChangeSet, ContentBinding, ContentDigest, ContentRef, ContextId, Direction,
    Evidence, HistoryEntry, HistoryPosition, Precondition, ReadView, Relationship, RelationshipId,
    RelationshipKind, RelationshipQuery, Revision, StorageEngine, StorageError,
};
use cce_storage::NativeStore;
use sha2::Digest;

const FILES: [&str; 3] = ["meta.cce", "content.cce", "LOCK"];

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name)
}

/// The files of a store directory, by name.
fn files(dir: &Path) -> Vec<(&'static str, Vec<u8>)> {
    FILES
        .map(|f| (f, std::fs::read(dir.join(f)).unwrap()))
        .to_vec()
}

/// A copy of fixture `name` in a fresh test directory, and the guard of that directory.
fn copy_of(name: &str) -> (common::resources::TestDir, PathBuf) {
    let base = common::test_dir(&format!("golden-{name}"));
    let dir = base.join("store");
    std::fs::create_dir(&dir).unwrap();
    for (f, bytes) in files(&fixture(name)) {
        std::fs::write(dir.join(f), bytes).unwrap();
    }
    (base, dir)
}

/// The store identity of every golden fixture.
pub const GOLDEN_STORE_ID: [u8; 16] = *b"golden-fixture-1";

/// Content bound by the first commit: two segments, the second partial.
pub fn golden_first_content() -> Vec<u8> {
    (0..70_000u32).map(|i| (i * 7 + 3) as u8).collect()
}

/// Content bound by the third commit, replacing the first.
pub fn golden_second_content() -> Vec<u8> {
    (0..3_000u32).map(|i| (i * 13 + 1) as u8).collect()
}

fn ctx(s: &str) -> ContextId {
    ContextId::new(s.as_bytes().to_vec()).unwrap()
}

fn rel(s: &str) -> RelationshipId {
    RelationshipId::new(s.as_bytes().to_vec()).unwrap()
}

/// One request of the golden workload: its attempt, changes, the content it binds at index 0 (if
/// any) and its evidence.
pub type GoldenRequest = (AttemptId, Vec<Change>, Option<Vec<u8>>, Vec<Evidence>);

/// The three requests of the golden workload, in commit order.
pub fn golden_requests() -> Vec<GoldenRequest> {
    let evidence = |b: &str| {
        vec![Evidence {
            label: b"reason".to_vec(),
            bytes: b.as_bytes().to_vec(),
        }]
    };
    vec![
        (
            AttemptId::new(*b"golden-1").unwrap(),
            vec![
                Change::PutContext {
                    id: ctx("alpha"),
                    expect: Precondition::Absent,
                    body: b"alpha v1".to_vec(),
                    content: Some(0),
                },
                Change::PutContext {
                    id: ctx("beta"),
                    expect: Precondition::Absent,
                    body: b"beta v1".to_vec(),
                    content: None,
                },
                Change::PutRelationship {
                    id: rel("r-ab"),
                    expect: Precondition::Absent,
                    kind: RelationshipKind::new(*b"depends").unwrap(),
                    source: ctx("alpha"),
                    target: ctx("beta"),
                },
            ],
            Some(golden_first_content()),
            evidence("first"),
        ),
        (
            AttemptId::new(*b"golden-2").unwrap(),
            vec![
                Change::PutContext {
                    id: ctx("beta"),
                    expect: Precondition::AtRevision(Revision(1)),
                    body: b"beta v2".to_vec(),
                    content: None,
                },
                Change::PutContext {
                    id: ctx("gamma"),
                    expect: Precondition::Absent,
                    body: b"gamma v1".to_vec(),
                    content: None,
                },
                Change::PutRelationship {
                    id: rel("r-gb"),
                    expect: Precondition::Absent,
                    kind: RelationshipKind::new(*b"cites").unwrap(),
                    source: ctx("gamma"),
                    target: ctx("beta"),
                },
            ],
            None,
            evidence("second"),
        ),
        (
            AttemptId::new(*b"golden-3").unwrap(),
            vec![
                Change::RemoveRelationship {
                    id: rel("r-gb"),
                    expect: Precondition::AtRevision(Revision(2)),
                },
                Change::RemoveContext {
                    id: ctx("gamma"),
                    expect: Precondition::AtRevision(Revision(2)),
                },
                Change::PutContext {
                    id: ctx("alpha"),
                    expect: Precondition::AtRevision(Revision(1)),
                    body: b"alpha v2".to_vec(),
                    content: Some(0),
                },
            ],
            Some(golden_second_content()),
            evidence("third"),
        ),
    ]
}

/// Creates a store in `dir` and commits the golden workload to it.
pub fn write_golden_store(dir: &std::path::Path) {
    let store = NativeStore::create(dir, GOLDEN_STORE_ID).unwrap();
    for (attempt, changes, content, evidence) in golden_requests() {
        let content = content
            .map(|bytes| {
                let staged = store.stage_content(&mut &bytes[..]).unwrap();
                vec![ContentBinding::from(&staged)]
            })
            .unwrap_or_default();
        let view = store.begin().unwrap();
        store
            .commit(
                attempt,
                view,
                ChangeSet {
                    changes,
                    content,
                    evidence,
                    ..ChangeSet::default()
                },
            )
            .unwrap();
    }
    drop(store);
}

/// What a view of the golden store must read: every context record of the workload, the
/// relationships listed from each of them in both directions, and the whole history.
fn read_all(v: &mut dyn ReadView) -> String {
    let mut out = Vec::new();
    for name in ["alpha", "beta", "gamma"] {
        let record = v.context(&ctx(name)).unwrap().map(|r| {
            let content = r.content.map(|c| {
                let mut bytes = Vec::new();
                v.content(&c).unwrap().read_to_end(&mut bytes).unwrap();
                (c, sha2::Sha256::digest(&bytes).to_vec())
            });
            (r.revision, r.body, content)
        });
        out.push(format!("context {name}: {record:?}"));
        for direction in [Direction::Outgoing, Direction::Incoming] {
            let q = RelationshipQuery {
                from: ctx(name),
                direction,
                kind: None,
                after: None,
            };
            let listed: Vec<Relationship> =
                v.relationships(&q).unwrap().map(|r| r.unwrap()).collect();
            out.push(format!("{direction:?} {name}: {listed:?}"));
        }
    }
    let history: Vec<HistoryEntry> = v
        .history(HistoryPosition(0))
        .unwrap()
        .map(|h| h.unwrap())
        .collect();
    out.push(format!("history: {history:?}"));
    out.join("\n")
}

fn content_ref(bytes: &[u8]) -> ContentRef {
    ContentRef {
        digest: ContentDigest(sha2::Sha256::digest(bytes).into()),
        len: bytes.len() as u64,
    }
}

/// The contents the workload leaves, built from its requests and written-out final records.
fn expected() -> String {
    let second = golden_second_content();
    let r_ab = Relationship {
        id: rel("r-ab"),
        kind: RelationshipKind::new(*b"depends").unwrap(),
        source: ctx("alpha"),
        target: ctx("beta"),
        revision: Revision(1),
    };
    let alpha = Some((
        Revision(3),
        b"alpha v2".to_vec(),
        Some((content_ref(&second), sha2::Sha256::digest(&second).to_vec())),
    ));
    let beta = Some((
        Revision(2),
        b"beta v2".to_vec(),
        None::<(ContentRef, Vec<u8>)>,
    ));
    let none: Vec<Relationship> = Vec::new();
    let history: Vec<HistoryEntry> = golden_requests()
        .into_iter()
        .zip(1..)
        .map(
            |((attempt, changes, content, evidence), position)| HistoryEntry {
                position: HistoryPosition(position),
                attempt,
                evidence,
                changes,
                content: content.iter().map(|c| content_ref(c)).collect(),
            },
        )
        .collect();
    [
        format!("context alpha: {alpha:?}"),
        format!("Outgoing alpha: {:?}", [&r_ab]),
        format!("Incoming alpha: {none:?}"),
        format!("context beta: {beta:?}"),
        format!("Outgoing beta: {none:?}"),
        format!("Incoming beta: {:?}", [&r_ab]),
        "context gamma: None".to_string(),
        format!("Outgoing gamma: {none:?}"),
        format!("Incoming gamma: {none:?}"),
        format!("history: {history:?}"),
    ]
    .join("\n")
}

#[test]
fn a_format_4_store_is_refused_as_unsupported_and_left_unchanged() {
    let (_base, dir) = copy_of("format-4");
    let before = files(&dir);
    for how in ["read-write", "read-only"] {
        let err = match how {
            "read-write" => NativeStore::open(&dir).err(),
            _ => NativeStore::open_read_only(&dir).err(),
        }
        .unwrap_or_else(|| panic!("the format-4 store opened {how}"));
        assert!(
            matches!(&err, StorageError::Unsupported(m) if m.contains("format version 4")),
            "{how}: {err:?}"
        );
        assert!(
            files(&dir) == before,
            "refusing the format-4 store {how} changed it"
        );
    }
}

#[test]
fn a_format_5_store_opens_with_exactly_the_workload_contents() {
    let (_base, dir) = copy_of("format-5");
    let before = files(&dir);
    let ro = NativeStore::open_read_only(&dir).unwrap();
    assert_eq!(read_all(&mut ro.begin().unwrap()), expected(), "read-only");
    assert!(
        ro.verify(cce_storage::VerifyLevel::Scrub)
            .unwrap()
            .is_consistent()
    );
    drop(ro);
    assert!(files(&dir) == before, "a read-only open changed the store");
    let rw = NativeStore::open(&dir).unwrap();
    assert_eq!(read_all(&mut rw.begin().unwrap()), expected(), "read-write");
}

#[test]
fn this_engine_writes_the_format_5_fixture_byte_for_byte() {
    // A failure means the stored format changed: such a change needs a new format version and a
    // new fixture (`regenerate_the_format_5_fixture`), with the old one kept as a refused format.
    let base = common::test_dir("golden-write");
    let dir = base.join("store");
    write_golden_store(&dir);
    for ((name, written), (_, committed)) in
        files(&dir).into_iter().zip(files(&fixture("format-5")))
    {
        assert!(
            written == committed,
            "{name} differs from the format-5 fixture"
        );
    }
}

/// Rewrites `fixtures/format-5` with this engine. Run explicitly, only for a new format version.
#[test]
#[ignore]
fn regenerate_the_format_5_fixture() {
    let dir = fixture("format-5");
    let _ = std::fs::remove_dir_all(&dir);
    write_golden_store(&dir);
}
