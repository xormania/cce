//! Integrity inspection at the structure level: a consistent store has no findings, and each
//! crafted, checksum-valid store that breaks exactly one rule is reported under exactly that check.
//! Inspection never changes the store and never reports stored bodies, content, evidence or
//! identifiers.

use std::collections::BTreeSet;

use cce_core::storage::{
    AttemptId, Change, ChangeSet, ContentBinding, ContextId, Evidence, Precondition,
    RelationshipId, RelationshipKind, StorageEngine, StorageError,
};

use crate::engine::{Inner, Options, context_key};
use crate::format::{Descriptor, PAGE_SIZE, PageKind, PageRef, StoreId, encode_page};
use crate::io::{FileId, Io};
use crate::records::{
    AttemptValue, ContextValue, P_ATTEMPT, P_CONTENT, P_IN, P_OUT, history_key, key,
};
use crate::sim::SimIo;
use crate::tree::{Entries, Node, NodeSink, TreeTxn};
use crate::{NativeReadOnly, NativeStore, Report, VerifyLevel};

const STORE: StoreId = StoreId(*b"verify-test-stor");

fn cid(s: &str) -> ContextId {
    ContextId::new(s.as_bytes().to_vec()).unwrap()
}

fn rel(id: &str, source: &str, target: &str) -> Change {
    Change::PutRelationship {
        id: RelationshipId::new(id.as_bytes().to_vec()).unwrap(),
        expect: Precondition::Absent,
        kind: RelationshipKind::new(*b"cites").unwrap(),
        source: cid(source),
        target: cid(target),
    }
}

fn put(id: &str, content: Option<usize>) -> Change {
    Change::PutContext {
        id: cid(id),
        expect: Precondition::Any,
        body: format!("body of {id}").into_bytes(),
        content,
    }
}

fn commit(s: &NativeStore, attempt: &str, cs: ChangeSet) {
    s.commit(
        AttemptId::new(attempt.as_bytes().to_vec()).unwrap(),
        s.begin().unwrap(),
        cs,
    )
    .unwrap();
}

/// Contexts with and without content (two segments), relationships in both directions, a removal,
/// evidence, and enough records for a multi-level tree. Released.
fn rich() -> SimIo {
    let sim = SimIo::new();
    let s = NativeStore {
        inner: Inner::create(Box::new(sim.clone()), STORE, Options::default()).unwrap(),
    };
    let payload: Vec<u8> = (0..70_000u32).map(|i| (i % 249) as u8).collect();
    let staged = s.stage_content(&mut &payload[..]).unwrap();
    let small = s.stage_content(&mut &[5u8; 1_000][..]).unwrap();
    commit(
        &s,
        "one",
        ChangeSet {
            changes: vec![
                put("A", Some(0)),
                put("B", None),
                put("C", Some(1)),
                rel("r1", "A", "B"),
                rel("r2", "B", "A"),
            ],
            content: vec![ContentBinding::from(&staged), ContentBinding::from(&small)],
            evidence: vec![Evidence {
                label: b"why".to_vec(),
                bytes: b"because".to_vec(),
            }],
        },
    );
    commit(
        &s,
        "two",
        ChangeSet {
            changes: vec![Change::RemoveRelationship {
                id: RelationshipId::new(*b"r2").unwrap(),
                expect: Precondition::Any,
            }],
            ..ChangeSet::default()
        },
    );
    commit(
        &s,
        "three",
        ChangeSet {
            changes: (0..300)
                .map(|i| put(&format!("ctx-{i:04}"), None))
                .collect(),
            ..ChangeSet::default()
        },
    );
    assert!(s.inner.height().unwrap() >= 2);
    drop(s);
    sim.release();
    sim
}

/// A store that was created and never committed to. Released.
fn empty() -> SimIo {
    let sim = SimIo::new();
    drop(Inner::create(Box::new(sim.clone()), STORE, Options::default()).unwrap());
    sim.release();
    sim
}

