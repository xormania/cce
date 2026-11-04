//! Decoder corruption matrix: every decoder of stored bytes, given truncated, overflowing,
//! unsupported or inconsistent input, returns a specific bounded error — never a panic, a hang or
//! a large allocation — and still accepts valid inputs at the edges of its ranges.
//!
//! Byte offsets come from the documented layouts, not from the decoders under test. Page header:
//! magic 0..4, version 4..6, kind 6, reserved 7, store 8..24, page 24..32, incarnation 32..40,
//! payload length 40..44, checksum 44..48.

use cce_core::storage::StorageError;

use crate::format::{
    Descriptor, FORMAT_VERSION, PAGE_HEADER_LEN, PAGE_PAYLOAD_MAX, PAGE_SIZE, PageKind, PageRef,
    Slot, StoreId, decode_page, decode_slot, encode_page,
};

const STORE: StoreId = StoreId(*b"decoder-matrix-1");
const AT: PageRef = PageRef {
    page: 7,
    incarnation: 3,
};

fn page(payload: &[u8]) -> Vec<u8> {
    encode_page(STORE, PageKind::Node, AT, payload).unwrap()
}

/// Recomputes the page checksum after a deliberate edit, as a writer of that content would have.
fn reseal(p: &mut [u8]) {
    let len = u32::from_le_bytes(p[40..44].try_into().unwrap()) as usize;
    p[44..48].fill(0);
    let crc = crc32fast::hash(&p[..PAGE_HEADER_LEN + len]);
    p[44..48].copy_from_slice(&crc.to_le_bytes());
}

fn integrity(r: Result<Vec<u8>, StorageError>, needle: &str, what: &str) {
    match r {
        Err(StorageError::Integrity(m)) => assert!(m.contains(needle), "{what}: {m}"),
        other => panic!("{what}: expected an integrity error containing {needle:?}, got {other:?}"),
    }
}

fn unsupported(r: Result<Vec<u8>, StorageError>, needle: &str, what: &str) {
    match r {
        Err(StorageError::Unsupported(m)) => assert!(m.contains(needle), "{what}: {m}"),
        other => panic!("{what}: expected unsupported containing {needle:?}, got {other:?}"),
    }
}

fn decode(p: &[u8]) -> Result<Vec<u8>, StorageError> {
    decode_page(STORE, PageKind::Node, AT, p)
}

#[test]
fn pages_at_the_edges_of_the_valid_range_decode() {
    // The documented sizes, pinned independently of the constants.
    assert_eq!(PAGE_SIZE, 4096);
    assert_eq!(PAGE_HEADER_LEN, 48);
    assert_eq!(PAGE_PAYLOAD_MAX, 4048);
    for len in [0, 1, PAGE_PAYLOAD_MAX] {
        let payload: Vec<u8> = (0..len).map(|i| i as u8).collect();
        assert_eq!(
            decode(&page(&payload)).unwrap(),
            payload,
            "payload of {len} bytes"
        );
    }
    // The layout is as documented.
    let p = page(b"xy");
    assert_eq!(p.len(), PAGE_SIZE);
    assert_eq!(&p[0..4], b"CCEP");
    assert_eq!(u16::from_le_bytes([p[4], p[5]]), FORMAT_VERSION);
    assert_eq!(p[6], PageKind::Node as u8);
    assert_eq!(p[7], 0, "reserved byte");
    assert_eq!(&p[8..24], &STORE.0);
    assert_eq!(u64::from_le_bytes(p[24..32].try_into().unwrap()), AT.page);
    assert_eq!(
        u64::from_le_bytes(p[32..40].try_into().unwrap()),
        AT.incarnation
    );
    assert_eq!(u32::from_le_bytes(p[40..44].try_into().unwrap()), 2);
    assert_eq!(&p[48..50], b"xy");
}

