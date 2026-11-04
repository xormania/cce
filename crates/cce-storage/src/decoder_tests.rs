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

/// Publication record layout: magic 0..4, version 4..6, store 6..22, generation 22..30, root page
/// 30..38, root incarnation 38..46, next page 46..54, content length 54..62, history position
/// 62..70, checksum 70..74.
fn reseal_slot(s: &mut [u8]) {
    let crc = crc32fast::hash(&s[..70]);
    s[70..74].copy_from_slice(&crc.to_le_bytes());
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
    assert_eq!(&good[0..4], b"CCED");
    assert_eq!(
        u64::from_le_bytes(good[22..30].try_into().unwrap()),
        d.generation
    );
    assert!(good[74..].iter().all(|&b| b == 0), "padding");

    assert!(matches!(decode_slot(&vec![0; PAGE_SIZE]), Slot::Empty));
    for len in [0, 1, 73] {
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
    assert!(matches!(decode_slot(&future), Slot::Unsupported(v) if v == FORMAT_VERSION + 1));
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
    assert!(matches!(Node::decode(&leaf(&[])), Ok(Node::Leaf(e)) if e.is_empty()));
    assert!(matches!(Node::decode(&leaf(&[(b"", b"")])), Ok(Node::Leaf(e)) if e.len() == 1));
    let mut branch = vec![2];
    branch.extend(le32(2));
    child(&mut branch, 2);
    branch.extend(1u16.to_le_bytes());
    branch.push(b'm');
    child(&mut branch, 3);
    assert!(
        matches!(Node::decode(&branch), Ok(Node::Branch { children, .. }) if children.len() == 2)
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
    assert!(Manifest::decode(&manifest(0, &[])).is_ok(), "empty content");
    assert!(
        Manifest::decode(&manifest(SEGMENT_MAX, &[(0, SEGMENT_MAX)])).is_ok(),
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
    assert!(
        decode_history(&entry(&request(&[], 0, 0))).is_ok(),
        "empty request"
    );
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
    assert!(
        decode_history(&entry(&request(&put(&[0]), 1, 0))).is_ok(),
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
    assert!(
        decode_history(&entry(&request(&put(&index), 1, 1))).is_ok(),
        "index 0 of 1"
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
            decode_history(&entry(&[2])),
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
    assert!(ContextValue::decode(&ctx).is_ok());
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
fn a_history_extent_beyond_its_limit_is_refused_before_it_is_read() {
    use cce_core::storage::{
        AttemptId, ChangeSet, Evidence, HistoryPosition, ReadView, StorageEngine,
    };
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use crate::NativeStore;
    use crate::engine::{Inner, Options};
    use crate::sim::SimIo;

    // A history entry of about 100 KB, written under the default limits.
    let sim = SimIo::new();
    let s = NativeStore {
        inner: Inner::create(Box::new(sim.clone()), STORE, Options::default()).unwrap(),
    };
    let cs = ChangeSet {
        evidence: vec![Evidence {
            label: b"big".to_vec(),
            bytes: vec![7; 100_000],
        }],
        ..ChangeSet::default()
    };
    s.commit(AttemptId::new(*b"a").unwrap(), s.begin().unwrap(), cs)
        .unwrap();
    drop(s);
    sim.release();

    // Reopened with a limit far below that entry: the extent's recorded length alone refuses it.
    let longest = Arc::new(AtomicUsize::new(0));
    let limit = 1000;
    let s = NativeStore {
        inner: Inner::open(
            Box::new(LongestRead {
                sim: sim.clone(),
                longest: longest.clone(),
            }),
            Options {
                max_request_len: limit,
                ..Options::default()
            },
        )
        .unwrap(),
    };
    let mut v = s.begin().unwrap();
    longest.store(0, Ordering::SeqCst);
    let entries: Vec<_> = v.history(HistoryPosition(0)).unwrap().collect();
    assert_eq!(entries.len(), 1);
    match &entries[0] {
        Err(StorageError::Integrity(m)) => {
            assert!(
                m.contains("history entry of") && m.contains("byte limit"),
                "{m}"
            )
        }
        other => panic!("expected the history extent to be refused, got {other:?}"),
    }
    let read = longest.load(Ordering::SeqCst);
    assert!(
        read < 100_000 && (read as u64) <= limit + crate::format::PAGE_SIZE as u64,
        "a refused extent was read: {read} bytes in one read"
    );

    // Positive neighbour: under a limit that covers it, the same entry reads back.
    drop(v);
    drop(s);
    sim.release();
    let s = NativeStore {
        inner: Inner::open(Box::new(sim.clone()), Options::default()).unwrap(),
    };
    let entry = s
        .begin()
        .unwrap()
        .history(HistoryPosition(0))
        .unwrap()
        .next()
        .unwrap();
    assert_eq!(entry.unwrap().evidence[0].bytes.len(), 100_000);
}
