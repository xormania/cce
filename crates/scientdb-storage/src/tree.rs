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

use scientdb_kernel::storage::StorageError;

use crate::codec::{Dec, Enc, corrupt};
use crate::format::{PAGE_PAYLOAD_MAX, PageRef};

/// Sorted key → value entries of one leaf.
pub(crate) type Entries = BTreeMap<Vec<u8>, Vec<u8>>;
/// One key → value entry.
pub(crate) type KeyValue = (Vec<u8>, Vec<u8>);

/// Largest key accepted into the tree.
pub(crate) const MAX_KEY: usize = 1024;
/// Largest key + value accepted into one leaf entry (at least two entries fit per leaf).
pub(crate) const MAX_ENTRY: usize = 1800;
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
                let mut seps: Vec<Vec<u8>> = Vec::with_capacity(n - 1);
                let mut children = Vec::with_capacity(n);
                for i in 0..n {
                    if i > 0 {
                        let sl = d.u16()? as usize;
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
}

impl<'a> Cursor<'a> {
    /// Keys starting with `prefix`, from `start` (which must itself start with `prefix`) onward.
    pub fn new(
        src: &'a dyn NodeSource,
        root: PageRef,
        prefix: &[u8],
        start: Option<&[u8]>,
    ) -> Result<Self, StorageError> {
        let next = start.unwrap_or(prefix).to_vec();
        let mut c = Cursor {
            src,
            stack: Vec::new(),
            leaf: None,
            next,
            prefix: prefix.to_vec(),
            done: false,
        };
        c.descend(root)?;
        Ok(c)
    }

    fn descend(&mut self, mut at: PageRef) -> Result<(), StorageError> {
        loop {
            if self.stack.len() >= MAX_HEIGHT {
                return Err(corrupt("tree deeper than the supported maximum"));
            }
            let node = self.src.load(at)?;
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
                    let n = self.src.load(at)?;
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
}

enum Outcome {
    Same,
    Split(Vec<u8>, usize),
    Empty,
}

impl<'a> TreeTxn<'a> {
    pub fn new(src: &'a dyn NodeSource, root: PageRef) -> Self {
        Self {
            src,
            arena: Vec::new(),
            root: Child::Clean(root),
        }
    }

    fn dirty(&mut self, c: Child) -> Result<usize, StorageError> {
        match c {
            Child::Dirty(i) => Ok(i),
            Child::Clean(at) => {
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
        if key.len() > MAX_KEY || key.len() + value.len() > MAX_ENTRY {
            return Err(StorageError::InvalidRequest(format!(
                "record too large for metadata: key {} + value {} bytes (limit {MAX_ENTRY})",
                key.len(),
                value.len()
            )));
        }
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
        // Split at the first key where the left half reaches half the encoded size, keeping at
        // least one entry on each side.
        let mut acc = 5;
        let mut split_key = None;
        for (idx, (k, v)) in e.iter().enumerate() {
            if idx > 0 && acc >= total / 2 {
                split_key = Some(k.clone());
                break;
            }
            acc += 4 + k.len() + v.len();
        }
        let k = split_key.unwrap_or_else(|| e.keys().next_back().expect("entries").clone());
        let right = e.split_off(&k);
        let sep = right.keys().next().expect("right half").clone();
        self.arena.push(Work::Leaf(right));
        Outcome::Split(sep, self.arena.len() - 1)
    }

    fn split_branch_if_needed(&mut self, i: usize) -> Outcome {
        let Work::Branch { seps, children } = &mut self.arena[i] else {
            unreachable!()
        };
        if branch_size(seps, children.len()) <= PAGE_PAYLOAD_MAX {
            return Outcome::Same;
        }
        let m = children.len() / 2;
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
        let root = self.dirty(self.root)?;
        self.root = Child::Dirty(root);
        if let Outcome::Empty = self.delete_in(root, key, 0)? {
            self.arena[root] = Work::Leaf(Entries::new());
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
        {
            let Work::Branch { seps, children } = &mut self.arena[i] else {
                unreachable!()
            };
            children[ci] = Child::Dirty(d);
            if let Outcome::Empty = out {
                children.remove(ci);
                if ci > 0 {
                    seps.remove(ci - 1);
                } else if !seps.is_empty() {
                    seps.remove(0);
                }
                if children.is_empty() {
                    return Ok(Outcome::Empty);
                }
                return Ok(Outcome::Same);
            }
        }
        self.merge_leaf_if_small(i, ci)?;
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
        Cursor::new(m, root, b"", None)
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
        let b: Vec<_> = Cursor::new(&m, root, b"b", None)
            .unwrap()
            .map(|r| r.unwrap().0)
            .collect();
        assert_eq!(b.len(), 1500);
        assert!(b.windows(2).all(|w| w[0] < w[1]));
        let mut start = b"b".to_vec();
        start.extend(k(2001));
        let tail: Vec<_> = Cursor::new(&m, root, b"b", Some(&start))
            .unwrap()
            .map(|r| r.unwrap().0)
            .collect();
        assert_eq!(tail.first(), Some(&start));
        assert_eq!(tail.len(), 500);
        assert_eq!(Cursor::new(&m, root, b"c", None).unwrap().count(), 0);
        assert_eq!(Cursor::new(&m, root, b"a\xff", None).unwrap().count(), 0);
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

    #[test]
    fn oversized_records_are_refused_explicitly() {
        let m = Mem::default();
        let mut t = TreeTxn::new(&m, empty(&m));
        assert!(t.put(vec![1; MAX_KEY + 1], vec![]).is_err());
        assert!(t.put(vec![1; 10], vec![0; MAX_ENTRY]).is_err());
        t.put(vec![1; 10], vec![0; MAX_ENTRY - 10]).unwrap();
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
