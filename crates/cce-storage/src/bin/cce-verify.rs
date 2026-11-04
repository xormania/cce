//! `cce-verify <store-dir> [--json]`: inspects a store's structure without changing it.
//!
//! Opens the store read-only, which is refused while another process owns it, so it never runs
//! against a live owner. Prints a summary, the publication slots, recovery diagnostics and every
//! inconsistency found; with `--json`, one JSON object per line in a deterministic order. Output
//! never contains stored bodies, content, evidence or identifiers.
//!
//! Exit status: 0 consistent; 1 inconsistencies found; 2 the store could not be inspected (missing,
//! owned by another process, of an unsupported format, or unreadable) or the arguments are wrong.

use std::path::PathBuf;
use std::process::ExitCode;

use cce_storage::{NativeStore, VerifyLevel};

const USAGE: &str = "usage: cce-verify <store-dir> [--json]";

fn main() -> ExitCode {
    let mut dir = None;
    let mut json = false;
    for arg in std::env::args().skip(1) {
        if arg == "--json" {
            json = true;
        } else if dir.is_none() && !arg.starts_with('-') {
            dir = Some(PathBuf::from(arg));
        } else {
            eprintln!("{USAGE}");
            return ExitCode::from(2);
        }
    }
    let Some(dir) = dir else {
        eprintln!("{USAGE}");
        return ExitCode::from(2);
    };
    let report =
        match NativeStore::open_read_only(&dir).and_then(|s| s.verify(VerifyLevel::Structure)) {
            Ok(r) => r,
            Err(e) => {
                eprintln!("cce-verify: cannot inspect {}: {e}", dir.display());
                return ExitCode::from(2);
            }
        };
    if json {
        for line in report.json_lines() {
            println!("{line}");
        }
    } else {
        println!(
            "generation {} at history position {}: {} pages, {} records",
            report.generation, report.history_position, report.pages, report.records
        );
        for (i, s) in report.slots.iter().enumerate() {
            println!("slot {i}: {s}");
        }
        for d in &report.diagnostics {
            println!("diagnostic: {d}");
        }
        for f in &report.findings {
            println!("{} at {}: {}", f.check, f.location, f.detail);
        }
        if report.is_consistent() {
            println!("consistent");
        } else {
            println!("{} inconsistencies", report.findings.len());
        }
    }
    if report.is_consistent() {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(1)
    }
}
