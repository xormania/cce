//! Publication record selection on open, end to end through the simulator: a damaged or torn
//! newest record is ignored and reported, never selected; damage to what the newest valid record
//! references is an integrity error, never a silent return to an older generation.

use crate::test_support::{cid, create_on, open_on};

use std::io::Read;

use cce_core::storage::{
    AttemptId, AttemptOutcome, Change, ChangeSet, ContentBinding, Evidence,
    HistoryPosition, Precondition, ReadView, StorageEngine, StorageError,
};

use crate::NativeStore;
use crate::format::{DESC_MAGIC, Descriptor, PAGE_HEADER_LEN, PAGE_SIZE, StoreId};
use crate::io::{FileId, Io};
use crate::sim::SimIo;

const STORE: StoreId = StoreId(*b"descriptor-tests");
const IDS: [&str; 2] = ["A", "B"];
/// Encoded publication record length: magic, version, store, six counters, checksum.
const RECORD_LEN: usize = 4 + 2 + 16 + 8 * 6 + 4;

fn step_attempt(step: u32) -> AttemptId {
    AttemptId::new(format!("attempt-{step}").into_bytes()).unwrap()
}

fn try_open(sim: &SimIo) -> Result<NativeStore, StorageError> {
    open_on(Box::new(sim.clone()), Default::default())
}

fn open(sim: &SimIo) -> NativeStore {
    try_open(sim).expect("open")
}

fn commit_step(s: &NativeStore, step: u32) {
    let payload: Vec<u8> = (0..(9_000 + step as usize * 7))
        .map(|i| (i * 31 + step as usize) as u8)
        .collect();
    let staged = s.stage_content(&mut &payload[..]).unwrap();
    let cs = ChangeSet {
        changes: vec![Change::PutContext {
            id: cid(IDS[step as usize % 2]),
            expect: Precondition::Any,
            body: format!("body at step {step}").into_bytes(),
            content: Some(0),
        }],
        content: vec![ContentBinding::from(&staged)],
        evidence: vec![Evidence {
            label: b"decision".to_vec(),
            bytes: format!("evidence {step}").into_bytes(),
        }],
    };
    let r = s
        .commit(step_attempt(step), s.begin().unwrap(), cs)
        .unwrap();
    assert_eq!(r.position, HistoryPosition(step as u64));
}

fn logical_state(s: &NativeStore) -> Vec<String> {
    let mut v = s.begin().unwrap();
    let mut out = Vec::new();
    for id in IDS {
        match v.context(&cid(id)).unwrap() {
            None => out.push(format!("{id} absent")),
            Some(r) => {
                let mut bytes = Vec::new();
                if let Some(c) = r.content {
                    v.content(&c).unwrap().read_to_end(&mut bytes).unwrap();
                }
                out.push(format!("{r:?} bytes {bytes:?}"));
            }
        }
    }
    for h in v.history(HistoryPosition(0)).unwrap() {
        out.push(format!("{:?}", h.unwrap()));
    }
    out
}

fn descriptor(s: &NativeStore) -> Descriptor {
    s.inner.snapshot().unwrap().desc
}

fn read_slot(sim: &SimIo, slot: u64) -> Vec<u8> {
    let mut buf = vec![0; PAGE_SIZE];
    sim.read_at(FileId::Meta, slot * PAGE_SIZE as u64, &mut buf)
        .unwrap();
    buf
}

/// Replaces a metadata page in the durable image.
fn write_durably(sim: &SimIo, offset: u64, bytes: &[u8]) {
    sim.write_at(FileId::Meta, offset, bytes).unwrap();
    sim.sync(FileId::Meta).unwrap();
}

/// A store with generations 1 (create), 2 (step 1, slot 0) and 3 (step 2, slot 1).
struct TwoCommits {
    sim: SimIo,
    /// Durable image of slot 1 before step 2 overwrote it (the generation 1 record).
    gen1_slot: Vec<u8>,
    gen2: Descriptor,
    gen3: Descriptor,
    /// Logical state after step 1 (generation 2).
    old: Vec<String>,
    /// Logical state after step 2 (generation 3).
    new: Vec<String>,
}

fn two_commits() -> TwoCommits {
    let sim = SimIo::new();
    let s = create_on(Box::new(sim.clone()), STORE, Default::default()).unwrap();
    commit_step(&s, 1);
    let gen2 = descriptor(&s);
    let old = logical_state(&s);
    let gen1_slot = read_slot(&sim, 1);
    commit_step(&s, 2);
    let gen3 = descriptor(&s);
    let new = logical_state(&s);
    drop(s);
    sim.release();
    assert_eq!((gen2.generation, gen3.generation), (2, 3));
    assert_eq!(Descriptor::slot(gen3.generation), 1);
    assert_eq!(read_slot(&sim, 1), gen3.encode());
    assert_ne!(old, new);
    TwoCommits {
        sim,
        gen1_slot,
        gen2,
        gen3,
        old,
        new,
    }
}

/// Opens `img` and requires exactly the generation 2 store, with nothing of generation 3 visible,
/// and the given diagnostics.
fn assert_recovers_generation_2(t: &TwoCommits, img: &SimIo, diagnostics: &[&str], what: &str) {
    let s = open(img);
    assert_eq!(descriptor(&s), t.gen2, "{what}: selected record");
    assert_eq!(logical_state(&s), t.old, "{what}: recovered state");
    assert_eq!(
        s.outcome(&step_attempt(2)).unwrap(),
        AttemptOutcome::Absent,
        "{what}: commit of the damaged record visible"
    );
    assert_eq!(s.recovery_diagnostics(), diagnostics, "{what}: diagnostics");
}

