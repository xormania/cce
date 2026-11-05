//! Integrity inspection of an opened store (see [`crate::NativeReadOnly::verify`]).
//!
//! The structure level walks every metadata page reachable from the inspected publication record
//! and checks the records it finds and their relations. It reads the history entries (to recompute
//! attempt fingerprints) but no content bytes. Memory grows with the metadata: every record is held
//! for the cross-checks. Findings name records by key family and a hash prefix of the key, never by
//! stored bytes.

use std::collections::{BTreeMap, HashMap, HashSet};

use cce_core::storage::{
    AttemptId, ContentRef, ContextId, RelationshipId, RelationshipKind, StorageError,
};
use sha2::{Digest, Sha256};

use crate::codec::key_components;
use crate::engine::{HISTORY_ENTRY_MAX, Inner};
use crate::format::{Descriptor, FIRST_NODE_PAGE, PageRef, Slot, decode_slot, first_padding_byte};
use crate::records::{
    AttemptValue, ContentEntry, ContextValue, ExtentRef, Manifest, P_ATTEMPT, P_CONTENT, P_CONTEXT,
    P_HISTORY, P_IN, P_OUT, P_REL, P_RETIRED, RelValue, Retired, SEGMENT_MAX, decode_history,
    decode_retired, history_key, key, stored_fingerprint,
};
use crate::tree::{MAX_HEIGHT, Node, NodeSource};
use crate::{Finding, Report, VerifyLevel};

pub(crate) fn run(
    inner: &std::sync::Arc<Inner>,
    level: VerifyLevel,
) -> Result<Report, StorageError> {
    // A pinned state: nothing the walk reads can be reused under it.
    let (snap, _pin) = inner.pin_current()?;
    let d = snap.desc;
    let mut v = Verifier {
        inner,
        desc: d,
        findings: Vec::new(),
        visited: HashSet::new(),
        leaf_depth: None,
        records: BTreeMap::new(),
        content_bytes: 0,
        history_bindings: Vec::new(),
        retired_pages: Vec::new(),
        extents: Vec::new(),
    };
    let slots = v.slots()?;
    v.visit(d.root, 1, None, None);
    v.records_and_relations();
    v.history_and_attempts();
    if level == VerifyLevel::Scrub {
        v.content();
    }
    v.space();
    let mut findings = std::mem::take(&mut v.findings);
    findings.sort();
    findings.dedup();
    Ok(Report {
        generation: d.generation,
        history_position: d.history_pos,
        slots,
        diagnostics: inner.diagnostics.clone(),
        pages: v.visited.len() as u64,
        records: v.records.len() as u64,
        content_bytes: v.content_bytes,
        findings,
    })
}

struct Verifier<'a> {
    inner: &'a Inner,
    desc: Descriptor,
    findings: Vec<Finding>,
    visited: HashSet<u64>,
    leaf_depth: Option<usize>,
    records: BTreeMap<Vec<u8>, Vec<u8>>,
    content_bytes: u64,
    /// Content each history entry binds, by position (compared with the manifests by a scrub).
    history_bindings: Vec<(u64, ContentRef)>,
    /// Retired tree pages: page, retiring generation and the record's location.
    retired_pages: Vec<(u64, u64, String)>,
    /// Content-file extents the state uses or has retired, `(offset, len, location)`: manifests,
    /// history entries and retired extents, and at the scrub level the segments too.
    extents: Vec<(u64, u64, String)>,
}

/// Offset of the first non-zero padding byte of a publication slot whose page is `page`, where this
/// build knows what follows the record: after a valid record of this version, or anywhere in an
/// empty slot. A later format version may extend its record after this one's, and a damaged record
/// says nothing about what follows it, so the bytes after either are not checked.
pub(crate) fn slot_padding(page: &[u8], slot: &Slot) -> Option<usize> {
    match slot {
        Slot::Valid(_) | Slot::Empty => first_padding_byte(page, true),
        Slot::Invalid(_) | Slot::Unsupported(_) => None,
    }
}

/// A record's location: its key family and the first 8 bytes of the SHA-256 of its key.
fn locate(k: &[u8]) -> String {
    let family = match k.first() {
        Some(&P_CONTEXT) => "context",
        Some(&P_REL) => "relationship",
        Some(&P_OUT) => "outgoing index",
        Some(&P_IN) => "incoming index",
        Some(&P_HISTORY) => "history",
        Some(&P_ATTEMPT) => "attempt",
        Some(&P_CONTENT) => "content",
        Some(&P_RETIRED) => "retirement",
        _ => "unknown family",
    };
    let h = Sha256::digest(k);
    let prefix: String = h[..8].iter().map(|b| format!("{b:02x}")).collect();
    format!("{family} record {prefix}")
}

