//! Copy-on-write B+tree over metadata pages.
//!
//! * Leaf nodes hold sorted key → value entries. Branch nodes hold `n` children and `n-1`
//!   separator keys; child `i` holds keys `k` with `sep[i-1] <= k < sep[i]`.
//! * A commit edits an in-memory working copy of only the nodes on the paths it touches
//!   ([`TreeTxn`]); untouched subtrees stay referenced by their existing page and incarnation.
//!   [`TreeTxn::finish`] writes every touched node to a fresh page, children before parents, so a
//!   published root never refers to a page that is later modified.
//! * Nodes split by encoded size. Deletion removes empty nodes, merges an underfull leaf into an
//!   adjacent sibling when the merged node fits, and collapses a root branch with a single child.
//!   There is no other minimum-occupancy rule.
//! * Readers walk the tree with [`Cursor`], which holds a root-to-leaf stack for one generation and
//!   loads one leaf at a time.

use std::collections::BTreeMap;
use std::ops::Bound;
use std::sync::Arc;

use cce_core::storage::StorageError;

use crate::codec::{Dec, Enc, corrupt};
use crate::format::{PAGE_PAYLOAD_MAX, PageRef};

/// Sorted key → value entries of one leaf.
pub(crate) type Entries = BTreeMap<Vec<u8>, Vec<u8>>;
/// One key → value entry.
pub(crate) type KeyValue = (Vec<u8>, Vec<u8>);

/// Largest key + value accepted into one leaf entry (at least two entries fit per leaf).
pub(crate) const MAX_ENTRY: usize = 1800;
/// Largest key accepted into the tree: as long as an entry may be. Keys become branch separators;
/// a branch of three children with two such separators (5 + 3 * 16 + 2 * (2 + 1800) = 3657 bytes)
/// still fits a page, so a branch overflows only with four or more children and always splits into
/// halves of at least two children that fit.
pub(crate) const MAX_KEY: usize = MAX_ENTRY;
/// A leaf whose encoded size falls below this is merged with a sibling when the result fits.
const MERGE_BELOW: usize = PAGE_PAYLOAD_MAX / 4;
/// Deepest tree accepted when reading; deeper means corruption or a reference cycle.
pub(crate) const MAX_HEIGHT: usize = 32;

/// A decoded node.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Node {
    Leaf(Entries),
    Branch {
        seps: Vec<Vec<u8>>,
        children: Vec<PageRef>,
    },
}

const LEAF_TAG: u8 = 1;
const BRANCH_TAG: u8 = 2;

fn leaf_size(e: &Entries) -> usize {
    5 + e.iter().map(|(k, v)| 4 + k.len() + v.len()).sum::<usize>()
}

fn branch_size(seps: &[Vec<u8>], n_children: usize) -> usize {
    5 + n_children * 16 + seps.iter().map(|s| 2 + s.len()).sum::<usize>()
}

impl Node {
    /// Node payload: tag `u8`, then
    /// leaf: `count u32` × (`key_len u16`, key, `val_len u16`, value), keys strictly increasing;
    /// branch: `children u32`, child 0 (`page u64`, `incarnation u64`), then per further child
    /// `sep_len u16`, separator, child ref. Separators strictly increasing.
    pub fn encode(&self) -> Vec<u8> {
        let mut e = Enc::default();
        match self {
            Node::Leaf(entries) => {
                e.u8(LEAF_TAG).u32(entries.len() as u32);
                for (k, v) in entries {
                    e.u16(k.len() as u16).raw(k).u16(v.len() as u16).raw(v);
                }
            }
            Node::Branch { seps, children } => {
                e.u8(BRANCH_TAG).u32(children.len() as u32);
                for (i, c) in children.iter().enumerate() {
                    if i > 0 {
                        let s = &seps[i - 1];
                        e.u16(s.len() as u16).raw(s);
                    }
                    e.u64(c.page).u64(c.incarnation);
                }
            }
        }
        e.buf
    }

    pub fn decode(payload: &[u8]) -> Result<Self, StorageError> {
        let mut d = Dec::new(payload, "tree node");
        match d.u8()? {
            LEAF_TAG => {
                let n = d.count(4)?;
                let mut out = Entries::new();
                let mut prev: Option<Vec<u8>> = None;
                for _ in 0..n {
                    let kl = d.u16()? as usize;
                    let k = d.raw(kl)?.to_vec();
                    let vl = d.u16()? as usize;
                    let v = d.raw(vl)?.to_vec();
                    // The limits every write enforces; splitting a leaf relies on them.
                    if kl > MAX_KEY {
                        return Err(corrupt(format!(
                            "leaf: key of {kl} bytes exceeds the {MAX_KEY}-byte key limit"
                        )));
                    }
                    if kl + vl > MAX_ENTRY {
                        return Err(corrupt(format!(
                            "leaf: entry of {} bytes exceeds the {MAX_ENTRY}-byte entry limit",
                            kl + vl
                        )));
                    }
                    if prev.as_ref().is_some_and(|p| *p >= k) {
                        return Err(corrupt("leaf: keys not strictly increasing"));
                    }
                    prev = Some(k.clone());
                    out.insert(k, v);
                }
                d.finish()?;
                Ok(Node::Leaf(out))
            }
            BRANCH_TAG => {
                let n = d.count(16)?;
                if n < 2 {
                    return Err(corrupt(format!("branch with {n} children")));
                }
                let mut seps: Vec<Vec<u8>> =
                    Vec::with_capacity(d.capacity_for(n, 16).saturating_sub(1));
                let mut children = Vec::with_capacity(d.capacity_for(n, 16));
                for i in 0..n {
                    if i > 0 {
                        let sl = d.u16()? as usize;
                        // Separators are leaf keys; splitting a branch relies on the key limit.
                        if sl > MAX_KEY {
                            return Err(corrupt(format!(
                                "branch: separator of {sl} bytes exceeds the {MAX_KEY}-byte key \
                                 limit"
                            )));
                        }
                        let s = d.raw(sl)?.to_vec();
                        if seps.last().is_some_and(|p| *p >= s) {
                            return Err(corrupt("branch: separators not strictly increasing"));
                        }
                        seps.push(s);
                    }
                    children.push(PageRef {
                        page: d.u64()?,
                        incarnation: d.u64()?,
                    });
                }
                d.finish()?;
                Ok(Node::Branch { seps, children })
            }
            t => Err(corrupt(format!("tree node tag {t}"))),
        }
    }