#[test]
fn damaged_or_inconsistent_pages_are_integrity_errors() {
    let good = page(b"payload");
    for len in [
        0,
        1,
        PAGE_HEADER_LEN - 1,
        PAGE_HEADER_LEN,
        PAGE_SIZE - 1,
        PAGE_SIZE + 1,
    ] {
        let mut short = good.clone();
        short.resize(len, 0);
        integrity(decode(&short), "short read", &format!("{len}-byte page"));
    }
    let edit = |f: &dyn Fn(&mut Vec<u8>), seal: bool| {
        let mut p = good.clone();
        f(&mut p);
        if seal {
            reseal(&mut p);
        }
        p
    };
    integrity(decode(&edit(&|p| p[0] ^= 1, true)), "bad magic", "magic");
    integrity(
        decode(&edit(
            &|p| p[40..44].copy_from_slice(&((PAGE_PAYLOAD_MAX + 1) as u32).to_le_bytes()),
            false,
        )),
        "payload length",
        "payload length beyond the page",
    );
    integrity(
        decode(&edit(
            &|p| p[40..44].copy_from_slice(&u32::MAX.to_le_bytes()),
            false,
        )),
        "payload length",
        "maximal payload length",
    );
    integrity(
        decode(&edit(&|p| p[PAGE_HEADER_LEN] ^= 1, false)),
        "checksum",
        "payload byte",
    );
    integrity(
        decode(&edit(&|p| p[44] ^= 1, false)),
        "checksum",
        "checksum byte",
    );
    // A damaged version field is damage, not a page of another format version.
    integrity(
        decode(&edit(&|p| p[4] ^= 0x40, false)),
        "checksum",
        "damaged version",
    );
    integrity(
        decode(&edit(&|p| p[7] ^= 1, false)),
        "checksum",
        "damaged reserved byte",
    );
    integrity(
        decode(&edit(&|p| p[8] ^= 1, true)),
        "different store",
        "foreign store",
    );
    integrity(
        decode(&edit(
            &|p| p[32..40].copy_from_slice(&2u64.to_le_bytes()),
            true,
        )),
        "incarnation",
        "stale incarnation",
    );
    integrity(
        decode(&edit(
            &|p| p[24..32].copy_from_slice(&8u64.to_le_bytes()),
            true,
        )),
        "incarnation",
        "another page's content",
    );
    integrity(decode(&edit(&|p| p[6] = 9, true)), "kind", "unknown kind");
}

#[test]
fn intact_pages_of_another_encoding_are_unsupported() {
    let mut future = page(b"payload");
    future[4..6].copy_from_slice(&(FORMAT_VERSION + 1).to_le_bytes());
    reseal(&mut future);
    unsupported(decode(&future), "format version", "newer version");
    let mut flagged = page(b"payload");
    flagged[7] = 1;
    reseal(&mut flagged);
    unsupported(
        decode(&flagged),
        "reserved",
        "reserved byte set by its writer",
    );
}

fn descriptor() -> Descriptor {
    Descriptor {
        store: STORE,
        generation: u64::MAX - 1,
        root: PageRef {
            page: u64::MAX,
            incarnation: u64::MAX,
        },
        next_page: u64::MAX,
        content_len: u64::MAX,
        history_pos: u64::MAX,
    }
}

/// Recomputes a publication record's checksum, in the current layout: magic 0..4, version 4..6,
/// required features 6..10, store 10..26, generation 26..34, root page 34..42, root incarnation
/// 42..50, next page 50..58, content length 58..66, history position 66..74, checksum 74..78.
fn reseal_slot(s: &mut [u8]) {
    let n = crate::format::DESC_LEN - 4;
    let crc = crc32fast::hash(&s[..n]);
    s[n..n + 4].copy_from_slice(&crc.to_le_bytes());
}

#[test]
fn publication_records_classify_by_their_bytes() {
    let d = descriptor();
    let good = d.encode();
    assert_eq!(good.len(), PAGE_SIZE);
    assert!(
        matches!(decode_slot(&good), Slot::Valid(x) if x == d),
        "extreme field values"
    );
    let len = crate::format::DESC_LEN;
    assert_eq!(&good[0..4], b"CCED");
    assert_eq!(good[6..10], [0; 4], "no required features");
    assert_eq!(
        u64::from_le_bytes(good[26..34].try_into().unwrap()),
        d.generation
    );
    assert!(good[len..].iter().all(|&b| b == 0), "padding");

    assert!(matches!(decode_slot(&vec![0; PAGE_SIZE]), Slot::Empty));
    for len in [0, 1, len - 1] {
        assert!(
            matches!(decode_slot(&good[..len]), Slot::Invalid(m) if m.contains("short")),
            "{len}-byte slot"
        );
    }
    let mut torn = good.clone();
    torn[40..].fill(0);
    assert!(matches!(decode_slot(&torn), Slot::Invalid(m) if m.contains("checksum")));
    let mut version = good.clone();
    version[4] ^= 0x40;
    assert!(
        matches!(decode_slot(&version), Slot::Invalid(m) if m.contains("checksum")),
        "a damaged version is damage"
    );
    let mut magic = good.clone();
    magic[0] ^= 1;
    reseal_slot(&mut magic);
    assert!(matches!(decode_slot(&magic), Slot::Invalid(m) if m.contains("magic")));
    let mut future = good.clone();
    future[4..6].copy_from_slice(&(FORMAT_VERSION + 1).to_le_bytes());
    reseal_slot(&mut future);
    assert_eq!(
        decode_slot(&future),
        Slot::Unsupported(format!("format version {}", FORMAT_VERSION + 1))
    );
}

fn err_contains<T: std::fmt::Debug>(r: Result<T, StorageError>, needle: &str, what: &str) {
    match r {
        Err(StorageError::Integrity(m)) => assert!(m.contains(needle), "{what}: {m}"),
        other => panic!("{what}: expected an integrity error containing {needle:?}, got {other:?}"),
    }
}

