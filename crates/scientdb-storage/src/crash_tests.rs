//! Crash and adapter-parity tests: the production engine under the simulator and the filesystem.
//!
//! Expected states come from an independent run of the same logical requests, never from what
//! recovery chose.

use std::io::Read;
use std::sync::{Arc, Mutex};

use scientdb_kernel::storage::{
    AttemptId, Change, ChangeSet, ContentBinding, ContextId, Direction, Evidence, HistoryPosition,
    Precondition, ReadView, RelationshipId, RelationshipKind, RelationshipQuery, StorageEngine,
};

use crate::NativeStore;
use crate::engine::Inner;
use crate::format::StoreId;
use crate::io::{FsIo, Io, Op, Traced};
use crate::sim::{Persist, SimIo};

const STORE: StoreId = StoreId(*b"crash-test-store");
const IDS: [&str; 4] = ["A", "B", "C", "D"];

fn cid(s: &str) -> ContextId {
    ContextId::new(s.as_bytes().to_vec()).unwrap()
}

fn traced<I: Io + 'static>(io: I, trace: &Arc<Mutex<Vec<Op>>>) -> Box<dyn Io> {
    Box::new(Traced {
        inner: io,
        trace: trace.clone(),
    })
}

fn create(io: Box<dyn Io>) -> NativeStore {
    NativeStore {
        inner: Inner::create(io, STORE, Default::default()).expect("create"),
    }
}

fn open(io: Box<dyn Io>) -> NativeStore {
    NativeStore {
        inner: Inner::open(io, Default::default()).expect("recovery must open the store"),
    }
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
    let sim = SimIo::new();
    let s = create(Box::new(sim));
    let mut states = vec![logical_state(&s)];
    for step in 1..=steps {
        assert!(commit_step(&s, step));
        states.push(logical_state(&s));
    }
    states
}

#[test]
fn every_cut_of_a_commit_recovers_a_whole_old_or_new_state() {
    let states = reference_states(2);
    let (old, new) = (&states[1], &states[2]);
    assert_ne!(old, new);

    // Count the mutating operations of the second commit (staging included).
    let probe = SimIo::new();
    let s = create(Box::new(probe.clone()));
    assert!(commit_step(&s, 1));
    let trace = Arc::new(Mutex::new(Vec::new()));
    drop(s);
    probe.release();
    let s = open(traced(probe.clone(), &trace));
    // Count only the commit's own operations (opening also syncs).
    trace.lock().unwrap().clear();
    let before = probe.ops();
    assert!(commit_step(&s, 2));
    let total = probe.ops() - before;
    let ops = trace.lock().unwrap().clone();
    assert_eq!(ops.len() as u64, total);
    // The publication record write is the second-to-last operation.
    let publication = total - 2;

    let mut checked = 0;
    for cut in 0..=total {
        let sim = SimIo::new();
        let s = create(Box::new(sim.clone()));
        assert!(commit_step(&s, 1));
        sim.crash_after(cut);
        let acknowledged = commit_step(&s, 2);
        assert_eq!(
            acknowledged,
            cut == total,
            "cut {cut}: acknowledgment only after every barrier"
        );
        drop(s);
        let persists = [Persist::None, Persist::All]
            .into_iter()
            .chain((1..=12).map(Persist::Seeded));
        for p in persists {
            let img = sim.after_crash(p);
            let recovered = open(Box::new(img.clone()));
            let got = logical_state(&recovered);
            assert!(
                &got == old || &got == new,
                "cut {cut} {p:?}: recovered a partial state:\n{got:#?}"
            );
            if acknowledged {
                assert_eq!(&got, new, "cut {cut} {p:?}: acknowledged commit lost");
            }
            if cut <= publication {
                assert_eq!(
                    &got, old,
                    "cut {cut} {p:?}: change visible before its publication record was written"
                );
            }
            // The recovered store keeps working: commit again and reopen.
            assert!(
                commit_step(&recovered, 3),
                "cut {cut} {p:?}: store unusable after recovery"
            );
            let after = logical_state(&recovered);
            drop(recovered);
            img.release();
            assert_eq!(logical_state(&open(Box::new(img))), after);
            checked += 1;
        }
    }
    assert_eq!(checked, (total + 1) * 14);
}