impl Verifier<'_> {
    fn find(
        &mut self,
        check: &'static str,
        location: impl Into<String>,
        detail: impl Into<String>,
    ) {
        self.findings.push(Finding {
            check,
            location: location.into(),
            detail: detail.into(),
        });
    }

    /// A finding for a non-zero byte at offset `at` of a page's zero padding.
    fn padding_finding(&mut self, at: usize, slot: bool, location: String) {
        let after = if slot { "record" } else { "payload" };
        self.find(
            "padding",
            location,
            format!("non-zero byte at offset {at} of the zero padding after the {after}"),
        );
    }

    /// What each publication slot holds; a valid record in the wrong slot is a finding, and so is a
    /// non-zero byte after a valid record or in an empty slot. A later format version may extend
    /// its record after this one's, so the bytes after a record this build cannot read are not
    /// checked.
    fn slots(&mut self) -> Result<[String; 2], StorageError> {
        let mut out = Vec::with_capacity(2);
        for i in 0..2u64 {
            let page = self.inner.meta_page(i)?;
            let slot = decode_slot(&page);
            if let Some(at) = slot_padding(&page, &slot) {
                self.padding_finding(at, true, format!("slot {i}"));
            }
            out.push(match slot {
                Slot::Empty => "empty".into(),
                Slot::Valid(d) => {
                    if Descriptor::slot(d.generation) != i {
                        self.find(
                            "slot",
                            format!("slot {i}"),
                            format!(
                                "holds generation {}, which belongs in slot {}",
                                d.generation,
                                Descriptor::slot(d.generation)
                            ),
                        );
                    }
                    format!("valid, generation {}", d.generation)
                }
                Slot::Invalid(why) => format!("invalid: {why}"),
                Slot::Unsupported(why) => format!("unsupported: {why}"),
            });
        }
        Ok([out.remove(0), out.remove(0)])
    }

    /// Walks the subtree at `at`, whose keys must lie in `[lo, hi)`.
    fn visit(&mut self, at: PageRef, depth: usize, lo: Option<Vec<u8>>, hi: Option<Vec<u8>>) {
        let here = format!("page {}", at.page);
        if depth > MAX_HEIGHT {
            self.find("depth", here, format!("deeper than {MAX_HEIGHT} levels"));
            return;
        }
        if !self.visited.insert(at.page) {
            self.find("shared-page", here, "reached more than once");
            return;
        }
        // Decoding checks the page's store, kind, address and incarnation; loading checks that it
        // lies within committed metadata with an incarnation no newer than the generation.
        let node = match self.inner.load(at) {
            Ok(n) => n,
            Err(e) => {
                self.find("page", here, e.to_string());
                return;
            }
        };
        match self.inner.meta_page(at.page) {
            Ok(page) => {
                if let Some(off) = first_padding_byte(&page, false) {
                    self.padding_finding(off, false, here.clone());
                }
            }
            Err(e) => self.find("page", here.clone(), e.to_string()),
        }
        let outside = |k: &[u8]| {
            lo.as_deref().is_some_and(|l| k < l) || hi.as_deref().is_some_and(|h| k >= h)
        };
        match &*node {
            Node::Leaf(entries) => {
                match self.leaf_depth {
                    None => self.leaf_depth = Some(depth),
                    Some(d) if d != depth => self.find(
                        "depth",
                        here.clone(),
                        format!("leaf at depth {depth}, other leaves at depth {d}"),
                    ),
                    Some(_) => {}
                }
                for (k, val) in entries {
                    if outside(k) {
                        self.find(
                            "separator",
                            here.clone(),
                            format!("{} lies outside its parent's range", locate(k)),
                        );
                    }
                    if self.records.insert(k.clone(), val.clone()).is_some() {
                        self.find("key", locate(k), "stored in more than one leaf");
                    }
                }
            }
            Node::Branch { seps, children } => {
                if seps.iter().any(|s| outside(s)) {
                    self.find(
                        "separator",
                        here,
                        "a separator lies outside its parent's range",
                    );
                }
                for (i, child) in children.iter().enumerate() {
                    let lo = if i == 0 {
                        lo.clone()
                    } else {
                        Some(seps[i - 1].clone())
                    };
                    let hi = if i + 1 == children.len() {
                        hi.clone()
                    } else {
                        Some(seps[i].clone())
                    };
                    self.visit(*child, depth + 1, lo, hi);
                }
            }
        }
    }

    /// Key families and their components, record values, relationship indexes in both
    /// directions, and content references.
    fn records_and_relations(&mut self) {
        let records = std::mem::take(&mut self.records);
        let committed = self.desc.content_len;
        // Contexts binding each live content entry, and the count each entry records.
        let mut bound: BTreeMap<Vec<u8>, u64> = BTreeMap::new();
        let mut live: Vec<(Vec<u8>, u64)> = Vec::new();
        for (k, val) in &records {
            let at = locate(k);
            // Keys and entries over the tree limits never get here: decoding their page refuses
            // them, which is reported as a page finding naming the limit.
            let Some(&family) = k.first() else {
                self.find("key", at, "empty key");
                continue;
            };
            if family == P_HISTORY {
                if k.len() != 9 {
                    self.find("key", at.clone(), "history key is not a position");
                }
                if let Some(x) = self.extent(val, committed, &at) {
                    self.extents.push((x.offset, x.len, at));
                }
                continue;
            }
            if family == P_RETIRED {
                self.retirement(k, val, committed, at);
                continue;
            }
            let parts = match key_components(&k[1..]) {
                Ok(p) => p,
                Err(e) => {
                    self.find("key", at, e.to_string());
                    continue;
                }
            };
            let arity = match family {
                P_CONTEXT | P_REL | P_ATTEMPT | P_CONTENT => 1,
                P_OUT | P_IN => 4,
                other => {
                    self.find("key", at, format!("unknown key family {other:#04x}"));
                    continue;
                }
            };
            if parts.len() != arity {
                self.find(
                    "key",
                    at,
                    format!("{} components, expected {arity}", parts.len()),
                );
                continue;
            }
            let id_ok = match family {
                P_CONTEXT => ContextId::new(parts[0].clone()).is_ok(),
                P_REL => RelationshipId::new(parts[0].clone()).is_ok(),
                P_ATTEMPT => AttemptId::new(parts[0].clone()).is_ok(),
                P_CONTENT => parts[0].len() == 32,
                _ => {
                    ContextId::new(parts[0].clone()).is_ok()
                        && RelationshipKind::new(parts[1].clone()).is_ok()
                        && ContextId::new(parts[2].clone()).is_ok()
                        && RelationshipId::new(parts[3].clone()).is_ok()
                }
            };
            if !id_ok {
                self.find("key", at, "invalid identifier in key");
                continue;
            }
            match family {
                P_CONTEXT => match ContextValue::decode(val) {
                    Ok(cv) => {
                        if let Some(c) = cv.content {
                            let ck = key(P_CONTENT, &[&c.digest.0]);
                            match records.get(&ck).map(|v| ContentEntry::decode(v)) {
                                None => self.find(
                                    "content-ref",
                                    at,
                                    "bound content has no manifest record",
                                ),
                                Some(Ok(ContentEntry::Removed)) => self.find(
                                    "content-ref",
                                    at,
                                    "bound content is no longer retained",
                                ),
                                Some(_) => *bound.entry(ck).or_insert(0) += 1,
                            }
                        }
                    }
                    Err(e) => self.find("value", at, e.to_string()),
                },
                P_REL => match RelValue::decode(val) {
                    Ok(rv) => {
                        for (name, index) in [
                            ("outgoing", rv.out_key(&parts[0])),
                            ("incoming", rv.in_key(&parts[0])),
                        ] {
                            if !records.contains_key(&index) {
                                self.find(
                                    "relationship-index",
                                    at.clone(),
                                    format!("no {name} index entry"),
                                );
                            }
                        }
                    }
                    Err(e) => self.find("value", at, e.to_string()),
                },
                P_OUT | P_IN => {
                    if !val.is_empty() {
                        self.find("value", at.clone(), "index entry has a value");
                    }
                    let (source, target) = if family == P_OUT {
                        (&parts[0], &parts[2])
                    } else {
                        (&parts[2], &parts[0])
                    };
                    let matches = records
                        .get(&key(P_REL, &[&parts[3]]))
                        .and_then(|v| RelValue::decode(v).ok())
                        .is_some_and(|rv| {
                            rv.kind == parts[1] && rv.source == *source && rv.target == *target
                        });
                    if !matches {
                        self.find("relationship-index", at, "no matching relationship record");
                    }
                }
                P_ATTEMPT => {
                    if let Err(e) = AttemptValue::decode(val) {
                        self.find("value", at, e.to_string());
                    }
                }
                _ => match ContentEntry::decode(val) {
                    Ok(ContentEntry::Live { manifest, refs }) => {
                        if self.within(&manifest, committed, &at) {
                            self.extents.push((manifest.offset, manifest.len, at));
                        }
                        live.push((k.clone(), refs));
                    }
                    Ok(ContentEntry::Removed) => {}
                    Err(e) => self.find("value", at, e.to_string()),
                },
            }
        }
        // A live content entry counts the contexts that bind it.
        for (k, refs) in live {
            let binding = bound.get(&k).copied().unwrap_or(0);
            if refs != binding {
                self.find(
                    "content-ref",
                    locate(&k),
                    format!("counts {refs} binding records, {binding} contexts bind it"),
                );
            }
        }
        self.records = records;
    }

    /// A retirement record: its key and value decode, its generation is no newer than the state's,
    /// and an extent lies within committed content. Pages are checked once all are known.
    fn retirement(&mut self, k: &[u8], val: &[u8], committed: u64, at: String) {
        match decode_retired(k, val) {
            Ok(r) => {
                let generation = match r {
                    Retired::Page { generation, .. } | Retired::Extent { generation, .. } => {
                        generation
                    }
                };
                if generation > self.desc.generation {
                    self.find(
                        "retirement",
                        at.clone(),
                        format!(
                            "retired at generation {generation}, after the state's {}",
                            self.desc.generation
                        ),
                    );
                }
                match r {
                    Retired::Page { page, .. } => self.retired_pages.push((page, generation, at)),
                    Retired::Extent { offset, len, .. } => {
                        let x = ExtentRef {
                            offset,
                            len,
                            digest: [0; 32],
                        };
                        if self.within(&x, committed, &at) {
                            self.extents.push((offset, len, at));
                        }
                    }
                }
            }
            Err(e) => self.find("key", at, e.to_string()),
        }
    }

    /// Whether `x` lies within committed content; a finding at `at` if not.
    fn within(&mut self, x: &ExtentRef, committed: u64, at: &str) -> bool {
        let inside = x
            .offset
            .checked_add(x.len)
            .is_some_and(|end| end <= committed);
        if !inside {
            self.find("value", at, "extent lies outside committed content");
        }
        inside
    }

    /// Space accounting of the inspected state: every retired page is unreachable, retired once,
    /// and a tree page; every tree page is reachable or retired; and the content-file extents the
    /// state uses or has retired do not overlap. (Content-file space a staging wrote but no commit
    /// bound is not accounted: abandoning it is a documented leak.)
    fn space(&mut self) {
        let end = self.desc.next_page;
        let mut retired: BTreeMap<u64, usize> = BTreeMap::new();
        for (page, _, at) in std::mem::take(&mut self.retired_pages) {
            if !(FIRST_NODE_PAGE..end).contains(&page) {
                self.find(
                    "retirement",
                    at,
                    format!("retired page {page} is not a tree page"),
                );
                continue;
            }
            if self.visited.contains(&page) {
                self.find(
                    "retirement",
                    at.clone(),
                    format!("retired page {page} is reachable"),
                );
            }
            let n = retired.entry(page).or_insert(0);
            *n += 1;
            if *n == 2 {
                self.find(
                    "retirement",
                    at,
                    format!("page {page} is retired more than once"),
                );
            }
        }
        for page in FIRST_NODE_PAGE..end {
            if !self.visited.contains(&page) && !retired.contains_key(&page) {
                self.find(
                    "page-coverage",
                    format!("page {page}"),
                    "neither reachable nor retired",
                );
            }
        }
        let mut extents = std::mem::take(&mut self.extents);
        extents.retain(|(_, len, _)| *len > 0);
        extents.sort();
        let mut reach: Option<(u64, String)> = None;
        for (offset, len, at) in extents {
            if let Some((until, other)) = &reach
                && offset < *until
            {
                self.find("extent", at.clone(), format!("overlaps {other}"));
            }
            let end = offset + len;
            if reach.as_ref().is_none_or(|(until, _)| end > *until) {
                reach = Some((end, at));
            }
        }
    }

    /// An extent record must decode and lie within committed content.
    fn extent(&mut self, val: &[u8], committed: u64, at: &str) -> Option<ExtentRef> {
        match ExtentRef::decode(val) {
            Ok(x)
                if x.offset
                    .checked_add(x.len)
                    .is_some_and(|end| end <= committed) =>
            {
                Some(x)
            }
            Ok(_) => {
                self.find("value", at, "extent lies outside committed content");
                None
            }
            Err(e) => {
                self.find("value", at, e.to_string());
                None
            }
        }
    }

    /// History positions are exactly 1..=history_pos; each entry's attempt record points back to
    /// it with the fingerprint recomputed from the entry; every attempt record points at an entry
    /// that records that attempt. Missing positions are reported once per gap, so the findings
    /// grow with the records present, not with the position the publication record claims.
    fn history_and_attempts(&mut self) {
        let last = self.desc.history_pos;
        let positions: Vec<u64> = self
            .records
            .keys()
            .filter(|k| k.first() == Some(&P_HISTORY) && k.len() == 9)
            .map(|k| u64::from_be_bytes(k[1..9].try_into().unwrap_or_default()))
            .collect();
        for p in &positions {
            if *p == 0 || *p > last {
                self.find(
                    "history",
                    format!("history position {p}"),
                    format!("beyond the committed position {last}"),
                );
            }
        }
        let mut held: Vec<u64> = positions
            .iter()
            .copied()
            .filter(|p| (1..=last).contains(p))
            .collect();
        held.sort_unstable();
        held.dedup();
        // The next position the history must hold (`None` once past the largest position).
        let mut expected = Some(1u64);
        for &p in &held {
            if let Some(e) = expected
                && e < p
            {
                self.missing_history(e, p - 1);
            }
            expected = p.checked_add(1);
        }
        if let Some(e) = expected
            && e <= last
        {
            self.missing_history(e, last);
        }
        let present: HashSet<u64> = held.iter().copied().collect();
        // The attempt record key each readable entry names.
        let mut owners: HashMap<u64, Vec<u8>> = HashMap::new();
        for &p in &held {
            let at = format!("history position {p}");
            let Some(x) = self
                .records
                .get(&history_key(p))
                .and_then(|v| ExtentRef::decode(v).ok())
            else {
                continue; // reported as a value finding
            };
            let (entry, recorded) = match self
                .inner
                .read_extent(&x, HISTORY_ENTRY_MAX, "history entry")
                .and_then(|b| Ok((decode_history(&b)?, stored_fingerprint(&b)?.1)))
            {
                Ok(e) => e,
                Err(e) => {
                    self.find("history", at, e.to_string());
                    continue;
                }
            };
            if entry.position.0 != p {
                self.find(
                    "history",
                    at.clone(),
                    format!("entry records position {}", entry.position.0),
                );
            }
            // Content a history entry binds stays committed: its manifest record must exist (and a
            // scrub compares it with the manifest).
            for c in &entry.content {
                if !self.records.contains_key(&key(P_CONTENT, &[&c.digest.0])) {
                    self.find(
                        "content-ref",
                        at.clone(),
                        "history entry binds content that has no manifest record",
                    );
                }
                self.history_bindings.push((p, *c));
            }
            let akey = key(P_ATTEMPT, &[entry.attempt.as_bytes()]);
            owners.insert(p, akey.clone());
            match self
                .records
                .get(&akey)
                .and_then(|v| AttemptValue::decode(v).ok())
            {
                None => self.find("attempt", at, "the entry's attempt has no attempt record"),
                Some(a) if a.position != p => self.find(
                    "attempt",
                    locate(&akey),
                    format!("points at position {}, its entry is at {p}", a.position),
                ),
                // The commit fingerprinted the request bytes it stored, in whichever encoding.
                Some(a) if a.fingerprint != recorded => self.find(
                    "attempt",
                    locate(&akey),
                    "fingerprint differs from its history entry",
                ),
                Some(_) => {}
            }
        }
        let attempts: Vec<(Vec<u8>, u64)> = self
            .records
            .iter()
            .filter(|(k, _)| k.first() == Some(&P_ATTEMPT))
            .filter_map(|(k, v)| {
                AttemptValue::decode(v)
                    .ok()
                    .map(|a| (k.clone(), a.position))
            })
            .collect();
        for (k, p) in attempts {
            if p == 0 || p > last || !present.contains(&p) {
                self.find(
                    "attempt",
                    locate(&k),
                    format!("points at missing history position {p}"),
                );
            } else if owners.get(&p).is_some_and(|owner| *owner != k) {
                self.find(
                    "attempt",
                    locate(&k),
                    format!("points at position {p}, whose entry records another attempt"),
                );
            }
        }
    }

    /// Reports history positions `first..=last` as missing, as one finding.
    fn missing_history(&mut self, first: u64, last: u64) {
        let at = if first == last {
            format!("history position {first}")
        } else {
            format!("history positions {first} to {last}")
        };
        self.find("history", at, "missing");
    }
}

