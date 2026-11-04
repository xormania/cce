//! Saving a failing sweep case for offline reproduction, and naming what survived a simulated
//! crash: shared by the crash and recovery sweeps, and tested here.

use std::panic::{AssertUnwindSafe, catch_unwind, resume_unwind};
use std::path::PathBuf;

use crate::io::{FileId, Op};
use crate::sim::{Persist, SimIo};

/// Names which unsynced writes survive a power loss under `p`. The seeded choices are a sample of
/// the possible subsets, not an enumeration of them.
pub(crate) fn survival(p: Persist) -> String {
    match p {
        Persist::None => "power loss, no unsynced write survives".into(),
        Persist::All => "power loss, every unsynced write survives".into(),
        Persist::Seeded(seed) => {
            format!("power loss, sampled subset of unsynced writes, seed {seed}")
        }
    }
}

/// Names the image a process exit leaves: unsynced writes are still in the operating system's
/// cache.
pub(crate) const PROCESS_EXIT: &str = "process exit, unsynced writes still cached";

/// Runs one case of a sweep. If it fails, the images of `images` (durable bytes, and cached bytes
/// where they differ) and the operation trace are written to a new directory under the system
/// temporary directory, with the case label in `case.txt`, and the failure is re-raised naming that
/// directory, so the failing case can be reproduced offline. Nothing is written when the case
/// passes.
pub(crate) fn with_repro(
    label: &str,
    images: &[(&str, &SimIo)],
    trace: &[Op],
    case: impl FnOnce(),
) {
    let Err(panic) = catch_unwind(AssertUnwindSafe(case)) else {
        return;
    };
    let why = panic
        .downcast_ref::<String>()
        .cloned()
        .or_else(|| panic.downcast_ref::<&str>().map(|s| s.to_string()))
        .unwrap_or_else(|| "non-string panic".into());
    match save_repro(label, images, trace) {
        Ok(dir) => panic!(
            "{label}: {why}\nreproduction images and trace: {}",
            dir.display()
        ),
        Err(e) => {
            eprintln!("{label}: could not save reproduction images: {e}");
            resume_unwind(panic)
        }
    }
}

fn save_repro(label: &str, images: &[(&str, &SimIo)], trace: &[Op]) -> std::io::Result<PathBuf> {
    let safe: String = label
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    let dir = std::env::temp_dir().join(format!(
        "cce-repro-{}-{safe}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos())
    ));
    std::fs::create_dir_all(&dir)?;
    for (name, sim) in images {
        for file in [FileId::Meta, FileId::Content] {
            if let Some((durable, volatile)) = sim.file_images_for_test(file) {
                // A process-exit image also holds bytes only the operating system's cache has.
                if volatile != durable {
                    std::fs::write(
                        dir.join(format!("{name}.{}.cached", file.name())),
                        &volatile,
                    )?;
                }
                std::fs::write(dir.join(format!("{name}.{}.durable", file.name())), durable)?;
            }
        }
    }
    let ops: String = trace.iter().map(|op| format!("{op:?}\n")).collect();
    std::fs::write(dir.join("trace.txt"), ops)?;
    std::fs::write(dir.join("case.txt"), format!("{label}\n"))?;
    Ok(dir)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::io::Io;

    /// Reproduction directories whose case label starts with `label`.
    fn dirs(label: &str) -> Vec<PathBuf> {
        let prefix = format!("cce-repro-{}-{label}", std::process::id());
        std::fs::read_dir(std::env::temp_dir())
            .unwrap()
            .map(|e| e.unwrap().path())
            .filter(|p| {
                p.file_name()
                    .unwrap()
                    .to_string_lossy()
                    .starts_with(&prefix)
            })
            .collect()
    }

    fn image() -> SimIo {
        let sim = SimIo::new();
        sim.create().unwrap();
        sim.sync_dir().unwrap();
        sim.write_at(FileId::Meta, 0, b"durable").unwrap();
        sim.sync(FileId::Meta).unwrap();
        // Still cached only: a process exit keeps it, a power loss may not.
        sim.write_at(FileId::Meta, 7, b"cached").unwrap();
        sim
    }

    #[test]
    fn a_passing_case_writes_nothing() {
        let sim = image();
        let mut ran = false;
        with_repro("selftest-pass", &[("image", &sim)], &[Op::SyncDir], || {
            ran = true
        });
        assert!(ran);
        assert!(dirs("selftest-pass").is_empty());
    }

    #[test]
    fn a_failing_case_saves_its_images_trace_and_label_and_names_the_directory() {
        let sim = image();
        let trace = [Op::SyncDir, Op::Sync { file: FileId::Meta }];
        let r = catch_unwind(AssertUnwindSafe(|| {
            with_repro("selftest-fail", &[("image", &sim)], &trace, || {
                panic!("the case failed")
            })
        }));
        let msg = *r.unwrap_err().downcast::<String>().unwrap();
        assert!(msg.contains("selftest-fail: the case failed"), "{msg}");
        let found = dirs("selftest-fail");
        assert_eq!(found.len(), 1);
        let dir = &found[0];
        assert!(msg.ends_with(&dir.display().to_string()), "{msg}");
        let read = |f: &str| std::fs::read(dir.join(f)).unwrap();
        assert_eq!(read("image.meta.cce.durable"), b"durable");
        assert_eq!(read("image.meta.cce.cached"), b"durablecached");
        assert_eq!(read("image.content.cce.durable"), b"");
        assert!(
            !dir.join("image.content.cce.cached").exists(),
            "nothing cached-only"
        );
        assert_eq!(read("case.txt"), b"selftest-fail\n");
        assert_eq!(read("trace.txt"), b"SyncDir\nSync { file: Meta }\n");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn survival_labels_name_the_choice_and_say_seeded_subsets_are_sampled() {
        assert_eq!(
            survival(Persist::None),
            "power loss, no unsynced write survives"
        );
        assert_eq!(
            survival(Persist::All),
            "power loss, every unsynced write survives"
        );
        assert_eq!(
            survival(Persist::Seeded(4)),
            "power loss, sampled subset of unsynced writes, seed 4"
        );
    }
}
