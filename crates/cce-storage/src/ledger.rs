//! Ownership of the published state's retired content space while the store is open.
//!
//! Every retired content extent the published state records is, in memory, exactly one of: free
//! for reuse, reserved by a staging of this open instance, or reserved by a publication whose
//! outcome is uncertain. Extents of discarded stagings and of rejected commits wait in a queue
//! until a publication retires them with a record. Only the operations below change any of this;
//! each checks its precondition first and leaves the pool unchanged when it does not hold, and an
//! operation over several extents applies all of them or none.

use std::collections::{BTreeMap, HashMap};

use cce_core::storage::AttemptId;

/// One retired content extent a staging took for reuse: it writes `used` bytes at its start.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Reuse {
    pub(crate) offset: u64,
    pub(crate) used: u64,
    pub(crate) record_len: u64,
    pub(crate) retired_at: u64,
}

/// Whether two content extents `(offset, len)` share a byte.
pub(crate) fn overlap(a: (u64, u64), b: (u64, u64)) -> bool {
    a.0 < b.0 + b.1 && b.0 < a.0 + a.1
}

/// The reuse pool of one open instance.
#[derive(Default)]
pub(crate) struct ContentPool {
    /// Not reserved: offset -> (length, retiring generation).
    free: BTreeMap<u64, (u64, u64)>,
    /// Reserved by stagings of this instance, by staging serial, until a commit binding the
    /// staging publishes (consuming them), the staging is discarded, or the store is reopened.
    held: HashMap<u64, Vec<Reuse>>,
    /// Stagings bound by admitted commits (by serial, with how many); they cannot be discarded.
    admitted: HashMap<u64, u32>,
    /// Extents no record covers yet (discarded stagings, rejected commits' history entries);
    /// the next publication retires them.
    discarded: Vec<(u64, u64)>,
    /// Reservations of stagings bound by a commit whose publication is uncertain, by staging
    /// serial, with that commit's attempt and generation: neither offered for reuse nor freed by
    /// a discard until reopen decides which publication won.
    uncertain: HashMap<u64, (AttemptId, u64, Vec<Reuse>)>,
    /// Taken and still being written, not yet reserved or given back, with the taker that took
    /// each (a staging call or a maintenance step, by taker id).
    taking: Vec<(u64, Reuse)>,
    /// Writes are fenced: nothing more is taken.
    fenced: bool,
}

impl ContentPool {
    // ----- reading ------------------------------------------------------------------------

    /// Total length of free extents.
    pub(crate) fn free_bytes(&self) -> u64 {
        self.free.values().map(|(len, _)| len).sum()
    }

