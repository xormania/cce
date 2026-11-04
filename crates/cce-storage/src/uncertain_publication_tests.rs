//! A commit whose publication record may or may not be durable, while a staging that already
//! passed its entry checks waits to take space: whichever record the store reopens with, content
//! that record references is never overwritten.

use std::io::Read;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex};

use cce_core::storage::{
    Change, ChangeSet, CommitError, ContentBinding, Precondition, ReadView, StorageEngine,
    StorageError,
};

use crate::NativeStore;
use crate::engine::CONTENT_CHUNK;
use crate::format::{PAGE_SIZE, StoreId};
use crate::io::{FileId, Io};
use crate::sim::{Persist, SimIo};
use crate::test_support::{attempt, changes, cid, create_on, open_on, put_context};

const STORE: StoreId = StoreId(*b"uncertain-public");

/// I/O over a simulator that runs `on_record` when a publication record is written and then
/// fails the metadata sync that would make it durable: the record may or may not survive.
struct UncertainRecord {
    sim: SimIo,
    state: Arc<UncertainState>,
}

#[derive(Default)]
struct UncertainState {
    on_record: Mutex<Option<Box<dyn FnOnce() + Send>>>,
    fail_meta_sync: AtomicBool,
}

impl Io for UncertainRecord {
    fn create(&self) -> Result<(), StorageError> {
        self.sim.create()
    }
    fn open(&self) -> Result<(), StorageError> {
        self.sim.open()
    }
    fn read_at(&self, file: FileId, offset: u64, buf: &mut [u8]) -> Result<(), StorageError> {
        self.sim.read_at(file, offset, buf)
    }
    fn write_at(&self, file: FileId, offset: u64, data: &[u8]) -> Result<(), StorageError> {
        if file == FileId::Meta && offset < 2 * PAGE_SIZE as u64 {
            let hook = self.state.on_record.lock().unwrap().take();
            if let Some(hook) = hook {
                hook();
                self.state.fail_meta_sync.store(true, Ordering::SeqCst);
            }
        }
        self.sim.write_at(file, offset, data)
    }
    fn len(&self, file: FileId) -> Result<u64, StorageError> {
        self.sim.len(file)
    }
    fn sync(&self, file: FileId) -> Result<(), StorageError> {
        if file == FileId::Meta && self.state.fail_meta_sync.swap(false, Ordering::SeqCst) {
            return Err(StorageError::Io(
                "injected: publication record sync failed".into(),
            ));
        }
        self.sim.sync(file)
    }
    fn sync_dir(&self) -> Result<(), StorageError> {
        self.sim.sync_dir()
    }
}

/// A reader that signals its first read, then waits to be released before serving `data`.
struct GatedReader {
    entered: Option<Sender<()>>,
    release: Receiver<()>,
    data: Vec<u8>,
    at: usize,
}

impl Read for GatedReader {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if let Some(tx) = self.entered.take() {
            tx.send(()).unwrap();
            self.release.recv().unwrap();
        }
        let n = buf.len().min(self.data.len() - self.at);
        buf[..n].copy_from_slice(&self.data[self.at..self.at + n]);
        self.at += n;
        Ok(n)
    }
}

fn bytes_of(seed: u8, len: usize) -> Vec<u8> {
    (0..len)
        .map(|i| (i as u8).wrapping_mul(31).wrapping_add(seed))
        .collect()
}

fn commit(s: &NativeStore, name: &str, cs: ChangeSet) {
    let r = s.commit(attempt(name), s.begin().unwrap(), cs);
    assert!(r.is_ok(), "fixture commit {name}: {r:?}");
}

fn read_content(s: &NativeStore, id: &str) -> Option<Result<Vec<u8>, StorageError>> {
    let mut v = s.begin().unwrap();
    let record = v.context(&cid(id)).unwrap()?;
    let c = record.content.expect("the record has content");
    let mut out = Vec::new();
    Some(
        v.content(&c)
            .and_then(|mut r| {
                r.read_to_end(&mut out)
                    .map_err(|e| StorageError::Io(e.to_string()))
            })
            .map(|_| out),
    )
}

