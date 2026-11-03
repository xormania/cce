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
