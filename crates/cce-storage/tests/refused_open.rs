//! Opening an existing store on real files whose publication records are damaged, written in
//! another format version (current or earlier record layout) or require a feature this build
//! lacks is refused, read-write and read-only, and the refusal leaves every file of the store
//! directory byte-identical: nothing is reinitialized, rewritten or added.

mod common;

use std::io::Read;
use std::path::{Path, PathBuf};

use cce_core::storage::{
    AttemptId, Change, ChangeSet, ContentBinding, ContextId, Precondition, ReadView, StorageEngine,
    StorageError,
};
use cce_storage::NativeStore;

const PAGE: usize = 4096;
/// Publication record: magic 4, version 2, required features 4, store identity 16, six u64
/// fields, then CRC-32 of everything before it.
const RECORD: usize = 4 + 2 + 4 + 16 + 6 * 8;
/// The record of earlier format versions: no required-features field.
const EARLIER_RECORD: usize = 4 + 2 + 16 + 6 * 8;

fn context() -> ContextId {
    ContextId::new(*b"A").unwrap()
}

fn payload() -> Vec<u8> {
    common::gen_bytes(0x0f3e, 70_000)
}

/// A store with one committed record that binds content.
fn committed_store(dir: &Path) -> PathBuf {
    let store = dir.join("store");
    let s = NativeStore::create(&store, *b"refused-open-tst").expect("create");
    let staged = s.stage_content(&mut &payload()[..]).expect("stage");
    let cs = ChangeSet {
        changes: vec![Change::PutContext {
            id: context(),
            expect: Precondition::Absent,
            body: b"committed body".to_vec(),
            content: Some(0),
        }],
        content: vec![ContentBinding::from(&staged)],
        ..ChangeSet::default()
    };
    let view = s.begin().expect("begin");
    s.commit(AttemptId::new(*b"first").unwrap(), view, cs)
        .expect("commit");
    s.close().expect("close");
    store
}

/// Every entry of a directory with its complete bytes, in name order.
fn image(dir: &Path) -> Vec<(String, Vec<u8>)> {
    let mut v: Vec<(String, Vec<u8>)> = std::fs::read_dir(dir)
        .expect("read store directory")
        .map(|e| {
            let e = e.expect("entry");
            (
                e.file_name().to_string_lossy().into_owned(),
                std::fs::read(e.path()).expect("read store file"),
            )
        })
        .collect();
    v.sort();
    v
}

fn copy_store(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for (name, bytes) in image(from) {
        std::fs::write(to.join(name), bytes).unwrap();
    }
}

/// Applies `edit` to every publication record in `meta` and gives it a valid checksum again, as an
/// engine writing such a record would have.
fn rewrite_records(meta: &mut [u8], edit: impl Fn(&mut [u8])) {
    for slot in [0, PAGE] {
        let rec = &mut meta[slot..slot + RECORD + 4];
        if &rec[..4] != b"CCED" {
            continue;
        }
        edit(rec);
        let crc = crc32fast::hash(&rec[..RECORD]);
        rec[RECORD..].copy_from_slice(&crc.to_le_bytes());
    }
}

/// Rewrites every publication record in the earlier layout (no required-features field) as format
/// `version`, as an engine of that version would have written it.
fn rewrite_earlier_layout(meta: &mut [u8], version: u16) {
    for slot in [0, PAGE] {
        let rec = &mut meta[slot..slot + RECORD + 4];
        if &rec[..4] != b"CCED" {
            continue;
        }
        let mut old = Vec::with_capacity(EARLIER_RECORD + 4);
        old.extend_from_slice(&rec[..4]);
        old.extend_from_slice(&version.to_le_bytes());
        old.extend_from_slice(&rec[10..RECORD]);
        let crc = crc32fast::hash(&old);
        old.extend_from_slice(&crc.to_le_bytes());
        rec.fill(0);
        rec[..old.len()].copy_from_slice(&old);
    }
}

fn current_version(meta: &[u8]) -> u16 {
    assert_eq!(&meta[..4], b"CCED", "slot 0 holds the newest record");
    u16::from_le_bytes([meta[4], meta[5]])
}

#[test]
fn opening_a_damaged_or_other_version_store_is_refused_and_changes_no_file() {
    let dir = common::test_dir("refused-open");
    let store = committed_store(&dir);
    let original = std::fs::read(store.join("meta.cce")).unwrap();
    let version = current_version(&original);

    let mut damaged = original.clone();
    for b in &mut damaged[..2 * PAGE] {
        *b ^= 0x5a;
    }
    // (what, meta file, expected refusal message)
    let mut cases: Vec<(String, Vec<u8>, String)> = vec![(
        "damaged records".into(),
        damaged,
        "no valid publication record".into(),
    )];
    for v in [version - 1, 1, version + 1] {
        let mut m = original.clone();
        rewrite_records(&mut m, |r| r[4..6].copy_from_slice(&v.to_le_bytes()));
        cases.push((
            format!("format version {v}"),
            m,
            format!("format version {v}"),
        ));
    }
    let mut m = original.clone();
    rewrite_earlier_layout(&mut m, version - 1);
    cases.push((
        format!("earlier record layout, format version {}", version - 1),
        m,
        format!("format version {}", version - 1),
    ));
    let mut m = original.clone();
    // Bit 0 (history markers) is known; the next bit is not.
    rewrite_records(&mut m, |r| r[6..10].copy_from_slice(&2u32.to_le_bytes()));
    cases.push((
        "unknown required feature".into(),
        m,
        "unknown required features 0x2".into(),
    ));

    for (i, (what, meta, message)) in cases.iter().enumerate() {
        let copy = dir.join(format!("case-{i}"));
        copy_store(&store, &copy);
        std::fs::write(copy.join("meta.cce"), meta).unwrap();
        let before = image(&copy);
        for read_only in [false, true] {
            let err = if read_only {
                NativeStore::open_read_only(&copy).err()
            } else {
                NativeStore::open(&copy).err()
            };
            let err = err.unwrap_or_else(|| panic!("{what}: opened (read-only {read_only})"));
            if what.starts_with("damaged") {
                assert!(
                    matches!(&err, StorageError::Integrity(m) if m.contains(message.as_str())),
                    "{what}: {err:?}"
                );
            } else {
                assert!(
                    matches!(&err, StorageError::Unsupported(m) if m.contains(message.as_str())),
                    "{what}: {err:?}"
                );
            }
            assert!(
                image(&copy) == before,
                "{what}: refused open (read-only {read_only}) changed the store directory"
            );
        }
    }

    // Positive neighbour: the untouched store opens with its committed record and content.
    let s = NativeStore::open(&store).expect("open the untouched store");
    let mut v = s.begin().unwrap();
    let rec = v.context(&context()).unwrap().expect("committed record");
    assert_eq!(rec.body, b"committed body");
    let mut bytes = Vec::new();
    v.content(&rec.content.expect("bound content"))
        .unwrap()
        .read_to_end(&mut bytes)
        .unwrap();
    assert!(bytes == payload(), "committed content bytes");
    drop(v);
    s.close().unwrap();
}
