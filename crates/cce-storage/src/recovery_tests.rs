//! Recovery and open tests: failed opens, interrupted store creation, and the ordering between
//! publication durability and visibility.
//!
//! Expected states come from an independent run of the same logical requests, never from what
//! recovery chose.

use std::io::Read;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock, Weak};

use cce_core::storage::{
    AttemptId, AttemptOutcome, Change, ChangeSet, CommitError, ContentBinding, ContextId,
    Direction, Evidence, HistoryPosition, Precondition, ReadView, RelationshipId, RelationshipKind,
    RelationshipQuery, StorageEngine, StorageError,
};

use crate::NativeStore;
use crate::engine::Inner;
use crate::format::{PAGE_SIZE, StoreId};
use crate::io::{FileId, Io};
use crate::sim::{Persist, SimIo};

const STORE: StoreId = StoreId(*b"recovery-store-1");
const IDS: [&str; 4] = ["A", "B", "C", "D"];

fn cid(s: &str) -> ContextId {
    ContextId::new(s.as_bytes().to_vec()).unwrap()
}

fn create(io: Box<dyn Io>) -> NativeStore {
    NativeStore {
        inner: Inner::create(io, STORE, Default::default()).expect("create"),
    }
}

fn try_open(io: Box<dyn Io>) -> Result<NativeStore, StorageError> {
    Inner::open(io, Default::default()).map(|inner| NativeStore { inner })
}

fn open(io: Box<dyn Io>) -> NativeStore {
    try_open(io).expect("recovery must open the store")
}

fn generation(s: &NativeStore) -> u64 {
    s.inner.snapshot().expect("snapshot").desc.generation
}

/// Both store files' durable and volatile images.
fn images(sim: &SimIo) -> Vec<Option<(Vec<u8>, Vec<u8>)>> {
    [FileId::Meta, FileId::Content]
        .into_iter()
        .map(|f| sim.file_images_for_test(f))
        .collect()
}

/// A complete logical request: records, both relationship directions, content, evidence.
fn request(store: &NativeStore, step: u32) -> Option<(AttemptId, ChangeSet)> {
    let payload: Vec<u8> = (0..(150_000 + step as usize * 7))
        .map(|i| (i * 31 + step as usize) as u8)
        .collect();
    let staged = store.stage_content(&mut &payload[..]).ok()?;
    let mut cs = ChangeSet {
        content: vec![ContentBinding::from(&staged)],
        ..ChangeSet::default()
    };
    let (a, b) = (IDS[step as usize % 4], IDS[(step as usize + 1) % 4]);
    cs.changes.push(Change::PutContext {
        id: cid(a),
        expect: Precondition::Any,
        body: format!("body {a} at step {step}").into_bytes(),
        content: Some(0),
    });
    cs.changes.push(Change::PutRelationship {
        id: RelationshipId::new(format!("r{step}").into_bytes()).unwrap(),
        expect: Precondition::Absent,
        kind: RelationshipKind::new(*b"depends").unwrap(),
        source: cid(a),
        target: cid(b),
    });
    cs.evidence.push(Evidence {
        label: b"decision".to_vec(),
        bytes: format!("evidence {step}").into_bytes(),
    });
    Some((
        AttemptId::new(format!("attempt-{step}").into_bytes()).unwrap(),
        cs,
    ))
}

fn commit_step(store: &NativeStore, step: u32) -> bool {
    let Some((attempt, cs)) = request(store, step) else {
        return false;
    };
    let Ok(view) = store.begin() else {
        return false;
    };
    store.commit(attempt, view, cs).is_ok()
}

fn history_len(store: &NativeStore) -> usize {
    let mut v = store.begin().expect("begin");
    v.history(HistoryPosition(0))
        .expect("history")
        .collect::<Result<Vec<_>, _>>()
        .expect("history entry")
        .len()
}

