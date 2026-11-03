//! Copy-on-write ordered tree over the metadata file.
//!
//! Current limitation (documented in `docs/storage-format.md`): a tree is a single leaf
//! page. A commit whose resulting leaf does not fit returns an explicit capacity error. Nodes are
//! never modified in place: each commit writes a new leaf at a fresh page.

use std::collections::BTreeMap;
use std::ops::Bound;

use scientdb_kernel::storage::StorageError;

use crate::codec::{Dec, Enc, corrupt};

/// Sorted key → value entries of one leaf.
pub(crate) type Entries = BTreeMap<Vec<u8>, Vec<u8>>;

/// Leaf payload: `count u32`, then `count` × (`key_len u16`, key, `val_len u16`, value), keys
/// strictly increasing.
pub(crate) fn encode_leaf(entries: &Entries) -> Vec<u8> {
    let mut e = Enc::default();
    e.u32(entries.len() as u32);
    for (k, v) in entries {
        e.u16(u16::try_from(k.len()).expect("key under 64 KiB"))
            .raw(k);
        e.u16(u16::try_from(v.len()).expect("value under 64 KiB"))
            .raw(v);
    }
    e.buf
}

pub(crate) fn decode_leaf(payload: &[u8]) -> Result<Entries, StorageError> {
    let mut d = Dec::new(payload, "leaf");
    let n = d.count(4)?;
    let mut out = Entries::new();
    let mut prev: Option<Vec<u8>> = None;
    for _ in 0..n {
        let kl = d.u16()? as usize;
        let k = d.raw(kl)?.to_vec();
        let vl = d.u16()? as usize;
        let v = d.raw(vl)?.to_vec();
        if prev.as_ref().is_some_and(|p| *p >= k) {
            return Err(corrupt("leaf: keys not strictly increasing"));
        }
        prev = Some(k.clone());
        out.insert(k, v);
    }
    d.finish()?;
    Ok(out)
}

/// Entries whose key starts with `prefix`, from `start` (inclusive) if given, in key order.
pub(crate) fn prefix_range<'a>(
    entries: &'a Entries,
    prefix: &'a [u8],
    start: Option<&'a [u8]>,
) -> impl Iterator<Item = (&'a Vec<u8>, &'a Vec<u8>)> + 'a {
    let lower = start.unwrap_or(prefix);
    entries
        .range::<[u8], _>((Bound::Included(lower), Bound::Unbounded))
        .take_while(move |(k, _)| k.starts_with(prefix))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn leaf_roundtrip_and_order_validation() {
        let mut m = Entries::new();
        m.insert(b"a".to_vec(), b"1".to_vec());
        m.insert(b"b".to_vec(), Vec::new());
        assert_eq!(decode_leaf(&encode_leaf(&m)).unwrap(), m);
        let mut bad = Enc::default();
        bad.u32(2).u16(1).raw(b"b").u16(0).u16(1).raw(b"a").u16(0);
        assert!(decode_leaf(&bad.buf).is_err());
    }

    #[test]
    fn prefix_range_is_bounded_by_prefix() {
        let mut m = Entries::new();
        for k in [&b"\x01a"[..], b"\x02a", b"\x02b", b"\x03"] {
            m.insert(k.to_vec(), Vec::new());
        }
        let got: Vec<_> = prefix_range(&m, b"\x02", None)
            .map(|(k, _)| k.clone())
            .collect();
        assert_eq!(got, vec![b"\x02a".to_vec(), b"\x02b".to_vec()]);
        assert_eq!(prefix_range(&m, b"\x04", None).count(), 0);
    }
}
