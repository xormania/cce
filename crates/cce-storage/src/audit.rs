//! Independent reachability audit of a store image: what each published state reaches, found by
//! reading the publication slots and walking each root page by page, without the engine's cache or
//! its in-memory state.

use std::collections::{BTreeMap, BTreeSet};

use cce_core::storage::StorageError;
use sha2::{Digest, Sha256};

use crate::codec::corrupt;
use crate::format::{Descriptor, PAGE_SIZE, PageKind, PageRef, Slot, decode_page, decode_slot};
use crate::io::{FileId, Io};
use crate::records::{ExtentRef, Manifest, P_CONTENT, P_HISTORY, P_RETIRED, decode_retired_page};
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
    /// Incarnation (writing generation) of each reached page.
    pub incarnations: BTreeMap<u64, u64>,
    /// Content manifests `(offset, len)`, one per committed digest.
    pub manifests: BTreeSet<(u64, u64)>,
    /// Content segments `(offset, len)` named by content manifests.
    pub content_segments: BTreeSet<(u64, u64)>,
    /// Retained history entries `(offset, len)`.
    pub history_extents: BTreeSet<(u64, u64)>,
    /// Retired metadata pages recorded in this state: page -> (retiring generation, incarnation).
    pub retired: BTreeMap<u64, (u64, u64)>,
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
        r.incarnations.insert(at.page, at.incarnation);
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
                        Some(&P_RETIRED) => {
                            let (generation, page, incarnation) = decode_retired_page(&k, &v)?;
                            if r.retired.insert(page, (generation, incarnation)).is_some() {
                                return Err(corrupt(format!("page {page} retired twice")));
                            }
                        }
                        _ => {}
                    }
                }
            }
        }
    }
    Ok(r)
}

/// Checks the retirement records of the newest published state against what the states reach:
/// no retired page is reachable, every retirement lies inside the state, and when the other slot
/// holds the directly preceding generation, the pages retired by the newest commit are exactly the
/// pages the preceding state reached and the newest no longer does, while older records carry over
/// unchanged.
pub(crate) fn check_retirement(io: &dyn Io) -> Result<(), String> {
    let e = |x: StorageError| x.to_string();
    let slots = published(io).map_err(e)?;
    let cur = slots.last().ok_or("no valid publication record")?;
    let rc = reach(io, cur).map_err(e)?;
    if !rc.disjoint() {
        return Err(format!(
            "generation {}: overlapping extents",
            cur.generation
        ));
    }
    for (page, (generation, _)) in &rc.retired {
        if rc.pages.contains(page) {
            return Err(format!(
                "generation {}: retired page {page} is reachable",
                cur.generation
            ));
        }
        if *generation > cur.generation || *page >= cur.next_page {
            return Err(format!(
                "generation {}: retirement of page {page} at {generation} lies outside the state",
                cur.generation
            ));
        }
    }
    if let [prev, _] = slots.as_slice()
        && prev.generation + 1 == cur.generation
    {
        let rp = reach(io, prev).map_err(e)?;
        let dropped: BTreeSet<u64> = rp.pages.difference(&rc.pages).copied().collect();
        let now: BTreeSet<u64> = rc
            .retired
            .iter()
            .filter(|(_, (g, _))| *g == cur.generation)
            .map(|(p, _)| *p)
            .collect();
        if now != dropped {
            return Err(format!(
                "generation {}: retired {now:?}, but the commit stopped reaching {dropped:?}",
                cur.generation
            ));
        }
        let carried: BTreeMap<u64, (u64, u64)> = rc
            .retired
            .iter()
            .filter(|(_, (g, _))| *g != cur.generation)
            .map(|(p, v)| (*p, *v))
            .collect();
        // Earlier records either carry over unchanged or were consumed by reusing their page.
        if carried.iter().any(|(p, v)| rp.retired.get(p) != Some(v)) {
            return Err(format!(
                "generation {}: an earlier retirement record changed or appeared",
                cur.generation
            ));
        }
        let consumed: BTreeMap<u64, (u64, u64)> = rp
            .retired
            .iter()
            .filter(|(p, _)| !carried.contains_key(p))
            .map(|(p, v)| (*p, *v))
            .collect();
        // Pages the newest commit wrote below the previous allocation cursor are reused pages.
        let reused: BTreeSet<u64> = rc
            .incarnations
            .iter()
            .filter(|(p, inc)| **inc == cur.generation && **p < prev.next_page)
            .map(|(p, _)| *p)
            .collect();
        let consumed_pages: BTreeSet<u64> = consumed.keys().copied().collect();
        if consumed_pages != reused {
            return Err(format!(
                "generation {}: consumed records {consumed_pages:?}, but reused pages {reused:?}",
                cur.generation
            ));
        }
        for (page, (retired_at, _)) in &consumed {
            if cur.generation < retired_at + 2 {
                return Err(format!(
                    "generation {}: reused page {page} retired at {retired_at}, which the fallback \
                     publication record can still reach",
                    cur.generation
                ));
            }
        }
    }
    Ok(())
}