/// The complete logical state, read through the port.
fn logical_state(store: &NativeStore) -> Vec<String> {
    let mut v = store.begin().expect("begin");
    let mut out = Vec::new();
    for id in IDS {
        let id = cid(id);
        match v.context(&id).expect("context") {
            None => out.push(format!("{id:?} absent")),
            Some(r) => {
                out.push(format!("{id:?} rev {} body {:?}", r.revision.0, r.body));
                if let Some(c) = r.content {
                    let mut bytes = Vec::new();
                    v.content(&c)
                        .expect("content")
                        .read_to_end(&mut bytes)
                        .expect("read content");
                    out.push(format!(
                        "{id:?} content {} bytes, sum {}",
                        bytes.len(),
                        bytes.iter().map(|&b| b as u64).sum::<u64>()
                    ));
                }
            }
        }
        for d in [Direction::Outgoing, Direction::Incoming] {
            let q = RelationshipQuery {
                from: id.clone(),
                direction: d,
                kind: None,
                after: None,
            };
            for r in v.relationships(&q).expect("rels") {
                out.push(format!("{id:?} {d:?} {:?}", r.expect("rel")));
            }
        }
    }
    for h in v.history(HistoryPosition(0)).expect("history") {
        let h = h.expect("history entry");
        out.push(format!(
            "history {} {:?} {:?} {} changes",
            h.position.0,
            h.attempt,
            h.evidence,
            h.changes.len()
        ));
    }
    out
}

/// Independent expected states after each committed step, from an uninterrupted run.
fn reference_states(steps: u32) -> Vec<Vec<String>> {
    let s = create(Box::new(SimIo::new()));
    let mut states = vec![logical_state(&s)];
    for step in 1..=steps {
        assert!(commit_step(&s, step));
        states.push(logical_state(&s));
    }
    states
}

#[test]
fn failed_opens_change_nothing_and_a_clean_open_recovers_the_committed_state() {
    let expected = reference_states(2).pop().unwrap();

    let sim = SimIo::new();
    let s = create(Box::new(sim.clone()));
    assert!(commit_step(&s, 1));
    assert!(commit_step(&s, 2));
    drop(s);
    let img = sim.after_process_exit();
    let before = images(&img);

    // The first mutating operation of open is its content barrier.
    img.fail_nth_op(1);
    let err = try_open(Box::new(img.clone()))
        .err()
        .expect("open must fail");
    assert!(
        matches!(&err, StorageError::Io(m) if m.contains("simulated I/O error")),
        "{err:?}"
    );
    assert_eq!(
        images(&img),
        before,
        "a failed open changed the store files"
    );
    // A failed open does not give the ownership lock back.
    assert!(
        matches!(try_open(Box::new(img.clone())), Err(StorageError::Busy(_))),
        "the lock taken by a failed open must persist until released"
    );
    img.release();

    img.fail_next_read();
    let err = try_open(Box::new(img.clone()))
        .err()
        .expect("open must fail");
    assert!(
        matches!(&err, StorageError::Io(m) if m.contains("simulated read error")),
        "{err:?}"
    );
    assert_eq!(
        images(&img),
        before,
        "a failed open changed the store files"
    );
    img.release();

    let s = open(Box::new(img.clone()));
    assert_eq!(logical_state(&s), expected);
    assert_eq!(history_len(&s), 2);
    let first = images(&img);
    // Opening only makes existing bytes durable; it never writes.
    let volatile = |v: &[Option<(Vec<u8>, Vec<u8>)>]| -> Vec<Vec<u8>> {
        v.iter()
            .map(|f| f.clone().expect("file exists").1)
            .collect()
    };
    assert_eq!(volatile(&first), volatile(&before));
    for f in &first {
        let (durable, volatile) = f.as_ref().expect("file exists");
        assert_eq!(durable, volatile, "open left referenced bytes non-durable");
    }
    drop(s);
    img.release();

    let s = open(Box::new(img.clone()));
    assert_eq!(logical_state(&s), expected);
    assert_eq!(history_len(&s), 2);
    assert_eq!(
        images(&img),
        first,
        "reopening mutated or duplicated store bytes"
    );
}