/// Commits content E over reused retired space; the record write starts a staging that pauses
/// past its entry checks, the record's sync fails, then (with `discard`) the caller discards E's
/// staging, and the paused staging resumes. Returns, for every recovered image in which E is
/// committed, a description of any image where E does not read back exactly.
fn uncertain_commit_then_staging(discard: bool) -> Vec<String> {
    let len = CONTENT_CHUNK + 300;
    let sim = SimIo::new();
    let state = Arc::new(UncertainState::default());
    let s = create_on(
        Box::new(UncertainRecord {
            sim: sim.clone(),
            state: state.clone(),
        }),
        STORE,
        Default::default(),
    )
    .unwrap();
    // Retired space for the next staging: content committed, removed, two generations on.
    let x = bytes_of(1, len);
    let sx = s.stage_content(&mut &x[..]).unwrap();
    commit(
        &s,
        "x",
        ChangeSet {
            changes: vec![put_context("X", Precondition::Any, b"X", Some(0))],
            content: vec![ContentBinding::from(&sx)],
            ..ChangeSet::default()
        },
    );
    commit(
        &s,
        "x-removed",
        changes(vec![Change::RemoveContext {
            id: cid("X"),
            expect: Precondition::Any,
        }]),
    );
    for k in 0..2 {
        commit(
            &s,
            &format!("advance-{k}"),
            changes(vec![put_context("advance", Precondition::Any, b"a", None)]),
        );
    }
    let free = s.inner.reusable_content_bytes();
    let content = bytes_of(7, len);
    let staged = s.stage_content(&mut &content[..]).unwrap();
    assert!(
        s.inner.reusable_content_bytes() < free,
        "E's staging did not take retired space"
    );
    let binding = ContentBinding::from(&staged);
    let mut staged = Some(staged);

    let (go_tx, go_rx) = channel::<()>();
    let (entered_tx, entered_rx) = channel::<()>();
    let (release_tx, release_rx) = channel::<()>();
    *state.on_record.lock().unwrap() = Some(Box::new(move || {
        go_tx.send(()).unwrap();
        entered_rx.recv().unwrap();
    }));
    let other = bytes_of(9, len);
    let committed = std::thread::scope(|t| {
        let (store, data) = (&s, other.clone());
        let stager = t.spawn(move || {
            go_rx.recv().unwrap();
            store.stage_content(&mut GatedReader {
                entered: Some(entered_tx),
                release: release_rx,
                data,
                at: 0,
            })
        });
        let committed = s.commit(
            attempt("uncertain"),
            s.begin().unwrap(),
            ChangeSet {
                changes: vec![put_context("E", Precondition::Any, b"e", Some(0))],
                content: vec![binding],
                ..ChangeSet::default()
            },
        );
        if discard {
            // Refused or accepted, the space must stay out of the later staging's reach.
            let _ = s.discard_staged(staged.take().unwrap());
        }
        release_tx.send(()).unwrap();
        let _ = stager.join().unwrap();
        committed
    });
    assert!(
        matches!(committed, Err(CommitError::Indeterminate(_))),
        "the schedule must make the commit's publication uncertain: {committed:?}"
    );
    drop(staged);
    drop(s);

    let images = [("process exit".to_string(), sim.after_process_exit())]
        .into_iter()
        .chain(
            [Persist::None, Persist::All]
                .into_iter()
                .chain((1..=4).map(Persist::Seeded))
                .map(|p| (format!("{p:?}"), sim.after_crash(p))),
        );
    let mut lost = Vec::new();
    for (how, img) in images {
        let recovered = open_on(Box::new(img.clone()), Default::default()).unwrap();
        if let Some(got) = read_content(&recovered, "E")
            && got != Ok(content.clone())
        {
            lost.push(format!("{how}: {got:?}"));
        }
    }
    lost
}

#[test]
fn a_staging_never_takes_space_an_uncertain_commit_references_after_a_discard() {
    let lost = uncertain_commit_then_staging(true);
    assert!(lost.is_empty(), "committed content overwritten: {lost:?}");
}

#[test]
fn without_a_discard_an_uncertain_commit_keeps_its_space() {
    let lost = uncertain_commit_then_staging(false);
    assert!(lost.is_empty(), "committed content overwritten: {lost:?}");
}