fn verify(sim: &SimIo) -> Report {
    verify_at(sim, VerifyLevel::Structure)
}

fn verify_at(sim: &SimIo, level: VerifyLevel) -> Report {
    let ro = NativeReadOnly {
        inner: Inner::open_read_only(Box::new(sim.clone()), Options::default()).unwrap(),
    };
    let r = ro.verify(level).unwrap();
    drop(ro);
    sim.release();
    r
}

fn checks(r: &Report) -> BTreeSet<&'static str> {
    r.findings.iter().map(|f| f.check).collect()
}

fn assert_only(r: &Report, check: &str) {
    assert_eq!(
        checks(r),
        BTreeSet::from([check]),
        "expected only {check} findings: {:#?}",
        r.findings
    );
}

/// Writes nodes as the next generation's pages.
struct Sink {
    sim: SimIo,
    next: u64,
    incarnation: u64,
    /// Committed content length of the next generation (raised when content is appended).
    content_len: u64,
}

impl NodeSink for Sink {
    fn write(&mut self, n: &Node) -> Result<PageRef, StorageError> {
        let at = PageRef {
            page: self.next,
            incarnation: self.incarnation,
        };
        self.sim.write_at(
            FileId::Meta,
            at.page * PAGE_SIZE as u64,
            &encode_page(STORE, PageKind::Node, at, &n.encode())?,
        )?;
        self.next += 1;
        Ok(at)
    }
}

/// Publishes a crafted next generation: `build` writes its tree through the sink and returns the
/// root. Every page and record is checksum-valid; only the structure is wrong.
fn publish(sim: &SimIo, build: impl FnOnce(&Inner, &Descriptor, &mut Sink) -> PageRef) {
    let inner = Inner::open(Box::new(sim.clone()), Options::default()).unwrap();
    let cur = inner.snapshot().unwrap().desc;
    let mut sink = Sink {
        sim: sim.clone(),
        next: cur.next_page,
        incarnation: cur.generation + 1,
        content_len: cur.content_len,
    };
    let root = build(&inner, &cur, &mut sink);
    let d = Descriptor {
        generation: cur.generation + 1,
        root,
        next_page: sink.next,
        content_len: sink.content_len,
        ..cur
    };
    sim.write_at(
        FileId::Meta,
        Descriptor::slot(d.generation) * PAGE_SIZE as u64,
        &d.encode(),
    )
    .unwrap();
    sim.sync(FileId::Meta).unwrap();
    drop(inner);
    sim.release();
}

/// Publishes the current tree with `edit` applied.
fn edit_tree(sim: &SimIo, edit: impl FnOnce(&mut TreeTxn)) {
    publish(sim, |inner, cur, sink| {
        let mut t = TreeTxn::new(inner, cur.root);
        edit(&mut t);
        t.finish(sink).unwrap()
    });
}

fn leaf(ids: &[&str]) -> Node {
    let entries: Entries = ids
        .iter()
        .map(|id| {
            let v = ContextValue {
                revision: 1,
                body: b"x".to_vec(),
                content: None,
            };
            (context_key(&cid(id)), v.encode())
        })
        .collect();
    Node::Leaf(entries)
}

fn images(sim: &SimIo) -> Vec<Option<(Vec<u8>, Vec<u8>)>> {
    [FileId::Meta, FileId::Content]
        .map(|f| sim.file_images_for_test(f))
        .to_vec()
}

