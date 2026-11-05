//! Instrumentation counters and the space report, each checked against a value computed
//! independently of the counter: metadata reads observed below the engine, query sizes known from
//! the fixture, the segment size, and the test-only audit's own walk of the published state.

use std::io::Read;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use cce_core::storage::{
    AttemptId, Change, ChangeSet, ContentBinding, ContextId, Direction, Precondition, ReadView,
    RelationshipId, RelationshipKind, RelationshipQuery, StorageEngine, StorageError,
};

use crate::engine::{CONTENT_CHUNK, Inner};
use crate::format::StoreId;
use crate::io::{FileId, Io};
use crate::sim::SimIo;
use crate::{NativeStore, StoreStats};

const STORE: StoreId = StoreId(*b"instrument-tests");

/// The simulator, counting every read of the metadata file (the engine reads it a page at a time).
struct CountingIo {
    inner: SimIo,
    meta_reads: Arc<AtomicU64>,
}

impl Io for CountingIo {
    fn create(&self) -> Result<(), StorageError> {
        self.inner.create()
    }
    fn open(&self) -> Result<(), StorageError> {
        self.inner.open()
    }
    fn read_at(&self, file: FileId, offset: u64, buf: &mut [u8]) -> Result<(), StorageError> {
        if file == FileId::Meta {
            self.meta_reads.fetch_add(1, Ordering::SeqCst);
        }
        self.inner.read_at(file, offset, buf)
    }
    fn write_at(&self, file: FileId, offset: u64, data: &[u8]) -> Result<(), StorageError> {
        self.inner.write_at(file, offset, data)
    }
    fn len(&self, file: FileId) -> Result<u64, StorageError> {
        self.inner.len(file)
    }
    fn sync(&self, file: FileId) -> Result<(), StorageError> {
        self.inner.sync(file)
    }
    fn sync_dir(&self) -> Result<(), StorageError> {
        self.inner.sync_dir()
    }
}

fn counted(sim: &SimIo) -> (Box<dyn Io>, Arc<AtomicU64>) {
    let reads = Arc::new(AtomicU64::new(0));
    let io = CountingIo {
        inner: sim.clone(),
        meta_reads: reads.clone(),
    };
    (Box::new(io), reads)
}

fn create(sim: &SimIo) -> NativeStore {
    NativeStore {
        inner: Inner::create(Box::new(sim.clone()), STORE, Default::default()).unwrap(),
    }
}

fn cid(s: &str) -> ContextId {
    ContextId::new(s.as_bytes().to_vec()).unwrap()
}

fn commit(s: &NativeStore, attempt: &str, changes: Vec<Change>, content: Vec<ContentBinding>) {
    s.commit(
        AttemptId::new(attempt.as_bytes().to_vec()).unwrap(),
        s.begin().unwrap(),
        ChangeSet {
            changes,
            content,
            ..ChangeSet::default()
        },
    )
    .unwrap();
}

fn put(id: &str, body: Vec<u8>, content: Option<usize>) -> Change {
    Change::PutContext {
        id: cid(id),
        expect: Precondition::Any,
        body,
        content,
    }
}

/// Records with long bodies, a few per commit: a tree several pages wide.
fn fill(s: &NativeStore, n: usize) {
    for chunk in 0..n / 10 {
        let changes = (chunk * 10..chunk * 10 + 10)
            .map(|i| put(&format!("R{i:05}"), vec![i as u8; 600], None))
            .collect();
        commit(s, &format!("fill-{chunk}"), changes, Vec::new());
    }
}

fn stats(s: &NativeStore) -> StoreStats {
    s.stats().unwrap()
}

#[test]
fn pages_read_counts_every_metadata_read_including_those_of_opening() {
    let sim = SimIo::new();
    let s = create(&sim);
    fill(&s, 300);
    assert!(stats(&s).tree_height >= 2);
    drop(s);
    sim.release();

    let (io, reads) = counted(&sim);
    let s = NativeStore {
        inner: Inner::open(io, Default::default()).unwrap(),
    };
    let at_open = reads.load(Ordering::SeqCst);
    // Both publication records and at least the root page.
    assert!(at_open >= 3, "{at_open}");
    let st = stats(&s);
    assert_eq!(st.pages_read_at_open, at_open, "pages read while opening");
    assert_eq!(
        st.pages_read,
        reads.load(Ordering::SeqCst),
        "pages read so far"
    );

    // Reads after opening add to the total, not to the opening count.
    let mut v = s.begin().unwrap();
    for i in (0..300).step_by(37) {
        assert!(v.context(&cid(&format!("R{i:05}"))).unwrap().is_some());
    }
    let st = stats(&s);
    assert!(
        reads.load(Ordering::SeqCst) > at_open,
        "the reads loaded no page"
    );
    assert_eq!(st.pages_read, reads.load(Ordering::SeqCst));
    assert_eq!(st.pages_read_at_open, at_open);
}

