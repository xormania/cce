//! Content lifecycle tests: segmented streaming, staging validation, integrity on read.

use std::io::{self, Read};

use scientdb_kernel::storage::{
    AttemptId, Change, ChangeSet, ContentBinding, ContextId, Precondition, ReadView, StorageEngine,
};

use crate::NativeStore;
use crate::engine::{CONTENT_CHUNK, Inner};
use crate::format::StoreId;
use crate::sim::SimIo;

pub(crate) fn sim_store(sim: &SimIo) -> NativeStore {
    NativeStore {
        inner: Inner::create(Box::new(sim.clone()), StoreId([4; 16]), Default::default()).unwrap(),
    }
}

pub(crate) fn bytes(n: usize, seed: u8) -> Vec<u8> {
    (0..n)
        .map(|i| (i as u8).wrapping_mul(31).wrapping_add(seed))
        .collect()
}

pub(crate) fn bind(
    s: &NativeStore,
    id: &str,
    payload: &[u8],
    attempt: &str,
) -> scientdb_kernel::storage::ContentRef {
    let staged = s.stage_content(&mut &payload[..]).unwrap();
    let cs = ChangeSet {
        changes: vec![Change::PutContext {
            id: ContextId::new(id.as_bytes().to_vec()).unwrap(),
            expect: Precondition::Any,
            body: b"b".to_vec(),
            content: Some(0),
        }],
        content: vec![ContentBinding::from(&staged)],
        ..ChangeSet::default()
    };
    s.commit(
        AttemptId::new(attempt.as_bytes().to_vec()).unwrap(),
        s.begin().unwrap(),
        cs,
    )
    .unwrap();
    staged.content
}

#[test]
fn large_content_is_stored_as_bounded_segments_and_streams_back_exactly() {
    let sim = SimIo::new();
    let s = sim_store(&sim);
    let payload = bytes(8 * CONTENT_CHUNK + 4321, 7);
    let c = bind(&s, "A", &payload, "a1");
    let segs = s
        .inner
        .content_segments(&s.inner.snapshot().unwrap(), &c)
        .unwrap();
    assert!(
        segs.len() >= 9,
        "expected ≥ 9 segments of at most one buffer, got {}",
        segs.len()
    );
    assert!(
        segs.iter().all(|x| x.len as usize <= CONTENT_CHUNK),
        "a segment exceeds the streaming buffer"
    );
    let mut offsets: Vec<u64> = segs.iter().map(|x| x.offset).collect();
    offsets.dedup();
    assert_eq!(
        offsets.len(),
        segs.len(),
        "segments must be distinct allocations"
    );
    assert_eq!(
        segs.iter().map(|x| x.len).sum::<u64>(),
        payload.len() as u64
    );

    // Reads never hand back more than one buffer at a time, and reproduce the bytes exactly.
    let mut r = s.begin().unwrap().content(&c).unwrap();
    let mut got = Vec::new();
    let mut buf = vec![0; 4 * CONTENT_CHUNK];
    loop {
        let n = r.read(&mut buf).unwrap();
        if n == 0 {
            break;
        }
        assert!(n <= CONTENT_CHUNK);
        got.extend_from_slice(&buf[..n]);
    }
    assert_eq!(got, payload);

    // Zero-length and exactly-one-buffer payloads are valid neighbours.
    for (id, n) in [("Z", 0), ("O", CONTENT_CHUNK), ("P", CONTENT_CHUNK + 1)] {
        let p = bytes(n, 3);
        let c = bind(&s, id, &p, &format!("n{n}"));
        let mut out = Vec::new();
        s.begin()
            .unwrap()
            .content(&c)
            .unwrap()
            .read_to_end(&mut out)
            .unwrap();
        assert_eq!(out, p, "{n}-byte payload");
    }
}

/// A source that returns short reads and retryable interruptions.
struct Choppy<'a> {
    data: &'a [u8],
    step: usize,
}

impl Read for Choppy<'_> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.step += 1;
        if self.step.is_multiple_of(3) {
            return Err(io::Error::new(io::ErrorKind::Interrupted, "retry"));
        }
        let n = buf.len().min(self.data.len()).min(1 + self.step % 977);
        buf[..n].copy_from_slice(&self.data[..n]);
        self.data = &self.data[n..];
        Ok(n)
    }
}

