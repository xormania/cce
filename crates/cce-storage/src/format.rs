//! On-disk format, version 1. Documented in `docs/storage-format.md`.
//!
//! The metadata file is a sequence of `PAGE_SIZE` pages. Pages 0 and 1 are the two publication
//! slots; pages from 2 hold tree nodes. All integers are little-endian.

use cce_core::storage::StorageError;

use crate::codec::{Dec, Enc, corrupt};

pub(crate) const PAGE_SIZE: usize = 4096;
/// Version 4: page and record magic `CCEP`/`CCED` and the `cce.attempt.v1` fingerprint domain. Version 3 introduced content stored
/// as verified segments with manifests and context records holding logical content references
/// (version 2 had single-extent content; version 1 single-leaf trees). Older versions are refused,
/// never reinterpreted.
pub(crate) const FORMAT_VERSION: u16 = 5;
pub(crate) const PAGE_MAGIC: [u8; 4] = *b"CCEP";
pub(crate) const DESC_MAGIC: [u8; 4] = *b"CCED";
/// Pages 0 and 1 are publication slots.
pub(crate) const FIRST_NODE_PAGE: u64 = 2;
/// magic 4 + version 2 + kind 1 + reserved 1 + store 16 + page 8 + incarnation 8 + len 4 + crc 4.
pub(crate) const PAGE_HEADER_LEN: usize = 48;
pub(crate) const PAGE_PAYLOAD_MAX: usize = PAGE_SIZE - PAGE_HEADER_LEN;
/// magic 4 + version 2 + required features 4 + store 16 + six counters 8 each + crc 4.
const DESC_LEN: usize = 4 + 2 + 4 + 16 + 8 * 6 + 4;
/// Length of the publication record of format versions before 5, which had no required features.
const EARLIER_DESC_LEN: usize = 4 + 2 + 16 + 8 * 6 + 4;
/// Required feature: a history marker may be present (some retained history entry was removed).
/// Set by the first publication that removes history and carried by every later one.
pub(crate) const FEATURE_HISTORY_MARKERS: u32 = 1 << 0;
/// Required features this build implements. A publication record that requires any other is
/// refused as unsupported, like an unknown format version.
pub(crate) const KNOWN_REQUIRED_FEATURES: u32 = FEATURE_HISTORY_MARKERS;

/// Identity of one store; recorded in every page and publication record.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) struct StoreId(pub [u8; 16]);

/// A physical reference to a tree node: location plus the incarnation that wrote it.
/// The incarnation distinguishes a reused location from its earlier contents.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) struct PageRef {
    pub page: u64,
    pub incarnation: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub(crate) enum PageKind {
    /// A copy-on-write tree node (leaf or branch; see `tree::Node`).
    Node = 1,
}

/// Encodes a node page. Fails with an explicit capacity error if the payload does not fit.
pub(crate) fn encode_page(
    store: StoreId,
    kind: PageKind,
    at: PageRef,
    payload: &[u8],
) -> Result<Vec<u8>, StorageError> {
    if payload.len() > PAGE_PAYLOAD_MAX {
        return Err(StorageError::InvalidRequest(format!(
            "capacity: node payload {} exceeds {PAGE_PAYLOAD_MAX} bytes ",
            payload.len()
        )));
    }
    let mut e = Enc::default();
    e.raw(&PAGE_MAGIC)
        .u16(FORMAT_VERSION)
        .u8(kind as u8)
        .u8(0)
        .raw(&store.0)
        .u64(at.page)
        .u64(at.incarnation)
        .u32(payload.len() as u32)
        .u32(0);
    e.raw(payload);
    let crc = crc32fast::hash(&e.buf);
    e.buf[PAGE_HEADER_LEN - 4..PAGE_HEADER_LEN].copy_from_slice(&crc.to_le_bytes());
    e.buf.resize(PAGE_SIZE, 0);
    Ok(e.buf)
}