    fn size(&self) -> usize {
        match self {
            Node::Leaf(e) => leaf_size(e),
            Node::Branch { seps, children } => branch_size(seps, children.len()),
        }
    }
}

/// Index of the child of a branch that may hold `key`.
fn child_index(seps: &[Vec<u8>], key: &[u8]) -> usize {
    seps.partition_point(|s| s.as_slice() <= key)
}

/// Reads committed nodes (through the engine's cache).
pub(crate) trait NodeSource {
    fn load(&self, at: PageRef) -> Result<Arc<Node>, StorageError>;
}

/// Allocates and writes new node pages.
pub(crate) trait NodeSink {
    fn write(&mut self, node: &Node) -> Result<PageRef, StorageError>;
}

/// Point lookup in a committed tree.
pub(crate) fn get(
    src: &dyn NodeSource,
    root: PageRef,
    key: &[u8],
) -> Result<Option<Vec<u8>>, StorageError> {
    let mut at = root;
    for _ in 0..MAX_HEIGHT {
        match &*src.load(at)? {
            Node::Leaf(e) => return Ok(e.get(key).cloned()),
            Node::Branch { seps, children } => at = children[child_index(seps, key)],
        }
    }
    Err(corrupt("tree deeper than the supported maximum"))
}

/// Height of a committed tree (a lone leaf has height 1).
pub(crate) fn height(src: &dyn NodeSource, root: PageRef) -> Result<usize, StorageError> {
    let mut at = root;
    for h in 1..=MAX_HEIGHT {
        match &*src.load(at)? {
            Node::Leaf(_) => return Ok(h),
            Node::Branch { children, .. } => at = children[0],
        }
    }
    Err(corrupt("tree deeper than the supported maximum"))
}

/// Forward cursor over the keys with a given prefix in one committed tree, from an optional start.
/// Holds only the current root-to-leaf path; leaves are loaded as the cursor reaches them.
pub(crate) struct Cursor<'a> {
    src: &'a dyn NodeSource,
    /// Branches on the path, with the index of the child currently being visited.
    stack: Vec<(Arc<Node>, usize)>,
    leaf: Option<Arc<Node>>,
    /// Next key to return is the first key >= this.
    next: Vec<u8>,
    prefix: Vec<u8>,
    done: bool,
    /// Nodes this cursor may still load. A valid tree never loads a page twice, so the store's
    /// page count bounds the work; a shared or cyclic subtree exhausts it.
    budget: u64,
}

impl<'a> Cursor<'a> {
    /// Keys starting with `prefix`, from `start` (which must itself start with `prefix`) onward.
    pub fn new(
        src: &'a dyn NodeSource,
        root: PageRef,
        prefix: &[u8],
        start: Option<&[u8]>,
        budget: u64,
    ) -> Result<Self, StorageError> {
        let next = start.unwrap_or(prefix).to_vec();
        let mut c = Cursor {
            src,
            stack: Vec::new(),
            leaf: None,
            next,
            prefix: prefix.to_vec(),
            done: false,
            budget,
        };
        c.descend(root)?;
        Ok(c)
    }

    fn load(&mut self, at: PageRef) -> Result<Arc<Node>, StorageError> {
        self.budget = self.budget.checked_sub(1).ok_or_else(|| {
            corrupt("tree traversal exceeded the store's page count (shared or cyclic subtree)")
        })?;
        self.src.load(at)
    }

    fn descend(&mut self, mut at: PageRef) -> Result<(), StorageError> {
        loop {
            if self.stack.len() >= MAX_HEIGHT {
                return Err(corrupt("tree deeper than the supported maximum"));
            }
            let node = self.load(at)?;
            match &*node {
                Node::Leaf(_) => {
                    self.leaf = Some(node);
                    return Ok(());
                }
                Node::Branch { seps, children } => {
                    let i = child_index(seps, &self.next);
                    at = children[i];
                    self.stack.push((node.clone(), i));
                }
            }
        }
    }

