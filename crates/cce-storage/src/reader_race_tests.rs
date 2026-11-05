//! Readers racing commits, staging and store drop.
//!
//! The engine has no reader registration and reclaims no committed space: committed content bytes
//! and metadata pages are never rewritten, a view keeps its snapshot, and a content reader holds the
//! store internals and its own manifest. The only space that is reused is staging space beyond the
//! allocation cursor that a failed staging left behind. These tests race what can interleave today:
//! views and content readers opened and read while commits publish new versions, while other
//! stagings succeed, fail or are abandoned, and after the store handle itself is dropped.

use crate::test_support::create_on;

use std::collections::BTreeSet;
use std::io::Read;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use cce_core::storage::{
    AttemptId, Change, ChangeSet, ContentBinding, ContentDigest, ContentRef, ContextId,
    HistoryPosition, Precondition, ReadView, StorageEngine,
};

use crate::NativeStore;
use crate::format::StoreId;
use crate::sim::SimIo;

const VERSIONS: u64 = 24;
const READERS: usize = 3;

fn a() -> ContextId {
    ContextId::new(*b"A").unwrap()
}

/// The content of version `n`: a few segments, distinct for every version.
fn payload(n: u64) -> Vec<u8> {
    (0..150_000 + n as usize * 1013)
        .map(|i| (i as u64 * 131 + n * 7 + (i as u64 >> 9)) as u8)
        .collect()
}

fn xorshift(s: &mut u64) -> u64 {
    *s ^= *s << 13;
    *s ^= *s >> 7;
    *s ^= *s << 17;
    *s
}

/// Stops every thread of the race when one of them fails, so a failure ends the test instead of
/// leaving the others running.
struct StopOnPanic<'a>(&'a AtomicBool);

impl Drop for StopOnPanic<'_> {
    fn drop(&mut self) {
        if std::thread::panicking() {
            self.0.store(true, Ordering::SeqCst);
        }
    }
}

/// Upper bound on concurrent stagings, which keeps the simulated files small.
const MAX_STAGINGS: u64 = 400;

/// Commits version `n` of A. Another staging lands between this version's staging and its commit,
/// as a concurrent client's would.
fn commit_version(s: &NativeStore, n: u64) {
    let staged = s.stage_content(&mut &payload(n)[..]).unwrap();
    let decoy: Vec<u8> = (0..70_000u32).map(|i| (i ^ n as u32) as u8).collect();
    s.stage_content(&mut &decoy[..]).unwrap();
    let cs = ChangeSet {
        changes: vec![Change::PutContext {
            id: a(),
            expect: Precondition::Any,
            body: format!("v{n}").into_bytes(),
            content: Some(0),
        }],
        content: vec![ContentBinding::from(&staged)],
        ..ChangeSet::default()
    };
    let attempt = AttemptId::new(format!("v{n}").into_bytes()).unwrap();
    s.commit(attempt, s.begin().unwrap(), cs).unwrap();
}

/// Reads A through `v` in irregular chunks, yielding between them so commits and stagings
/// interleave with the read. Returns the version the view holds.
fn read_version(v: &mut crate::NativeView, rng: &mut u64) -> u64 {
    let r = v.context(&a()).unwrap().expect("A exists in every view");
    let n: u64 = std::str::from_utf8(&r.body)
        .unwrap()
        .strip_prefix('v')
        .unwrap()
        .parse()
        .unwrap();
    // The view is one consistent snapshot: its history ends at the commit that wrote this version.
    let positions: Vec<u64> = v
        .history(HistoryPosition(0))
        .unwrap()
        .map(|h| h.unwrap().position.0)
        .collect();
    assert_eq!(positions, (1..=n + 1).collect::<Vec<_>>(), "view of v{n}");
    let mut reader = v
        .content(&r.content.unwrap())
        .unwrap_or_else(|e| panic!("view of v{n}: its content must stay readable: {e:?}"));
    let mut got = Vec::new();
    loop {
        let mut buf = vec![0; 1 + (xorshift(rng) % 20_000) as usize];
        let k = reader
            .read(&mut buf)
            .unwrap_or_else(|e| panic!("v{n}: {e}"));
        if k == 0 {
            break;
        }
        got.extend_from_slice(&buf[..k]);
        std::thread::yield_now();
    }
    assert!(got == payload(n), "v{n}: a reader returned other bytes");
    n
}

