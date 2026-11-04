//! Fuzzing harness: one entry point per decoder of stored bytes, one that opens a whole store
//! image read-only and reads everything it can reach, and one that runs operation sequences
//! against the logical model. For any input each entry point must return —
//! with or without an error — and never panic, hang or allocate beyond what its input bounds.
//! Decoders that accept an input must also re-encode it to an equal value.
//!
//! The fuzz targets in `fuzz/` call these functions, and a stable test replays the saved seeds and
//! regression inputs through the same functions, so every past finding stays covered without a
//! fuzzing toolchain. Compiled for tests and with the `fuzz-api` feature only.

use std::io::Read;

use cce_core::storage::{ContextId, Direction, HistoryPosition, ReadView, RelationshipQuery};

use crate::codec::key_components;
use crate::engine::{Inner, Options};
use crate::format::{PageKind, PageRef, StoreId, decode_page, decode_slot};
use crate::records::{
    AttemptValue, ContextValue, ExtentRef, Manifest, P_CONTEXT, RelValue, decode_history,
};
use crate::sim::SimIo;
use crate::tree::Node;
use crate::{NativeReadOnly, NativeView};

/// Bounds on the whole-image walk, so a large valid image cannot make one input run long.
const MAX_KEYS: usize = 10_000;
const MAX_LISTED: usize = 1_000;
const MAX_CONTENT_BYTES: usize = 4 << 20;

/// A fuzz target's entry point: takes arbitrary bytes.
pub type Harness = fn(&[u8]);

/// Runs the named target's harness on `data`.
pub fn run(target: &str, data: &[u8]) {
    let Some((_, harness)) = TARGETS.iter().find(|(name, _)| *name == target) else {
        panic!("unknown fuzz target {target}");
    };
    harness(data);
}

/// The fuzz targets, by name.
pub const TARGETS: &[(&str, Harness)] = &[
    ("page", page),
    ("slots", slots),
    ("node", node),
    ("manifest", manifest),
    ("history", history),
    ("records", records),
    ("image", image),
    ("ops", ops),
];

fn le_u64(data: &[u8], at: usize) -> u64 {
    data.get(at..at + 8)
        .map_or(0, |b| u64::from_le_bytes(b.try_into().unwrap_or_default()))
}

/// A metadata page, checked against the store, address and incarnation its own header claims, so
/// inputs can reach the checks after the address.
pub fn page(data: &[u8]) {
    let store = StoreId(
        data.get(8..24)
            .map_or([0; 16], |b| b.try_into().unwrap_or([0; 16])),
    );
    let at = PageRef {
        page: le_u64(data, 24),
        incarnation: le_u64(data, 32),
    };
    let _ = decode_page(store, PageKind::Node, at, data);
}

/// Two publication slots: the first and second half of the input.
pub fn slots(data: &[u8]) {
    let (a, b) = data.split_at(data.len() / 2);
    let _ = (decode_slot(a), decode_slot(b));
}

pub fn node(data: &[u8]) {
    if let Ok(n) = Node::decode(data) {
        assert_eq!(
            Node::decode(&n.encode()).ok(),
            Some(n),
            "node does not round-trip"
        );
    }
}

pub fn manifest(data: &[u8]) {
    if let Ok(m) = Manifest::decode(data) {
        assert_eq!(
            Manifest::decode(&m.encode()).ok(),
            Some(m),
            "manifest does not round-trip"
        );
    }
}

pub fn history(data: &[u8]) {
    let _ = decode_history(data);
}

/// The first byte selects a record value decoder or the key-component parser.
pub fn records(data: &[u8]) {
    let Some((&which, rest)) = data.split_first() else {
        return;
    };
    match which % 5 {
        0 => {
            if let Ok(v) = ContextValue::decode(rest) {
                assert_eq!(ContextValue::decode(&v.encode()).ok(), Some(v));
            }
        }
        1 => {
            if let Ok(v) = RelValue::decode(rest) {
                assert_eq!(RelValue::decode(&v.encode()).ok(), Some(v));
            }
        }
        2 => {
            if let Ok(v) = AttemptValue::decode(rest) {
                assert_eq!(AttemptValue::decode(&v.encode()).ok(), Some(v));
            }
        }
        3 => {
            if let Ok(v) = ExtentRef::decode(rest) {
                assert_eq!(ExtentRef::decode(&v.encode()).ok(), Some(v));
            }
        }
        _ => {
            let _ = key_components(rest);
        }
    }
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
pub fn image(data: &[u8]) {
    let (meta, content) = split_image(data);
    let opts = Options {
        cache_nodes: 16,
        ..Options::default()
    };
    let Ok(inner) = Inner::open_read_only(Box::new(SimIo::from_images(meta, content)), opts) else {
        return;
    };
    let store = NativeReadOnly { inner };
    let Ok(mut v) = store.begin() else {
        return;
    };
    let ids = context_ids(&store);
    let mut budget = MAX_CONTENT_BYTES;
    for id in ids {
        read_context(&mut v, &id, &mut budget);
    }
    if let Ok(entries) = v.history(HistoryPosition(0)) {
        for _ in entries.take(MAX_LISTED) {}
    }
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
pub fn ops(data: &[u8]) {
    crate::model::run_bytes(data);
}