fn le32(n: u32) -> [u8; 4] {
    n.to_le_bytes()
}

/// Tree node payload: tag (1 leaf, 2 branch), u32 count, then per leaf entry u16 key length, key,
/// u16 value length, value; per branch child (after the first, a u16-prefixed separator) page and
/// incarnation as u64.
#[test]
fn tree_nodes_reject_malformed_payloads_and_accept_edge_cases() {
    use crate::tree::Node;
    let leaf = |entries: &[(&[u8], &[u8])]| {
        let mut b = vec![1];
        b.extend(le32(entries.len() as u32));
        for (k, v) in entries {
            b.extend((k.len() as u16).to_le_bytes());
            b.extend(*k);
            b.extend((v.len() as u16).to_le_bytes());
            b.extend(*v);
        }
        b
    };
    let child = |b: &mut Vec<u8>, page: u64| {
        b.extend(page.to_le_bytes());
        b.extend(1u64.to_le_bytes());
    };
    // Valid edges: an empty leaf, an empty key and value, a two-child branch.
    assert_eq!(
        Node::decode(&leaf(&[])).unwrap(),
        Node::Leaf(Default::default())
    );
    assert_eq!(
        Node::decode(&leaf(&[(b"", b""), (b"k\0", b"v")])).unwrap(),
        Node::Leaf(
            [
                (b"".to_vec(), b"".to_vec()),
                (b"k\0".to_vec(), b"v".to_vec())
            ]
            .into_iter()
            .collect()
        )
    );
    let mut branch = vec![2];
    branch.extend(le32(2));
    child(&mut branch, 2);
    branch.extend(1u16.to_le_bytes());
    branch.push(b'm');
    child(&mut branch, 3);
    assert_eq!(
        Node::decode(&branch).unwrap(),
        Node::Branch {
            seps: vec![b"m".to_vec()],
            children: vec![
                PageRef {
                    page: 2,
                    incarnation: 1
                },
                PageRef {
                    page: 3,
                    incarnation: 1
                },
            ],
        }
    );

    err_contains(Node::decode(&[]), "truncated", "empty payload");
    err_contains(Node::decode(&[9, 0, 0, 0, 0]), "tag", "unknown node tag");
    // A count far beyond the payload is refused before anything is allocated.
    let mut huge = vec![1];
    huge.extend(le32(u32::MAX));
    err_contains(Node::decode(&huge), "exceeds remaining", "leaf count");
    huge[0] = 2;
    err_contains(Node::decode(&huge), "exceeds remaining", "branch count");
    err_contains(
        Node::decode(&leaf(&[(b"b", b"1"), (b"a", b"2")])),
        "increasing",
        "unsorted keys",
    );
    err_contains(
        Node::decode(&leaf(&[(b"a", b"1"), (b"a", b"2")])),
        "increasing",
        "duplicate keys",
    );
    let mut trailing = leaf(&[(b"a", b"1")]);
    trailing.push(0);
    err_contains(Node::decode(&trailing), "trailing", "trailing byte");
    let mut cut = leaf(&[(b"key", b"value")]);
    cut.pop();
    err_contains(Node::decode(&cut), "truncated", "truncated value");
    let mut one = vec![2];
    one.extend(le32(1));
    child(&mut one, 2);
    err_contains(Node::decode(&one), "children", "branch with one child");
    let mut unsorted = vec![2];
    unsorted.extend(le32(3));
    child(&mut unsorted, 2);
    unsorted.extend(1u16.to_le_bytes());
    unsorted.push(b'm');
    child(&mut unsorted, 3);
    unsorted.extend(1u16.to_le_bytes());
    unsorted.push(b'c');
    child(&mut unsorted, 4);
    err_contains(Node::decode(&unsorted), "increasing", "unsorted separators");
}