#[test]
fn staging_tolerates_short_and_interrupted_reads_and_reports_permanent_failure() {
    let sim = SimIo::new();
    let s = sim_store(&sim);
    let payload = bytes(3 * CONTENT_CHUNK + 17, 9);
    let staged = s
        .stage_content(&mut Choppy {
            data: &payload,
            step: 0,
        })
        .unwrap();
    assert_eq!(staged.content.len, payload.len() as u64);
    let c = bind(&s, "A", &payload, "a1");
    assert_eq!(
        c, staged.content,
        "same bytes must stage to the same logical reference"
    );

    struct Broken(usize);
    impl Read for Broken {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            if self.0 == 0 {
                return Err(io::Error::other("source failed"));
            }
            self.0 -= 1;
            buf[0] = 1;
            Ok(1)
        }
    }
    let err = s.stage_content(&mut Broken(5)).unwrap_err();
    assert!(
        matches!(err, scientdb_kernel::storage::StorageError::Io(_)),
        "{err:?}"
    );
}

#[test]
fn staging_against_a_declared_length_and_digest_rejects_any_mismatch() {
    use scientdb_kernel::storage::{ContentDigest, ContentRef, StorageError};
    use sha2::{Digest, Sha256};
    let sim = SimIo::new();
    let s = sim_store(&sim);
    let payload = bytes(2 * CONTENT_CHUNK + 5, 1);
    let right = ContentRef {
        digest: ContentDigest(Sha256::digest(&payload).into()),
        len: payload.len() as u64,
    };

    // Positive neighbour: the declared stream stages and commits.
    let ok = s
        .stage_content_expecting(&mut &payload[..], &right)
        .expect("matching stream must stage");
    assert_eq!(ok.content, right);

    let shorter = ContentRef {
        len: right.len - 1,
        ..right
    };
    let longer = ContentRef {
        len: right.len + 1,
        ..right
    };
    let other = ContentRef {
        digest: ContentDigest([9; 32]),
        ..right
    };
    for (what, expect, input) in [
        ("declared shorter than the stream", shorter, &payload[..]),
        (
            "declared longer than the stream (truncated input)",
            longer,
            &payload[..],
        ),
        ("different digest", other, &payload[..]),
        ("truncated input", right, &payload[..payload.len() - 1]),
    ] {
        let err = s
            .stage_content_expecting(&mut &input[..], &expect)
            .expect_err(what);
        assert!(
            matches!(err, StorageError::InvalidRequest(_)),
            "{what}: {err:?}"
        );
    }
}

use crate::io::{FileId, Io};
use crate::sim::Persist;

fn reopen(sim: &SimIo) -> NativeStore {
    sim.release();
    NativeStore {
        inner: Inner::open(Box::new(sim.clone()), Default::default()).expect("open"),
    }
}

#[test]
fn a_damaged_segment_is_reported_before_any_of_its_bytes_are_delivered() {
    let sim = SimIo::new();
    let s = sim_store(&sim);
    let payload = bytes(6 * CONTENT_CHUNK, 5);
    let c = bind(&s, "A", &payload, "a1");
    let segs = s
        .inner
        .content_segments(&s.inner.snapshot().unwrap(), &c)
        .unwrap();
    drop(s);
    // Flip one byte inside segment 3 on disk.
    let target = segs[3].offset + 100;
    let mut b = [0u8];
    sim.read_at(FileId::Content, target, &mut b).unwrap();
    sim.write_at(FileId::Content, target, &[b[0] ^ 0xff])
        .unwrap();
    sim.sync(FileId::Content).unwrap();

    let s = reopen(&sim);
    let mut r = s.begin().unwrap().content(&c).unwrap();
    let mut delivered = Vec::new();
    let mut buf = vec![0; 1000];
    let err = loop {
        match r.read(&mut buf) {
            Ok(0) => panic!("a damaged stream reported a complete read"),
            Ok(n) => delivered.extend_from_slice(&buf[..n]),
            Err(e) => break e,
        }
    };
    assert_eq!(
        delivered,
        payload[..3 * CONTENT_CHUNK],
        "bytes beyond the verified segments were delivered"
    );
    assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    assert!(
        err.to_string().contains("segment 3"),
        "error must name the failed segment: {err}"
    );
    assert!(
        r.read(&mut buf).is_err(),
        "reads after a failed check must keep failing"
    );
}