impl Verifier<'_> {
    /// Every committed content stream: its manifest matches its record, every segment matches its
    /// digest, the stream matches the content's digest and length, and every context binding
    /// matches its manifest. Each manifest and segment goes through the verified read.
    fn content(&mut self) {
        let committed = self.desc.content_len;
        let mut manifests: BTreeMap<Vec<u8>, ContentRef> = BTreeMap::new();
        let records: Vec<(Vec<u8>, Vec<u8>)> = self
            .records
            .iter()
            .filter(|(k, _)| k.first() == Some(&P_CONTENT))
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        for (k, val) in records {
            let at = locate(&k);
            let Some(digest) = key_components(&k[1..])
                .ok()
                .and_then(|p| p.into_iter().next())
            else {
                continue; // reported as a key finding
            };
            // Content no record uses any more is not retained and never read.
            let Ok(ContentEntry::Live { manifest: x, .. }) = ContentEntry::decode(&val) else {
                continue; // removed, or reported as a value finding
            };
            let m = match self
                .inner
                .read_extent(
                    &x,
                    Manifest::encoded_len(u32::MAX as usize),
                    "content manifest",
                )
                .and_then(|b| Manifest::decode(&b))
            {
                Ok(m) => m,
                Err(e) => {
                    self.find("content", at, e.to_string());
                    continue;
                }
            };
            if m.content.digest.0.as_slice() != digest.as_slice() {
                self.find("content", at, "manifest describes other content");
                continue;
            }
            let mut stream = Sha256::new();
            let mut intact = true;
            for (i, seg) in m.segments.iter().enumerate() {
                if seg
                    .offset
                    .checked_add(seg.len)
                    .is_none_or(|end| end > committed)
                {
                    self.find(
                        "content",
                        at.clone(),
                        format!("segment {i} lies outside committed content"),
                    );
                    intact = false;
                    break;
                }
                self.extents
                    .push((seg.offset, seg.len, format!("{at} segment {i}")));
                match self.inner.read_extent(seg, SEGMENT_MAX, "content segment") {
                    Ok(bytes) => {
                        stream.update(&bytes);
                        self.content_bytes += bytes.len() as u64;
                    }
                    Err(e) => {
                        self.find("content", at.clone(), format!("segment {i}: {e}"));
                        intact = false;
                        break;
                    }
                }
            }
            if intact && <[u8; 32]>::from(stream.finalize()) != m.content.digest.0 {
                self.find(
                    "content",
                    at,
                    "stream digest differs from the content's digest",
                );
                continue;
            }
            manifests.insert(digest, m.content);
        }
        let bindings: Vec<(Vec<u8>, ContentRef)> = self
            .records
            .iter()
            .filter(|(k, _)| k.first() == Some(&P_CONTEXT))
            .filter_map(|(k, v)| {
                ContextValue::decode(v)
                    .ok()
                    .and_then(|cv| cv.content)
                    .map(|c| (k.clone(), c))
            })
            .collect();
        for (k, c) in bindings {
            if let Some(m) = manifests.get(c.digest.0.as_slice())
                && *m != c
            {
                self.find(
                    "content",
                    locate(&k),
                    "binding differs from its content's manifest",
                );
            }
        }
        for (p, c) in std::mem::take(&mut self.history_bindings) {
            if let Some(m) = manifests.get(c.digest.0.as_slice())
                && *m != c
            {
                self.find(
                    "content",
                    format!("history position {p}"),
                    "history binding differs from its content's manifest",
                );
            }
        }
    }
}

/// A JSON string literal for `s`.
pub(crate) fn json_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}
