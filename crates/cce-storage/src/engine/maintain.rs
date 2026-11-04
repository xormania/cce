//! Physical maintenance (see [`crate::NativeStore::maintain`]): explicit, bounded steps that publish
//! a new physical generation without any logical change. The writer lock orders each step with
//! commits, so a step always works on the newest published state.

use cce_core::storage::{CommitError, StorageError};

use std::sync::{Arc, Mutex};

use super::{ContentPool, Inner, Reuse, Taken, lock, rejected, sha256};
use crate::codec::{corrupt, key_components};
use crate::format::Descriptor;
use crate::format::PageRef;
use crate::io::FileId;
use crate::records::{
    ContentEntry, ExtentRef, Manifest, P_CONTENT, P_HISTORY, P_IN, P_OUT, P_REL, P_RETIRED,
    RelValue, Retired, decode_retired, retired_extent_key,
};
use crate::tree::{Cursor, MAX_HEIGHT, Node, NodeSource, TreeTxn};
use crate::{MaintenanceBudget, MaintenanceReport, MaintenanceTask};

impl Inner {
    pub(crate) fn maintain(
        self: &Arc<Self>,
        task: MaintenanceTask,
        budget: MaintenanceBudget,
    ) -> Result<MaintenanceReport, CommitError> {
        self.refuse_if_read_only("maintenance").map_err(rejected)?;
        if task == MaintenanceTask::Relocate {
            return self.relocate(budget);
        }
        let _w = lock(&self.writer);
        let (cur, fence) = {
            let st = lock(&self.state);
            (st.current, st.fenced.clone())
        };
        if let Some(f) = fence {
            return Err(rejected(StorageError::StoreState(format!(
                "writes fenced: {}",
                f.why
            ))));
        }
        match task {
            MaintenanceTask::ReclaimContentHoles => self.reclaim_holes(&cur, budget),
            MaintenanceTask::RebuildRelationshipIndexes => self.rebuild_indexes(&cur),
            other => Err(rejected(StorageError::InvalidRequest(format!(
                "maintenance task {other:?} is not implemented yet"
            )))),
        }
    }

    /// Content-file space below the committed length that is in use: reachable from the published
    /// state (manifests, segments, history entries), covered by a retirement record, held by a
    /// staging of this process, or discarded and awaiting retirement. Sorted `(offset, len)`.
    fn content_in_use(&self, cur: &Descriptor) -> Result<Vec<(u64, u64)>, StorageError> {
        let mut used = Vec::new();
        let scan = |prefix: u8| Cursor::new(self, cur.root, &[prefix], None, cur.next_page);
        for entry in scan(P_CONTENT)? {
            let (_, v) = entry?;
            if let ContentEntry::Live { manifest, .. } = ContentEntry::decode(&v)? {
                used.push((manifest.offset, manifest.len));
                for s in Manifest::decode(&self.read_extent(&manifest)?)?.segments {
                    used.push((s.offset, s.len));
                }
            }
        }
        for entry in scan(P_HISTORY)? {
            let (_, v) = entry?;
            let x = ExtentRef::decode(&v)?;
            used.push((x.offset, x.len));
        }
        for entry in scan(P_RETIRED)? {
            let (k, v) = entry?;
            if let Retired::Extent { offset, len, .. } = decode_retired(&k, &v)? {
                used.push((offset, len));
            }
        }
        // Stagings of this process are not published; their manifests may lie beyond committed
        // content, so they are read directly and checked against their digest.
        let staged: Vec<ExtentRef> = lock(&self.state).staged.values().map(|(m, _)| *m).collect();
        for m in staged {
            used.push((m.offset, m.len));
            let mut buf = vec![
                0;
                usize::try_from(m.len).map_err(|_| StorageError::Integrity(
                    "manifest too large".into()
                ))?
            ];
            self.io.read_at(FileId::Content, m.offset, &mut buf)?;
            if sha256(&buf) != m.digest {
                return Err(StorageError::Integrity(format!(
                    "staged manifest at {}: digest mismatch",
                    m.offset
                )));
            }
            for s in Manifest::decode(&buf)?.segments {
                used.push((s.offset, s.len));
            }
        }
        used.extend(lock(&self.pool).discarded.iter().copied());
        used.sort_unstable();
        Ok(used)
    }

