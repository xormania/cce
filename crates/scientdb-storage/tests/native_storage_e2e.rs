//! End-to-end tests: a parent test process drives the native engine through a separate driver
//! process and checks results against independently computed expectations.

mod common;

use std::time::Duration;

use common::story::first_bundle;
use common::{Driver, Step, answered, test_dir};

#[test]
fn harness_distinguishes_answer_exit_and_hang() {
    let mut ok = Driver::spawn();
    assert_eq!(ok.step("probe ok"), Step::Answered(vec!["probe ok".into()]));
    assert_eq!(ok.finish().and_then(|s| s.code()), Some(0));

    let mut failing = Driver::spawn();
    assert_eq!(
        failing.step("probe fail"),
        Step::Exited {
            lines: vec!["probe failing".into()],
            code: Some(3)
        }
    );

    let mut hanging = Driver::spawn();
    assert_eq!(
        hanging.step_with_deadline("probe hang", Duration::from_millis(300)),
        Step::Hung {
            lines: vec!["probe hanging".into()]
        }
    );
    let status = hanging.kill();
    assert!(
        !status.success(),
        "a killed hanging driver must not report success"
    );
}

#[test]
fn committed_bundle_survives_writer_kill_and_fresh_process_reopen() {
    let dir = test_dir("bundle-kill-reopen");
    let store = dir.join("store");
    let bundle = first_bundle();

    // Writer process: create, stage, submit one complete transaction, then hold without shutdown.
    let mut writer = Driver::spawn();
    answered(
        writer.step(&format!("create {}", store.display())),
        "create",
    );
    for cmd in bundle.commands() {
        answered(writer.step(&cmd), &cmd);
    }
    let receipt = answered(writer.step(&format!("commit {}", bundle.attempt)), "commit");
    assert_eq!(
        receipt,
        vec!["committed 1".to_string()],
        "commit must be acknowledged at position 1"
    );
    assert_eq!(writer.wait_for("hold", "holding"), Step::Answered(vec![]));
    let status = writer.kill();
    assert!(
        status.code().is_none(),
        "writer must die by signal, not exit: {status:?}"
    );

    // Fresh process: acquire the store and observe the complete committed state.
    let mut reader = Driver::spawn();
    answered(reader.step(&format!("open {}", store.display())), "open");
    let dump = answered(reader.step("dump A,B,C"), "dump");
    let expected = bundle.expected_dump(1);
    assert_eq!(dump.len(), expected.len(), "dump line count");
    for (got, want) in dump.iter().zip(&expected) {
        assert_eq!(got, want);
    }
    answered(reader.step("close"), "close");
}