    /// Free extents as `(offset, len, retiring generation)`, by offset.
    #[cfg(test)]
    pub(crate) fn free(&self) -> impl Iterator<Item = (u64, u64, u64)> + '_ {
        self.free.iter().map(|(o, (l, g))| (*o, *l, *g))
    }

    /// Reservations by staging serial.
    #[cfg(test)]
    pub(crate) fn held(&self) -> impl Iterator<Item = (u64, &[Reuse])> + '_ {
        self.held.iter().map(|(s, rs)| (*s, rs.as_slice()))
    }

    /// The reservations of staging `serial`.
    pub(crate) fn held_by(&self, serial: u64) -> &[Reuse] {
        self.held.get(&serial).map_or(&[], Vec::as_slice)
    }

    /// Reservations owned by uncertain publications: staging serial, attempt, generation.
    #[cfg(test)]
    pub(crate) fn uncertain(&self) -> impl Iterator<Item = (u64, &AttemptId, u64, &[Reuse])> + '_ {
        self.uncertain
            .iter()
            .map(|(s, (a, g, rs))| (*s, a, *g, rs.as_slice()))
    }

    pub(crate) fn discarded(&self) -> &[(u64, u64)] {
        &self.discarded
    }

    pub(crate) fn is_admitted(&self, serial: u64) -> bool {
        self.admitted.get(&serial).is_some_and(|n| *n > 0)
    }

    pub(crate) fn is_uncertain(&self, serial: u64) -> bool {
        self.uncertain.contains_key(&serial)
    }

    /// Whether `x` overlaps space the pool accounts for: a free extent, a reservation of a
    /// staging other than those in `except`, or a reservation an uncertain publication owns. Free
    /// extents are found by range query (they never overlap each other, so only the one starting
    /// before `x` and those starting inside it can); reservations are few and are scanned.
    pub(crate) fn overlaps(&self, x: (u64, u64), except: &[u64]) -> bool {
        let end = x.0.saturating_add(x.1);
        self.free
            .range(..x.0)
            .next_back()
            .is_some_and(|(o, (l, _))| overlap(x, (*o, *l)))
            || self.free.range(x.0..end).next().is_some()
            || self
                .held
                .iter()
                .filter(|(serial, _)| !except.contains(serial))
                .flat_map(|(_, rs)| rs)
                .any(|r| overlap(x, (r.offset, r.record_len)))
            || self
                .uncertain
                .values()
                .flat_map(|(_, _, rs)| rs)
                .chain(self.taking.iter().map(|(_, r)| r))
                .any(|r| overlap(x, (r.offset, r.record_len)))
    }

    /// Extents taken and still being written, by any taker.
    #[cfg(test)]
    pub(crate) fn taking(&self) -> Vec<Reuse> {
        self.taking.iter().map(|(_, r)| *r).collect()
    }

    /// The extents taker `owner` has taken and not yet reserved or given back, for its own
    /// cleanup. (Used by maintenance steps that take space.)
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn taking_of(&self, owner: u64) -> Vec<Reuse> {
        self.taking
            .iter()
            .filter(|(o, _)| *o == owner)
            .map(|(_, r)| *r)
            .collect()
    }

    // ----- changing ----------------------------------------------------------------------

    /// Makes retired extents free, all or none: each must be non-empty and overlap neither space
    /// the pool accounts for, nor an extent queued for retirement, nor another of `extents`.
    pub(crate) fn insert_free(&mut self, extents: &[(u64, u64, u64)]) -> Result<(), String> {
        for (i, (offset, len, g)) in extents.iter().enumerate() {
            if *len == 0 {
                return Err(format!("empty extent at offset {offset} cannot be freed"));
            }
            if self.overlaps((*offset, *len), &[])
                || self.discarded.iter().any(|q| overlap((*offset, *len), *q))
            {
                return Err(format!(
                    "content at offset {offset} ({len} bytes, generation {g}) overlaps space the \
                     pool already accounts for"
                ));
            }
            if extents[..i]
                .iter()
                .any(|(o, l, _)| overlap((*offset, *len), (*o, *l)))
            {
                return Err(format!("content at offset {offset} is freed twice"));
            }
        }
        for (offset, len, g) in extents {
            self.free.insert(*offset, (*len, *g));
        }
        Ok(())
    }

    /// Takes the smallest free extent of at least `need` bytes retired no later than `bound`, if
    /// any, for taker `owner` to write into. With `below`, only an extent whose first `need`
    /// bytes end at or below that offset qualifies (moving content towards the start of the
    /// file). Refused while writes are fenced.
    pub(crate) fn take(
        &mut self,
        owner: u64,
        need: u64,
        bound: u64,
        below: Option<u64>,
    ) -> Result<Option<Reuse>, String> {
        if self.fenced {
            return Err("writes fenced while staging".into());
        }
        let best = self
            .free
            .iter()
            .filter(|(offset, (len, retired_at))| {
                *len >= need
                    && crate::engine::may_reuse(*retired_at, Some(bound))
                    && below.is_none_or(|b| offset.checked_add(need).is_some_and(|end| end <= b))
            })
            .min_by_key(|(offset, (len, _))| (*len, **offset))
            .map(|(offset, (len, retired_at))| (*offset, *len, *retired_at));
        Ok(best.map(|(offset, record_len, retired_at)| {
            self.free.remove(&offset);
            let r = Reuse {
                offset,
                used: need,
                record_len,
                retired_at,
            };
            self.taking.push((owner, r));
            r
        }))
    }

    /// Returns extents taker `owner` took and will not use to the free space, at their original
    /// generation. Only that taker's own takes are accepted.
    pub(crate) fn give_back(&mut self, owner: u64, taken: &[Reuse]) -> Result<(), String> {
        let out = self.leave_taking(owner, taken)?;
        let extents: Vec<(u64, u64, u64)> = taken
            .iter()
            .map(|r| (r.offset, r.record_len, r.retired_at))
            .collect();
        self.insert_free(&extents)
            .inspect_err(|_| self.taking = out)
    }

    /// Removes `taken` from the extents in flight, each of which `owner` must have taken; returns
    /// the set as it was, for undoing.
    fn leave_taking(&mut self, owner: u64, taken: &[Reuse]) -> Result<Vec<(u64, Reuse)>, String> {
        if let Some(r) = taken.iter().find(|r| !self.taking.contains(&(owner, **r))) {
            return Err(format!(
                "content at offset {} was not taken by taker {owner}",
                r.offset
            ));
        }
        let before = self.taking.clone();
        self.taking
            .retain(|(o, r)| !(*o == owner && taken.contains(r)));
        Ok(before)
    }

    /// Records the extents taker `owner` took as reserved by staging `serial`, which it has
    /// become. Only that taker's own takes are accepted; a staging reserves once.
    pub(crate) fn hold(
        &mut self,
        owner: u64,
        serial: u64,
        taken: Vec<Reuse>,
    ) -> Result<(), Vec<Reuse>> {
        if self.held.contains_key(&serial) || self.uncertain.contains_key(&serial) {
            return Err(taken);
        }
        if self.leave_taking(owner, &taken).is_err() {
            return Err(taken);
        }
        self.held.insert(serial, taken);
        Ok(())
    }

    /// Discards staging `serial`: its reservations become free again at their original
    /// generation and its own extents (`own`, those not reserved) wait for the next publication
    /// to retire them; all or none. Refused for a staging an admitted commit binds or an
    /// uncertain publication owns (the caller reports those first).
    pub(crate) fn discard(&mut self, serial: u64, own: &[(u64, u64)]) -> Result<(), String> {
        if self.is_admitted(serial) || self.is_uncertain(serial) {
            return Err(format!("staging {serial} cannot be discarded now"));
        }
        let removed = self.held.remove(&serial);
        let held = removed.clone().unwrap_or_default();
        // Undoing restores exactly what was there, including nothing.
        let restore = |p: &mut Self| {
            if let Some(rs) = removed {
                p.held.insert(serial, rs);
            }
        };
        let queued: Vec<(u64, u64)> = own
            .iter()
            .copied()
            .filter(|(offset, _)| !held.iter().any(|r| r.offset == *offset))
            .collect();
        let extents: Vec<(u64, u64, u64)> = held
            .iter()
            .map(|r| (r.offset, r.record_len, r.retired_at))
            .collect();
        if let Err(why) = self.insert_free(&extents) {
            restore(self);
            return Err(why);
        }
        if let Err(why) = self.check_queue(&queued) {
            self.ungive(&held);
            restore(self);
            return Err(why);
        }
        self.discarded.extend(queued);
        Ok(())
    }

    /// Takes back extents `give_back` just freed (undoing it).
    fn ungive(&mut self, freed: &[Reuse]) {
        for r in freed {
            self.free.remove(&r.offset);
        }
    }

    fn check_queue(&self, extents: &[(u64, u64)]) -> Result<(), String> {
        for (i, x) in extents.iter().enumerate() {
            if self.overlaps(*x, &[])
                || self.discarded.iter().any(|q| overlap(*x, *q))
                || extents[..i].iter().any(|q| overlap(*x, *q))
            {
                return Err(format!(
                    "content at offset {} is queued for retirement over space already accounted for",
                    x.0
                ));
            }
        }
        Ok(())
    }

    /// Queues an extent no record covers yet for the next publication to retire.
    pub(crate) fn queue_discarded(&mut self, x: (u64, u64)) -> Result<(), String> {
        self.check_queue(&[x])?;
        self.discarded.push(x);
        Ok(())
    }

    /// Removes from the queue exactly the extents a publication retired (`sealed`); extents
    /// queued meanwhile stay.
    pub(crate) fn trim_discarded(&mut self, sealed: &[(u64, u64)]) {
        self.discarded.retain(|x| !sealed.contains(x));
    }

    /// A definite rejection before publication of work that reserved space for staging
    /// `serial` (a relocation step, say): its reservations become free again at their original
    /// retiring generation, all or none. Refused for a serial that reserves nothing, that an
    /// admitted commit binds (that commit is about to consume the reservations), or whose
    /// reservations an uncertain publication owns. (Used by maintenance steps that reserve space.)
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn release(&mut self, serial: u64) -> Result<(), String> {
        if self.uncertain.contains_key(&serial) {
            return Err(format!(
                "staging {serial}'s reservations belong to an uncertain publication"
            ));
        }
        if self.is_admitted(serial) {
            return Err(format!("staging {serial} is bound by an admitted commit"));
        }
        let Some(held) = self.held.remove(&serial) else {
            return Err(format!("staging {serial} reserves nothing"));
        };
        let extents: Vec<(u64, u64, u64)> = held
            .iter()
            .map(|r| (r.offset, r.record_len, r.retired_at))
            .collect();
        self.insert_free(&extents).inspect_err(|_| {
            self.held.insert(serial, held);
        })
    }

    /// The reservations of staging `serial`, consumed by the publication of a commit binding it.
    pub(crate) fn consume(&mut self, serial: u64) -> Vec<Reuse> {
        self.held.remove(&serial).unwrap_or_default()
    }

    /// The publication of `attempt` as `generation` is uncertain: the reservations of the
    /// stagings it binds become its own. A serial listed again adds nothing and replaces nothing.
    pub(crate) fn hand_to_uncertain(
        &mut self,
        attempt: &AttemptId,
        generation: u64,
        serials: &[u64],
    ) {
        self.fenced = true;
        for serial in serials {
            let held = self.held.remove(serial).unwrap_or_default();
            self.uncertain
                .entry(*serial)
                .or_insert_with(|| (attempt.clone(), generation, Vec::new()))
                .2
                .extend(held);
        }
    }

    pub(crate) fn fence(&mut self) {
        self.fenced = true;
    }

    /// Staging `serial` is bound by one more admitted commit.
    pub(crate) fn admit(&mut self, serial: u64) {
        *self.admitted.entry(serial).or_default() += 1;
    }

    /// One admitted commit binding staging `serial` has resolved.
    pub(crate) fn resolve_admission(&mut self, serial: u64) {
        if let Some(n) = self.admitted.get_mut(&serial) {
            *n -= 1;
            if *n == 0 {
                self.admitted.remove(&serial);
            }
        }
    }

    // ----- tests only --------------------------------------------------------------------

    /// Puts an extent in the free space without any check (tests planting inconsistencies).
    #[cfg(test)]
    pub(crate) fn plant_free_for_test(&mut self, offset: u64, len: u64, retired_at: u64) {
        self.free.insert(offset, (len, retired_at));
    }

    /// Removes the free extent at the lowest offset (tests planting inconsistencies).
    #[cfg(test)]
    pub(crate) fn drop_first_free_for_test(&mut self) -> Option<(u64, u64)> {
        let (offset, (len, _)) = self.free.pop_first()?;
        Some((offset, len))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    type Snapshot = (
        Vec<(u64, u64, u64)>,
        Vec<(u64, Vec<Reuse>)>,
        Vec<(u64, u64)>,
        Vec<(u64, u64, Vec<Reuse>)>,
        Vec<(u64, u32)>,
        bool,
    );

    /// Everything the pool holds, in a comparable order.
    fn snapshot(p: &ContentPool) -> Snapshot {
        let mut held: Vec<_> = p.held.iter().map(|(s, r)| (*s, r.clone())).collect();
        held.sort_by_key(|x| x.0);
        let mut unc: Vec<_> = p
            .uncertain
            .iter()
            .map(|(s, (_, g, r))| (*s, *g, r.clone()))
            .collect();
        unc.sort_by_key(|x| x.0);
        let mut adm: Vec<_> = p.admitted.iter().map(|(s, n)| (*s, *n)).collect();
        adm.sort();
        let mut free: Vec<_> = p.free().collect();
        free.extend(
            p.taking
                .iter()
                .map(|(o, r)| (r.offset, r.record_len, u64::MAX - r.retired_at * 8 - o)),
        );
        (free, held, p.discarded.clone(), unc, adm, p.fenced)
    }

    /// Free, reserved and queued extents never overlap one another.
    fn disjoint(p: &ContentPool) -> Result<(), String> {
        let mut all: Vec<(u64, u64)> = p.free().map(|(o, l, _)| (o, l)).collect();
        all.extend(p.held.values().flatten().map(|r| (r.offset, r.record_len)));
        all.extend(
            p.uncertain
                .values()
                .flat_map(|(_, _, rs)| rs)
                .map(|r| (r.offset, r.record_len)),
        );
        all.extend(p.discarded.iter().copied());
        all.extend(p.taking.iter().map(|(_, r)| (r.offset, r.record_len)));
        all.sort_unstable();
        match all.windows(2).find(|w| overlap(w[0], w[1])) {
            Some(w) => Err(format!("{:?} and {:?} overlap", w[0], w[1])),
            None => Ok(()),
        }
    }

    /// Frees each extent and takes it again, as a staging would: what `hold` accepts.
    fn taken(p: &mut ContentPool, extents: &[(u64, u64, u64)]) -> Vec<Reuse> {
        extents
            .iter()
            .map(|&(o, l, g)| {
                assert_eq!(p.insert_free(&[(o, l, g)]), Ok(()));
                let r = p.take(0, l, g, None).unwrap().unwrap();
                assert_eq!(r.offset, o);
                r
            })
            .collect()
    }

    fn attempt() -> AttemptId {
        AttemptId::new(b"a".to_vec()).unwrap()
    }

    /// Runs `op`; if it fails, the pool must be exactly as before.
    #[track_caller]
    fn refused<T: std::fmt::Debug>(p: &mut ContentPool, op: impl FnOnce(&mut ContentPool) -> T) {
        let before = snapshot(p);
        let _ = op(p);
        assert_eq!(snapshot(p), before, "a refused operation changed the pool");
    }

    #[test]
    fn freeing_refuses_any_overlap_and_changes_nothing_when_it_does() {
        let mut p = ContentPool::default();
        assert_eq!(p.insert_free(&[(100, 50, 1)]), Ok(()));
        let t = taken(&mut p, &[(300, 20, 1)]);
        assert_eq!(p.hold(0, 7, t), Ok(()));
        let t = taken(&mut p, &[(400, 20, 1)]);
        assert!(p.hold(0, 8, t).is_ok());
        p.hand_to_uncertain(&attempt(), 9, &[8]);
        for x in [
            (120, 10, 2),
            (90, 20, 2),
            (310, 5, 2),
            (410, 1, 2),
            (500, 0, 2),
        ] {
            refused(&mut p, |p| {
                assert!(p.insert_free(&[(1000, 10, 1), x]).is_err(), "{x:?}")
            });
        }
        refused(&mut p, |p| {
            assert!(p.insert_free(&[(1000, 10, 1), (1005, 10, 1)]).is_err())
        });
        assert_eq!(p.insert_free(&[(150, 10, 2), (200, 10, 2)]), Ok(()));
        assert_eq!(disjoint(&p), Ok(()));
    }

    #[test]
    fn taking_picks_the_smallest_fit_under_the_bound_and_stops_once_fenced() {
        let mut p = ContentPool::default();
        assert_eq!(
            p.insert_free(&[(0, 100, 1), (200, 60, 1), (300, 60, 5)]),
            Ok(())
        );
        assert_eq!(
            p.take(0, 50, 4, None),
            Ok(Some(Reuse {
                offset: 200,
                used: 50,
                record_len: 60,
                retired_at: 1
            }))
        );
        assert_eq!(p.take(0, 500, 4, None), Ok(None));
        p.fence();
        refused(&mut p, |p| assert!(p.take(0, 10, 9, None).is_err()));
    }

    #[test]
    fn taking_below_a_limit_only_uses_space_that_ends_at_or_below_it() {
        let mut p = ContentPool::default();
        assert_eq!(
            p.insert_free(&[(0, 100, 1), (200, 30, 1), (400, 20, 1)]),
            Ok(())
        );
        // The smallest fit (400) ends past the limit; the next smallest that fits below it wins.
        assert_eq!(
            p.take(0, 20, 9, Some(230)).map(|r| r.map(|r| r.offset)),
            Ok(Some(200))
        );
        // Exactly at the limit qualifies; one byte past does not.
        assert_eq!(
            p.take(0, 20, 9, Some(20)).map(|r| r.map(|r| r.offset)),
            Ok(Some(0))
        );
        assert_eq!(p.take(0, 20, 9, Some(419)), Ok(None));
        assert_eq!(
            p.take(0, 20, 9, Some(420)).map(|r| r.map(|r| r.offset)),
            Ok(Some(400))
        );
    }

    #[test]
    fn a_staging_reserves_once_and_a_refused_discard_keeps_its_reservations() {
        let mut p = ContentPool::default();
        let t = taken(&mut p, &[(0, 10, 1)]);
        assert_eq!(p.hold(0, 1, t), Ok(()));
        let t = taken(&mut p, &[(50, 10, 1)]);
        refused(&mut p, |p| assert!(p.hold(0, 1, t).is_err()));
        // A staging cannot reserve space it did not take.
        let stray = Reuse {
            offset: 60,
            used: 1,
            record_len: 1,
            retired_at: 1,
        };
        refused(&mut p, |p| assert!(p.hold(0, 2, vec![stray]).is_err()));
        // Nor space another taker took: taker 1 cannot reserve or give back taker 0's take.
        let others = p.taking_of(0);
        assert!(!others.is_empty());
        let copy = others.clone();
        refused(&mut p, |p| assert!(p.hold(1, 2, copy).is_err()));
        refused(&mut p, |p| assert!(p.give_back(1, &others).is_err()));
        p.admit(1);
        refused(&mut p, |p| assert!(p.discard(1, &[(100, 10)]).is_err()));
        refused(&mut p, |p| assert!(p.release(1).is_err()));
        p.resolve_admission(1);
        // Its own extent overlaps space already queued: nothing is given back or queued.
        assert_eq!(p.queue_discarded((100, 10)), Ok(()));
        refused(&mut p, |p| {
            assert!(p.discard(1, &[(0, 10), (105, 3)]).is_err())
        });
        assert_eq!(p.discard(1, &[(0, 10), (120, 4)]), Ok(()));
        assert_eq!(p.free().collect::<Vec<_>>(), [(0, 10, 1)]);
        assert_eq!(
            p.taking().len(),
            1,
            "the refused reservation is still being taken"
        );
        assert_eq!(p.discarded(), [(100, 10), (120, 4)]);
        assert_eq!(disjoint(&p), Ok(()));
    }

    #[test]
    fn an_uncertain_publication_owns_every_reservation_of_a_repeated_staging() {
        let mut p = ContentPool::default();
        let t = taken(&mut p, &[(0, 10, 1), (20, 10, 1)]);
        assert_eq!(p.hold(0, 3, t), Ok(()));
        p.hand_to_uncertain(&attempt(), 4, &[3, 3]);
        assert_eq!(
            p.uncertain().map(|(_, _, _, rs)| rs.len()).sum::<usize>(),
            2
        );
        refused(&mut p, |p| assert!(p.discard(3, &[]).is_err()));
        refused(&mut p, |p| assert!(p.release(3).is_err()));
        refused(&mut p, |p| assert!(p.release(99).is_err()));
        refused(&mut p, |p| assert!(p.take(0, 1, 9, None).is_err()));
    }

    /// Random sequences of every operation: free, reserved and queued space never overlaps, and
    /// an operation that fails leaves the pool exactly as it was.
    #[test]
    fn random_operation_sequences_keep_the_pool_consistent() {
        fn rng(s: &mut u64) -> u64 {
            *s ^= *s << 13;
            *s ^= *s >> 7;
            *s ^= *s << 17;
            *s
        }
        for seed in 1..=200u64 {
            let mut s = seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1;
            let mut p = ContentPool::default();
            // Extents in flight, by the taker that took them.
            let mut taken: Vec<(u64, Reuse)> = Vec::new();
            for step in 0..120 {
                let before = snapshot(&p);
                let at = rng(&mut s) % 64 * 16;
                let len = 1 + rng(&mut s) % 40;
                let serial = rng(&mut s) % 6;
                let ok = match rng(&mut s) % 12 {
                    11 => {
                        let mine: Vec<Reuse> = p.held_by(serial).to_vec();
                        let admitted = p.is_admitted(serial);
                        match p.release(serial) {
                            Ok(()) => {
                                assert!(
                                    !admitted,
                                    "seed {seed} step {step}: released an admitted staging"
                                );
                                // Exactly that serial's reservations, at their own generation.
                                let mut want = before.0.clone();
                                want.extend(
                                    mine.iter().map(|r| (r.offset, r.record_len, r.retired_at)),
                                );
                                want.sort_unstable();
                                let mut got = snapshot(&p).0;
                                got.sort_unstable();
                                assert_eq!(got, want, "seed {seed} step {step}: release");
                                assert!(p.held_by(serial).is_empty());
                                true
                            }
                            Err(why) => {
                                // Reserved space came out of free, so it always fits back: only
                                // an unknown or uncertain-owned serial is refused.
                                let reserved = before.1.iter().any(|(s, _)| *s == serial);
                                assert!(
                                    !reserved || p.is_uncertain(serial) || p.is_admitted(serial),
                                    "seed {seed} step {step}: release refused: {why}"
                                );
                                false
                            }
                        }
                    }
                    0 | 1 => p.insert_free(&[(at, len, rng(&mut s) % 5)]).is_ok(),
                    2 => {
                        let below = rng(&mut s).is_multiple_of(2).then(|| rng(&mut s) % 1100);
                        let owner = rng(&mut s) % 3;
                        match p.take(owner, len, rng(&mut s) % 6, below) {
                            Ok(Some(r)) => {
                                assert!(
                                    below.is_none_or(|b| r.offset + len <= b),
                                    "seed {seed} step {step}: {r:?} for {len} bytes, below {below:?}"
                                );
                                taken.push((owner, r));
                                true
                            }
                            Ok(None) => true,
                            Err(_) => false,
                        }
                    }
                    3 | 4 => {
                        // A taker settles a list of takes: its own, or (wrongly) another's.
                        let (owner, whose) = (rng(&mut s) % 3, rng(&mut s) % 3);
                        let list: Vec<Reuse> = taken
                            .iter()
                            .filter(|(o, _)| *o == whose)
                            .map(|(_, r)| *r)
                            .collect();
                        let settled = if rng(&mut s).is_multiple_of(2) {
                            p.give_back(owner, &list).is_ok()
                        } else {
                            p.hold(owner, serial, list.clone()).is_ok()
                        };
                        if owner != whose && !list.is_empty() {
                            assert!(
                                !settled,
                                "seed {seed} step {step}: taker {owner} settled taker {whose}'s takes"
                            );
                        }
                        if settled {
                            taken.retain(|(o, r)| !(*o == whose && list.contains(r)));
                        }
                        settled
                    }
                    5 => p.discard(serial, &[(at, len)]).is_ok(),
                    6 => p.queue_discarded((at, len)).is_ok(),
                    7 => {
                        let q: Vec<(u64, u64)> = p.discarded().iter().take(1).copied().collect();
                        p.trim_discarded(&q);
                        true
                    }
                    8 => {
                        p.consume(serial);
                        true
                    }
                    9 => {
                        if rng(&mut s).is_multiple_of(2) {
                            p.admit(serial);
                        } else {
                            p.resolve_admission(serial);
                        }
                        true
                    }
                    _ => {
                        if rng(&mut s).is_multiple_of(8) {
                            p.hand_to_uncertain(&attempt(), step, &[serial]);
                        }
                        true
                    }
                };
                let what = format!("seed {seed} step {step}");
                if !ok {
                    assert_eq!(
                        snapshot(&p),
                        before,
                        "{what}: a refused operation changed the pool"
                    );
                }
                assert_eq!(disjoint(&p), Ok(()), "{what}");
            }
        }
    }
}