/// Content manifest: encoding version byte (1), digest 32, total length u64, u32 segment count,
/// then per segment offset u64, length u64, digest 32.
#[test]
fn content_manifests_reject_inconsistent_segments() {
    use crate::records::{Manifest, SEGMENT_MAX};
    let manifest = |total: u64, segs: &[(u64, u64)]| {
        let mut b = vec![1u8];
        b.extend([7u8; 32]);
        b.extend(total.to_le_bytes());
        b.extend(le32(segs.len() as u32));
        for (off, len) in segs {
            b.extend(off.to_le_bytes());
            b.extend(len.to_le_bytes());
            b.extend([1u8; 32]);
        }
        b
    };
    let content = |len| cce_core::storage::ContentRef {
        digest: cce_core::storage::ContentDigest([7; 32]),
        len,
    };
    assert_eq!(
        Manifest::decode(&manifest(0, &[])).unwrap(),
        Manifest {
            content: content(0),
            segments: vec![],
        },
        "empty content"
    );
    assert_eq!(
        Manifest::decode(&manifest(SEGMENT_MAX, &[(9, SEGMENT_MAX)])).unwrap(),
        Manifest {
            content: content(SEGMENT_MAX),
            segments: vec![crate::records::ExtentRef {
                offset: 9,
                len: SEGMENT_MAX,
                digest: [1; 32],
            }],
        },
        "full segment"
    );
    err_contains(
        Manifest::decode(&manifest(1, &[(0, 0)])),
        "segment length",
        "empty segment",
    );
    err_contains(
        Manifest::decode(&manifest(SEGMENT_MAX + 1, &[(0, SEGMENT_MAX + 1)])),
        "segment length",
        "oversized segment",
    );
    err_contains(
        Manifest::decode(&manifest(5, &[(0, 4)])),
        "segments hold 4 bytes, content is 5",
        "segment sum differs from total",
    );
    let mut version = manifest(0, &[]);
    version[0] = 2;
    assert!(
        matches!(
            Manifest::decode(&version),
            Err(StorageError::Unsupported(_))
        ),
        "a manifest encoding of another version is unsupported"
    );
    let mut huge = vec![1u8];
    huge.extend([7u8; 32]);
    huge.extend(0u64.to_le_bytes());
    huge.extend(le32(u32::MAX));
    err_contains(
        Manifest::decode(&huge),
        "exceeds remaining",
        "segment count",
    );
    let mut trailing = manifest(0, &[]);
    trailing.push(0);
    err_contains(Manifest::decode(&trailing), "trailing", "trailing byte");
}

/// History entry: position u64, u32-prefixed attempt id, u32-prefixed request (encoding byte, u32
/// change count, changes, u32 binding count, bindings, u32 evidence count, evidence).
#[test]
fn history_entries_reject_malformed_requests() {
    use crate::records::decode_history;
    let entry = |req: &[u8]| {
        let mut b = 1u64.to_le_bytes().to_vec();
        b.extend(le32(1));
        b.push(b'a');
        b.extend(le32(req.len() as u32));
        b.extend(req);
        b
    };
    let request = |changes: &[u8], n: u32, bindings: u32| {
        let mut r = vec![1];
        r.extend(le32(n));
        r.extend(changes);
        r.extend(le32(bindings));
        r.extend(std::iter::repeat_n(0u8, 40 * bindings as usize));
        r.extend(le32(0));
        r
    };
    let empty = decode_history(&entry(&request(&[], 0, 0))).unwrap();
    assert_eq!(empty.position, cce_core::storage::HistoryPosition(1));
    // Version 2 ends with the removal mode: 0 or 1, nothing else, and nothing after it.
    let v2 = |mode: &[u8]| {
        let mut r = request(&[], 0, 0);
        r[0] = 2;
        r.extend(mode);
        r
    };
    for mode in [0u8, 1] {
        assert!(
            decode_history(&entry(&v2(&[mode]))).is_ok(),
            "removal mode {mode}"
        );
    }
    err_contains(
        decode_history(&entry(&v2(&[2]))),
        "removal mode",
        "unknown removal mode",
    );
    err_contains(
        decode_history(&entry(&v2(&[]))),
        "truncated",
        "missing removal mode",
    );
    err_contains(
        decode_history(&entry(&v2(&[0, 0]))),
        "trailing",
        "byte after the mode",
    );
    assert_eq!(empty.attempt.as_bytes(), b"a");
    assert!(empty.changes.is_empty() && empty.content.is_empty() && empty.evidence.is_empty());
    err_contains(
        decode_history(&entry(&request(&[9], 1, 0))),
        "change tag",
        "unknown change tag",
    );
    // PutContext id "A", any precondition (tag 0), empty body, then the content tag.
    let put = |content: &[u8]| {
        let mut c = vec![1];
        c.extend(le32(1));
        c.push(b'A');
        c.push(0);
        c.extend(le32(0));
        c.extend(content);
        c
    };
    let put_a = |content| cce_core::storage::Change::PutContext {
        id: cce_core::storage::ContextId::new(*b"A").unwrap(),
        expect: cce_core::storage::Precondition::Any,
        body: vec![],
        content,
    };
    assert_eq!(
        decode_history(&entry(&request(&put(&[0]), 1, 0)))
            .unwrap()
            .changes,
        vec![put_a(None)],
        "put without content"
    );
    err_contains(
        decode_history(&entry(&request(&put(&[7]), 1, 0))),
        "content tag",
        "content tag",
    );
    let mut index = vec![1];
    index.extend(0u64.to_le_bytes());
    err_contains(
        decode_history(&entry(&request(&put(&index), 1, 0))),
        "missing content binding",
        "content index without a binding",
    );
    let bound = decode_history(&entry(&request(&put(&index), 1, 1))).unwrap();
    assert_eq!(bound.changes, vec![put_a(Some(0))], "index 0 of 1");
    assert_eq!(
        bound.content,
        vec![cce_core::storage::ContentRef {
            digest: cce_core::storage::ContentDigest([0; 32]),
            len: 0,
        }]
    );
    let mut huge = vec![1];
    huge.extend(le32(u32::MAX));
    err_contains(
        decode_history(&entry(&huge)),
        "exceeds remaining",
        "change count",
    );
    let mut trailing = entry(&request(&[], 0, 0));
    trailing.push(0);
    err_contains(decode_history(&trailing), "trailing", "trailing byte");
    err_contains(decode_history(&entry(&[])), "truncated", "empty request");
    assert!(
        matches!(
            decode_history(&entry(&[3])),
            Err(StorageError::Unsupported(_))
        ),
        "a request encoding of another version is unsupported"
    );
}