    /// Moves to the leftmost leaf of the next subtree; returns false at the end of the tree.
    fn advance_leaf(&mut self) -> Result<bool, StorageError> {
        while let Some((node, i)) = self.stack.pop() {
            let Node::Branch { children, .. } = &*node else {
                unreachable!("stack holds branches")
            };
            if i + 1 < children.len() {
                let child = children[i + 1];
                self.stack.push((node.clone(), i + 1));
                let mut at = child;
                loop {
                    if self.stack.len() >= MAX_HEIGHT {
                        return Err(corrupt("tree deeper than the supported maximum"));
                    }
                    let n = self.load(at)?;
                    match &*n {
                        Node::Leaf(_) => {
                            self.leaf = Some(n);
                            return Ok(true);
                        }
                        Node::Branch { children, .. } => {
                            at = children[0];
                            self.stack.push((n.clone(), 0));
                        }
                    }
                }
            }
        }
        Ok(false)
    }

    pub fn next_entry(&mut self) -> Result<Option<KeyValue>, StorageError> {
        while !self.done {
            let Some(leaf) = self.leaf.clone() else { break };
            let Node::Leaf(e) = &*leaf else {
                unreachable!("leaf")
            };
            if let Some((k, v)) = e
                .range::<[u8], _>((Bound::Included(self.next.as_slice()), Bound::Unbounded))
                .next()
            {
                if !k.starts_with(&self.prefix) {
                    self.done = true;
                    return Ok(None);
                }
                let mut after = k.clone();
                after.push(0);
                self.next = after;
                return Ok(Some((k.clone(), v.clone())));
            }
            if !self.advance_leaf()? {
                self.done = true;
            }
        }
        Ok(None)
    }
}

impl Iterator for Cursor<'_> {
    type Item = Result<KeyValue, StorageError>;
    fn next(&mut self) -> Option<Self::Item> {
        self.next_entry().transpose()
    }
}

/// A child reference inside the working copy.
#[derive(Clone, Copy, Debug)]
enum Child {
    Clean(PageRef),
    Dirty(usize),
}

#[derive(Clone, Debug)]
enum Work {
    Leaf(Entries),
    Branch {
        seps: Vec<Vec<u8>>,
        children: Vec<Child>,
    },
}

/// A copy-on-write edit of one committed tree.
pub(crate) struct TreeTxn<'a> {
    src: &'a dyn NodeSource,
    arena: Vec<Work>,
    root: Child,
    /// Every key put or deleted, in order (the transaction's write set).
    touched: Vec<Vec<u8>>,
    /// Committed pages this transaction copied in order to change them, in the order copied. The
    /// result never refers to them.
    replaced: Vec<PageRef>,
}

enum Outcome {
    Same,
    Split(Vec<u8>, usize),
    Empty,
    /// The branch was left with one child; its parent should refer to that child directly.
    Single(Child),
}

impl<'a> TreeTxn<'a> {
    pub fn new(src: &'a dyn NodeSource, root: PageRef) -> Self {
        Self {
            src,
            arena: Vec::new(),
            root: Child::Clean(root),
            touched: Vec::new(),
            replaced: Vec::new(),
        }
    }

    fn dirty(&mut self, c: Child) -> Result<usize, StorageError> {
        match c {
            Child::Dirty(i) => Ok(i),
            Child::Clean(at) => {
                self.replaced.push(at);
                let w = match &*self.src.load(at)? {
                    Node::Leaf(e) => Work::Leaf(e.clone()),
                    Node::Branch { seps, children } => Work::Branch {
                        seps: seps.clone(),
                        children: children.iter().map(|&p| Child::Clean(p)).collect(),
                    },
                };
                self.arena.push(w);
                Ok(self.arena.len() - 1)
            }
        }
    }

    pub fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>, StorageError> {
        let mut c = self.root;
        for _ in 0..MAX_HEIGHT {
            match c {
                Child::Clean(at) => return get(self.src, at, key),
                Child::Dirty(i) => match &self.arena[i] {
                    Work::Leaf(e) => return Ok(e.get(key).cloned()),
                    Work::Branch { seps, children } => c = children[child_index(seps, key)],
                },
            }
        }
        Err(corrupt("tree deeper than the supported maximum"))
    }

    pub fn put(&mut self, key: Vec<u8>, value: Vec<u8>) -> Result<(), StorageError> {
        if key.len() > MAX_KEY {
            return Err(StorageError::InvalidRequest(format!(
                "record too large for metadata: key of {} bytes exceeds the {MAX_KEY}-byte key limit",
                key.len()
            )));
        }
        if key.len() + value.len() > MAX_ENTRY {
            return Err(StorageError::InvalidRequest(format!(
                "record too large for metadata: key {} + value {} bytes exceed the {MAX_ENTRY}-byte \
                 entry limit",
                key.len(),
                value.len()
            )));
        }
        self.touched.push(key.clone());
        let root = self.dirty(self.root)?;
        self.root = Child::Dirty(root);
        if let Outcome::Split(sep, right) = self.put_in(root, key, value, 0)? {
            self.arena.push(Work::Branch {
                seps: vec![sep],
                children: vec![Child::Dirty(root), Child::Dirty(right)],
            });
            self.root = Child::Dirty(self.arena.len() - 1);
        }
        Ok(())
    }