    fn reclaim_holes(
        &self,
        cur: &Descriptor,
        budget: MaintenanceBudget,
    ) -> Result<MaintenanceReport, CommitError> {
        let used = self.content_in_use(cur).map_err(rejected)?;
        // The complement of what is in use, within the committed content.
        let mut holes = Vec::new();
        let mut at = 0u64;
        for (offset, len) in used {
            if offset > at && at < cur.content_len {
                holes.push((at, offset.min(cur.content_len) - at));
            }
            at = at.max(offset.saturating_add(len));
        }
        if at < cur.content_len {
            holes.push((at, cur.content_len - at));
        }
        let mut taken = Vec::new();
        let mut bytes = 0u64;
        for &(offset, len) in &holes {
            let room = budget.max_content_bytes.saturating_sub(bytes);
            if room == 0 {
                break;
            }
            let len = len.min(room);
            taken.push((offset, len));
            bytes += len;
        }
        let complete = bytes == holes.iter().map(|(_, l)| l).sum::<u64>();
        if taken.is_empty() {
            return Ok(report(cur.generation, 0, complete));
        }
        let Some(generation) = cur.generation.checked_add(1) else {
            return Err(rejected(StorageError::InvalidRequest(
                "generation counter exhausted".into(),
            )));
        };
        let mut txn = TreeTxn::new(self, cur.root);
        for &(offset, len) in &taken {
            txn.put(
                retired_extent_key(generation, offset),
                len.to_le_bytes().to_vec(),
            )
            .map_err(rejected)?;
        }
        // A physical generation: the history position does not move.
        let (desc, discarded) = self.publish(cur, txn, generation, cur.history_pos, None)?;
        self.adopt(desc, &taken, &discarded);
        Ok(report(generation, bytes, complete))
    }

    /// Rebuilds both relationship indexes from the relationship records, in one publication: every
    /// index entry is removed and each relationship's outgoing and incoming entries are written
    /// again. The old index is not consulted. Memory grows with the size of the indexes.
    fn rebuild_indexes(&self, cur: &Descriptor) -> Result<MaintenanceReport, CommitError> {
        let scan = |prefix: u8| Cursor::new(self, cur.root, &[prefix], None, cur.next_page);
        let mut stale = Vec::new();
        for prefix in [P_OUT, P_IN] {
            for entry in scan(prefix).map_err(rejected)? {
                stale.push(entry.map_err(rejected)?.0);
            }
        }
        let mut fresh = Vec::new();
        for entry in scan(P_REL).map_err(rejected)? {
            let (k, v) = entry.map_err(rejected)?;
            let id = key_components(&k[1..])
                .map_err(rejected)?
                .into_iter()
                .next()
                .ok_or_else(|| rejected(corrupt("relationship record without an identifier")))?;
            let r = RelValue::decode(&v).map_err(rejected)?;
            fresh.push(r.out_key(&id));
            fresh.push(r.in_key(&id));
        }
        let Some(generation) = cur.generation.checked_add(1) else {
            return Err(rejected(StorageError::InvalidRequest(
                "generation counter exhausted".into(),
            )));
        };
        let mut txn = TreeTxn::new(self, cur.root);
        for k in &stale {
            txn.delete(k).map_err(rejected)?;
        }
        for k in &fresh {
            txn.put(k.clone(), Vec::new()).map_err(rejected)?;
        }
        let (desc, discarded) = self.publish(cur, txn, generation, cur.history_pos, None)?;
        self.adopt(desc, &[], &discarded);
        Ok(MaintenanceReport {
            index_entries_written: fresh.len() as u64,
            ..report(generation, 0, true)
        })
    }

