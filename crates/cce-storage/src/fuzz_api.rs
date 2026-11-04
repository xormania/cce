//! Fuzzing harness: one entry point per decoder of stored bytes, one that opens a whole store
//! image read-only and reads everything it can reach, and one that runs operation sequences
//! against the logical model. For any input each entry point must return —
//! with or without an error — and never panic, hang or allocate beyond what its input bounds.
//! Decoders that accept an input must also re-encode the decoded value to exactly the input
//! bytes: an accepted input is the canonical encoding of what it decoded to, so a decoder that
//! ignores a field, a trailing byte or a reserved bit fails its harness. The one exception is a
//! history entry whose request uses encoding version 1 (see [`history`]). Two targets repair the
//! checksums of their input before decoding, so mutations reach the checks behind them.
//!
//! The fuzz targets in `fuzz/` call these functions, and a stable test replays the saved seeds and
//! regression inputs through the same functions, so every past finding stays covered without a
//! fuzzing toolchain. Compiled for tests and with the `fuzz-api` feature only.

use std::io::Read;

use cce_core::storage::{
    ChangeSet, ContentBinding, ContextId, Direction, HistoryPosition, ReadView, RelationshipQuery,
    RemovalMode, StagingToken, StorageError,
};

use crate::codec::{key_component, key_components};
use crate::engine::{Inner, Options};
use crate::format::{
    DESC_LEN, DESC_MAGIC, PAGE_HEADER_LEN, PAGE_MAGIC, PAGE_PAYLOAD_MAX, PAGE_SIZE, PageKind,
    PageRef, Slot, StoreId, decode_page, decode_slot, encode_page,
};
use crate::records::{
    AttemptValue, ContextValue, ExtentRef, Manifest, P_CONTEXT, RelValue, decode_history,
    encode_history,
};
use crate::sim::SimIo;
use crate::tree::Node;
use crate::{NativeReadOnly, NativeView};

/// Bounds on the whole-image walk, so a large valid image cannot make one input run long.
const MAX_KEYS: usize = 10_000;
const MAX_LISTED: usize = 1_000;
const MAX_CONTENT_BYTES: usize = 4 << 20;

/// A fuzz target's entry point: takes arbitrary bytes and returns whether the input was accepted
/// (it decoded, the image opened, or the sequence had steps), so a sweep can count both kinds.
pub type Harness = fn(&[u8]) -> bool;

/// Runs the named target's harness on `data`; true if the input was accepted.
pub fn run(target: &str, data: &[u8]) -> bool {
    let Some((_, harness)) = TARGETS.iter().find(|(name, _)| *name == target) else {
        panic!("unknown fuzz target {target}");
    };
    harness(data)
}

/// The fuzz targets, by name.
pub const TARGETS: &[(&str, Harness)] = &[
    ("page", page),
    ("page_repaired", page_repaired),
    ("slots", slots),
    ("node", node),
    ("manifest", manifest),
    ("history", history),
    ("records", records),
    ("image", image),
    ("image_repaired", image_repaired),
    ("ops", ops),
];

fn le_u64(data: &[u8], at: usize) -> u64 {
    data.get(at..at + 8)
        .map_or(0, |b| u64::from_le_bytes(b.try_into().unwrap_or_default()))
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|b| format!("{b:02x}")).collect()
}

/// Fails unless `input`, which a decoder accepted, is exactly the encoding of the decoded value;
/// the message names the first differing offset and the bytes from there.
fn assert_canonical(what: &str, input: &[u8], canonical: &[u8]) {
    if input == canonical {
        return;
    }
    let at = input
        .iter()
        .zip(canonical)
        .position(|(a, b)| a != b)
        .unwrap_or(input.len().min(canonical.len()));
    let near = |b: &[u8]| hex(&b[at.min(b.len())..(at + 16).min(b.len())]);
    panic!(
        "{what}: accepted a non-canonical encoding: {} bytes, canonical {}, first difference at \
         offset {at}: input {} canonical {}",
        input.len(),
        canonical.len(),
        near(input),
        near(canonical)
    );
}

/// The payload length a page header claims, if it fits a page.
fn page_payload_len(page: &[u8]) -> Option<usize> {
    let len = u32::from_le_bytes(page.get(40..44)?.try_into().ok()?) as usize;
    (len <= PAGE_PAYLOAD_MAX).then_some(len)
}

/// The checksum of a page: over its header, with the checksum field zero, and its payload.
fn page_checksum(page: &[u8], len: usize) -> u32 {
    let mut h = crc32fast::Hasher::new();
    h.update(&page[..PAGE_HEADER_LEN - 4]);
    h.update(&[0; 4]);
    h.update(&page[PAGE_HEADER_LEN..PAGE_HEADER_LEN + len]);
    h.finalize()
}

/// A metadata page, checked against the store, address and incarnation its own header claims, so
/// inputs can reach the checks after the address. A page whose checksum does not match is damage,
/// whatever else its header claims; an accepted page is exactly the page the encoder writes for its
/// payload.
pub fn page(data: &[u8]) -> bool {
    page_payload(data).is_some()
}