    fn put_in(
        &mut self,
        i: usize,
        key: Vec<u8>,
        value: Vec<u8>,
        depth: usize,
    ) -> Result<Outcome, StorageError> {
        if depth >= MAX_HEIGHT {
            return Err(corrupt("tree deeper than the supported maximum"));
        }
        let (ci, child) = match &mut self.arena[i] {
            Work::Leaf(e) => {
                e.insert(key, value);
                return Ok(self.split_leaf_if_needed(i));
            }
            Work::Branch { seps, children } => {
                let ci = child_index(seps, &key);
                (ci, children[ci])
            }
        };
        let d = self.dirty(child)?;
        let out = self.put_in(d, key, value, depth + 1)?;
        let Work::Branch { seps, children } = &mut self.arena[i] else {
            unreachable!()
        };
        children[ci] = Child::Dirty(d);
        if let Outcome::Split(sep, right) = out {
            seps.insert(ci, sep);
            children.insert(ci + 1, Child::Dirty(right));
        }
        Ok(self.split_branch_if_needed(i))
    }

    fn split_leaf_if_needed(&mut self, i: usize) -> Outcome {
        let Work::Leaf(e) = &mut self.arena[i] else {
            unreachable!()
        };
        let total = leaf_size(e);
        if total <= PAGE_PAYLOAD_MAX {
            return Outcome::Same;
        }
        // Choose the split point that minimizes the larger half while both halves fit. Every entry
        // is at most MAX_ENTRY, so such a point exists.
        let sizes: Vec<usize> = e.iter().map(|(k, v)| 4 + k.len() + v.len()).collect();
        let mut left = 5;
        let mut best: Option<(usize, usize)> = None;
        for (idx, sz) in sizes.iter().enumerate().take(sizes.len() - 1) {
            left += sz;
            let right = total - left + 5;
            if left <= PAGE_PAYLOAD_MAX
                && right <= PAGE_PAYLOAD_MAX
                && best.is_none_or(|(_, worst)| left.max(right) < worst)
            {
                best = Some((idx + 1, left.max(right)));
            }
        }
        let (at, _) = best.expect("an oversized leaf of bounded entries has a fitting split");
        let k = e.keys().nth(at).expect("split key").clone();
        let right = e.split_off(&k);
        self.arena.push(Work::Leaf(right));
        Outcome::Split(k, self.arena.len() - 1)
    }

    fn split_branch_if_needed(&mut self, i: usize) -> Outcome {
        let Work::Branch { seps, children } = &mut self.arena[i] else {
            unreachable!()
        };
        let n = children.len();
        if branch_size(seps, n) <= PAGE_PAYLOAD_MAX {
            return Outcome::Same;
        }
        // Left keeps children[..m] and seps[..m-1]; seps[m-1] moves up; right keeps the rest.
        // Each side keeps at least two children; pick the most balanced m where both fit.
        let mut best: Option<(usize, usize)> = None;
        for m in 2..=n.saturating_sub(2) {
            let l = branch_size(&seps[..m - 1], m);
            let r = branch_size(&seps[m..], n - m);
            if l <= PAGE_PAYLOAD_MAX
                && r <= PAGE_PAYLOAD_MAX
                && best.is_none_or(|(_, worst)| l.max(r) < worst)
            {
                best = Some((m, l.max(r)));
            }
        }
        let (m, _) = best.expect("an oversized branch of bounded separators has a fitting split");
        let right_children = children.split_off(m);
        let mut right_seps = seps.split_off(m - 1);
        let promoted = right_seps.remove(0);
        self.arena.push(Work::Branch {
            seps: right_seps,
            children: right_children,
        });
        Outcome::Split(promoted, self.arena.len() - 1)
    }

    /// Removes `key`; returns whether it was present.
    pub fn delete(&mut self, key: &[u8]) -> Result<bool, StorageError> {
        if self.get(key)?.is_none() {
            return Ok(false);
        }
        self.touched.push(key.to_vec());
        let root = self.dirty(self.root)?;
        self.root = Child::Dirty(root);
        match self.delete_in(root, key, 0)? {
            Outcome::Empty => self.arena[root] = Work::Leaf(Entries::new()),
            Outcome::Single(c) => self.root = c,
            _ => {}
        }
        // Collapse root branches with a single child.
        while let Child::Dirty(r) = self.root
            && let Work::Branch { children, .. } = &self.arena[r]
            && children.len() == 1
        {
            self.root = children[0];
        }
        Ok(true)
    }