    /// Moves committed content streams from the highest offsets into retired space below them.
    ///
    /// The plan and the copy run without the writer lock, on a pinned basis (so nothing they read
    /// can be reused meanwhile), into destinations reserved in the pool as staging reserves them
    /// (so no staging can take them). Publication then takes the writer lock and applies each move
    /// to the newest state only if that content still has the manifest the move copied; any other
    /// move is dropped and its reservations returned, leaving the copied bytes in retired space.
    fn relocate_content(
        self: &Arc<Self>,
        budget: MaintenanceBudget,
    ) -> Result<MaintenanceReport, CommitError> {
        let (snap, _pin, ()) = self.pin_current_then(false, |_| ());
        let basis = snap.desc;
        let Some(bound) = self.reusable_through(basis.generation.saturating_add(1)) else {
            return Ok(report(basis.generation, 0, true));
        };
        // Content streams by the end of their highest extent, highest first.
        let mut candidates = Vec::new();
        for entry in Cursor::new(&**self, basis.root, &[P_CONTENT], None, basis.next_page)
            .map_err(rejected)?
        {
            let (k, v) = entry.map_err(rejected)?;
            if let ContentEntry::Live { manifest, .. } =
                ContentEntry::decode(&v).map_err(rejected)?
            {
                let m = Manifest::decode(&self.read_extent(&manifest).map_err(rejected)?)
                    .map_err(rejected)?;
                let end = m
                    .segments
                    .iter()
                    .chain(std::iter::once(&manifest))
                    .map(|x| x.offset + x.len)
                    .max()
                    .unwrap_or(0);
                candidates.push((end, k, manifest, m));
            }
        }
        candidates.sort_by_key(|c| std::cmp::Reverse(c.0));

        let mut taken = Taken {
            pool: &self.pool,
            taken: Vec::new(),
        };
        let mut moves = Vec::new();
        let mut bytes = 0u64;
        let mut complete = true;
        for (_, key, old, m) in candidates {
            let total = m.segments.iter().map(|s| s.len).sum::<u64>() + old.len;
            if bytes + total > budget.max_content_bytes {
                complete = false;
                break;
            }
            let mark = taken.taken.len();
            // Every piece must land below where it is now, or the move gains nothing.
            let mut placed = Vec::new();
            for s in m.segments.iter().chain(std::iter::once(&old)) {
                match self.take_retired(s.len, bound, &mut taken) {
                    Some(at) if at < s.offset => placed.push(at),
                    _ => break,
                }
            }
            if placed.len() != m.segments.len() + 1 {
                give_back(&self.pool, &mut taken, mark);
                continue;
            }
            // Copy one segment at a time, through the verified read.
            let mut segments = Vec::with_capacity(m.segments.len());
            for (s, &at) in m.segments.iter().zip(&placed) {
                let data = self.read_extent(s).map_err(rejected)?;
                self.io
                    .write_at(FileId::Content, at, &data)
                    .map_err(rejected)?;
                segments.push(ExtentRef { offset: at, ..*s });
            }
            let encoded = Manifest {
                content: m.content,
                segments,
            }
            .encode();
            let at = placed[placed.len() - 1];
            self.io
                .write_at(FileId::Content, at, &encoded)
                .map_err(rejected)?;
            let new = ExtentRef {
                offset: at,
                len: encoded.len() as u64,
                digest: sha256(&encoded),
            };
            moves.push(Move {
                key,
                old,
                old_segments: m.segments,
                new,
                takes: mark..taken.taken.len(),
                bytes: total,
            });
            bytes += total;
        }
        #[cfg(test)]
        {
            let hook = lock(&self.relocate_hook).take();
            if let Some(hook) = hook {
                hook();
            }
        }

        let _w = lock(&self.writer);
        let (cur, fence) = {
            let st = lock(&self.state);
            (st.current, st.fenced.clone())
        };
        if let Some(f) = fence {
            return Err(rejected(StorageError::StoreState(format!(
                "writes fenced: {}",
                f.why
            ))));
        }
        let mut txn = TreeTxn::new(&**self, cur.root);
        let mut applied = Vec::new();
        let mut dropped = Vec::new();
        for mv in moves {
            let still = match txn.get(&mv.key).map_err(rejected)? {
                Some(v) => match ContentEntry::decode(&v).map_err(rejected)? {
                    ContentEntry::Live { manifest, refs } if manifest == mv.old => Some(refs),
                    _ => None,
                },
                None => None,
            };
            match still {
                Some(refs) => {
                    txn.put(
                        mv.key.clone(),
                        ContentEntry::Live {
                            manifest: mv.new,
                            refs,
                        }
                        .encode(),
                    )
                    .map_err(rejected)?;
                    applied.push(mv);
                }
                None => dropped.push(mv),
            }
        }
        // Reservations of dropped moves go back to the pool; their copies stay in retired space.
        for mv in dropped.iter().rev() {
            for r in taken.taken.drain(mv.takes.clone()) {
                lock(&self.pool)
                    .free
                    .insert(r.offset, (r.record_len, r.retired_at));
            }
        }
        if applied.is_empty() {
            return Ok(report(cur.generation, 0, complete && dropped.is_empty()));
        }
        let Some(generation) = cur.generation.checked_add(1) else {
            return Err(rejected(StorageError::InvalidRequest(
                "generation counter exhausted".into(),
            )));
        };
        let mut retired = Vec::new();
        for mv in &applied {
            for x in mv.old_segments.iter().chain(std::iter::once(&mv.old)) {
                txn.put(
                    retired_extent_key(generation, x.offset),
                    x.len.to_le_bytes().to_vec(),
                )
                .map_err(rejected)?;
                retired.push((x.offset, x.len));
            }
        }
        // The reservations become consumed space, exactly as a commit consumes a staging's.
        let serial = {
            let mut st = lock(&self.state);
            let s = st.next_stage;
            st.next_stage += 1;
            s
        };
        let held: Vec<Reuse> = std::mem::take(&mut taken.taken);
        lock(&self.pool).held.insert(serial, held);
        let unbound = (
            ExtentRef {
                offset: 0,
                len: 0,
                digest: [0; 32],
            },
            cce_core::storage::ContentRef {
                digest: cce_core::storage::ContentDigest([0; 32]),
                len: 0,
            },
        );
        let remainders = self.consume_reuse(&mut txn, &[(serial, unbound)]);
        let published = remainders.and_then(|rem| {
            self.publish(&cur, txn, generation, cur.history_pos, None)
                .map(|p| (p, rem))
        });
        let ((desc, discarded), remainders) = match published {
            Ok(p) => p,
            Err(e) => {
                // Nothing was published: the reservations return to the pool.
                let mut pool = lock(&self.pool);
                for r in pool.held.remove(&serial).unwrap_or_default() {
                    pool.free.insert(r.offset, (r.record_len, r.retired_at));
                }
                return Err(e);
            }
        };
        self.adopt(desc, &retired, &discarded);
        let mut pool = lock(&self.pool);
        pool.held.remove(&serial);
        for (offset, len, retired_at) in remainders {
            pool.free.insert(offset, (len, retired_at));
        }
        drop(pool);
        let moved = applied.iter().map(|m| m.bytes).sum();
        Ok(MaintenanceReport {
            content_bytes_moved: moved,
            ..report(generation, 0, complete && dropped.is_empty())
        })
    }