#[test]
fn record_values_reject_truncation_trailing_bytes_and_unknown_tags() {
    use crate::records::{AttemptValue, ContextValue, ExtentRef, RelValue};
    let ctx = ContextValue {
        revision: u64::MAX,
        body: vec![],
        content: None,
    }
    .encode();
    assert_eq!(
        ContextValue::decode(&ctx).unwrap(),
        ContextValue {
            revision: u64::MAX,
            body: vec![],
            content: None,
        }
    );
    let mut tag = ctx.clone();
    *tag.last_mut().unwrap() = 2;
    err_contains(
        ContextValue::decode(&tag),
        "content tag",
        "context content tag",
    );
    let ext = ExtentRef {
        offset: u64::MAX,
        len: u64::MAX,
        digest: [0; 32],
    }
    .encode();
    let rel = RelValue {
        revision: 1,
        kind: b"k".to_vec(),
        source: b"s".to_vec(),
        target: b"t".to_vec(),
    }
    .encode();
    let att = AttemptValue {
        position: 1,
        fingerprint: [0; 32],
    }
    .encode();
    assert_eq!(ExtentRef::decode(&ext).unwrap().len, u64::MAX);
    assert_eq!(
        RelValue::decode(&rel).unwrap(),
        RelValue {
            revision: 1,
            kind: b"k".to_vec(),
            source: b"s".to_vec(),
            target: b"t".to_vec(),
        }
    );
    assert_eq!(AttemptValue::decode(&att).unwrap().position, 1);
    type Decoder = fn(&[u8]) -> Result<(), StorageError>;
    let decoders: [(&str, Vec<u8>, Decoder); 4] = [
        ("context", ctx, |b| ContextValue::decode(b).map(|_| ())),
        ("extent", ext, |b| ExtentRef::decode(b).map(|_| ())),
        ("relationship", rel, |b| RelValue::decode(b).map(|_| ())),
        ("attempt", att, |b| AttemptValue::decode(b).map(|_| ())),
    ];
    for (what, good, decode) in decoders {
        assert!(decode(&good).is_ok(), "{what}: valid value");
        for len in 0..good.len() {
            err_contains(
                decode(&good[..len]),
                "truncated",
                &format!("{what} cut to {len}"),
            );
        }
        let mut longer = good.clone();
        longer.push(0);
        err_contains(
            decode(&longer),
            "trailing",
            &format!("{what} with a trailing byte"),
        );
    }
}

#[test]
fn key_components_reject_bad_escapes() {
    use crate::codec::{key_component, key_components};
    let mut k = Vec::new();
    key_component(&mut k, b"a\0b");
    key_component(&mut k, b"");
    assert_eq!(
        key_components(&k).unwrap(),
        vec![b"a\0b".to_vec(), b"".to_vec()]
    );
    err_contains(key_components(b"a\0"), "escape", "unterminated escape");
    err_contains(key_components(b"a\0\x01"), "escape", "unknown escape");
}

/// Records the longest single read, to show that a refused extent was never read.
struct LongestRead {
    sim: crate::sim::SimIo,
    longest: std::sync::Arc<std::sync::atomic::AtomicUsize>,
}

impl crate::io::Io for LongestRead {
    fn create(&self) -> Result<(), StorageError> {
        self.sim.create()
    }
    fn open(&self) -> Result<(), StorageError> {
        self.sim.open()
    }
    fn read_at(
        &self,
        file: crate::io::FileId,
        offset: u64,
        buf: &mut [u8],
    ) -> Result<(), StorageError> {
        self.longest
            .fetch_max(buf.len(), std::sync::atomic::Ordering::SeqCst);
        self.sim.read_at(file, offset, buf)
    }
    fn write_at(
        &self,
        file: crate::io::FileId,
        offset: u64,
        data: &[u8],
    ) -> Result<(), StorageError> {
        self.sim.write_at(file, offset, data)
    }
    fn len(&self, file: crate::io::FileId) -> Result<u64, StorageError> {
        self.sim.len(file)
    }
    fn sync(&self, file: crate::io::FileId) -> Result<(), StorageError> {
        self.sim.sync(file)
    }
    fn sync_dir(&self) -> Result<(), StorageError> {
        self.sim.sync_dir()
    }
}