#[test]
fn readers_racing_commits_staging_and_store_drop_read_exactly_their_views_bytes() {
    let s = create_on(Box::new(SimIo::new()), StoreId([9; 16]), Default::default()).unwrap();
    commit_version(&s, 0);

    let done = AtomicBool::new(false);
    let reads = AtomicU64::new(0);
    let stagings = AtomicU64::new(0);
    let seen = Mutex::new(BTreeSet::new());
    // Views kept from every version, read only after the store handle is gone.
    let kept = Mutex::new(Vec::new());
    let (s_ref, done_ref, reads_ref, seen_ref, kept_ref, stagings_ref) =
        (&s, &done, &reads, &seen, &kept, &stagings);
    std::thread::scope(|t| {
        for r in 0..READERS {
            let (s, done, reads, seen) = (s_ref, done_ref, reads_ref, seen_ref);
            t.spawn(move || {
                let _stop = StopOnPanic(done);
                let mut rng = 0x9E37_79B9 + r as u64;
                while !done.load(Ordering::SeqCst) {
                    let n = read_version(&mut s.begin().unwrap(), &mut rng);
                    seen.lock().unwrap().insert(n);
                    reads.fetch_add(1, Ordering::SeqCst);
                }
            });
        }
        // Stagings that fail part-way or are abandoned, reusing and extending staging space.
        let (s, done, stagings) = (s_ref, done_ref, stagings_ref);
        t.spawn(move || {
            let _stop = StopOnPanic(done);
            let mut i = 0u32;
            while !done.load(Ordering::SeqCst) && u64::from(i) < MAX_STAGINGS {
                i += 1;
                let bytes: Vec<u8> = (0..(1 + i % 5) * 40_000).map(|b| (b ^ i) as u8).collect();
                if i.is_multiple_of(2) {
                    let wrong = ContentRef {
                        digest: ContentDigest([i as u8; 32]),
                        len: bytes.len() as u64,
                    };
                    assert!(s.stage_content_expecting(&mut &bytes[..], &wrong).is_err());
                } else {
                    s.stage_content(&mut &bytes[..]).unwrap();
                }
                stagings.fetch_add(1, Ordering::SeqCst);
                std::thread::yield_now();
            }
        });
        let (s, done, reads, kept) = (s_ref, done_ref, reads_ref, kept_ref);
        t.spawn(move || {
            let _stop = StopOnPanic(done);
            for n in 1..=VERSIONS {
                // Let the readers make progress on the current version before replacing it.
                let before = reads.load(Ordering::SeqCst);
                while reads.load(Ordering::SeqCst) == before {
                    assert!(!done.load(Ordering::SeqCst), "a reader failed");
                    std::thread::yield_now();
                }
                kept.lock().unwrap().push((n - 1, s.begin().unwrap()));
                commit_version(s, n);
            }
            kept.lock().unwrap().push((VERSIONS, s.begin().unwrap()));
            done.store(true, Ordering::SeqCst);
        });
    });
    // Every commit waited for a completed read, so reads interleaved with all of them, and
    // stagings ran alongside.
    assert!(reads.load(Ordering::SeqCst) >= VERSIONS);
    assert!(
        stagings.load(Ordering::SeqCst) > 0,
        "no staging ran during the race"
    );
    let seen = seen.into_inner().unwrap();
    assert!(seen.len() >= 2, "readers saw only versions {seen:?}");

    // Readers opened before the store handle is dropped, finished after; and views from every
    // version opened only after it is dropped.
    let mut kept = kept.into_inner().unwrap();
    let mut open_readers = Vec::new();
    for (n, v) in kept.iter_mut().step_by(2) {
        let r = v.context(&a()).unwrap().unwrap();
        let mut reader = v.content(&r.content.unwrap()).unwrap();
        let mut head = vec![0; 1000];
        reader.read_exact(&mut head).unwrap();
        open_readers.push((*n, head, reader));
    }
    drop(s);
    for (n, mut got, mut reader) in open_readers {
        reader.read_to_end(&mut got).unwrap();
        assert!(got == payload(n), "v{n}: reader finished after drop");
    }
    let mut rng = 7;
    for (n, mut v) in kept {
        assert_eq!(read_version(&mut v, &mut rng), n);
    }
}