#[test]
fn a_consistent_store_has_no_findings_and_is_not_changed() {
    let sim = rich();
    let before = images(&sim);
    let r = verify(&sim);
    assert!(r.is_consistent(), "{:#?}", r.findings);
    assert_eq!(r.generation, 4);
    assert_eq!(r.history_position, 3);
    assert!(
        r.pages > 1 && r.records > 300,
        "pages {} records {}",
        r.pages,
        r.records
    );
    assert!(
        r.slots.iter().all(|s| s.starts_with("valid")),
        "{:?}",
        r.slots
    );
    assert_eq!(images(&sim), before, "inspection changed the store");
    let lines = r.json_lines();
    assert!(lines.first().unwrap().starts_with("{\"type\":\"summary\""));
    assert_eq!(
        lines.last().unwrap(),
        "{\"type\":\"result\",\"consistent\":true,\"findings\":0}"
    );
    assert_eq!(verify(&sim).json_lines(), lines, "output is deterministic");
}

#[test]
fn a_record_in_the_wrong_slot_is_a_slot_finding() {
    let sim = rich();
    let d = verify(&sim).generation;
    let mut cur = vec![0; PAGE_SIZE];
    sim.read_at(
        FileId::Meta,
        Descriptor::slot(d) * PAGE_SIZE as u64,
        &mut cur,
    )
    .unwrap();
    sim.write_at(
        FileId::Meta,
        Descriptor::slot(d + 1) * PAGE_SIZE as u64,
        &cur,
    )
    .unwrap();
    sim.sync(FileId::Meta).unwrap();
    assert_only(&verify(&sim), "slot");
}

#[test]
fn an_unreadable_reachable_page_is_a_page_finding() {
    let sim = empty();
    publish(&sim, |_, cur, sink| {
        let a = sink.write(&leaf(&["a"])).unwrap();
        let missing = PageRef {
            page: cur.next_page + 50,
            incarnation: cur.generation + 1,
        };
        sink.write(&Node::Branch {
            seps: vec![context_key(&cid("m"))],
            children: vec![a, missing],
        })
        .unwrap()
    });
    assert_only(&verify(&sim), "page");
}

#[test]
fn a_page_reached_twice_is_a_shared_page_finding() {
    let sim = empty();
    publish(&sim, |_, _, sink| {
        let shared = sink.write(&leaf(&[])).unwrap();
        sink.write(&Node::Branch {
            seps: vec![context_key(&cid("m"))],
            children: vec![shared, shared],
        })
        .unwrap()
    });
    assert_only(&verify(&sim), "shared-page");
}

#[test]
fn leaves_at_different_depths_are_a_depth_finding() {
    let sim = empty();
    publish(&sim, |_, _, sink| {
        let a = sink.write(&leaf(&["a"])).unwrap();
        let n = sink.write(&leaf(&["n"])).unwrap();
        let u = sink.write(&leaf(&["u"])).unwrap();
        let inner = sink
            .write(&Node::Branch {
                seps: vec![context_key(&cid("t"))],
                children: vec![n, u],
            })
            .unwrap();
        sink.write(&Node::Branch {
            seps: vec![context_key(&cid("m"))],
            children: vec![a, inner],
        })
        .unwrap()
    });
    assert_only(&verify(&sim), "depth");
}

#[test]
fn a_key_outside_its_parents_range_is_a_separator_finding() {
    let sim = empty();
    publish(&sim, |_, _, sink| {
        // "z" sorts after the separator "m", so it does not belong in the first child.
        let low = sink.write(&leaf(&["a", "z"])).unwrap();
        let high = sink.write(&leaf(&["n"])).unwrap();
        sink.write(&Node::Branch {
            seps: vec![context_key(&cid("m"))],
            children: vec![low, high],
        })
        .unwrap()
    });
    assert_only(&verify(&sim), "separator");
}

#[test]
fn an_unknown_key_family_is_a_key_finding() {
    let sim = rich();
    edit_tree(&sim, |t| t.put(vec![0x7f, 1, 2], vec![]).unwrap());
    assert_only(&verify(&sim), "key");
}

#[test]
fn an_undecodable_record_is_a_value_finding() {
    let sim = rich();
    edit_tree(&sim, |t| t.put(context_key(&cid("B")), vec![0xff]).unwrap());
    assert_only(&verify(&sim), "value");
}