#[test]
fn data_written_under_larger_limits_reads_back_under_smaller_ones() {
    use cce_core::storage::{
        AttemptId, Change, ChangeSet, ContentBinding, ContextId, Evidence, HistoryPosition,
        Precondition, ReadView, StorageEngine,
    };
    use std::io::Read;

    use crate::NativeStore;
    use crate::engine::{Inner, Options};
    use crate::sim::SimIo;

    // Written under the default limits: a 100 KB history entry and content of five segments.
    let sim = SimIo::new();
    let s = NativeStore {
        inner: Inner::create(Box::new(sim.clone()), STORE, Options::default()).unwrap(),
    };
    let payload: Vec<u8> = (0..300_000u32).map(|i| (i % 251) as u8).collect();
    let staged = s.stage_content(&mut &payload[..]).unwrap();
    let cs = ChangeSet {
        removal: Default::default(),
        changes: vec![Change::PutContext {
            id: ContextId::new(*b"A").unwrap(),
            expect: Precondition::Absent,
            body: b"a".to_vec(),
            content: Some(0),
        }],
        content: vec![ContentBinding::from(&staged)],
        evidence: vec![Evidence {
            label: b"big".to_vec(),
            bytes: vec![7; 100_000],
        }],
    };
    s.commit(AttemptId::new(*b"a").unwrap(), s.begin().unwrap(), cs)
        .unwrap();
    drop(s);
    sim.release();

    // A later open with smaller limits bounds what it writes, not what it reads.
    let s = NativeStore {
        inner: Inner::open(
            Box::new(sim.clone()),
            Options {
                max_request_len: 1000,
                max_content_segments: 2,
                ..Options::default()
            },
        )
        .unwrap(),
    };
    let mut v = s.begin().unwrap();
    let entry = v.history(HistoryPosition(0)).unwrap().next().unwrap();
    assert_eq!(entry.unwrap().evidence[0].bytes, vec![7; 100_000]);
    let r = v.context(&ContextId::new(*b"A").unwrap()).unwrap().unwrap();
    let mut got = Vec::new();
    v.content(&r.content.unwrap())
        .expect("content of five segments under a two-segment limit")
        .read_to_end(&mut got)
        .unwrap();
    assert!(got == payload, "content read back differs");
}

#[test]
fn the_computed_manifest_length_matches_the_encoding() {
    use crate::records::{ExtentRef, Manifest};
    // Read limits are computed from `encoded_len`, so it must be exact for every segment count.
    for n in [0usize, 1, 2, 1000] {
        let m = Manifest {
            content: cce_core::storage::ContentRef {
                digest: cce_core::storage::ContentDigest([7; 32]),
                len: n as u64,
            },
            segments: (0..n)
                .map(|i| ExtentRef {
                    offset: i as u64,
                    len: 1,
                    digest: [i as u8; 32],
                })
                .collect(),
        };
        assert_eq!(
            m.encode().len() as u64,
            Manifest::encoded_len(n),
            "{n} segments"
        );
    }
}

#[test]
fn an_extent_beyond_the_format_maximum_is_refused_before_it_is_read() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use crate::NativeStore;
    use crate::engine::{HISTORY_ENTRY_MAX, Inner, Options};
    use crate::records::ExtentRef;
    use crate::sim::SimIo;

    // The format's own maximum: position, u32-prefixed attempt id of at most 255 bytes, and a
    // u32-prefixed request of at most u32::MAX bytes.
    assert_eq!(HISTORY_ENTRY_MAX, 8 + 4 + 255 + 4 + u32::MAX as u64);

    let longest = Arc::new(AtomicUsize::new(0));
    let s = NativeStore {
        inner: Inner::create(
            Box::new(LongestRead {
                sim: SimIo::new(),
                longest: longest.clone(),
            }),
            STORE,
            Options::default(),
        )
        .unwrap(),
    };
    longest.store(0, Ordering::SeqCst);
    let crafted = ExtentRef {
        offset: 0,
        len: HISTORY_ENTRY_MAX + 1,
        digest: [0; 32],
    };
    let r = s
        .inner
        .read_extent(&crafted, HISTORY_ENTRY_MAX, "history entry");
    match r {
        Err(StorageError::Integrity(m)) => {
            assert!(
                m.contains("history entry of") && m.contains("byte limit"),
                "{m}"
            )
        }
        other => panic!("expected the crafted extent to be refused, got {other:?}"),
    }
    assert_eq!(
        longest.load(Ordering::SeqCst),
        0,
        "a refused extent was read"
    );
}