    /// Moves committed content, then tree pages; each part publishes only if it moved something.
    fn relocate(
        self: &Arc<Self>,
        budget: MaintenanceBudget,
    ) -> Result<MaintenanceReport, CommitError> {
        let content = self.relocate_content(budget)?;
        let pages = self.relocate_pages(budget)?;
        Ok(MaintenanceReport {
            generation: pages.generation.max(content.generation),
            content_bytes_moved: content.content_bytes_moved,
            pages_moved: pages.pages_moved,
            ..report(0, 0, content.complete && pages.complete)
        })
    }

    /// Rewrites the highest tree pages, at most `budget.max_pages` of them, into retired pages
    /// numbered below all of them, under the writer lock like a commit. Rewriting a page rewrites
    /// its path from the root; every page written must be such a retired page, so the step never
    /// extends the metadata file. With too few, it tries half as many pages, down to none.
    fn relocate_pages(&self, budget: MaintenanceBudget) -> Result<MaintenanceReport, CommitError> {
        let _w = lock(&self.writer);
        let (cur, fence) = {
            let st = lock(&self.state);
            (st.current, st.fenced.clone())
        };
        if let Some(f) = fence {
            return Err(rejected(StorageError::StoreState(format!(
                "writes fenced: {}",
                f.why
            ))));
        }
        let mut pages = Vec::new();
        first_key_under(self, cur.root, 0, &mut pages).map_err(rejected)?;
        pages.sort_by_key(|(page, _)| std::cmp::Reverse(*page));
        let Some(generation) = cur.generation.checked_add(1) else {
            return Err(rejected(StorageError::InvalidRequest(
                "generation counter exhausted".into(),
            )));
        };
        let budget_pages = usize::try_from(budget.max_pages).unwrap_or(usize::MAX);
        let mut n = pages.len().min(budget_pages);
        while n > 0 {
            let chosen = &pages[..n];
            let below = chosen[n - 1].0;
            let mut txn = TreeTxn::new(self, cur.root);
            for (_, key) in chosen {
                // Writing a record back unchanged rewrites every page on its path.
                let value = txn
                    .get(key)
                    .map_err(rejected)?
                    .ok_or_else(|| rejected(corrupt("a tree page's first key is missing")))?;
                txn.put(key.clone(), value).map_err(rejected)?;
            }
            if let Some((desc, discarded)) =
                self.publish_below(&cur, txn, generation, cur.history_pos, None, Some(below))?
            {
                self.adopt(desc, &[], &discarded);
                return Ok(MaintenanceReport {
                    pages_moved: n as u64,
                    ..report(generation, 0, n == pages.len())
                });
            }
            n /= 2;
        }
        // Nothing could move: complete unless the budget allowed no page at all.
        Ok(report(
            cur.generation,
            0,
            pages.is_empty() || budget_pages > 0,
        ))
    }