#[test]
fn a_missing_or_stray_index_entry_is_a_relationship_index_finding() {
    // r1 is A -> B ("cites"): its incoming entry is keyed by target, kind, source, id.
    let sim = rich();
    edit_tree(&sim, |t| {
        t.delete(&key(P_IN, &[b"B", b"cites", b"A", b"r1"]))
            .unwrap();
    });
    assert_only(&verify(&sim), "relationship-index");

    let sim = rich();
    edit_tree(&sim, |t| {
        t.put(key(P_OUT, &[b"B", b"cites", b"A", b"ghost"]), vec![])
            .unwrap()
    });
    assert_only(&verify(&sim), "relationship-index");
}

#[test]
fn a_content_reference_without_its_manifest_record_is_a_content_ref_finding() {
    let sim = rich();
    publish(&sim, |inner, cur, sink| {
        let a = crate::tree::get(inner, cur.root, &context_key(&cid("A")))
            .unwrap()
            .unwrap();
        let c = ContextValue::decode(&a).unwrap().content.unwrap();
        let mut t = TreeTxn::new(inner, cur.root);
        t.delete(&key(P_CONTENT, &[&c.digest.0])).unwrap();
        t.finish(sink).unwrap()
    });
    assert_only(&verify(&sim), "content-ref");
}

#[test]
fn a_history_entry_beyond_the_committed_position_is_a_history_finding() {
    let sim = rich();
    publish(&sim, |inner, cur, sink| {
        let first = crate::tree::get(inner, cur.root, &history_key(1))
            .unwrap()
            .unwrap();
        let mut t = TreeTxn::new(inner, cur.root);
        t.put(history_key(cur.history_pos + 1), first).unwrap();
        t.finish(sink).unwrap()
    });
    assert_only(&verify(&sim), "history");
}

#[test]
fn an_attempt_record_that_does_not_match_its_history_entry_is_an_attempt_finding() {
    let sim = rich();
    publish(&sim, |inner, cur, sink| {
        let k = key(P_ATTEMPT, &[b"one"]);
        let mut v =
            AttemptValue::decode(&crate::tree::get(inner, cur.root, &k).unwrap().unwrap()).unwrap();
        v.fingerprint[0] ^= 1;
        let mut t = TreeTxn::new(inner, cur.root);
        t.put(k, v.encode()).unwrap();
        t.finish(sink).unwrap()
    });
    assert_only(&verify(&sim), "attempt");
}

#[test]
fn inspection_reports_no_stored_bytes_and_changes_nothing() {
    const MARK: &str = "SECRET-MARK-7319";
    let sim = SimIo::new();
    let s = NativeStore {
        inner: Inner::create(Box::new(sim.clone()), STORE, Options::default()).unwrap(),
    };
    let staged = s.stage_content(&mut MARK.repeat(100).as_bytes()).unwrap();
    commit(
        &s,
        &format!("{MARK}-attempt"),
        ChangeSet {
            changes: vec![
                Change::PutContext {
                    id: cid(&format!("{MARK}-ctx")),
                    expect: Precondition::Any,
                    body: MARK.as_bytes().to_vec(),
                    content: Some(0),
                },
                put("plain", None),
                rel(&format!("{MARK}-rel"), &format!("{MARK}-ctx"), "plain"),
            ],
            content: vec![ContentBinding::from(&staged)],
            evidence: vec![Evidence {
                label: MARK.as_bytes().to_vec(),
                bytes: MARK.as_bytes().to_vec(),
            }],
        },
    );
    drop(s);
    sim.release();
    // Break the index of the marked relationship, so findings name it.
    let (ctx, r) = (format!("{MARK}-ctx"), format!("{MARK}-rel"));
    edit_tree(&sim, |t| {
        t.delete(&key(
            P_IN,
            &[b"plain", b"cites", ctx.as_bytes(), r.as_bytes()],
        ))
        .unwrap();
    });
    let before = images(&sim);
    let report = verify(&sim);
    assert_only(&report, "relationship-index");
    assert_eq!(images(&sim), before, "inspection changed the store");
    let text = report.json_lines().join("\n") + &format!("{report:?}");
    let hex: String = MARK.bytes().map(|b| format!("{b:02x}")).collect();
    for needle in [MARK.to_string(), hex.clone(), hex.to_uppercase()] {
        assert!(
            !text.contains(&needle),
            "inspection output contains stored bytes: {text}"
        );
    }
}

