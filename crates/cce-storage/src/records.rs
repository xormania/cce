//! Logical records and their encodings inside the metadata tree.
//!
//! One ordered key space holds several record families, distinguished by a one-byte prefix:
//!
//! | prefix | key components                          | value                     |
//! |--------|------------------------------------------|---------------------------|
//! | `0x01` | context id                               | [`ContextValue`]          |
//! | `0x02` | relationship id                          | [`RelValue`]              |
//! | `0x03` | source, kind, target, relationship id    | empty (outgoing index)    |
//! | `0x04` | target, kind, source, relationship id    | empty (incoming index)    |
//! | `0x05` | history position (u64 big-endian)        | [`ExtentRef`] of entry    |
//! | `0x06` | attempt id                               | [`AttemptValue`]          |
//! | `0x07` | content digest                           | [`ExtentRef`] of its [`Manifest`] |
//!
//! Components use the order-preserving encoding in `codec::key_component`.

use cce_core::storage::{
    AttemptId, Change, ChangeSet, ContentDigest, ContentRef, ContextId, Evidence, HistoryEntry,
    HistoryPosition, Precondition, RelationshipId, RelationshipKind, Revision, StorageError,
};
use sha2::{Digest, Sha256};

use crate::codec::{Dec, Enc, corrupt, key_component};

pub(crate) const P_CONTEXT: u8 = 0x01;
pub(crate) const P_REL: u8 = 0x02;
pub(crate) const P_OUT: u8 = 0x03;
pub(crate) const P_IN: u8 = 0x04;
pub(crate) const P_HISTORY: u8 = 0x05;
pub(crate) const P_ATTEMPT: u8 = 0x06;
pub(crate) const P_CONTENT: u8 = 0x07;
/// Retirement records: space a published generation no longer reaches.
pub(crate) const P_RETIRED: u8 = 0x08;

/// Kind byte of a retired metadata page in a retirement key.
pub(crate) const RETIRED_PAGE: u8 = 0;

/// Version tag of the canonical request encoding used for attempt fingerprints.
const REQUEST_ENCODING_V1: u8 = 1;

pub(crate) fn key(prefix: u8, parts: &[&[u8]]) -> Vec<u8> {
    let mut k = vec![prefix];
    for p in parts {
        key_component(&mut k, p);
    }
    k
}

/// Key recording that metadata page `page` is unreachable from generation `generation` on. Keys
/// sort by retiring generation. The value is the retired page's incarnation.
pub(crate) fn retired_page_key(generation: u64, page: u64) -> Vec<u8> {
    let mut k = vec![P_RETIRED];
    k.extend_from_slice(&generation.to_be_bytes());
    k.push(RETIRED_PAGE);
    k.extend_from_slice(&page.to_be_bytes());
    k
}

/// Parses a retired-page record: `(retiring generation, page, incarnation)`.
pub(crate) fn decode_retired_page(
    key: &[u8],
    value: &[u8],
) -> Result<(u64, u64, u64), StorageError> {
    let bad = || corrupt("retirement record");
    let [P_RETIRED, rest @ ..] = key else {
        return Err(bad());
    };
    let (generation, rest) = rest.split_first_chunk::<8>().ok_or_else(bad)?;
    let [RETIRED_PAGE, page @ ..] = rest else {
        return Err(bad());
    };
    let page: [u8; 8] = page.try_into().map_err(|_| bad())?;
    let incarnation: [u8; 8] = value.try_into().map_err(|_| bad())?;
    Ok((
        u64::from_be_bytes(*generation),
        u64::from_be_bytes(page),
        u64::from_le_bytes(incarnation),
    ))
}

pub(crate) fn history_key(pos: u64) -> Vec<u8> {
    let mut k = vec![P_HISTORY];
    k.extend_from_slice(&pos.to_be_bytes());
    k
}

/// Location and integrity of bytes in the content file.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ExtentRef {
    pub offset: u64,
    pub len: u64,
    pub digest: [u8; 32],
}