    fn delete_in(&mut self, i: usize, key: &[u8], depth: usize) -> Result<Outcome, StorageError> {
        if depth >= MAX_HEIGHT {
            return Err(corrupt("tree deeper than the supported maximum"));
        }
        let (ci, child) = match &mut self.arena[i] {
            Work::Leaf(e) => {
                e.remove(key);
                return Ok(if e.is_empty() {
                    Outcome::Empty
                } else {
                    Outcome::Same
                });
            }
            Work::Branch { seps, children } => {
                let ci = child_index(seps, key);
                (ci, children[ci])
            }
        };
        let d = self.dirty(child)?;
        let out = self.delete_in(d, key, depth + 1)?;
        let removed = {
            let Work::Branch { seps, children } = &mut self.arena[i] else {
                unreachable!()
            };
            match out {
                Outcome::Empty => {
                    children.remove(ci);
                    if ci > 0 {
                        seps.remove(ci - 1);
                    } else if !seps.is_empty() {
                        seps.remove(0);
                    }
                    if children.is_empty() {
                        return Ok(Outcome::Empty);
                    }
                    true
                }
                // The child branch kept one child: refer to that grandchild directly.
                Outcome::Single(c) => {
                    children[ci] = c;
                    false
                }
                _ => {
                    children[ci] = Child::Dirty(d);
                    false
                }
            }
        };
        if !removed {
            self.merge_leaf_if_small(i, ci)?;
        }
        // A branch left with one child is replaced by that child in its parent.
        let Work::Branch { children, .. } = &self.arena[i] else {
            unreachable!()
        };
        if children.len() == 1 {
            return Ok(Outcome::Single(children[0]));
        }
        Ok(Outcome::Same)
    }

    /// Merges child `ci` of branch `i` into an adjacent sibling if it is a small leaf and fits.
    fn merge_leaf_if_small(&mut self, i: usize, ci: usize) -> Result<(), StorageError> {
        let Work::Branch { children, .. } = &self.arena[i] else {
            unreachable!()
        };
        let Child::Dirty(d) = children[ci] else {
            return Ok(());
        };
        let Work::Leaf(e) = &self.arena[d] else {
            return Ok(());
        };
        if leaf_size(e) >= MERGE_BELOW || children.len() < 2 {
            return Ok(());
        }
        // Merge with the right sibling if there is one, otherwise with the left.
        let (l, r) = if ci + 1 < children.len() {
            (ci, ci + 1)
        } else {
            (ci - 1, ci)
        };
        let (lc, rc) = (children[l], children[r]);
        let (ld, rd) = (self.dirty(lc)?, self.dirty(rc)?);
        let (Work::Leaf(a), Work::Leaf(b)) = (&self.arena[ld], &self.arena[rd]) else {
            return Ok(());
        };
        if leaf_size(a) + leaf_size(b) - 5 > PAGE_PAYLOAD_MAX {
            return Ok(());
        }
        let mut merged = a.clone();
        merged.extend(b.clone());
        self.arena[ld] = Work::Leaf(merged);
        let Work::Branch { seps, children } = &mut self.arena[i] else {
            unreachable!()
        };
        children[l] = Child::Dirty(ld);
        children.remove(r);
        seps.remove(r - 1);
        Ok(())
    }

    /// Keys put or deleted by this transaction.
    /// Pages `finish` will write: the changed nodes reachable from the root.
    pub fn dirty_pages(&self) -> usize {
        fn count(arena: &[Work], c: Child) -> usize {
            match c {
                Child::Clean(_) => 0,
                Child::Dirty(i) => {
                    1 + match &arena[i] {
                        Work::Leaf(_) => 0,
                        Work::Branch { children, .. } => {
                            children.iter().map(|&ch| count(arena, ch)).sum()
                        }
                    }
                }
            }
        }
        count(&self.arena, self.root)
    }

    /// Committed pages replaced so far (see `replaced`).
    pub fn replaced(&self) -> &[PageRef] {
        &self.replaced
    }

    pub fn touched(&self) -> &[Vec<u8>] {
        &self.touched
    }