/// Context `id`'s bound content and its segments, read through the engine.
fn bound(
    sim: &SimIo,
    id: &str,
) -> (
    cce_core::storage::ContentRef,
    Vec<crate::records::ExtentRef>,
) {
    let inner = Inner::open_read_only(Box::new(sim.clone()), Options::default()).unwrap();
    let snap = inner.snapshot().unwrap();
    let v = crate::tree::get(&*inner, snap.desc.root, &context_key(&cid(id)))
        .unwrap()
        .unwrap();
    let c = ContextValue::decode(&v).unwrap().content.unwrap();
    let segments = inner.content_segments(&snap, &c).unwrap();
    drop(inner);
    sim.release();
    (c, segments)
}

#[test]
fn a_scrub_of_a_consistent_store_reads_every_content_byte() {
    let sim = rich();
    let before = images(&sim);
    let structure = verify(&sim);
    assert_eq!(structure.content_bytes, 0);
    let scrub = verify_at(&sim, VerifyLevel::Scrub);
    assert!(scrub.is_consistent(), "{:#?}", scrub.findings);
    assert_eq!(scrub.content_bytes, 70_000 + 1_000, "both bound contents");
    assert_eq!(images(&sim), before, "a scrub changed the store");
}

#[test]
fn a_damaged_content_segment_is_found_only_by_a_scrub() {
    let sim = rich();
    let (_, segments) = bound(&sim, "A");
    assert!(segments.len() >= 2);
    let at = segments[1].offset + 5;
    let mut b = [0u8];
    sim.read_at(FileId::Content, at, &mut b).unwrap();
    sim.write_at(FileId::Content, at, &[b[0] ^ 0xff]).unwrap();
    sim.sync(FileId::Content).unwrap();
    assert!(
        verify(&sim).is_consistent(),
        "content bytes are not read at the structure level"
    );
    assert_only(&verify_at(&sim, VerifyLevel::Scrub), "content");
}

#[test]
fn a_manifest_record_naming_other_content_is_found_by_a_scrub() {
    let sim = rich();
    let (a, _) = bound(&sim, "A");
    let (c, _) = bound(&sim, "C");
    publish(&sim, |inner, cur, sink| {
        let (ka, kc) = (
            key(P_CONTENT, &[&a.digest.0]),
            key(P_CONTENT, &[&c.digest.0]),
        );
        let va = crate::tree::get(inner, cur.root, &ka).unwrap().unwrap();
        let vc = crate::tree::get(inner, cur.root, &kc).unwrap().unwrap();
        let mut t = TreeTxn::new(inner, cur.root);
        t.put(ka, vc).unwrap();
        t.put(kc, va).unwrap();
        t.finish(sink).unwrap()
    });
    assert!(verify(&sim).is_consistent());
    assert_only(&verify_at(&sim, VerifyLevel::Scrub), "content");
}

#[test]
fn a_binding_whose_length_differs_from_its_manifest_is_found_by_a_scrub() {
    let sim = rich();
    publish(&sim, |inner, cur, sink| {
        let k = context_key(&cid("C"));
        let mut v =
            ContextValue::decode(&crate::tree::get(inner, cur.root, &k).unwrap().unwrap()).unwrap();
        v.content.as_mut().unwrap().len += 1;
        let mut t = TreeTxn::new(inner, cur.root);
        t.put(k, v.encode()).unwrap();
        t.finish(sink).unwrap()
    });
    assert!(verify(&sim).is_consistent());
    assert_only(&verify_at(&sim, VerifyLevel::Scrub), "content");
}