#[test]
fn entries_visited_counts_one_per_lookup_and_one_per_entry_a_scan_yields() {
    let sim = SimIo::new();
    let s = create(&sim);
    fill(&s, 300);
    let rels: Vec<Change> = (0..50)
        .map(|i| Change::PutRelationship {
            id: RelationshipId::new(format!("e{i:03}").into_bytes()).unwrap(),
            expect: Precondition::Absent,
            kind: RelationshipKind::new(b"links".to_vec()).unwrap(),
            source: cid("HUB"),
            target: cid(&format!("R{i:05}")),
        })
        .collect();
    commit(&s, "hub", rels, Vec::new());

    let mut v = s.begin().unwrap();
    let before = stats(&s).entries_visited;
    assert!(v.context(&cid("R00123")).unwrap().is_some());
    assert_eq!(stats(&s).entries_visited - before, 1, "one point lookup");
    let before = stats(&s).entries_visited;
    assert!(v.context(&cid("missing")).unwrap().is_none());
    assert_eq!(
        stats(&s).entries_visited - before,
        1,
        "a lookup that finds nothing"
    );

    let q = RelationshipQuery {
        from: cid("HUB"),
        direction: Direction::Outgoing,
        kind: None,
        after: None,
    };
    // Each result is one direction-index entry plus a lookup of the relationship record.
    let before = stats(&s).entries_visited;
    let all = v.relationships(&q).unwrap().count();
    assert_eq!(all, 50);
    assert_eq!(stats(&s).entries_visited - before, 2 * 50, "a whole scan");
    let before = stats(&s).entries_visited;
    assert_eq!(v.relationships(&q).unwrap().take(7).count(), 7);
    assert_eq!(
        stats(&s).entries_visited - before,
        2 * 7,
        "a partly consumed scan"
    );
}

/// A source that checks the store's buffer counters while the store is staging from it.
struct Watching<'a> {
    store: &'a NativeStore,
    bytes: &'a [u8],
    seen: Vec<u64>,
}

impl Read for Watching<'_> {
    fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
        self.seen
            .push(self.store.stats().unwrap().content_buffer_bytes);
        let n = out.len().min(self.bytes.len());
        out[..n].copy_from_slice(&self.bytes[..n]);
        self.bytes = &self.bytes[n..];
        Ok(n)
    }
}

#[test]
fn content_buffer_bytes_follow_stagings_and_readers_and_the_peak_is_kept() {
    let sim = SimIo::new();
    let s = create(&sim);
    let data: Vec<u8> = (0..2 * CONTENT_CHUNK + 7)
        .map(|i| (i % 251) as u8)
        .collect();
    let chunk = CONTENT_CHUNK as u64;
    assert_eq!(stats(&s).content_buffer_bytes, 0);

    let mut src = Watching {
        store: &s,
        bytes: &data,
        seen: Vec::new(),
    };
    let staged = s.stage_content(&mut src).unwrap();
    // While staging, one segment buffer is held.
    assert!(src.seen.iter().all(|b| *b == chunk), "{:?}", src.seen);
    assert_eq!(
        stats(&s).content_buffer_bytes,
        0,
        "staging released its buffer"
    );
    commit(
        &s,
        "c",
        vec![put("C", b"c".to_vec(), Some(0))],
        vec![ContentBinding::from(&staged)],
    );
    let c = staged.content;
    drop(staged);

    let mut v = s.begin().unwrap();
    let mut r1 = v.content(&c).unwrap();
    let mut r2 = v.content(&c).unwrap();
    assert_eq!(
        stats(&s).content_buffer_bytes,
        0,
        "readers hold nothing before reading"
    );
    let mut one = [0u8; 1];
    r1.read_exact(&mut one).unwrap();
    r2.read_exact(&mut one).unwrap();
    assert_eq!(
        stats(&s).content_buffer_bytes,
        2 * chunk,
        "two readers, one segment each"
    );
    // r1 reaches its last segment (7 bytes). Its buffer keeps the full segment's allocation,
    // and the counter reports what is allocated, not how much of it is in use.
    let mut rest = Vec::new();
    r1.read_to_end(&mut rest).unwrap();
    assert_eq!(rest.len(), data.len() - 1);
    assert_eq!(stats(&s).content_buffer_bytes, 2 * chunk);
    drop((r1, r2));
    let st = stats(&s);
    assert_eq!(
        st.content_buffer_bytes, 0,
        "dropped readers released their buffers"
    );
    assert_eq!(st.content_buffer_bytes_peak, 2 * chunk);
}

