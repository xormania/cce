//! Physical maintenance (see [`crate::NativeStore::maintain`]): explicit, bounded steps that publish
//! a new physical generation without any logical change. The writer lock orders each step with
//! commits, so a step always works on the newest published state.

use cce_core::storage::{AttemptId, CommitError, StorageError};

use std::cell::Cell;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use super::{BufferHold, Fence, Inner, MaintenanceSpace, Reuse, Taken, lock, rejected, sha256};
use crate::codec::{corrupt, key_components};
use crate::format::PageRef;
use crate::format::{Descriptor, PAGE_SIZE, PageKind, decode_page};
use crate::io::FileId;
use crate::records::{
    ContentEntry, ExtentRef, Manifest, P_CONTENT, P_HISTORY, P_IN, P_OUT, P_REL, P_RETIRED,
    RelValue, Retired, decode_retired, retired_extent_key,
};
use crate::tree::{Cursor, MAX_HEIGHT, Node, NodeSource, TreeTxn};
use crate::{MaintenanceBudget, MaintenanceReport, MaintenanceTask};

/// Manifests are read up to the format's maximum (a u32 segment count), as by readers: the open's
/// segment limit bounds staging only.
fn manifest_limit() -> u64 {
    Manifest::encoded_len(u32::MAX as usize)
}
const MANIFEST: &str = "content manifest";