impl ExtentRef {
    fn enc(&self, e: &mut Enc) {
        e.u64(self.offset).u64(self.len).raw(&self.digest);
    }
    fn dec(d: &mut Dec) -> Result<Self, StorageError> {
        Ok(Self {
            offset: d.u64()?,
            len: d.u64()?,
            digest: d.array()?,
        })
    }
    pub fn encode(&self) -> Vec<u8> {
        let mut e = Enc::default();
        self.enc(&mut e);
        e.buf
    }
    pub fn decode(b: &[u8]) -> Result<Self, StorageError> {
        let mut d = Dec::new(b, "extent ref");
        let r = Self::dec(&mut d)?;
        d.finish()?;
        Ok(r)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ContextValue {
    pub revision: u64,
    pub body: Vec<u8>,
    /// Logical reference to the bound content; its physical layout lives under `P_CONTENT`.
    pub content: Option<ContentRef>,
}

impl ContextValue {
    pub fn encode(&self) -> Vec<u8> {
        let mut e = Enc::default();
        e.u64(self.revision).bytes(&self.body);
        match &self.content {
            None => {
                e.u8(0);
            }
            Some(c) => {
                e.u8(1).raw(&c.digest.0).u64(c.len);
            }
        }
        e.buf
    }
    pub fn decode(b: &[u8]) -> Result<Self, StorageError> {
        let mut d = Dec::new(b, "context record");
        let revision = d.u64()?;
        let body = d.bytes()?.to_vec();
        let content = match d.u8()? {
            0 => None,
            1 => Some(ContentRef {
                digest: ContentDigest(d.array()?),
                len: d.u64()?,
            }),
            t => return Err(corrupt(format!("context record: content tag {t}"))),
        };
        d.finish()?;
        Ok(Self {
            revision,
            body,
            content,
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct RelValue {
    pub revision: u64,
    pub kind: Vec<u8>,
    pub source: Vec<u8>,
    pub target: Vec<u8>,
}

impl RelValue {
    pub fn encode(&self) -> Vec<u8> {
        let mut e = Enc::default();
        e.u64(self.revision)
            .bytes(&self.kind)
            .bytes(&self.source)
            .bytes(&self.target);
        e.buf
    }
    pub fn decode(b: &[u8]) -> Result<Self, StorageError> {
        let mut d = Dec::new(b, "relationship record");
        let v = Self {
            revision: d.u64()?,
            kind: d.bytes()?.to_vec(),
            source: d.bytes()?.to_vec(),
            target: d.bytes()?.to_vec(),
        };
        d.finish()?;
        Ok(v)
    }
    pub fn out_key(&self, id: &[u8]) -> Vec<u8> {
        key(P_OUT, &[&self.source, &self.kind, &self.target, id])
    }
    pub fn in_key(&self, id: &[u8]) -> Vec<u8> {
        key(P_IN, &[&self.target, &self.kind, &self.source, id])
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct AttemptValue {
    pub position: u64,
    pub fingerprint: [u8; 32],
}

impl AttemptValue {
    pub fn encode(&self) -> Vec<u8> {
        let mut e = Enc::default();
        e.u64(self.position).raw(&self.fingerprint);
        e.buf
    }
    pub fn decode(b: &[u8]) -> Result<Self, StorageError> {
        let mut d = Dec::new(b, "attempt receipt");
        let v = Self {
            position: d.u64()?,
            fingerprint: d.array()?,
        };
        d.finish()?;
        Ok(v)
    }
}

fn enc_expect(e: &mut Enc, p: &Precondition) {
    match p {
        Precondition::Any => {
            e.u8(0);
        }
        Precondition::Absent => {
            e.u8(1);
        }
        Precondition::AtRevision(r) => {
            e.u8(2).u64(r.0);
        }
    }
}

fn dec_expect(d: &mut Dec) -> Result<Precondition, StorageError> {
    Ok(match d.u8()? {
        0 => Precondition::Any,
        1 => Precondition::Absent,
        2 => Precondition::AtRevision(Revision(d.u64()?)),
        t => return Err(corrupt(format!("precondition tag {t}"))),
    })
}

fn dec_id<T>(d: &mut Dec, f: fn(Vec<u8>) -> Result<T, StorageError>) -> Result<T, StorageError> {
    f(d.bytes()?.to_vec()).map_err(|e| corrupt(format!("stored identifier: {e}")))
}

/// Canonical encoding of the logical request: changes in order, bound content by digest and
/// length, and evidence in order. Staging tokens and physical locations are excluded, so the same
/// logical request restaged elsewhere has the same encoding.
pub(crate) fn encode_request(changes: &ChangeSet) -> Vec<u8> {
    let mut e = Enc::default();
    e.u8(REQUEST_ENCODING_V1);
    e.u32(changes.changes.len() as u32);
    for c in &changes.changes {
        match c {
            Change::PutContext {
                id,
                expect,
                body,
                content,
            } => {
                e.u8(1).bytes(id.as_bytes());
                enc_expect(&mut e, expect);
                e.bytes(body);
                match content {
                    None => e.u8(0),
                    Some(i) => e.u8(1).u64(*i as u64),
                };
            }
            Change::RemoveContext { id, expect } => {
                e.u8(2).bytes(id.as_bytes());
                enc_expect(&mut e, expect);
            }
            Change::PutRelationship {
                id,
                expect,
                kind,
                source,
                target,
            } => {
                e.u8(3).bytes(id.as_bytes());
                enc_expect(&mut e, expect);
                e.bytes(kind.as_bytes())
                    .bytes(source.as_bytes())
                    .bytes(target.as_bytes());
            }
            Change::RemoveRelationship { id, expect } => {
                e.u8(4).bytes(id.as_bytes());
                enc_expect(&mut e, expect);
            }
        }
    }
    e.u32(changes.content.len() as u32);
    for b in &changes.content {
        e.raw(&b.content.digest.0).u64(b.content.len);
    }
    e.u32(changes.evidence.len() as u32);
    for ev in &changes.evidence {
        e.bytes(&ev.label).bytes(&ev.bytes);
    }
    debug_assert_eq!(e.buf.len() as u64, encoded_request_len(changes));
    e.buf
}

/// Exact length of `encode_request(changes)`, computed without encoding (checked in debug builds).
pub(crate) fn encoded_request_len(changes: &ChangeSet) -> u64 {
    let field = |len: usize| 4 + len as u64;
    let expect = |p: &Precondition| match p {
        Precondition::AtRevision(_) => 9,
        Precondition::Any | Precondition::Absent => 1,
    };
    let mut n: u64 = 1 + 4;
    for c in &changes.changes {
        n += 1 + match c {
            Change::PutContext {
                id,
                expect: p,
                body,
                content,
            } => {
                field(id.as_bytes().len())
                    + expect(p)
                    + field(body.len())
                    + if content.is_some() { 9 } else { 1 }
            }
            Change::RemoveContext { id, expect: p } => field(id.as_bytes().len()) + expect(p),
            Change::PutRelationship {
                id,
                expect: p,
                kind,
                source,
                target,
            } => {
                field(id.as_bytes().len())
                    + expect(p)
                    + field(kind.as_bytes().len())
                    + field(source.as_bytes().len())
                    + field(target.as_bytes().len())
            }
            Change::RemoveRelationship { id, expect: p } => field(id.as_bytes().len()) + expect(p),
        };
    }
    n += 4 + changes.content.len() as u64 * (32 + 8);
    n += 4;
    for ev in &changes.evidence {
        n += field(ev.label.len()) + field(ev.bytes.len());
    }
    n
}

/// Checks that every variable-length field and list of a request fits the encoding's 32-bit length
/// prefixes (`limit` is at most `u32::MAX`), so encoding never truncates a length or panics.
pub(crate) fn check_request_bounds(
    changes: &ChangeSet,
    limit: u64,
    total_limit: u64,
) -> Result<(), StorageError> {
    let limit = limit.min(u32::MAX as u64);
    let total_limit = total_limit.min(u32::MAX as u64);
    let check = |what: &str, len: usize| {
        if len as u64 > limit {
            Err(StorageError::InvalidRequest(format!(
                "{what} of {len} exceeds the limit of {limit}"
            )))
        } else {
            Ok(())
        }
    };
    check("change list", changes.changes.len())?;
    check("content binding list", changes.content.len())?;
    check("evidence list", changes.evidence.len())?;
    for c in &changes.changes {
        if let Change::PutContext { body, .. } = c {
            check("record body", body.len())?;
        }
    }
    for ev in &changes.evidence {
        check("evidence label", ev.label.len())?;
        check("evidence bytes", ev.bytes.len())?;
    }
    // The history entry stores the whole encoded request as one length-prefixed field.
    let total = encoded_request_len(changes);
    if total > total_limit {
        return Err(StorageError::InvalidRequest(format!(
            "encoded request of {total} bytes exceeds the limit of {total_limit}"
        )));
    }
    Ok(())
}

/// Fingerprint binding an attempt identity to its exact logical request.
pub(crate) fn fingerprint(attempt: &AttemptId, changes: &ChangeSet) -> [u8; 32] {
    let mut h = Sha256::new();
    h.update(b"cce.attempt.v1\0");
    h.update((attempt.as_bytes().len() as u32).to_le_bytes());
    h.update(attempt.as_bytes());
    h.update(encode_request(changes));
    h.finalize().into()
}

/// Encodes a retained history entry (stored as a content extent).
pub(crate) fn encode_history(position: u64, attempt: &AttemptId, changes: &ChangeSet) -> Vec<u8> {
    let mut e = Enc::default();
    e.u64(position)
        .bytes(attempt.as_bytes())
        .bytes(&encode_request(changes));
    e.buf
}

pub(crate) fn decode_history(b: &[u8]) -> Result<HistoryEntry, StorageError> {
    let mut d = Dec::new(b, "history entry");
    let position = HistoryPosition(d.u64()?);
    let attempt = dec_id(&mut d, AttemptId::new)?;
    let req = d.bytes()?;
    d.finish()?;
    let mut d = Dec::new(req, "history request");
    if d.u8()? != REQUEST_ENCODING_V1 {
        return Err(StorageError::Unsupported("history request encoding".into()));
    }
    let n = d.count(2)?;
    let mut changes = Vec::with_capacity(n);
    for _ in 0..n {
        changes.push(match d.u8()? {
            1 => {
                let id = dec_id(&mut d, ContextId::new)?;
                let expect = dec_expect(&mut d)?;
                let body = d.bytes()?.to_vec();
                let content = match d.u8()? {
                    0 => None,
                    1 => Some(usize::try_from(d.u64()?).map_err(|_| corrupt("content index"))?),
                    t => return Err(corrupt(format!("history content tag {t}"))),
                };
                Change::PutContext {
                    id,
                    expect,
                    body,
                    content,
                }
            }
            2 => Change::RemoveContext {
                id: dec_id(&mut d, ContextId::new)?,
                expect: dec_expect(&mut d)?,
            },
            3 => Change::PutRelationship {
                id: dec_id(&mut d, RelationshipId::new)?,
                expect: dec_expect(&mut d)?,
                kind: dec_id(&mut d, RelationshipKind::new)?,
                source: dec_id(&mut d, ContextId::new)?,
                target: dec_id(&mut d, ContextId::new)?,
            },
            4 => Change::RemoveRelationship {
                id: dec_id(&mut d, RelationshipId::new)?,
                expect: dec_expect(&mut d)?,
            },
            t => return Err(corrupt(format!("history change tag {t}"))),
        });
    }
    let nc = d.count(40)?;
    let mut content = Vec::with_capacity(nc);
    for _ in 0..nc {
        content.push(ContentRef {
            digest: ContentDigest(d.array()?),
            len: d.u64()?,
        });
    }
    if changes
        .iter()
        .any(|c| matches!(c, Change::PutContext { content: Some(i), .. } if *i >= nc))
    {
        return Err(corrupt(
            "history change refers to a missing content binding",
        ));
    }
    let ne = d.count(8)?;
    let mut evidence = Vec::with_capacity(ne);
    for _ in 0..ne {
        evidence.push(Evidence {
            label: d.bytes()?.to_vec(),
            bytes: d.bytes()?.to_vec(),
        });
    }
    d.finish()?;
    Ok(HistoryEntry {
        position,
        attempt,
        evidence,
        changes,
        content,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use cce_core::storage::{ContentBinding, StagingToken};

    fn sample(token: &[u8]) -> ChangeSet {
        ChangeSet {
            changes: vec![
                Change::PutContext {
                    id: ContextId::new(*b"A").unwrap(),
                    expect: Precondition::Absent,
                    body: b"body".to_vec(),
                    content: Some(0),
                },
                Change::PutRelationship {
                    id: RelationshipId::new(*b"r").unwrap(),
                    expect: Precondition::AtRevision(Revision(3)),
                    kind: RelationshipKind::new(*b"k").unwrap(),
                    source: ContextId::new(*b"A").unwrap(),
                    target: ContextId::new(*b"B").unwrap(),
                },
            ],
            content: vec![ContentBinding {
                content: ContentRef {
                    digest: ContentDigest([9; 32]),
                    len: 4,
                },
                token: StagingToken(token.to_vec()),
            }],
            evidence: vec![Evidence {
                label: b"l".to_vec(),
                bytes: b"\x00e".to_vec(),
            }],
        }
    }

    #[test]
    fn fingerprint_ignores_staging_location_but_binds_logical_content() {
        let a = AttemptId::new(*b"x").unwrap();
        assert_eq!(
            fingerprint(&a, &sample(b"tok1")),
            fingerprint(&a, &sample(b"tok2"))
        );
        let mut other = sample(b"tok1");
        other.evidence[0].bytes.push(1);
        assert_ne!(fingerprint(&a, &sample(b"tok1")), fingerprint(&a, &other));
        let mut other = sample(b"tok1");
        other.content[0].content.digest = ContentDigest([8; 32]);
        assert_ne!(fingerprint(&a, &sample(b"tok1")), fingerprint(&a, &other));
    }

    #[test]
    fn history_entry_roundtrips_exactly() {
        let a = AttemptId::new(*b"x").unwrap();
        let cs = sample(b"t");
        let h = decode_history(&encode_history(7, &a, &cs)).unwrap();
        assert_eq!(h.position, HistoryPosition(7));
        assert_eq!(h.attempt, a);
        assert_eq!(h.changes, cs.changes);
        assert_eq!(h.evidence, cs.evidence);
    }
}

/// Largest segment of stored content; one segment is read and verified at a time.
pub(crate) const SEGMENT_MAX: u64 = 64 * 1024;
const MANIFEST_V1: u8 = 1;

/// Physical layout of one content stream: its logical identity and its segments in order. Stored
/// as an extent of the content file; each segment carries its own digest so a reader can verify it
/// before delivering any of its bytes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Manifest {
    pub content: ContentRef,
    pub segments: Vec<ExtentRef>,
}

impl Manifest {
    /// Encoded size of a manifest with `segments` segments.
    pub fn encoded_len(segments: usize) -> u64 {
        1 + 32 + 8 + 4 + 48 * segments as u64
    }

    pub fn encode(&self) -> Vec<u8> {
        let mut e = Enc::default();
        e.u8(MANIFEST_V1)
            .raw(&self.content.digest.0)
            .u64(self.content.len)
            .u32(self.segments.len() as u32);
        for x in &self.segments {
            x.enc(&mut e);
        }
        debug_assert_eq!(e.buf.len() as u64, Self::encoded_len(self.segments.len()));
        e.buf
    }

    pub fn decode(b: &[u8]) -> Result<Self, StorageError> {
        let mut d = Dec::new(b, "content manifest");
        if d.u8()? != MANIFEST_V1 {
            return Err(StorageError::Unsupported("content manifest version".into()));
        }
        let content = ContentRef {
            digest: ContentDigest(d.array()?),
            len: d.u64()?,
        };
        let n = d.count(48)?;
        let mut segments = Vec::with_capacity(n);
        let mut total: u64 = 0;
        for _ in 0..n {
            let x = ExtentRef::dec(&mut d)?;
            if x.len == 0 || x.len > SEGMENT_MAX {
                return Err(corrupt(format!(
                    "content manifest: segment length {}",
                    x.len
                )));
            }
            total = total
                .checked_add(x.len)
                .ok_or_else(|| corrupt("content manifest: length overflow"))?;
            segments.push(x);
        }
        d.finish()?;
        if total != content.len {
            return Err(corrupt(format!(
                "content manifest: segments hold {total} bytes, content is {}",
                content.len
            )));
        }
        Ok(Self { content, segments })
    }
}