#[test]
fn filesystem_and_simulator_run_the_same_protocol() {
    fn scenario(io_for: &mut dyn FnMut(bool) -> Box<dyn Io>) -> Vec<String> {
        let s = create(io_for(true));
        assert!(commit_step(&s, 1));
        assert!(commit_step(&s, 2));
        s.close().expect("close");
        let s = open(io_for(false));
        logical_state(&s)
    }

    let sim_trace = Arc::new(Mutex::new(Vec::new()));
    let sim = SimIo::new();
    let sim_state = scenario(&mut |_| {
        sim.release();
        traced(sim.clone(), &sim_trace)
    });

    let fs_trace = Arc::new(Mutex::new(Vec::new()));
    let dir = std::env::temp_dir().join(format!("scientdb-parity-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let fs_state = scenario(&mut |_| traced(FsIo::new(&dir), &fs_trace));
    let _ = std::fs::remove_dir_all(&dir);

    assert_eq!(
        sim_state, fs_state,
        "logical results differ between adapters"
    );
    let (a, b) = (
        sim_trace.lock().unwrap().clone(),
        fs_trace.lock().unwrap().clone(),
    );
    assert!(!a.is_empty());
    assert_eq!(a, b, "the adapters saw different persistence protocols");

    // Required ordering: every commit syncs data before writing its publication record and
    // syncs that record before acknowledging.
    let writes_desc = |op: &Op| matches!(op, Op::Write { file: crate::io::FileId::Meta, offset, .. } if *offset < 2 * crate::format::PAGE_SIZE as u64);
    for (i, _) in a.iter().enumerate().filter(|(_, op)| writes_desc(op)) {
        assert!(
            i >= 2 && matches!(a[i - 1], Op::Sync { .. }) && matches!(a[i - 2], Op::Sync { .. })
                || i < 2,
            "publication at {i} not preceded by data barriers: {a:?}"
        );
        assert!(
            matches!(
                a.get(i + 1),
                Some(Op::Sync {
                    file: crate::io::FileId::Meta
                })
            ),
            "publication at {i} not followed by a barrier"
        );
    }

    // A repeated simulated run reproduces the trace exactly.
    let again = Arc::new(Mutex::new(Vec::new()));
    let sim2 = SimIo::new();
    let state2 = scenario(&mut |_| {
        sim2.release();
        traced(sim2.clone(), &again)
    });
    assert_eq!(state2, sim_state);
    assert_eq!(*again.lock().unwrap(), a);
}

#[test]
fn failed_publication_barrier_is_indeterminate_and_fences_writes() {
    let probe = SimIo::new();
    let s = create(Box::new(probe.clone()));
    assert!(commit_step(&s, 1));
    let before = probe.ops();
    assert!(commit_step(&s, 2));
    let per_commit = probe.ops() - before;

    let sim = SimIo::new();
    let s = create(Box::new(sim.clone()));
    assert!(commit_step(&s, 1));
    // The last operation of a commit is the publication barrier.
    sim.fail_nth_op(per_commit);
    let (attempt, cs) = request(&s, 2).expect("stage");
    let err = s
        .commit(attempt.clone(), s.begin().unwrap(), cs)
        .unwrap_err();
    assert!(
        matches!(err, scientdb_kernel::storage::CommitError::Indeterminate(_)),
        "{err:?}"
    );
    assert_eq!(
        s.outcome(&attempt).unwrap(),
        scientdb_kernel::storage::AttemptOutcome::Unknown
    );
    // Writes stay fenced: no further commit may proceed on unknown state.
    assert!(
        !commit_step(&s, 3),
        "commit accepted while publication outcome unknown"
    );
    // A neighbouring healthy store still commits (the fence is not a blanket refusal).
    let ok = create(Box::new(SimIo::new()));
    assert!(commit_step(&ok, 1));
}