/// Decodes and validates a node page read from `at`.
pub(crate) fn decode_page(
    store: StoreId,
    kind: PageKind,
    at: PageRef,
    bytes: &[u8],
) -> Result<Vec<u8>, StorageError> {
    if bytes.len() != PAGE_SIZE {
        return Err(corrupt(format!(
            "page {}: short read {}",
            at.page,
            bytes.len()
        )));
    }
    let mut d = Dec::new(&bytes[..PAGE_HEADER_LEN], "page header");
    if d.array::<4>()? != PAGE_MAGIC {
        return Err(corrupt(format!("page {}: bad magic", at.page)));
    }
    let version = d.u16()?;
    if version != FORMAT_VERSION {
        return Err(StorageError::Unsupported(format!(
            "page {}: format version {version}",
            at.page
        )));
    }
    let k = d.u8()?;
    let _reserved = d.u8()?;
    let s = d.array::<16>()?;
    let page = d.u64()?;
    let inc = d.u64()?;
    let len = d.u32()? as usize;
    let crc = d.u32()?;
    if len > PAGE_PAYLOAD_MAX {
        return Err(corrupt(format!("page {}: payload length {len}", at.page)));
    }
    let mut check = bytes[..PAGE_HEADER_LEN + len].to_vec();
    check[PAGE_HEADER_LEN - 4..PAGE_HEADER_LEN].fill(0);
    if crc32fast::hash(&check) != crc {
        return Err(corrupt(format!("page {}: checksum mismatch", at.page)));
    }
    if s != store.0 {
        return Err(corrupt(format!(
            "page {}: belongs to a different store",
            at.page
        )));
    }
    if page != at.page || inc != at.incarnation {
        return Err(corrupt(format!(
            "page {}: expected incarnation {} at {}, found {} at {}",
            at.page, at.incarnation, at.page, inc, page
        )));
    }
    if k != kind as u8 {
        return Err(corrupt(format!(
            "page {}: kind {k}, expected {}",
            at.page, kind as u8
        )));
    }
    Ok(bytes[PAGE_HEADER_LEN..PAGE_HEADER_LEN + len].to_vec())
}

/// A publication record: identifies one complete committed physical state.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Descriptor {
    pub store: StoreId,
    /// Physical generation; strictly increasing. Not a logical history position.
    pub generation: u64,
    pub root: PageRef,
    /// First metadata page never yet allocated.
    pub next_page: u64,
    /// Committed length of the content file; bytes beyond it are unreferenced.
    pub content_len: u64,
    /// Last committed logical history position (0 = none).
    pub history_pos: u64,
    /// Required features a reader must implement to read this state.
    pub features: u32,
}

impl Descriptor {
    /// The slot that holds a given generation.
    pub fn slot(generation: u64) -> u64 {
        generation % 2
    }

    pub fn encode(&self) -> Vec<u8> {
        let mut e = Enc::default();
        e.raw(&DESC_MAGIC)
            .u16(FORMAT_VERSION)
            .u32(self.features)
            .raw(&self.store.0)
            .u64(self.generation)
            .u64(self.root.page)
            .u64(self.root.incarnation)
            .u64(self.next_page)
            .u64(self.content_len)
            .u64(self.history_pos);
        let crc = crc32fast::hash(&e.buf);
        e.u32(crc);
        debug_assert_eq!(e.buf.len(), DESC_LEN);
        e.buf.resize(PAGE_SIZE, 0);
        e.buf
    }
}

/// What a publication slot contains.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Slot {
    /// Never written (all zero).
    Empty,
    Valid(Descriptor),
    /// Damaged or torn; the reason is kept for diagnostics.
    Invalid(String),
    /// A well-formed record of a format version, or requiring features, this build does not
    /// support; the reason names which.
    Unsupported(String),
}