/// The space report expected for the published state of `sim`, computed by the audit's own walk
/// and the reuse rule written out here: a retirement at generation `r` is reusable when
/// `r <= next - 2` and `r <=` the oldest pinned generation, with `next` the next generation.
fn expected_report(sim: &SimIo, oldest_pin: Option<u64>) -> crate::SpaceReport {
    let cur = *crate::audit::published(sim).unwrap().last().unwrap();
    let r = crate::audit::reach(sim, &cur).unwrap();
    let through = (cur.generation + 1)
        .checked_sub(2)
        .map(|b| oldest_pin.map_or(b, |p| b.min(p)));
    let ok = |g: u64| through.is_some_and(|b| g <= b);
    let mut e = crate::SpaceReport {
        live_pages: r.pages.len() as u64,
        live_content_bytes: r.content_segments.iter().map(|(_, l)| l).sum(),
        history_entries: r.history_extents.len() as u64,
        history_bytes: r.history_extents.iter().map(|(_, l)| l).sum(),
        history_entries_removed: r.removed_history.len() as u64,
        history_marker_bytes: r.marker_bytes,
        attempt_receipts: r.attempts.len() as u64,
        attempt_receipt_bytes: r.attempt_bytes,
        meta_file_bytes: sim.len(FileId::Meta).unwrap(),
        content_file_bytes: sim.len(FileId::Content).unwrap(),
        ..Default::default()
    };
    for (g, _) in r.retired.values() {
        if ok(*g) {
            e.retired_pages_reusable += 1;
        } else {
            e.retired_pages_protected += 1;
        }
    }
    for (g, len) in r.retired_extents.values() {
        if ok(*g) {
            e.retired_content_bytes_reusable += len;
        } else {
            e.retired_content_bytes_protected += len;
        }
    }
    e
}

#[test]
fn the_space_report_matches_an_independent_walk_and_the_reuse_rule() {
    let sim = SimIo::new();
    let s = create(&sim);
    fill(&s, 200);
    let big: Vec<u8> = (0..CONTENT_CHUNK + 300).map(|i| (i % 241) as u8).collect();
    let small = b"small content".to_vec();
    let (sb, ss) = (
        s.stage_content(&mut &big[..]).unwrap(),
        s.stage_content(&mut &small[..]).unwrap(),
    );
    commit(
        &s,
        "contents",
        vec![
            put("BIG", b"b".to_vec(), Some(0)),
            put("SMALL", b"s".to_vec(), Some(1)),
        ],
        vec![ContentBinding::from(&sb), ContentBinding::from(&ss)],
    );
    drop((sb, ss));
    // Retire the big content and some pages, then hold a view while more commits retire more.
    commit(
        &s,
        "rm",
        vec![Change::RemoveContext {
            id: cid("BIG"),
            expect: Precondition::Any,
        }],
        Vec::new(),
    );
    for k in 0..3 {
        commit(
            &s,
            &format!("a{k}"),
            vec![put("A", vec![k; 900], None)],
            Vec::new(),
        );
    }
    // The host removes two of those commits' history: their receipts stay.
    let removed = ["a0", "a1"].map(|a| {
        let r = s.outcome(&AttemptId::new(a.as_bytes().to_vec()).unwrap());
        match r {
            Ok(cce_core::storage::AttemptOutcome::Committed(r)) => Change::RemoveHistory {
                position: r.position,
            },
            other => panic!("{a} must be committed: {other:?}"),
        }
    });
    commit(&s, "forget", removed.into(), Vec::new());
    // Removing many records retires many pages while writing few: some stay reusable.
    commit(
        &s,
        "mass-remove",
        (0..120)
            .map(|i| Change::RemoveContext {
                id: cid(&format!("R{i:05}")),
                expect: Precondition::Any,
            })
            .collect(),
        Vec::new(),
    );
    for k in 0..2 {
        commit(
            &s,
            &format!("m{k}"),
            vec![put("M", vec![k; 10], None)],
            Vec::new(),
        );
    }
    let held = s.begin().unwrap();
    let pinned = s.inner.snapshot().unwrap().desc.generation;
    for k in 0..4 {
        commit(
            &s,
            &format!("b{k}"),
            vec![put(&format!("R{:05}", k * 13), vec![k; 700], None)],
            Vec::new(),
        );
    }

    let got = s.space_report().unwrap();
    let want = expected_report(&sim, Some(pinned));
    assert_eq!(got, want, "with a view pinning generation {pinned}");
    assert!(want.retired_pages_protected > 0 && want.retired_pages_reusable > 0);
    assert!(want.retired_content_bytes_reusable > 0 && want.live_content_bytes > 0);
    assert!(want.history_entries > 0 && want.history_bytes > 0);
    assert_eq!(want.history_entries_removed, 2);
    assert!(want.history_marker_bytes > 0);
    assert_eq!(
        want.attempt_receipts,
        want.history_entries + want.history_entries_removed,
        "one receipt per committed attempt"
    );

    drop(held);
    let got = s.space_report().unwrap();
    assert_eq!(got, expected_report(&sim, None), "with no view held");

    // A read-only inspection of the same files reports the same.
    drop(s);
    sim.release();
    let ro = crate::NativeReadOnly {
        inner: Inner::open_read_only(Box::new(sim.clone()), Default::default()).unwrap(),
    };
    assert_eq!(ro.space_report().unwrap(), expected_report(&sim, None));
}