    /// Makes a maintenance generation current. `retired` content extents it recorded, and the
    /// discarded extents its publication retired, become reusable under the ordinary rule.
    fn adopt(&self, desc: Descriptor, retired: &[(u64, u64)], discarded: &[(u64, u64)]) {
        let generation = desc.generation;
        lock(&self.state).current = desc;
        let mut pool = lock(&self.pool);
        for &(offset, len) in retired.iter().chain(discarded) {
            pool.free.insert(offset, (len, generation));
        }
        pool.discarded.retain(|x| !discarded.contains(x));
    }

    /// Installs a callback run once by the next relocation step between its copy and its
    /// publication.
    #[cfg(test)]
    pub(crate) fn set_relocate_hook_for_test(&self, hook: Option<Box<dyn FnOnce() + Send>>) {
        *lock(&self.relocate_hook) = hook;
    }

    /// Publishes the current tree with `edit` applied, as a physical generation (tests that craft
    /// damaged but checksum-valid stores).
    #[cfg(test)]
    pub(crate) fn publish_edit_for_test(
        &self,
        edit: impl FnOnce(&mut TreeTxn<'_>),
    ) -> Result<(), CommitError> {
        let _w = lock(&self.writer);
        let cur = lock(&self.state).current;
        let mut txn = TreeTxn::new(self, cur.root);
        edit(&mut txn);
        let (desc, discarded) =
            self.publish(&cur, txn, cur.generation + 1, cur.history_pos, None)?;
        self.adopt(desc, &[], &discarded);
        Ok(())
    }
}

/// One planned content move: from its old manifest and segments to a copy in reserved space.
struct Move {
    key: Vec<u8>,
    old: ExtentRef,
    old_segments: Vec<ExtentRef>,
    new: ExtentRef,
    /// Its reservations, as positions in the step's reservation list.
    takes: std::ops::Range<usize>,
    bytes: u64,
}

/// Lists every page of the tree under `at` with the smallest key below it into `out`, and returns
/// that key for `at` (none for an empty leaf, whose page is not listed).
fn first_key_under(
    src: &Inner,
    at: PageRef,
    depth: usize,
    out: &mut Vec<(u64, Vec<u8>)>,
) -> Result<Option<Vec<u8>>, StorageError> {
    if depth >= MAX_HEIGHT {
        return Err(corrupt("tree deeper than the supported maximum"));
    }
    let first = match &*src.load(at)? {
        Node::Leaf(entries) => entries.keys().next().cloned(),
        Node::Branch { children, .. } => {
            let mut first = None;
            for child in children {
                let f = first_key_under(src, *child, depth + 1, out)?;
                first = first.or(f);
            }
            first
        }
    };
    if let Some(k) = &first {
        out.push((at.page, k.clone()));
    }
    Ok(first)
}

/// Returns reservations taken since `mark` to the pool.
fn give_back(pool: &Mutex<ContentPool>, taken: &mut Taken, mark: usize) {
    let mut pool = lock(pool);
    for r in taken.taken.drain(mark..) {
        pool.free.insert(r.offset, (r.record_len, r.retired_at));
    }
}

fn report(generation: u64, reclaimed: u64, complete: bool) -> MaintenanceReport {
    MaintenanceReport {
        generation,
        pages_moved: 0,
        content_bytes_moved: 0,
        content_bytes_reclaimed: reclaimed,
        index_entries_written: 0,
        complete,
    }
}