/// Commit 1 binds content X to "A"; commit 2 rewrites "A" without content, so only history entry 1
/// still binds X. Released.
fn bound_only_by_history() -> (SimIo, cce_core::storage::ContentRef) {
    let sim = SimIo::new();
    let s = NativeStore {
        inner: Inner::create(Box::new(sim.clone()), STORE, Options::default()).unwrap(),
    };
    let staged = s.stage_content(&mut &[3u8; 5_000][..]).unwrap();
    commit(
        &s,
        "bind",
        ChangeSet {
            changes: vec![put("A", Some(0))],
            content: vec![ContentBinding::from(&staged)],
            ..ChangeSet::default()
        },
    );
    commit(
        &s,
        "unbind",
        ChangeSet {
            changes: vec![put("A", None)],
            ..ChangeSet::default()
        },
    );
    drop(s);
    sim.release();
    (sim, staged.content)
}

#[test]
fn content_bound_only_by_history_without_its_manifest_record_is_a_content_ref_finding() {
    let (sim, x) = bound_only_by_history();
    assert!(verify(&sim).is_consistent());
    edit_tree(&sim, |t| {
        t.delete(&key(P_CONTENT, &[&x.digest.0])).unwrap();
    });
    let r = verify(&sim);
    assert_only(&r, "content-ref");
    assert!(
        r.findings
            .iter()
            .any(|f| f.location == "history position 1"),
        "{:#?}",
        r.findings
    );
}

#[test]
fn a_history_binding_that_differs_from_its_manifest_is_found_by_a_scrub() {
    use cce_core::storage::StagingToken;
    use sha2::Digest;

    use crate::records::{ExtentRef, decode_history, encode_history, fingerprint};

    let (sim, _) = bound_only_by_history();
    let content = sim.clone();
    // Rewrite history entry 1 so that it binds X with the wrong length, as a new, checksum-valid
    // extent, with its attempt record's fingerprint updated to match: only the binding is wrong.
    publish(&sim, |inner, cur, sink| {
        let old = ExtentRef::decode(
            &crate::tree::get(inner, cur.root, &history_key(1))
                .unwrap()
                .unwrap(),
        )
        .unwrap();
        let entry =
            decode_history(&inner.read_extent(&old, u64::MAX, "history entry").unwrap()).unwrap();
        let request = ChangeSet {
            changes: entry.changes,
            content: entry
                .content
                .into_iter()
                .map(|mut c| {
                    c.len += 1;
                    ContentBinding {
                        content: c,
                        token: StagingToken(Vec::new()),
                    }
                })
                .collect(),
            evidence: entry.evidence,
        };
        let bytes = encode_history(1, &entry.attempt, &request);
        let at = cur.content_len;
        content.write_at(FileId::Content, at, &bytes).unwrap();
        content.sync(FileId::Content).unwrap();
        sink.content_len = at + bytes.len() as u64;
        let mut t = TreeTxn::new(inner, cur.root);
        t.put(
            history_key(1),
            ExtentRef {
                offset: at,
                len: bytes.len() as u64,
                digest: sha2::Sha256::digest(&bytes).into(),
            }
            .encode(),
        )
        .unwrap();
        t.put(
            key(P_ATTEMPT, &[entry.attempt.as_bytes()]),
            AttemptValue {
                position: 1,
                fingerprint: fingerprint(&entry.attempt, &request),
            }
            .encode(),
        )
        .unwrap();
        t.finish(sink).unwrap()
    });
    assert!(verify(&sim).is_consistent(), "{:#?}", verify(&sim).findings);
    assert_only(&verify_at(&sim, VerifyLevel::Scrub), "content");
}