impl Inner {
    pub(crate) fn maintain(
        self: &Arc<Self>,
        task: MaintenanceTask,
        budget: MaintenanceBudget,
    ) -> Result<MaintenanceReport, CommitError> {
        self.refuse_if_read_only("maintenance").map_err(rejected)?;
        // A step that panics may have written a durable publication record before it made it
        // current: its outcome is unknown, as for a commit that panics. Writes are fenced, and
        // space a relocation reserved stays owned by that uncertain publication.
        let reserved: Cell<Option<(u64, u64)>> = Cell::new(None);
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            self.maintain_step(task, budget, &reserved)
        }))
        .unwrap_or_else(|_| {
            let why = format!("maintenance step {task:?} failed unexpectedly");
            self.fence(Fence {
                why: why.clone(),
                uncertain: None,
            });
            if let Some((serial, generation)) = reserved.get() {
                self.hold_for_uncertain(&relocation_owner(), generation, &[serial]);
            }
            Err(CommitError::Indeterminate(why))
        })
    }

    /// One maintenance step; `reserved` names the reservations a relocation holds while their
    /// outcome is not settled, as `(serial, generation)`.
    fn maintain_step(
        self: &Arc<Self>,
        task: MaintenanceTask,
        budget: MaintenanceBudget,
        reserved: &Cell<Option<(u64, u64)>>,
    ) -> Result<MaintenanceReport, CommitError> {
        if task == MaintenanceTask::Relocate {
            return self.relocate(budget, reserved);
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

    /// Every content extent the published state `cur` reaches: each live entry's manifest and its
    /// segments, and each history entry. Found from the raw image, as an independent audit would:
    /// pages are read straight from the files (no cache, no cursor) and followed from the root,
    /// and manifests are read raw and checked against their digest. It shares nothing with how
    /// hole reclaim chooses its holes ([`Inner::content_in_use`]), so a hole that choice got wrong
    /// is still found reachable here.
    fn full_reach(&self, cur: &Descriptor) -> Result<Vec<(u64, u64)>, StorageError> {
        let mut reach = Vec::new();
        let mut seen = std::collections::HashSet::new();
        let mut stack = vec![cur.root];
        while let Some(at) = stack.pop() {
            if !seen.insert(at.page) || at.page >= cur.next_page || at.incarnation > cur.generation
            {
                return Err(corrupt(format!(
                    "page {}@{} reached twice or outside generation {}",
                    at.page, at.incarnation, cur.generation
                )));
            }
            let mut page = vec![0; PAGE_SIZE];
            self.counters.pages_read.fetch_add(1, Ordering::Relaxed);
            self.io
                .read_at(FileId::Meta, at.page * PAGE_SIZE as u64, &mut page)?;
            match Node::decode(&decode_page(cur.store, PageKind::Node, at, &page)?)? {
                Node::Branch { children, .. } => stack.extend(children),
                Node::Leaf(entries) => {
                    for (k, v) in entries {
                        match k.first() {
                            Some(&P_CONTENT) => {
                                if let ContentEntry::Live { manifest: x, .. } =
                                    ContentEntry::decode(&v)?
                                {
                                    if x.offset
                                        .checked_add(x.len)
                                        .is_none_or(|e| e > cur.content_len)
                                    {
                                        return Err(corrupt(format!(
                                            "manifest {}+{} beyond committed content",
                                            x.offset, x.len
                                        )));
                                    }
                                    reach.push((x.offset, x.len));
                                    let m = Manifest::decode(&self.read_raw(&x)?)?;
                                    reach.extend(m.segments.iter().map(|s| (s.offset, s.len)));
                                }
                            }
                            Some(&P_HISTORY) => {
                                let x = ExtentRef::decode(&v)?;
                                reach.push((x.offset, x.len));
                            }
                            _ => {}
                        }
                    }
                }
            }
        }
        Ok(reach)
    }

    /// Every extent of the stagings this process has registered and not yet committed or
    /// discarded: their manifests and segments, read from this process's own writes. Their bytes
    /// can lie below the committed length once a later commit covers them.
    fn staged_reach(&self) -> Result<Vec<(u64, u64)>, StorageError> {
        let staged: Vec<ExtentRef> = lock(&self.state).staged.values().map(|(m, _)| *m).collect();
        let mut reach = Vec::new();
        for x in staged {
            reach.push((x.offset, x.len));
            let m = Manifest::decode(&self.read_raw(&x)?)?;
            reach.extend(m.segments.iter().map(|s| (s.offset, s.len)));
        }
        Ok(reach)
    }

    /// Reads manifest extent `x` straight from the content file and checks it against its digest.
    /// Its length is bounded by the format's maximum, and its digest is first checked by streaming
    /// it through one chunk, so a damaged length never makes this hold more than a chunk before
    /// the extent proves authentic.
    pub(crate) fn read_raw(&self, x: &ExtentRef) -> Result<Vec<u8>, StorageError> {
        if x.len > manifest_limit() {
            return Err(corrupt(format!(
                "manifest of {} bytes exceeds the format's {} byte maximum",
                x.len,
                manifest_limit()
            )));
        }
        let len = usize::try_from(x.len).map_err(|_| corrupt("extent too large"))?;
        if len > super::VERIFY_CHUNK {
            let mut chunk = vec![0; super::VERIFY_CHUNK];
            let mut digest = <sha2::Sha256 as sha2::Digest>::new();
            let mut done = 0;
            while done < len {
                let n = (len - done).min(super::VERIFY_CHUNK);
                self.io
                    .read_at(FileId::Content, x.offset + done as u64, &mut chunk[..n])?;
                sha2::Digest::update(&mut digest, &chunk[..n]);
                done += n;
            }
            let got: [u8; 32] = sha2::Digest::finalize(digest).into();
            if got != x.digest {
                return Err(corrupt(format!(
                    "content extent at {}: digest mismatch",
                    x.offset
                )));
            }
        }
        let mut buf = vec![0; len];
        self.io.read_at(FileId::Content, x.offset, &mut buf)?;
        if sha256(&buf) != x.digest {
            return Err(corrupt(format!(
                "content extent at {}: digest mismatch",
                x.offset
            )));
        }
        Ok(buf)
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
                for s in
                    Manifest::decode(&self.read_extent(&manifest, manifest_limit(), MANIFEST)?)?
                        .segments
                {
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
        // content, so they are read straight from the file, bounded and checked against their
        // digest like every other manifest read.
        let staged: Vec<ExtentRef> = lock(&self.state).staged.values().map(|(m, _)| *m).collect();
        for m in staged {
            used.push((m.offset, m.len));
            for s in Manifest::decode(&self.read_raw(&m)?)?.segments {
                used.push((s.offset, s.len));
            }
        }
        used.extend(lock(&self.pool).discarded().iter().copied());
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
        // Sealed against everything the published state reaches, read from the raw image, and
        // every registered staging, found independently of how the holes were chosen: a hole over
        // either is refused.
        let space = MaintenanceSpace {
            retired: taken.clone(),
            still_reachable: {
                let mut reach = self.full_reach(cur).map_err(rejected)?;
                reach.extend(self.staged_reach().map_err(rejected)?);
                reach
            },
            ..MaintenanceSpace::default()
        };
        // A physical generation: the history position does not move.
        let (desc, discarded) = self.publish(cur, txn, generation, cur.history_pos, space)?;
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
        let (desc, discarded) = self.publish(
            cur,
            txn,
            generation,
            cur.history_pos,
            MaintenanceSpace::default(),
        )?;
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
        reserved: &Cell<Option<(u64, u64)>>,
    ) -> Result<MaintenanceReport, CommitError> {
        // A fenced store cannot publish: refuse before copying anything (checked again under the
        // writer lock before publishing).
        let (snap, _pin, fence) = self.pin_current_then(false, |st| st.fenced.clone());
        if let Some(f) = fence {
            return Err(rejected(StorageError::StoreState(format!(
                "writes fenced: {}",
                f.why
            ))));
        }
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
                let m = Manifest::decode(
                    &self
                        .read_extent(&manifest, manifest_limit(), MANIFEST)
                        .map_err(rejected)?,
                )
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

        // This step's own taker id: the pool accepts back, or reserves, only what it took.
        let mut taken = Taken {
            inner: self,
            owner: self
                .next_taker
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed),
            taken: Vec::new(),
        };
        let mut moves = Vec::new();
        let mut bytes = 0u64;
        let mut complete = true;
        let mut over_budget = 0u64;
        for (_, key, old, m) in candidates {
            let total = m.segments.iter().map(|s| s.len).sum::<u64>() + old.len;
            let mark = taken.taken.len();
            // Every piece must land entirely below where it is now, or the move gains nothing. A
            // stream with nowhere lower to go is not work left to do.
            let mut placed = Vec::new();
            for s in m.segments.iter().chain(std::iter::once(&old)) {
                // A checked take, refused once writes are fenced (then nothing moves).
                let got = lock(&self.pool)
                    .take(taken.owner, s.len, bound, Some(s.offset))
                    .ok()
                    .flatten();
                match got {
                    Some(r) => {
                        taken.taken.push(r);
                        placed.push(r.offset);
                    }
                    None => break,
                }
            }
            if placed.len() != m.segments.len() + 1 {
                give_back(&mut taken, mark);
                continue;
            }
            // A movable stream that does not fit is skipped, never a reason to stop: smaller
            // streams below it may still fit. One larger than the whole budget can only move with
            // a larger one; one that only exceeds what remains moves on a later call.
            if total > budget.max_content_bytes {
                give_back(&mut taken, mark);
                over_budget += 1;
                complete = false;
                continue;
            }
            if bytes + total > budget.max_content_bytes {
                give_back(&mut taken, mark);
                complete = false;
                continue;
            }
            // Copy one segment at a time, through the verified read.
            let mut segments = Vec::with_capacity(m.segments.len());
            for (s, &at) in m.segments.iter().zip(&placed) {
                let _held = BufferHold::new(self, s.len);
                let data = self
                    .read_extent(s, crate::records::SEGMENT_MAX, "content segment")
                    .map_err(rejected)?;
                self.io
                    .write_at(FileId::Content, at, &data)
                    .map_err(rejected)?;
                segments.push(ExtentRef { offset: at, ..*s });
            }
            let encoded = Manifest {
                content: m.content,
                segments: segments.clone(),
            }
            .encode();
            let new_segments = segments;
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
                new_segments,
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
        #[cfg(test)]
        let skip = self
            .skip_relocation_revalidation
            .swap(false, std::sync::atomic::Ordering::SeqCst);
        #[cfg(not(test))]
        let skip = false;
        for mv in moves {
            let still = match txn.get(&mv.key).map_err(rejected)? {
                Some(v) => match ContentEntry::decode(&v).map_err(rejected)? {
                    ContentEntry::Live { manifest, refs } if manifest == mv.old || skip => {
                        Some(refs)
                    }
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
            let back: Vec<Reuse> = taken.taken.drain(mv.takes.clone()).collect();
            let given = lock(&self.pool).give_back(taken.owner, &back);
            if let Err(why) = given {
                self.fault(&why);
            }
        }
        if applied.is_empty() {
            return Ok(MaintenanceReport {
                content_streams_over_budget: over_budget,
                ..report(cur.generation, 0, complete && dropped.is_empty())
            });
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
        let held_now = lock(&self.pool).hold(taken.owner, serial, held);
        if let Err(back) = held_now {
            // Not this step's to reserve: give it back and refuse the step.
            taken.taken = back;
            return Err(rejected(StorageError::StoreState(format!(
                "space check failed: relocation could not reserve its space as {serial}"
            ))));
        }
        reserved.set(Some((serial, generation)));
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
        let consumed = self.consume_reuse(&mut txn, &[(serial, unbound)]);
        let published = consumed.and_then(|(rem, consumed)| {
            // Old manifests and segments are retired; the copies must stay reachable, so the
            // seal's overlap check compares the two, and each entry must name its new manifest.
            let space = MaintenanceSpace {
                retired: retired.clone(),
                remainders: rem.clone(),
                consumed,
                still_reachable: applied
                    .iter()
                    .flat_map(|mv| std::iter::once(mv.new).chain(mv.new_segments.iter().copied()))
                    .map(|x| (x.offset, x.len))
                    .collect(),
                bound: vec![serial],
                entries: applied.iter().map(|mv| (mv.key.clone(), mv.new)).collect(),
            };
            self.publish(&cur, txn, generation, cur.history_pos, space)
                .map(|p| (p, rem))
        });
        let ((desc, discarded), remainders) = match published {
            Ok(p) => p,
            Err(e @ CommitError::Indeterminate(_)) => {
                // The record may be durable and reference the copies: the reservations stay
                // owned by this uncertain publication until reopen decides, never offered for
                // reuse (the publication already fenced writes).
                self.hold_for_uncertain(&relocation_owner(), generation, &[serial]);
                reserved.set(None);
                return Err(e);
            }
            Err(e) => {
                // Definitely not published: the reservations return to the pool at their
                // original generation.
                reserved.set(None);
                let released = lock(&self.pool).release(serial);
                if let Err(why) = released {
                    self.fault(&why);
                }
                return Err(e);
            }
        };
        #[cfg(test)]
        if self
            .panic_next_maintenance
            .swap(false, std::sync::atomic::Ordering::SeqCst)
        {
            panic!("injected: maintenance step panicked after its publication");
        }
        self.adopt(desc, &retired, &discarded);
        // The reservations are consumed; the unused parts of their records stay free at their
        // original generation.
        let freed = {
            let mut pool = lock(&self.pool);
            reserved.set(None);
            let held = pool.consume(serial);
            self.count_reused(&held);
            pool.insert_free(&remainders)
        };
        if let Err(why) = freed {
            self.fault(&why);
        }
        let moved = applied.iter().map(|m| m.bytes).sum();
        Ok(MaintenanceReport {
            content_bytes_moved: moved,
            content_streams_over_budget: over_budget,
            ..report(generation, 0, complete && dropped.is_empty())
        })
    }

    /// Moves committed content, then tree pages; each part publishes only if it moved something.
    fn relocate(
        self: &Arc<Self>,
        budget: MaintenanceBudget,
        reserved: &Cell<Option<(u64, u64)>>,
    ) -> Result<MaintenanceReport, CommitError> {
        let content = self.relocate_content(budget, reserved)?;
        let pages = self.relocate_pages(budget)?;
        Ok(MaintenanceReport {
            generation: pages.generation.max(content.generation),
            content_bytes_moved: content.content_bytes_moved,
            content_streams_over_budget: content.content_streams_over_budget,
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
            if let Some((desc, discarded)) = self.publish_below(
                &cur,
                txn,
                generation,
                cur.history_pos,
                MaintenanceSpace::default(),
                Some(below),
            )? {
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
        let freed: Vec<(u64, u64, u64)> = retired
            .iter()
            .chain(discarded)
            .map(|&(offset, len)| (offset, len, generation))
            .collect();
        let inserted = {
            let mut pool = lock(&self.pool);
            pool.trim_discarded(discarded);
            pool.insert_free(&freed)
        };
        // Sealed before publication, so this cannot fail; if it does, the record is durable and
        // nothing can be rejected any more: record the fault and fence writes.
        if let Err(why) = inserted {
            self.fault(&why);
        }
    }

    /// Installs a callback run once by the next relocation step between its copy and its
    /// publication.
    #[cfg(test)]
    pub(crate) fn set_relocate_hook_for_test(&self, hook: Option<Box<dyn FnOnce() + Send>>) {
        *lock(&self.relocate_hook) = hook;
    }

    /// Makes the next relocation step panic right after its publication, before it is applied.
    #[cfg(test)]
    pub(crate) fn panic_next_maintenance_for_test(&self) {
        self.panic_next_maintenance
            .store(true, std::sync::atomic::Ordering::SeqCst);
    }

    /// Makes the next relocation step skip revalidating its moves.
    #[cfg(test)]
    pub(crate) fn skip_relocation_revalidation_for_test(&self) {
        self.skip_relocation_revalidation
            .store(true, std::sync::atomic::Ordering::SeqCst);
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
        let (desc, discarded) = self.publish(
            &cur,
            txn,
            cur.generation + 1,
            cur.history_pos,
            MaintenanceSpace::default(),
        )?;
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
    new_segments: Vec<ExtentRef>,
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
/// The owner an uncertain relocation's reservations are recorded under.
fn relocation_owner() -> AttemptId {
    AttemptId::new(*b"maintenance: relocation").expect("a valid attempt identifier")
}

/// Gives back what `taken` took since `mark` (a stream that will not move).
fn give_back(taken: &mut Taken, mark: usize) {
    let back: Vec<Reuse> = taken.taken.drain(mark..).collect();
    let given = lock(&taken.inner.pool).give_back(taken.owner, &back);
    if let Err(why) = given {
        taken.inner.fault(&why);
    }
}

fn report(generation: u64, reclaimed: u64, complete: bool) -> MaintenanceReport {
    MaintenanceReport {
        generation,
        pages_moved: 0,
        content_bytes_moved: 0,
        content_bytes_reclaimed: reclaimed,
        index_entries_written: 0,
        content_streams_over_budget: 0,
        complete,
    }
}