fn page_payload(data: &[u8]) -> Option<Vec<u8>> {
    let store = StoreId(
        data.get(8..24)
            .map_or([0; 16], |b| b.try_into().unwrap_or([0; 16])),
    );
    let at = PageRef {
        page: le_u64(data, 24),
        incarnation: le_u64(data, 32),
    };
    let decoded = decode_page(store, PageKind::Node, at, data);
    if data.len() == PAGE_SIZE
        && let Some(len) = page_payload_len(data)
        && page_checksum(data, len).to_le_bytes() != data[PAGE_HEADER_LEN - 4..PAGE_HEADER_LEN]
    {
        assert!(
            matches!(decoded, Err(StorageError::Integrity(_))),
            "page: a page whose checksum does not match is reported as {decoded:?}"
        );
    }
    let payload = decoded.ok()?;
    let canonical = encode_page(store, PageKind::Node, at, &payload).expect("accepted payload");
    let checked = PAGE_HEADER_LEN + payload.len();
    assert_canonical("page", &data[..checked], &canonical[..checked]);
    Some(payload)
}

/// A metadata page whose checksum is repaired first: the input is cut or zero-filled to a page, the
/// payload length is brought into range, and the checksum is recomputed. Mutations therefore reach
/// the checks after the checksum, and the payload of an accepted page is decoded as a tree node.
pub fn page_repaired(data: &[u8]) -> bool {
    let mut page = data.to_vec();
    page.resize(PAGE_SIZE, 0);
    let len = u32::from_le_bytes(page[40..44].try_into().unwrap_or_default()) as usize
        % (PAGE_PAYLOAD_MAX + 1);
    page[40..44].copy_from_slice(&(len as u32).to_le_bytes());
    let crc = page_checksum(&page, len);
    page[PAGE_HEADER_LEN - 4..PAGE_HEADER_LEN].copy_from_slice(&crc.to_le_bytes());
    let Some(payload) = page_payload(&page) else {
        return false;
    };
    node(&payload);
    true
}

/// Two publication slots: the first and second half of the input. An accepted record is exactly
/// the record the encoder writes for the descriptor it decoded to.
pub fn slots(data: &[u8]) -> bool {
    let (a, b) = data.split_at(data.len() / 2);
    let mut valid = false;
    for half in [a, b] {
        if let Slot::Valid(desc) = decode_slot(half) {
            assert_canonical("slot", &half[..DESC_LEN], &desc.encode()[..DESC_LEN]);
            valid = true;
        }
    }
    valid
}

pub fn node(data: &[u8]) -> bool {
    let Ok(n) = Node::decode(data) else {
        return false;
    };
    assert_canonical("node", data, &n.encode());
    true
}

pub fn manifest(data: &[u8]) -> bool {
    let Ok(m) = Manifest::decode(data) else {
        return false;
    };
    assert_canonical("manifest", data, &m.encode());
    true
}

/// A retained history entry. The removal mode a request ends with is not part of the decoded entry,
/// so it is read back from the input. Encoding version 1 (from before the removal mode existed) is
/// still read, as preserving views, and is the one accepted encoding that is not canonical: its
/// canonical form is the version 2 encoding of the same request, which ends with that mode.
pub fn history(data: &[u8]) -> bool {
    let Ok(entry) = decode_history(data) else {
        return false;
    };
    // position u64, attempt (u32 length + bytes), request (u32 length + bytes) up to the end.
    let request_at = 8 + 4 + entry.attempt.as_bytes().len() + 4;
    let request = &data[request_at..];
    let removal = match (request[0], request.last()) {
        (2, Some(1)) => RemovalMode::RequireQuiescence,
        _ => RemovalMode::PreserveViews,
    };
    let changes = ChangeSet {
        changes: entry.changes.clone(),
        content: entry
            .content
            .iter()
            .map(|&content| ContentBinding {
                content,
                token: StagingToken(Vec::new()),
            })
            .collect(),
        evidence: entry.evidence.clone(),
        removal,
    };
    let canonical = encode_history(entry.position.0, &entry.attempt, &changes);
    let mut expected = data.to_vec();
    if request[0] == 1 {
        let len = (request.len() + 1) as u32;
        expected[request_at - 4..request_at].copy_from_slice(&len.to_le_bytes());
        expected[request_at] = 2;
        expected.push(0);
    }
    assert_canonical("history", &expected, &canonical);
    true
}

/// The first byte selects a record value decoder or the key-component parser.
pub fn records(data: &[u8]) -> bool {
    let Some((&which, rest)) = data.split_first() else {
        return false;
    };
    let canonical = match which % 5 {
        0 => ContextValue::decode(rest).map(|v| v.encode()),
        1 => RelValue::decode(rest).map(|v| v.encode()),
        2 => AttemptValue::decode(rest).map(|v| v.encode()),
        3 => ExtentRef::decode(rest).map(|v| v.encode()),
        _ => key_components(rest).map(|parts| {
            let mut key = Vec::new();
            for p in &parts {
                key_component(&mut key, p);
            }
            key
        }),
    };
    let Ok(canonical) = canonical else {
        return false;
    };
    assert_canonical("record", rest, &canonical);
    true
}

