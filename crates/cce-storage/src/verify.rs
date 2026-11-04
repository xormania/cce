//! Integrity inspection of an opened store (see [`crate::NativeReadOnly::verify`]).
//!
//! The structure level walks every metadata page reachable from the inspected publication record
//! and checks the records it finds and their relations. It reads the history entries (to recompute
//! attempt fingerprints) but no content bytes. Memory grows with the metadata: every record is held
//! for the cross-checks. Findings name records by key family and a hash prefix of the key, never by
//! stored bytes.

use std::collections::{BTreeMap, HashSet};

use cce_core::storage::{
    AttemptId, ChangeSet, ContentBinding, ContextId, RelationshipId, RelationshipKind,
    StagingToken, StorageError,
};
use sha2::{Digest, Sha256};

use crate::codec::key_components;
use crate::engine::{HISTORY_ENTRY_MAX, Inner};
use crate::format::{Descriptor, PageRef, Slot};
use crate::records::{
    AttemptValue, ContextValue, ExtentRef, P_ATTEMPT, P_CONTENT, P_CONTEXT, P_HISTORY, P_IN, P_OUT,
    P_REL, RelValue, decode_history, fingerprint, history_key, key,
};
use crate::tree::{MAX_HEIGHT, Node, NodeSource};
use crate::{Finding, Report, VerifyLevel};

pub(crate) fn run(inner: &Inner, _level: VerifyLevel) -> Result<Report, StorageError> {
    let d = inner.snapshot()?.desc;
    let mut v = Verifier {
        inner,
        desc: d,
        findings: Vec::new(),
        visited: HashSet::new(),
        leaf_depth: None,
        records: BTreeMap::new(),
    };
    let slots = v.slots()?;
    v.visit(d.root, 1, None, None);
    v.records_and_relations();
    v.history_and_attempts();
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

    /// What each publication slot holds; a valid record in the wrong slot is a finding.
    fn slots(&mut self) -> Result<[String; 2], StorageError> {
        let mut out = [String::new(), String::new()];
        for (i, s) in self.inner.publication_slots()?.into_iter().enumerate() {
            out[i] = match s {
                Slot::Empty => "empty".into(),
                Slot::Valid(d) => {
                    if Descriptor::slot(d.generation) != i as u64 {
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
                Slot::Unsupported(v) => format!("unsupported format version {v}"),
            };
        }
        Ok(out)
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
        for (k, val) in &records {
            let at = locate(k);
            let Some(&family) = k.first() else {
                self.find("key", at, "empty key");
                continue;
            };
            if family == P_HISTORY {
                if k.len() != 9 {
                    self.find("key", at.clone(), "history key is not a position");
                }
                self.extent(val, committed, &at);
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
                        if let Some(c) = cv.content
                            && !records.contains_key(&key(P_CONTENT, &[&c.digest.0]))
                        {
                            self.find("content-ref", at, "bound content has no manifest record");
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
                _ => {
                    self.extent(val, committed, &at);
                }
            }
        }
        self.records = records;
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
    /// it with the fingerprint recomputed from the entry; every attempt record points at an entry.
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
        let present: HashSet<u64> = positions.iter().copied().collect();
        for p in 1..=last {
            let at = format!("history position {p}");
            if !present.contains(&p) {
                self.find("history", at, "missing");
                continue;
            }
            let Some(x) = self
                .records
                .get(&history_key(p))
                .and_then(|v| ExtentRef::decode(v).ok())
            else {
                continue; // reported as a value finding
            };
            let entry = match self
                .inner
                .read_extent(&x, HISTORY_ENTRY_MAX, "history entry")
                .and_then(|b| decode_history(&b))
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
            // The fingerprint covers the canonical request; staging tokens are not part of it.
            let request = ChangeSet {
                changes: entry.changes,
                content: entry
                    .content
                    .into_iter()
                    .map(|content| ContentBinding {
                        content,
                        token: StagingToken(Vec::new()),
                    })
                    .collect(),
                evidence: entry.evidence,
            };
            let akey = key(P_ATTEMPT, &[entry.attempt.as_bytes()]);
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
                Some(a) if a.fingerprint != fingerprint(&entry.attempt, &request) => self.find(
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