pub(crate) fn decode_slot(bytes: &[u8]) -> Slot {
    if bytes.len() < DESC_LEN {
        return Slot::Invalid(format!("short slot ({} bytes)", bytes.len()));
    }
    if bytes[..DESC_LEN].iter().all(|&b| b == 0) {
        return Slot::Empty;
    }
    let (body, crc) = bytes[..DESC_LEN].split_at(DESC_LEN - 4);
    if crc32fast::hash(body).to_le_bytes() != crc {
        // Records of earlier versions have no required features, so their checksum ends 4 bytes
        // earlier: an intact one is a store this build does not support, not a damaged slot.
        let (old, old_crc) = bytes[..EARLIER_DESC_LEN].split_at(EARLIER_DESC_LEN - 4);
        if crc32fast::hash(old).to_le_bytes() == old_crc && old[..4] == DESC_MAGIC {
            let v = u16::from_le_bytes([old[4], old[5]]);
            if v != FORMAT_VERSION {
                return Slot::Unsupported(format!("format version {v}"));
            }
        }
        return Slot::Invalid("checksum mismatch".into());
    }
    let mut d = Dec::new(body, "descriptor");
    let parse = |d: &mut Dec| -> Result<Result<Descriptor, String>, StorageError> {
        if d.array::<4>()? != DESC_MAGIC {
            return Err(corrupt("bad magic"));
        }
        let v = d.u16()?;
        if v != FORMAT_VERSION {
            return Ok(Err(format!("format version {v}")));
        }
        let features = d.u32()?;
        let unknown = features & !KNOWN_REQUIRED_FEATURES;
        if unknown != 0 {
            return Ok(Err(format!("unknown required features {unknown:#x}")));
        }
        Ok(Ok(Descriptor {
            store: StoreId(d.array()?),
            generation: d.u64()?,
            root: PageRef {
                page: d.u64()?,
                incarnation: d.u64()?,
            },
            next_page: d.u64()?,
            content_len: d.u64()?,
            history_pos: d.u64()?,
            features,
        }))
    };
    match parse(&mut d) {
        Ok(Ok(desc)) => Slot::Valid(desc),
        Ok(Err(why)) => Slot::Unsupported(why),
        Err(e) => Slot::Invalid(e.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const S: StoreId = StoreId([7; 16]);

    #[test]
    fn page_roundtrip_and_rejections() {
        let at = PageRef {
            page: 5,
            incarnation: 3,
        };
        let p = encode_page(S, PageKind::Node, at, b"payload").unwrap();
        assert_eq!(p.len(), PAGE_SIZE);
        assert_eq!(decode_page(S, PageKind::Node, at, &p).unwrap(), b"payload");
        let stale = PageRef {
            page: 5,
            incarnation: 2,
        };
        assert!(
            decode_page(S, PageKind::Node, stale, &p).is_err(),
            "old incarnation accepted"
        );
        assert!(
            decode_page(StoreId([8; 16]), PageKind::Node, at, &p).is_err(),
            "foreign page accepted"
        );
        let mut flipped = p.clone();
        flipped[PAGE_HEADER_LEN + 2] ^= 1;
        assert!(
            decode_page(S, PageKind::Node, at, &flipped).is_err(),
            "corrupt payload accepted"
        );
        assert!(encode_page(S, PageKind::Node, at, &vec![0; PAGE_PAYLOAD_MAX + 1]).is_err());
    }

    #[test]
    fn descriptor_slots_classify_empty_valid_torn_and_unsupported() {
        let d = Descriptor {
            store: S,
            generation: 9,
            root: PageRef {
                page: 4,
                incarnation: 9,
            },
            next_page: 5,
            content_len: 100,
            history_pos: 2,
            features: 0,
        };
        let bytes = d.encode();
        assert_eq!(decode_slot(&bytes), Slot::Valid(d));
        assert_eq!(decode_slot(&vec![0; PAGE_SIZE]), Slot::Empty);
        let mut torn = bytes.clone();
        torn[20] ^= 0x40;
        assert!(matches!(decode_slot(&torn), Slot::Invalid(_)));
        let mut future = bytes.clone();
        future[4..6].copy_from_slice(&(FORMAT_VERSION + 1).to_le_bytes());
        let crc = crc32fast::hash(&future[..DESC_LEN - 4]);
        future[DESC_LEN - 4..DESC_LEN].copy_from_slice(&crc.to_le_bytes());
        assert_eq!(
            decode_slot(&future),
            Slot::Unsupported(format!("format version {}", FORMAT_VERSION + 1))
        );
        // Required features this build lacks: unsupported, whatever the bit.
        for bit in [1, 31] {
            let mut needs = bytes.clone();
            needs[6..10].copy_from_slice(&(1u32 << bit).to_le_bytes());
            let crc = crc32fast::hash(&needs[..DESC_LEN - 4]);
            needs[DESC_LEN - 4..DESC_LEN].copy_from_slice(&crc.to_le_bytes());
            assert_eq!(
                decode_slot(&needs),
                Slot::Unsupported(format!("unknown required features {:#x}", 1u32 << bit))
            );
        }
        assert_eq!(bytes[6..10], [0; 4], "a record requiring nothing");
    }
}