    /// Writes every touched node to fresh pages (children first) and returns the new root.
    /// Returns the existing root unchanged if nothing was touched.
    pub fn finish(self, sink: &mut dyn NodeSink) -> Result<PageRef, StorageError> {
        fn write(
            arena: &[Work],
            c: Child,
            sink: &mut dyn NodeSink,
        ) -> Result<PageRef, StorageError> {
            match c {
                Child::Clean(p) => Ok(p),
                Child::Dirty(i) => {
                    let node = match &arena[i] {
                        Work::Leaf(e) => Node::Leaf(e.clone()),
                        Work::Branch { seps, children } => Node::Branch {
                            seps: seps.clone(),
                            children: children
                                .iter()
                                .map(|&ch| write(arena, ch, sink))
                                .collect::<Result<_, _>>()?,
                        },
                    };
                    debug_assert!(node.size() <= PAGE_PAYLOAD_MAX);
                    sink.write(&node)
                }
            }
        }
        write(&self.arena, self.root, sink)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::collections::HashMap;

    /// In-memory page store for tree unit tests.
    #[derive(Default)]
    struct Mem {
        pages: RefCell<HashMap<PageRef, Arc<Node>>>,
        next: RefCell<u64>,
        writes: RefCell<u64>,
    }

    impl NodeSource for Mem {
        fn load(&self, at: PageRef) -> Result<Arc<Node>, StorageError> {
            self.pages
                .borrow()
                .get(&at)
                .cloned()
                .ok_or_else(|| corrupt("missing page"))
        }
    }

    struct Sink<'a>(&'a Mem, u64);
    impl NodeSink for Sink<'_> {
        fn write(&mut self, node: &Node) -> Result<PageRef, StorageError> {
            assert!(
                node.size() <= PAGE_PAYLOAD_MAX,
                "node too large: {}",
                node.size()
            );
            assert_eq!(Node::decode(&node.encode())?, *node);
            let mut n = self.0.next.borrow_mut();
            *n += 1;
            *self.0.writes.borrow_mut() += 1;
            let at = PageRef {
                page: *n,
                incarnation: self.1,
            };
            self.0.pages.borrow_mut().insert(at, Arc::new(node.clone()));
            Ok(at)
        }
    }

    fn empty(m: &Mem) -> PageRef {
        Sink(m, 0).write(&Node::Leaf(Entries::new())).unwrap()
    }

    fn commit(m: &Mem, root: PageRef, generation: u64, f: impl FnOnce(&mut TreeTxn)) -> PageRef {
        let mut t = TreeTxn::new(m, root);
        f(&mut t);
        t.finish(&mut Sink(m, generation)).unwrap()
    }

    fn all(m: &Mem, root: PageRef) -> Vec<(Vec<u8>, Vec<u8>)> {
        Cursor::new(m, root, b"", None, u64::MAX)
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap()
    }

    fn k(i: u32) -> Vec<u8> {
        format!("key-{i:08}").into_bytes()
    }

    #[test]
    fn inserts_grow_a_multilevel_tree_that_reads_back_in_order() {
        let m = Mem::default();
        let mut root = empty(&m);
        let mut model = Entries::new();
        for batch in 0..20u32 {
            root = commit(&m, root, batch as u64 + 1, |t| {
                for j in 0..500u32 {
                    let i = (j * 7919 + batch * 31) % 10_000;
                    t.put(k(i), vec![i as u8; 60]).unwrap();
                    model.insert(k(i), vec![i as u8; 60]);
                }
            });
        }
        assert!(
            height(&m, root).unwrap() >= 3,
            "height {}",
            height(&m, root).unwrap()
        );
        let got = all(&m, root);
        assert_eq!(got, model.into_iter().collect::<Vec<_>>());
    }

    #[test]
    fn range_cursor_respects_prefix_start_and_empty_ranges() {
        let m = Mem::default();
        let root = commit(&m, empty(&m), 1, |t| {
            for i in 0..3000u32 {
                let p = if i % 2 == 0 { b'a' } else { b'b' };
                let mut key = vec![p];
                key.extend(k(i));
                t.put(key, vec![1; 40]).unwrap();
            }
        });
        let b: Vec<_> = Cursor::new(&m, root, b"b", None, u64::MAX)
            .unwrap()
            .map(|r| r.unwrap().0)
            .collect();
        assert_eq!(b.len(), 1500);
        assert!(b.windows(2).all(|w| w[0] < w[1]));
        let mut start = b"b".to_vec();
        start.extend(k(2001));
        let tail: Vec<_> = Cursor::new(&m, root, b"b", Some(&start), u64::MAX)
            .unwrap()
            .map(|r| r.unwrap().0)
            .collect();
        assert_eq!(tail.first(), Some(&start));
        assert_eq!(tail.len(), 500);
        assert_eq!(
            Cursor::new(&m, root, b"c", None, u64::MAX).unwrap().count(),
            0
        );
        assert_eq!(
            Cursor::new(&m, root, b"a\xff", None, u64::MAX)
                .unwrap()
                .count(),
            0
        );
    }

    #[test]
    fn deletes_remove_leaves_shrink_the_root_and_allow_refill() {
        let m = Mem::default();
        let mut root = commit(&m, empty(&m), 1, |t| {
            for i in 0..4000u32 {
                t.put(k(i), vec![0; 50]).unwrap();
            }
        });
        assert!(height(&m, root).unwrap() >= 2);
        // First, last, middle, then a whole leaf's worth, then everything.
        root = commit(&m, root, 2, |t| {
            for i in [0, 3999, 2000] {
                assert!(t.delete(&k(i)).unwrap());
            }
            assert!(
                !t.delete(&k(0)).unwrap(),
                "deleting an absent key reports absence"
            );
            for i in 100..400 {
                t.delete(&k(i)).unwrap();
            }
        });
        let left: Vec<_> = all(&m, root).into_iter().map(|e| e.0).collect();
        let want: Vec<_> = (1..3999)
            .filter(|i| *i != 2000 && !(100..400).contains(i))
            .map(k)
            .collect();
        assert_eq!(left, want);
        root = commit(&m, root, 3, |t| {
            for i in 0..4000u32 {
                t.delete(&k(i)).unwrap();
            }
        });
        assert_eq!(
            height(&m, root).unwrap(),
            1,
            "empty tree must collapse to one leaf"
        );
        assert!(all(&m, root).is_empty());
        root = commit(&m, root, 4, |t| {
            for i in 0..2000u32 {
                t.put(k(i), vec![9; 50]).unwrap();
            }
        });
        assert_eq!(all(&m, root).len(), 2000);
    }

    #[test]
    fn unchanged_subtrees_are_shared_and_a_point_update_writes_one_path() {
        let m = Mem::default();
        let root = commit(&m, empty(&m), 1, |t| {
            for i in 0..20_000u32 {
                t.put(k(i), vec![0; 40]).unwrap();
            }
        });
        let h = height(&m, root).unwrap();
        let before = *m.writes.borrow();
        let root2 = commit(&m, root, 2, |t| t.put(k(12_345), vec![1; 40]).unwrap());
        let written = *m.writes.borrow() - before;
        assert_eq!(
            written as usize, h,
            "a point update must rewrite exactly one root-to-leaf path"
        );
        assert_eq!(get(&m, root2, &k(12_345)).unwrap(), Some(vec![1; 40]));
        assert_eq!(
            get(&m, root, &k(12_345)).unwrap(),
            Some(vec![0; 40]),
            "old generation changed"
        );
    }

    /// Deterministic workloads against an independent `BTreeMap` model, checking every committed
    /// generation (old roots stay readable and unchanged).
    #[test]
    fn workloads_match_an_independent_model_in_every_generation() {
        fn rng(s: &mut u64) -> u64 {
            *s ^= *s << 13;
            *s ^= *s >> 7;
            *s ^= *s << 17;
            *s
        }
        let workloads: Vec<(&str, Vec<u32>)> = vec![
            ("ascending", (0..6000).collect()),
            ("descending", (0..6000).rev().collect()),
            ("random", {
                let mut s = 0x2545_f491_4f6c_dd1d;
                (0..6000).map(|_| (rng(&mut s) % 4000) as u32).collect()
            }),
            ("duplicates", (0..6000).map(|i| i % 37).collect()),
        ];
        for (name, keys) in workloads {
            let m = Mem::default();
            let mut root = empty(&m);
            let mut model = Entries::new();
            let mut generations = Vec::new();
            let mut s = 99u64;
            for (g, chunk) in keys.chunks(700).enumerate() {
                root = commit(&m, root, g as u64 + 1, |t| {
                    for &i in chunk {
                        // Mix inserts, replacements (same key, new value) and deletions.
                        match rng(&mut s) % 10 {
                            0 | 1 => {
                                let present = model.remove(&k(i)).is_some();
                                assert_eq!(t.delete(&k(i)).unwrap(), present, "{name}: delete {i}");
                            }
                            _ => {
                                let v = format!("{name}-{i}-{g}").into_bytes();
                                t.put(k(i), v.clone()).unwrap();
                                model.insert(k(i), v);
                            }
                        }
                    }
                });
                generations.push((root, model.clone()));
            }
            for (gen_root, gen_model) in &generations {
                assert_eq!(
                    all(&m, *gen_root),
                    gen_model.clone().into_iter().collect::<Vec<_>>(),
                    "{name}"
                );
                for key in gen_model.keys().step_by(97) {
                    assert_eq!(
                        get(&m, *gen_root, key).unwrap().as_ref(),
                        gen_model.get(key),
                        "{name}"
                    );
                }
            }
        }
    }

    #[test]
    fn heavy_deletion_never_publishes_a_single_child_branch() {
        let m = Mem::default();
        let mut root = commit(&m, empty(&m), 1, |t| {
            for i in 0..20_000u32 {
                t.put(k(i), vec![1; 40]).unwrap();
            }
        });
        assert!(height(&m, root).unwrap() >= 3);
        // Delete in waves that empty most leaves under each branch but keep a few scattered keys.
        let mut survivors: Vec<u32> = (0..20_000).collect();
        for (g, keep) in [(2u64, 7u32), (3, 61), (4, 997), (5, 4999)] {
            root = commit(&m, root, g, |t| {
                for i in 0..20_000u32 {
                    if i % keep != 0 {
                        t.delete(&k(i)).unwrap();
                    }
                }
            });
            survivors.retain(|i| i % keep == 0);
            let want: Vec<_> = survivors.iter().copied().map(k).collect();
            let got: Vec<_> = all(&m, root).into_iter().map(|e| e.0).collect();
            assert_eq!(got, want, "after keeping every {keep}th key");
        }
    }

    #[test]
    fn uneven_entries_split_into_pages_that_fit() {
        let m = Mem::default();
        let entry = |n: usize, tag: u8| (vec![tag; 20], vec![0u8; n - 20]);
        // Key+value sizes 1200, 1200, 1500 in order, then 1800 inserted between them.
        let mut root = commit(&m, empty(&m), 1, |t| {
            for (n, tag) in [(1200, 1u8), (1200, 2), (1500, 4)] {
                let (key, v) = entry(n, tag);
                t.put(key, v).unwrap();
            }
        });
        root = commit(&m, root, 2, |t| {
            let (key, v) = entry(1800, 3);
            t.put(key, v).unwrap();
        });
        assert_eq!(all(&m, root).len(), 4);
    }

    #[test]
    fn branches_with_long_separators_split_into_pages_that_fit() {
        let m = Mem::default();
        let mut model = Entries::new();
        // A run of 1000-byte keys followed by many short keys: separators cluster by size, so a
        // split by child count would leave the long separators together on one side.
        let mut keys: Vec<Vec<u8>> = (0..60u32)
            .map(|i| {
                let mut key = format!("a{i:04}").into_bytes();
                key.resize(1000, b'L');
                key
            })
            .collect();
        keys.extend((0..4000u32).map(|i| format!("b{i:06}").into_bytes()));
        let mut root = empty(&m);
        for (g, chunk) in keys.chunks(400).enumerate() {
            root = commit(&m, root, g as u64 + 1, |t| {
                for key in chunk {
                    t.put(key.clone(), vec![3; 20]).unwrap();
                    model.insert(key.clone(), vec![3; 20]);
                }
            });
        }
        assert_eq!(all(&m, root), model.into_iter().collect::<Vec<_>>());
    }

    #[test]
    fn a_branch_split_keeps_both_halves_within_a_page() {
        let m = Mem::default();
        let mut t = TreeTxn::new(&m, empty(&m));
        // Ten children: four 1024-byte separators followed by five short ones (oversized branch).
        let mut seps: Vec<Vec<u8>> = (0..4u8)
            .map(|i| {
                let mut s = vec![b'a', i];
                s.resize(1024, b'L');
                s
            })
            .collect();
        seps.extend((0..5u8).map(|i| vec![b'b', i]));
        let children = (0..10)
            .map(|p| {
                Child::Clean(PageRef {
                    page: p,
                    incarnation: 1,
                })
            })
            .collect();
        t.arena.push(Work::Branch { seps, children });
        let Outcome::Split(_, right) = t.split_branch_if_needed(0) else {
            panic!("oversized branch did not split")
        };
        for i in [0, right] {
            let Work::Branch { seps, children } = &t.arena[i] else {
                panic!("branch")
            };
            assert!(
                branch_size(seps, children.len()) <= PAGE_PAYLOAD_MAX,
                "half {i} is {} bytes",
                branch_size(seps, children.len())
            );
            assert!(children.len() >= 2 && seps.len() == children.len() - 1);
        }
    }

    #[test]
    fn a_shared_subtree_graph_cannot_make_a_cursor_unbounded() {
        let m = Mem::default();
        // A crafted graph: 24 branches, each referring to the next one twice, over one leaf.
        let leaf = Sink(&m, 1)
            .write(&Node::Leaf(
                [(b"k".to_vec(), Vec::new())].into_iter().collect(),
            ))
            .unwrap();
        let mut top = leaf;
        for _ in 0..24 {
            top = Sink(&m, 1)
                .write(&Node::Branch {
                    seps: vec![b"k".to_vec()],
                    children: vec![top, top],
                })
                .unwrap();
        }
        let pages = *m.next.borrow();
        let r: Result<Vec<_>, _> = Cursor::new(&m, top, b"", None, pages).unwrap().collect();
        assert!(
            matches!(&r, Err(StorageError::Integrity(_))),
            "cursor walked a shared subtree: {:?}",
            r.map(|v| v.len())
        );
    }

    #[test]
    fn oversized_records_are_refused_explicitly() {
        let m = Mem::default();
        let mut t = TreeTxn::new(&m, empty(&m));
        // Each refusal names the limit it exceeds.
        let message = |r: Result<(), StorageError>| match r {
            Err(StorageError::InvalidRequest(m)) => m,
            other => panic!("expected InvalidRequest, got {other:?}"),
        };
        let m = message(t.put(vec![1; MAX_KEY + 1], vec![]));
        assert!(m.contains(&format!("{MAX_KEY}-byte key limit")), "{m}");
        let m = message(t.put(vec![1; 10], vec![0; MAX_ENTRY]));
        assert!(m.contains(&format!("{MAX_ENTRY}-byte entry limit")), "{m}");
        t.put(vec![1; MAX_KEY], vec![]).unwrap();
        t.put(vec![1; 10], vec![0; MAX_ENTRY - 10]).unwrap();
    }

    #[test]
    fn keys_at_the_key_limit_split_into_pages_that_fit() {
        // Keys of the largest accepted length: leaves hold two, branches two or three. The sink
        // checks that every node written fits a page.
        let m = Mem::default();
        let mut model = Entries::new();
        let key = |i: u32| {
            let mut k = format!("k{i:05}").into_bytes();
            k.resize(MAX_KEY, b'L');
            k
        };
        let mut root = empty(&m);
        for g in 0..6u32 {
            root = commit(&m, root, u64::from(g) + 1, |t| {
                for i in g * 40..(g + 1) * 40 {
                    t.put(key(i), vec![]).unwrap();
                    model.insert(key(i), vec![]);
                }
            });
        }
        assert!(
            height(&m, root).unwrap() >= 4,
            "branches never split under full-length separators"
        );
        assert_eq!(all(&m, root), model.clone().into_iter().collect::<Vec<_>>());
        // Removing every third key merges and collapses within pages too.
        root = commit(&m, root, 7, |t| {
            for i in (0..240).step_by(3) {
                assert!(t.delete(&key(i)).unwrap());
                model.remove(&key(i));
            }
        });
        assert_eq!(all(&m, root), model.into_iter().collect::<Vec<_>>());
    }

    #[test]
    fn decoder_rejects_malformed_nodes() {
        let leaf = Node::Leaf([(b"a".to_vec(), b"1".to_vec())].into_iter().collect()).encode();
        assert!(Node::decode(&leaf).is_ok());
        assert!(Node::decode(&leaf[..leaf.len() - 1]).is_err());
        let one_child = Node::Branch {
            seps: vec![],
            children: vec![PageRef {
                page: 1,
                incarnation: 1,
            }],
        }
        .encode();
        assert!(Node::decode(&one_child).is_err());
        let unsorted = Node::Branch {
            seps: vec![b"b".to_vec(), b"a".to_vec()],
            children: vec![
                PageRef {
                    page: 1,
                    incarnation: 1
                };
                3
            ],
        }
        .encode();
        assert!(Node::decode(&unsorted).is_err());
        assert!(Node::decode(&[9]).is_err());
    }
}
