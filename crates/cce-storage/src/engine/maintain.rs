//! Physical maintenance (see [`crate::NativeStore::maintain`]): explicit, bounded steps that publish
//! a new physical generation without any logical change. The writer lock orders each step with
//! commits, so a step always works on the newest published state.

use cce_core::storage::{CommitError, StorageError};

use super::{Inner, lock, rejected, sha256};
use crate::codec::{corrupt, key_components};
use crate::format::Descriptor;
use crate::io::FileId;
use crate::records::{
    ContentEntry, ExtentRef, Manifest, P_CONTENT, P_HISTORY, P_IN, P_OUT, P_REL, P_RETIRED,
    RelValue, Retired, decode_retired, retired_extent_key,
};
use crate::tree::{Cursor, TreeTxn};
use crate::{MaintenanceBudget, MaintenanceReport, MaintenanceTask};

impl Inner {
    pub(crate) fn maintain(
        &self,
        task: MaintenanceTask,
        budget: MaintenanceBudget,
    ) -> Result<MaintenanceReport, CommitError> {
        self.refuse_if_read_only("maintenance").map_err(rejected)?;
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