#[test]
fn a_reader_that_fails_a_segment_read_frees_its_buffer() {
    let sim = SimIo::new();
    let s = create(&sim);
    let data: Vec<u8> = (0..2 * CONTENT_CHUNK).map(|i| (i % 251) as u8).collect();
    let staged = s.stage_content(&mut &data[..]).unwrap();
    commit(
        &s,
        "c",
        vec![put("C", b"c".to_vec(), Some(0))],
        vec![ContentBinding::from(&staged)],
    );
    let c = staged.content;
    drop(staged);

    let mut v = s.begin().unwrap();
    let mut r = v.content(&c).unwrap();
    let mut first = vec![0u8; CONTENT_CHUNK];
    r.read_exact(&mut first).unwrap();
    assert_eq!(stats(&s).content_buffer_bytes, CONTENT_CHUNK as u64);
    sim.fail_next_read();
    let mut one = [0u8; 1];
    assert!(r.read(&mut one).is_err(), "the second segment's read fails");
    assert_eq!(
        stats(&s).content_buffer_bytes,
        0,
        "a failed reader holds no buffer"
    );
    drop(r);
    assert_eq!(stats(&s).content_buffer_bytes, 0);
}

#[test]
fn a_reader_that_finds_a_damaged_segment_frees_its_buffer() {
    let sim = SimIo::new();
    let s = create(&sim);
    let data: Vec<u8> = (0..2 * CONTENT_CHUNK).map(|i| (i % 251) as u8).collect();
    let staged = s.stage_content(&mut &data[..]).unwrap();
    commit(
        &s,
        "c",
        vec![put("C", b"c".to_vec(), Some(0))],
        vec![ContentBinding::from(&staged)],
    );
    let c = staged.content;
    drop(staged);
    let segs = s
        .inner
        .content_segments(&s.inner.snapshot().unwrap(), &c)
        .unwrap();

    let mut v = s.begin().unwrap();
    let mut r = v.content(&c).unwrap();
    let mut first = vec![0u8; CONTENT_CHUNK];
    r.read_exact(&mut first).unwrap();
    assert_eq!(stats(&s).content_buffer_bytes, CONTENT_CHUNK as u64);
    // Flip one byte inside the second segment before the reader reaches it.
    let target = segs[1].offset + 100;
    let mut b = [0u8];
    sim.read_at(FileId::Content, target, &mut b).unwrap();
    sim.write_at(FileId::Content, target, &[b[0] ^ 0xff])
        .unwrap();
    let mut one = [0u8; 1];
    let err = r.read(&mut one).unwrap_err();
    assert!(err.to_string().contains("digest mismatch"), "{err}");
    assert_eq!(
        stats(&s).content_buffer_bytes,
        0,
        "a reader that failed a segment's check holds no buffer"
    );
}
