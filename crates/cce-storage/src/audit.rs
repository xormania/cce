//! Independent reachability audit of a store image: what each published state reaches, found by
//! reading the publication slots and walking each root page by page, without the engine's cache or
//! its in-memory state.

use std::collections::BTreeSet;

use cce_core::storage::StorageError;
use sha2::{Digest, Sha256};

use crate::codec::corrupt;
use crate::format::{Descriptor, PAGE_SIZE, PageKind, PageRef, Slot, decode_page, decode_slot};
use crate::io::{FileId, Io};
use crate::records::{ExtentRef, Manifest, P_CONTENT, P_HISTORY};
use crate::tree::Node;

/// The valid publication records, in their proper slots, oldest generation first.
pub(crate) fn published(io: &dyn Io) -> Result<Vec<Descriptor>, StorageError> {
    let mut out = Vec::new();
    for slot in 0..2u64 {
        let mut buf = vec![0; PAGE_SIZE];
        io.read_at(FileId::Meta, slot * PAGE_SIZE as u64, &mut buf)?;
        if let Slot::Valid(d) = decode_slot(&buf)
            && Descriptor::slot(d.generation) == slot
        {
            out.push(d);
        }
    }
    out.sort_by_key(|d| d.generation);
    Ok(out)
}

/// Everything one published state reaches.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct Reach {
    /// Metadata pages of the tree.
    pub pages: BTreeSet<u64>,
    /// Content manifests `(offset, len)`, one per committed digest.
    pub manifests: BTreeSet<(u64, u64)>,
    /// Content segments `(offset, len)` named by content manifests.
    pub content_segments: BTreeSet<(u64, u64)>,
    /// Retained history entries `(offset, len)`.
    pub history_extents: BTreeSet<(u64, u64)>,
}

impl Reach {
    /// No two reached extents overlap.
    pub fn disjoint(&self) -> bool {
        let mut all: Vec<(u64, u64)> = self
            .manifests
            .iter()
            .chain(&self.content_segments)
            .chain(&self.history_extents)
            .copied()
            .collect();
        let n = all.len();
        all.sort_unstable();
        all.dedup();
        all.len() == n && all.windows(2).all(|w| w[0].0 + w[0].1 <= w[1].0)
    }
}

/// Reads a checked extent of the content file that `d` committed.
fn read_extent(io: &dyn Io, d: &Descriptor, x: &ExtentRef) -> Result<Vec<u8>, StorageError> {
    if x.offset
        .checked_add(x.len)
        .is_none_or(|end| end > d.content_len)
    {
        return Err(corrupt(format!(
            "generation {}: extent {}+{} beyond committed content",
            d.generation, x.offset, x.len
        )));
    }
    let mut buf = vec![0; usize::try_from(x.len).map_err(|_| corrupt("extent too large"))?];
    io.read_at(FileId::Content, x.offset, &mut buf)?;
    if Sha256::digest(&buf).as_slice() != x.digest {
        return Err(corrupt(format!(
            "generation {}: extent at {} fails its digest",
            d.generation, x.offset
        )));
    }
    Ok(buf)
}

/// Walks the tree of `d` from its root, decoding every page.
pub(crate) fn reach(io: &dyn Io, d: &Descriptor) -> Result<Reach, StorageError> {
    let mut r = Reach::default();
    let mut stack: Vec<PageRef> = vec![d.root];
    while let Some(at) = stack.pop() {
        if !r.pages.insert(at.page) {
            return Err(corrupt(format!(
                "generation {}: page {} reached twice",
                d.generation, at.page
            )));
        }
        if at.incarnation > d.generation || at.page >= d.next_page {
            return Err(corrupt(format!(
                "generation {}: reference {}@{} outside the state",
                d.generation, at.page, at.incarnation
            )));
        }
        let mut buf = vec![0; PAGE_SIZE];
        io.read_at(FileId::Meta, at.page * PAGE_SIZE as u64, &mut buf)?;
        match Node::decode(&decode_page(d.store, PageKind::Node, at, &buf)?)? {
            Node::Branch { children, .. } => stack.extend(children),
            Node::Leaf(entries) => {
                for (k, v) in entries {
                    match k.first() {
                        Some(&P_CONTENT) => {
                            let x = ExtentRef::decode(&v)?;
                            r.manifests.insert((x.offset, x.len));
                            for s in Manifest::decode(&read_extent(io, d, &x)?)?.segments {
                                r.content_segments.insert((s.offset, s.len));
                            }
                        }
                        Some(&P_HISTORY) => {
                            let x = ExtentRef::decode(&v)?;
                            r.history_extents.insert((x.offset, x.len));
                        }
                        _ => {}
                    }
                }
            }
        }
    }
    Ok(r)
}