const CHECKSUM_DIAGNOSTIC: &str = "slot 1: damaged or torn (checksum mismatch); ignored";

#[test]
fn a_newest_record_with_a_bad_checksum_is_ignored_and_the_previous_generation_recovered() {
    let t = two_commits();
    // Positive neighbour: undamaged, the newest record is selected without diagnostics.
    let s = open(&t.sim.after_process_exit());
    assert_eq!(descriptor(&s), t.gen3);
    assert_eq!(logical_state(&s), t.new);
    assert!(s.recovery_diagnostics().is_empty());
    drop(s);

    // Every single-byte change after the magic: version, store, counters, checksum.
    for at in DESC_MAGIC.len()..RECORD_LEN {
        let img = t.sim.after_process_exit();
        let mut slot = read_slot(&img, 1);
        slot[at] ^= 0x5a;
        assert_eq!(slot[..4], DESC_MAGIC);
        write_durably(&img, PAGE_SIZE as u64, &slot);
        assert_recovers_generation_2(
            &t,
            &img,
            &[CHECKSUM_DIAGNOSTIC],
            &format!("byte {at} changed"),
        );
    }
}

#[test]
fn a_torn_newest_record_is_ignored_and_the_previous_generation_recovered() {
    let t = two_commits();
    let gen3 = t.gen3.encode();
    // Only the first `cut` bytes of the generation 3 record reached the slot that held generation 1.
    for cut in 1..RECORD_LEN {
        let img = t.sim.after_process_exit();
        let mut slot = t.gen1_slot.clone();
        slot[..cut].copy_from_slice(&gen3[..cut]);
        write_durably(&img, PAGE_SIZE as u64, &slot);
        // A prefix the two records share leaves an intact generation 1 record behind.
        let expected: &[&str] = if slot == t.gen1_slot {
            &[]
        } else {
            &[CHECKSUM_DIAGNOSTIC]
        };
        assert_recovers_generation_2(&t, &img, expected, &format!("torn after {cut} bytes"));
    }
}

#[test]
fn a_store_recovered_past_a_damaged_record_commits_again_and_replaces_it() {
    let t = two_commits();
    let img = t.sim.after_process_exit();
    let mut slot = read_slot(&img, 1);
    slot[RECORD_LEN - 1] ^= 1;
    write_durably(&img, PAGE_SIZE as u64, &slot);
    let s = open(&img);
    assert_eq!(descriptor(&s).generation, 2);
    // The unrecovered attempt is absent, so retrying it commits it at the next position.
    commit_step(&s, 2);
    assert_eq!(descriptor(&s).generation, 3);
    let after = logical_state(&s);
    drop(s);
    img.release();
    let s = open(&img);
    assert!(
        s.recovery_diagnostics().is_empty(),
        "damaged slot rewritten"
    );
    assert_eq!(descriptor(&s).generation, 3);
    assert_eq!(logical_state(&s), after);
}

#[test]
fn newest_record_beyond_the_content_file_is_an_error_not_the_previous_generation() {
    let t = two_commits();
    // Generation 2 is fully intact; generation 3's committed content is cut off.
    let img = t.sim.after_process_exit();
    assert!(t.gen2.content_len < t.gen3.content_len);
    img.truncate_for_test(FileId::Content, t.gen2.content_len as usize);
    let err = try_open(&img).err().expect("open must fail");
    assert!(
        matches!(&err, StorageError::Integrity(m) if m.contains("generation 3")),
        "{err:?}"
    );
    // The image is unchanged by the refused open: the intact store still opens at generation 3
    // once its content is back.
    let s = open(&t.sim.after_process_exit());
    assert_eq!(descriptor(&s), t.gen3);
}

#[test]
fn a_damaged_root_page_of_the_newest_record_is_an_error_not_the_previous_generation() {
    let t = two_commits();
    let img = t.sim.after_process_exit();
    let root = t.gen3.root.page * PAGE_SIZE as u64;
    assert_ne!(t.gen3.root.page, t.gen2.root.page);
    let mut page = vec![0; PAGE_SIZE];
    img.read_at(FileId::Meta, root, &mut page).unwrap();
    // Damage the first payload byte, not the header: the page still claims to be generation 3's
    // root. (Bytes past the payload length are padding and not part of the node.)
    page[PAGE_HEADER_LEN] ^= 0xff;
    write_durably(&img, root, &page);

    match try_open(&img) {
        Err(e) => assert!(matches!(e, StorageError::Integrity(_)), "{e:?}"),
        Ok(s) => {
            // Opening reads only the publication record; generation 3 stays current and its first
            // read reports the damage instead of answering from generation 2.
            assert_eq!(
                descriptor(&s),
                t.gen3,
                "older generation presented as current"
            );
            let r = s.begin().unwrap().context(&cid("A"));
            assert!(matches!(r, Err(StorageError::Integrity(_))), "{r:?}");
            let r = s.outcome(&step_attempt(1));
            assert!(matches!(r, Err(StorageError::Integrity(_))), "{r:?}");
        }
    }
}