/// Records the size of every read of the content file. The verified read allocates exactly its
/// read buffers, so the largest read is the largest allocation for an extent.
struct ContentReads {
    sim: crate::sim::SimIo,
    sizes: std::sync::Arc<std::sync::Mutex<Vec<usize>>>,
}

impl crate::io::Io for ContentReads {
    fn create(&self) -> Result<(), StorageError> {
        self.sim.create()
    }
    fn open(&self) -> Result<(), StorageError> {
        self.sim.open()
    }
    fn open_read_only(&self) -> Result<(), StorageError> {
        self.sim.open_read_only()
    }
    fn read_at(
        &self,
        file: crate::io::FileId,
        offset: u64,
        buf: &mut [u8],
    ) -> Result<(), StorageError> {
        if file == crate::io::FileId::Content {
            self.sizes.lock().unwrap().push(buf.len());
        }
        self.sim.read_at(file, offset, buf)
    }
    fn write_at(
        &self,
        file: crate::io::FileId,
        offset: u64,
        data: &[u8],
    ) -> Result<(), StorageError> {
        self.sim.write_at(file, offset, data)
    }
    fn len(&self, file: crate::io::FileId) -> Result<u64, StorageError> {
        self.sim.len(file)
    }
    fn sync(&self, file: crate::io::FileId) -> Result<(), StorageError> {
        self.sim.sync(file)
    }
    fn sync_dir(&self) -> Result<(), StorageError> {
        self.sim.sync_dir()
    }
}

/// Largest buffer held for an extent before its digest is verified (the documented bound).
const VERIFY_CHUNK_BOUND: usize = 64 * 1024;

/// A store whose newest state has one tree entry replaced by a crafted, checksum-valid value: an
/// extent reference to `filler.len()` bytes of real committed content with a wrong digest. Returns
/// the reopened store, its read recorder, and the staged content of the one commit.
fn forged(
    entry_key: impl FnOnce(&cce_core::storage::StagedContent) -> Vec<u8>,
) -> (
    crate::NativeStore,
    std::sync::Arc<std::sync::Mutex<Vec<usize>>>,
    cce_core::storage::StagedContent,
) {
    use cce_core::storage::{
        AttemptId, Change, ChangeSet, ContentBinding, ContextId, Precondition, StorageEngine,
    };

    use crate::NativeStore;
    use crate::engine::{Inner, Options};
    use crate::format::{Descriptor, PAGE_SIZE, encode_page};
    use crate::io::{FileId, Io};
    use crate::records::ExtentRef;
    use crate::sim::SimIo;
    use crate::tree::{Node, NodeSink, TreeTxn};

    let sim = SimIo::new();
    let s = NativeStore {
        inner: Inner::create(Box::new(sim.clone()), STORE, Options::default()).unwrap(),
    };
    let staged = s.stage_content(&mut &b"bytes"[..]).unwrap();
    let cs = ChangeSet {
        changes: vec![Change::PutContext {
            id: ContextId::new(*b"B").unwrap(),
            expect: Precondition::Absent,
            body: b"b".to_vec(),
            content: Some(0),
        }],
        content: vec![ContentBinding::from(&staged)],
        ..ChangeSet::default()
    };
    s.commit(AttemptId::new(*b"x").unwrap(), s.begin().unwrap(), cs)
        .unwrap();
    let cur = s.inner.snapshot().unwrap().desc;
    // 3 MB of real bytes, committed: the crafted reference lies within committed content.
    let filler = vec![7u8; 3 << 20];
    sim.write_at(FileId::Content, cur.content_len, &filler)
        .unwrap();
    sim.sync(FileId::Content).unwrap();
    let crafted = ExtentRef {
        offset: cur.content_len,
        len: filler.len() as u64,
        digest: [0; 32],
    };
    let mut t = TreeTxn::new(&*s.inner, cur.root);
    // A content entry holds the manifest extent of live content; a history entry, the extent.
    let k = entry_key(&staged);
    let v = if k.first() == Some(&crate::records::P_CONTENT) {
        crate::records::ContentEntry::Live {
            manifest: crafted,
            refs: 1,
        }
        .encode()
    } else {
        crafted.encode()
    };
    t.put(k, v).unwrap();
    struct Sink<'a>(&'a SimIo, u64, u64);
    impl NodeSink for Sink<'_> {
        fn write(&mut self, n: &Node) -> Result<PageRef, StorageError> {
            let at = PageRef {
                page: self.1,
                incarnation: self.2,
            };
            self.0.write_at(
                FileId::Meta,
                at.page * PAGE_SIZE as u64,
                &encode_page(STORE, PageKind::Node, at, &n.encode())?,
            )?;
            self.1 += 1;
            Ok(at)
        }
    }
    let mut sink = Sink(&sim, cur.next_page, cur.generation + 1);
    let root = t.finish(&mut sink).unwrap();
    let d = Descriptor {
        generation: cur.generation + 1,
        root,
        next_page: sink.1,
        content_len: cur.content_len + filler.len() as u64,
        ..cur
    };
    sim.write_at(
        FileId::Meta,
        Descriptor::slot(d.generation) * PAGE_SIZE as u64,
        &d.encode(),
    )
    .unwrap();
    sim.sync(FileId::Meta).unwrap();
    drop(s);
    sim.release();
    let sizes = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let s = NativeStore {
        inner: Inner::open(
            Box::new(ContentReads {
                sim,
                sizes: sizes.clone(),
            }),
            Options::default(),
        )
        .unwrap(),
    };
    sizes.lock().unwrap().clear();
    (s, sizes, staged)
}