#[test]
fn every_cut_of_store_creation_opens_the_empty_store_or_refuses_to_initialize() {
    let empty = reference_states(0).pop().unwrap();

    let probe = SimIo::new();
    drop(create(Box::new(probe.clone())));
    let total = probe.ops();
    assert!(total > 0);

    let mut opened = 0;
    let mut refused = 0;
    for cut in 0..=total {
        let sim = SimIo::new();
        sim.crash_after(cut);
        let created = Inner::create(Box::new(sim.clone()), STORE, Default::default());
        assert_eq!(created.is_ok(), cut == total, "cut {cut}: creation result");
        drop(created);
        for p in [Persist::None, Persist::All] {
            let img = sim.after_crash(p);
            let before = images(&img);
            match try_open(Box::new(img.clone())) {
                Ok(s) => {
                    assert_eq!(logical_state(&s), empty, "cut {cut} {p:?}");
                    assert_eq!(history_len(&s), 0, "cut {cut} {p:?}");
                    assert_eq!(generation(&s), 1, "cut {cut} {p:?}");
                    opened += 1;
                }
                // Before the directory barrier no store file survives: there is no store.
                Err(StorageError::StoreState(m))
                    if before.iter().all(Option::is_none) && m.contains("missing") =>
                {
                    refused += 1;
                }
                Err(StorageError::Integrity(m))
                    if m.contains("initialization never completed")
                        || m.contains("no valid publication record") =>
                {
                    assert_eq!(
                        images(&img),
                        before,
                        "cut {cut} {p:?}: a refused open wrote to the store"
                    );
                    refused += 1;
                }
                Err(e) => panic!("cut {cut} {p:?}: unexpected open result {e:?}"),
            }
        }
    }
    // The completed creation always opens; some interrupted creation is always refused.
    assert!(opened >= 2, "opened {opened}");
    assert!(refused > 0, "refused {refused}");
    assert_eq!(opened + refused, (total + 1) * 2);
}

#[test]
fn a_new_generation_is_invisible_until_its_publication_barrier_returns() {
    // Observes the store's visible generation at the barrier that follows each publication
    // record write.
    struct ObservePublication {
        sim: SimIo,
        store: Arc<OnceLock<Weak<Inner>>>,
        record_written: AtomicBool,
        seen: Arc<Mutex<Vec<u64>>>,
    }
    impl Io for ObservePublication {
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
            self.sim.write_at(file, offset, data)?;
            if file == FileId::Meta && offset < 2 * PAGE_SIZE as u64 {
                self.record_written.store(true, Ordering::SeqCst);
            }
            Ok(())
        }
        fn len(&self, file: FileId) -> Result<u64, StorageError> {
            self.sim.len(file)
        }
        fn sync(&self, file: FileId) -> Result<(), StorageError> {
            self.sim.sync(file)?;
            if file == FileId::Meta
                && self.record_written.swap(false, Ordering::SeqCst)
                && let Some(inner) = self.store.get().and_then(Weak::upgrade)
            {
                self.seen
                    .lock()
                    .unwrap()
                    .push(inner.snapshot()?.desc.generation);
            }
            Ok(())
        }
        fn sync_dir(&self) -> Result<(), StorageError> {
            self.sim.sync_dir()
        }
    }

    let hook = Arc::new(OnceLock::new());
    let seen = Arc::new(Mutex::new(Vec::new()));
    let io = ObservePublication {
        sim: SimIo::new(),
        store: hook.clone(),
        record_written: AtomicBool::new(false),
        seen: seen.clone(),
    };
    let s = create(Box::new(io));
    hook.set(Arc::downgrade(&s.inner)).unwrap();
    assert_eq!(generation(&s), 1);
    assert!(commit_step(&s, 1));
    assert_eq!(generation(&s), 2);
    assert!(commit_step(&s, 2));
    assert_eq!(generation(&s), 3);
    assert_eq!(
        *seen.lock().unwrap(),
        vec![1, 2],
        "a generation became visible before its publication barrier returned"
    );
}

#[test]
fn an_indeterminate_publication_leaves_new_views_on_the_old_state() {
    let states = reference_states(1);

    let probe = SimIo::new();
    let s = create(Box::new(probe.clone()));
    assert!(commit_step(&s, 1));
    let before = probe.ops();
    assert!(commit_step(&s, 2));
    let per_commit = probe.ops() - before;

    let sim = SimIo::new();
    let s = create(Box::new(sim.clone()));
    assert!(commit_step(&s, 1));
    let old = generation(&s);
    // The last operation of a commit is the publication barrier.
    sim.fail_nth_op(per_commit);
    let (attempt, cs) = request(&s, 2).expect("stage");
    let err = s
        .commit(attempt.clone(), s.begin().unwrap(), cs)
        .unwrap_err();
    assert!(
        matches!(&err, CommitError::Indeterminate(m) if m.contains("uncertain")),
        "{err:?}"
    );
    assert_eq!(s.outcome(&attempt).unwrap(), AttemptOutcome::Unknown);
    assert_eq!(generation(&s), old, "uncertain generation became visible");
    assert_eq!(logical_state(&s), states[1]);
    assert_eq!(history_len(&s), 1);
}