#[test]
fn a_cancelled_stage_leaves_nothing_accepted_and_survives_restart() {
    let sim = SimIo::new();
    let s = sim_store(&sim);
    let kept = bind(&s, "K", &bytes(2 * CONTENT_CHUNK, 2), "keep");
    struct FailsMidway(usize);
    impl Read for FailsMidway {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            if self.0 == 0 {
                return Err(io::Error::other("caller cancelled"));
            }
            self.0 -= 1;
            buf.fill(7);
            Ok(buf.len())
        }
    }
    assert!(s.stage_content(&mut FailsMidway(3)).is_err());
    // The next staging may reuse the abandoned space; committed content is untouched.
    let next = bind(&s, "N", &bytes(CONTENT_CHUNK + 9, 4), "next");
    drop(s);
    let s = reopen(&sim.after_crash(Persist::None));
    let mut v = s.begin().unwrap();
    for (c, want) in [
        (kept, bytes(2 * CONTENT_CHUNK, 2)),
        (next, bytes(CONTENT_CHUNK + 9, 4)),
    ] {
        let mut got = Vec::new();
        v.content(&c).unwrap().read_to_end(&mut got).unwrap();
        assert_eq!(got, want);
    }
}

#[test]
fn staging_handles_are_bound_to_their_store_and_open_instance() {
    use scientdb_kernel::storage::{CommitError, StorageError};
    let a = SimIo::new();
    let store_a = sim_store(&a);
    let b = SimIo::new();
    let store_b = NativeStore {
        inner: Inner::create(Box::new(b.clone()), StoreId([5; 16]), Default::default()).unwrap(),
    };
    let payload = b"payload".to_vec();
    let from_a = store_a.stage_content(&mut &payload[..]).unwrap();
    let put = |id: &str| Change::PutContext {
        id: ContextId::new(id.as_bytes().to_vec()).unwrap(),
        expect: Precondition::Any,
        body: b"b".to_vec(),
        content: Some(0),
    };
    let cs = |staged: &scientdb_kernel::storage::StagedContent| ChangeSet {
        changes: vec![put("X")],
        content: vec![ContentBinding::from(staged)],
        ..ChangeSet::default()
    };
    let foreign = store_b.commit(
        AttemptId::new(*b"x").unwrap(),
        store_b.begin().unwrap(),
        cs(&from_a),
    );
    assert_eq!(
        foreign,
        Err(CommitError::Rejected(StorageError::ForeignHandle))
    );

    // A handle from before a reopen is stale; restaging the same bytes gives the same reference.
    drop(store_a);
    let store_a = reopen(&a);
    let stale = store_a.commit(
        AttemptId::new(*b"y").unwrap(),
        store_a.begin().unwrap(),
        cs(&from_a),
    );
    assert_eq!(
        stale,
        Err(CommitError::Rejected(StorageError::ForeignHandle))
    );
    let again = store_a.stage_content(&mut &payload[..]).unwrap();
    assert_eq!(again.content, from_a.content);
    store_a
        .commit(
            AttemptId::new(*b"y").unwrap(),
            store_a.begin().unwrap(),
            cs(&again),
        )
        .expect("restaged bytes commit");
}

#[test]
fn an_acknowledged_create_survives_power_loss() {
    let sim = SimIo::new();
    drop(sim_store(&sim));
    let after = sim.after_crash(Persist::None);
    let s = NativeStore {
        inner: Inner::open(Box::new(after), Default::default()).expect("created store lost"),
    };
    assert!(s.begin().is_ok());
}

#[test]
fn updating_metadata_does_not_rewrite_a_large_payload() {
    let sim = SimIo::new();
    let s = sim_store(&sim);
    let payload = bytes(16 * CONTENT_CHUNK, 8);
    bind(&s, "A", &payload, "a1");
    let before = s.stats().unwrap().content_bytes_written;
    let cs = ChangeSet {
        changes: vec![Change::PutContext {
            id: ContextId::new(*b"A").unwrap(),
            expect: Precondition::Any,
            body: b"new body".to_vec(),
            content: None,
        }],
        ..ChangeSet::default()
    };
    s.commit(AttemptId::new(*b"a2").unwrap(), s.begin().unwrap(), cs)
        .unwrap();
    let written = s.stats().unwrap().content_bytes_written - before;
    assert!(
        written < 4096,
        "a metadata-only update wrote {written} content bytes for a {} byte payload",
        payload.len()
    );
}