/// Splits an input into a metadata and a content file image: a little-endian `u32` metadata
/// length, the metadata bytes, then the content bytes.
pub fn split_image(data: &[u8]) -> (Vec<u8>, Vec<u8>) {
    let Some(len) = data.get(..4) else {
        return (data.to_vec(), Vec::new());
    };
    let len = u32::from_le_bytes(len.try_into().unwrap_or_default()) as usize;
    let rest = &data[4..];
    let len = len.min(rest.len());
    (rest[..len].to_vec(), rest[len..].to_vec())
}

/// Joins a metadata and a content file image into one input for [`image`] (seed generation).
#[cfg(test)]
pub fn join_image(meta: &[u8], content: &[u8]) -> Vec<u8> {
    let mut out = (meta.len() as u32).to_le_bytes().to_vec();
    out.extend_from_slice(meta);
    out.extend_from_slice(content);
    out
}

/// A whole store image: opened read-only, then every context the tree holds is read with its
/// content and relationships in both directions, and the history is listed.
pub fn image(data: &[u8]) -> bool {
    let (meta, content) = split_image(data);
    open_and_read(meta, content)
}

/// A whole store image whose checksums are repaired first: each publication record and each node
/// page that still has its magic, and a payload length that fits, gets the checksum of its current
/// bytes. Then it is opened and read like [`image`].
pub fn image_repaired(data: &[u8]) -> bool {
    let (mut meta, content) = split_image(data);
    for (i, page) in meta.chunks_mut(PAGE_SIZE).enumerate() {
        if i < 2 {
            if page.len() >= DESC_LEN && page[..4] == DESC_MAGIC {
                let crc = crc32fast::hash(&page[..DESC_LEN - 4]);
                page[DESC_LEN - 4..DESC_LEN].copy_from_slice(&crc.to_le_bytes());
            }
        } else if page.len() == PAGE_SIZE
            && page[..4] == PAGE_MAGIC
            && let Some(len) = page_payload_len(page)
        {
            let crc = page_checksum(page, len);
            page[PAGE_HEADER_LEN - 4..PAGE_HEADER_LEN].copy_from_slice(&crc.to_le_bytes());
        }
    }
    open_and_read(meta, content)
}

fn open_and_read(meta: Vec<u8>, content: Vec<u8>) -> bool {
    let opts = Options {
        cache_nodes: 16,
        ..Options::default()
    };
    let Ok(inner) = Inner::open_read_only(Box::new(SimIo::from_images(meta, content)), opts) else {
        return false;
    };
    let store = NativeReadOnly { inner };
    let Ok(mut v) = store.begin() else {
        return false;
    };
    let ids = context_ids(&store);
    let mut budget = MAX_CONTENT_BYTES;
    for id in ids {
        read_context(&mut v, &id, &mut budget);
    }
    if let Ok(entries) = v.history(HistoryPosition(0)) {
        for _ in entries.take(MAX_LISTED) {}
    }
    true
}

/// Context identities stored in the tree of the store's current state.
fn context_ids(store: &NativeReadOnly) -> Vec<ContextId> {
    // A pinned state: nothing the walk reads can be reused under it.
    let Ok((snap, _pin)) = store.inner.pin_current() else {
        return Vec::new();
    };
    let Ok(cursor) = crate::tree::Cursor::new(
        &*store.inner,
        snap.desc.root,
        &[P_CONTEXT],
        None,
        snap.desc.next_page,
    ) else {
        return Vec::new();
    };
    cursor
        .take(MAX_KEYS)
        .map_while(Result::ok)
        .filter_map(|(k, _)| key_components(&k[1..]).ok())
        .filter_map(|parts| parts.into_iter().next())
        .filter_map(|id| ContextId::new(id).ok())
        .collect()
}

fn read_context(v: &mut NativeView, id: &ContextId, budget: &mut usize) {
    if let Ok(Some(r)) = v.context(id)
        && let Some(c) = r.content
        && let Ok(mut reader) = v.content(&c)
    {
        let mut buf = vec![0; 64 * 1024];
        while *budget > 0 {
            match reader.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => *budget = budget.saturating_sub(n),
            }
        }
    }
    for direction in [Direction::Outgoing, Direction::Incoming] {
        let q = RelationshipQuery {
            from: id.clone(),
            direction,
            kind: None,
            after: None,
        };
        if let Ok(rels) = v.relationships(&q) {
            for _ in rels.take(MAX_LISTED) {}
        }
    }
}

/// An operation sequence decoded from the input and run on the simulator against the logical
/// model, completely or interrupted once by a crash or an I/O failure, with the model's checks
/// after every step and every recovery.
pub fn ops(data: &[u8]) -> bool {
    crate::model::run_bytes(data)
}