fn assert_digest_refused_within_chunk<T: std::fmt::Debug>(
    r: Result<T, StorageError>,
    sizes: &std::sync::Mutex<Vec<usize>>,
    what: &str,
) {
    match r {
        Err(StorageError::Integrity(m)) => assert!(m.contains("digest mismatch"), "{what}: {m}"),
        other => panic!("{what}: expected a digest mismatch, got {other:?}"),
    }
    let sizes = sizes.lock().unwrap();
    let longest = sizes.iter().copied().max().unwrap_or(0);
    assert!(
        longest <= VERIFY_CHUNK_BOUND,
        "{what}: {longest} bytes held before the digest was verified"
    );
    assert!(
        !sizes.is_empty(),
        "{what}: the crafted extent was never read"
    );
}

#[test]
fn a_crafted_history_extent_is_refused_before_more_than_a_chunk_is_held() {
    use cce_core::storage::{HistoryPosition, ReadView, StorageEngine};
    let (s, sizes, _) = forged(|_| crate::records::history_key(1));
    let mut v = s.begin().unwrap();
    let entry = v.history(HistoryPosition(0)).unwrap().next().unwrap();
    assert_digest_refused_within_chunk(entry, &sizes, "history entry");
}

#[test]
fn a_crafted_manifest_extent_is_refused_before_more_than_a_chunk_is_held() {
    use crate::records::{P_CONTENT, key};
    use cce_core::storage::{ReadView, StorageEngine};
    let (s, sizes, staged) = forged(|c| key(P_CONTENT, &[&c.content.digest.0]));
    let r = s.begin().unwrap().content(&staged.content).map(|_| ());
    assert_digest_refused_within_chunk(r, &sizes, "content manifest");
}

#[test]
fn verified_extents_read_back_whole_and_small_ones_in_one_read() {
    use std::sync::{Arc, Mutex};

    use cce_core::storage::{AttemptId, ChangeSet, Evidence, StorageEngine};

    use crate::NativeStore;
    use crate::engine::{Inner, Options};
    use crate::records::{ExtentRef, decode_history, history_key};
    use crate::sim::SimIo;

    let sizes = Arc::new(Mutex::new(Vec::new()));
    let s = NativeStore {
        inner: Inner::create(
            Box::new(ContentReads {
                sim: SimIo::new(),
                sizes: sizes.clone(),
            }),
            STORE,
            Options::default(),
        )
        .unwrap(),
    };
    let mut seen_small = false;
    let mut seen_large = false;
    for (pos, evidence) in [(1u64, 0usize), (2, 70_000), (3, 1 << 20)] {
        let cs = ChangeSet {
            evidence: vec![Evidence {
                label: b"e".to_vec(),
                bytes: vec![9; evidence],
            }],
            ..ChangeSet::default()
        };
        s.commit(
            AttemptId::new(format!("a{pos}").into_bytes()).unwrap(),
            s.begin().unwrap(),
            cs,
        )
        .unwrap();
        let root = s.inner.snapshot().unwrap().desc.root;
        let x = ExtentRef::decode(
            &crate::tree::get(&*s.inner, root, &history_key(pos))
                .unwrap()
                .unwrap(),
        )
        .unwrap();
        sizes.lock().unwrap().clear();
        let got = s.inner.read_extent(&x, u64::MAX, "history entry").unwrap();
        assert_eq!(
            decode_history(&got).unwrap().evidence[0].bytes.len(),
            evidence
        );
        let len = x.len as usize;
        let sizes = sizes.lock().unwrap().clone();
        if len <= VERIFY_CHUNK_BOUND {
            seen_small = true;
            assert_eq!(sizes, vec![len], "a small extent is read once");
        } else {
            seen_large = true;
            // Verified in chunks first, then read whole exactly once.
            let (whole, chunks) = sizes.split_last().unwrap();
            assert_eq!(*whole, len);
            assert!(
                chunks.iter().all(|&c| c <= VERIFY_CHUNK_BOUND),
                "{chunks:?}"
            );
            assert_eq!(
                chunks.iter().sum::<usize>(),
                len,
                "the verifying pass covers the extent"
            );
        }
    }
    assert!(seen_small && seen_large);
}
