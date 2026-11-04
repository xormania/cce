"""Behavioral controls for the comparative runner; Cargo outcomes use protocol fixtures."""
from contextlib import redirect_stdout
import io
import json
import os
from pathlib import Path
import signal
import subprocess
import sys
import tempfile
from types import SimpleNamespace
import unittest
from unittest import mock

ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT / "tools/testkit"))
import sidebyside as s


class ComparisonTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.repo = self.root / "repo"
        self.repo.mkdir()
        self.git("init", "-q")
        self.git("config", "user.name", "Test fixture")
        self.git("config", "user.email", "fixture@example.test")
        p = self.repo / "crates/cce-storage/src/engine.rs"
        p.parent.mkdir(parents=True)
        p.write_text("healthy\n")
        w = self.repo / "tests/witness.rs"
        w.parent.mkdir()
        w.write_text("assert!(observed);\n")
        self.git("add", ".")
        self.git("commit", "-qm", "Original fixture")
        self.original = self.git("rev-parse", "HEAD").strip()
        (self.repo / "tests/candidate.txt").write_text("independent candidate suite")
        self.git("add", ".")
        self.git("commit", "-qm", "Candidate fixture")
        self.candidate = self.git("rev-parse", "HEAD").strip()
        self.fault = {"id": "fixture", "path": str(p.relative_to(self.repo)),
                      "old": "healthy", "new": "broken",
                      "test": "fixture", "witness": "tests/witness.rs"}
        self.catalog = self.root / "faults.json"
        self.catalog.write_text(json.dumps([self.fault]))
        self.args = SimpleNamespace(repo=str(self.repo), output=str(self.root / "evidence"),
            catalog=str(self.catalog), fault=[], original=self.original,
            candidate=self.candidate, production=self.original, cargo="cargo", timeout=5)

    def git(self, *args):
        return subprocess.check_output(["git", "-C", str(self.repo), *args],
                                       text=True, stderr=subprocess.STDOUT)

    def result(self, text, code=0):
        return s.Run(["fixture"], code, text, 0.01)

    def fake_cargo(self, command, cwd, log, timeout, env):
        if command[0] == "rustc" or "-V" in command:
            text, code = "protocol fixture version\n", 0
        elif "--list" in command:
            text, code = "fixture: test\n", 0
        elif "--exact" in command and "broken" in (cwd / self.fault["path"]).read_text():
            text = ("test fixture ... FAILED\n"
                    "thread 'fixture' panicked at tests/witness.rs:1:1:\n"
                    "assertion failed: observed\n"
                    "test result: FAILED. 0 passed; 1 failed;\n")
            code = 101
        else:
            text, code = "test fixture ... ok\ntest result: ok. 1 passed; 0 failed;\n", 0
        log.write_text(text)
        return s.Run(command, code, text, 0.01)

    def execute(self, adapter=None):
        with mock.patch.object(s, "run", side_effect=adapter or self.fake_cargo), redirect_stdout(io.StringIO()):
            code = s.compare(self.args)
        return code, json.loads((Path(self.args.output) / "results.json").read_text())

    def test_campaign_records_both_sources_and_leaves_operator_checkout_intact(self):
        before = self.git("status", "--porcelain")
        code, report = self.execute()
        self.assertEqual(code, 0)
        self.assertEqual(report["status"], "passed")
        self.assertEqual(report["original"], self.original)
        row = report["faults"][0]
        for role in ("original", "candidate"):
            self.assertEqual(row[role]["result"], "detected")
            self.assertEqual(row[role]["source"]["replacements"], 1)
            self.assertEqual(row[role]["witness"]["path"], "tests/witness.rs")
        self.assertEqual(before, self.git("status", "--porcelain"))
        self.assertEqual(self.git("worktree", "list", "--porcelain").count("worktree "), 1)

    def test_surviving_or_incomplete_faults_cannot_claim_preservation(self):
        def survive(command, cwd, log, timeout, env):
            if "--exact" in command and cwd.name == "candidate":
                result = self.result("test fixture ... ok\ntest result: ok. 1 passed; 0 failed;\n")
                log.write_text(result.output)
                return result
            return self.fake_cargo(command, cwd, log, timeout, env)
        code, report = self.execute(survive)
        self.assertEqual(code, 1)
        self.assertEqual(report["faults"][0]["candidate"]["result"], "survived")
        self.assertEqual(report["status"], "failed")

    def test_changed_production_or_missing_discovery_is_incomplete(self):
        (self.repo / self.fault["path"]).write_text("other production\n")
        self.git("commit", "-qam", "Changed fixture implementation")
        self.args.candidate = self.git("rev-parse", "HEAD").strip()
        code, report = self.execute()
        self.assertEqual(code, 1)
        self.assertIn("production inputs differ", report["error"])

    def test_failed_patch_is_not_a_catch_and_cleans_worktrees(self):
        self.fault["old"] = "missing anchor"
        self.catalog.write_text(json.dumps([self.fault]))
        code, report = self.execute()
        self.assertEqual(code, 1)
        self.assertEqual(report["status"], "incomplete")
        self.assertIn("anchor count mismatch", report["error"])
        self.assertEqual(self.git("worktree", "list", "--porcelain").count("worktree "), 1)

    def test_empty_discovery_and_unhealthy_suite_are_incomplete(self):
        def empty(command, cwd, log, timeout, env):
            if "--list" in command:
                log.write_text("0 tests\n")
                return self.result("0 tests\n")
            return self.fake_cargo(command, cwd, log, timeout, env)
        code, report = self.execute(empty)
        self.assertEqual(code, 1)
        self.assertIn("discovery failed", report["error"])

    def test_classification_requires_the_selected_test_and_actual_assertion_location(self):
        cases = [
            ("error: could not compile", 101, "build/setup error"),
            ("running 0 tests\ntest result: ok. 0 passed; 0 failed;", 0, "not exercised"),
            ("test fixture ... ok\ntest result: ok. 1 passed; 0 failed;", 0, "survived"),
            ("test fixture ... FAILED\nthread 'fixture' panicked at tests/witness.rs:1:1:\nassertion failed", 101, "detected"),
            ("test unrelated ... FAILED\nthread 'unrelated' panicked at tests/witness.rs:1:1:", 101, "build/setup error"),
            ("test fixture ... FAILED\nthread 'fixture' panicked at unknown.rs:1:1:", 101, "build/setup error"),
            ("test fixture ... FAILED\nprocess didn't exit successfully (signal: 6, SIGABRT)", 101, "timeout/abort"),
        ]
        for output, code, expected in cases:
            with self.subTest(expected=expected, output=output):
                self.assertEqual(s.classify(self.result(output, code), self.fault, self.repo)[0], expected)
        (self.repo / "tests/witness.rs").write_text("setup().unwrap();\n")
        output = "test fixture ... FAILED\nthread 'fixture' panicked at tests/witness.rs:1:1:\nassertion failed"
        self.assertEqual(s.classify(self.result(output, 101), self.fault, self.repo)[0], "build/setup error")

    def test_catalog_anchors_match_current_sources_and_have_explicit_witnesses(self):
        catalog = json.loads((ROOT / "tools/testkit/faults.json").read_text())
        self.assertEqual(len({f["id"] for f in catalog}), len(catalog))
        for f in catalog:
            self.assertEqual((ROOT / f["path"]).read_text().count(f["old"]), f.get("count", 1), f["id"])
            self.assertIn("fn " + f["test"].split("::")[-1] + "(", (ROOT / f["witness"]).read_text(), f["id"])

    def test_deadline_terminates_owned_child_processes(self):
        pidfile = self.root / "child.pid"
        code = ("import subprocess,time; from pathlib import Path; "
                "p=subprocess.Popen(['sleep','30']); "
                "Path(" + repr(str(pidfile)) + ").write_text(str(p.pid)); time.sleep(30)")
        result = s.run([sys.executable, "-c", code], self.repo, self.root / "timeout.log", 0.4)
        self.assertTrue(result.timed_out)
        pid = int(pidfile.read_text())
        stat = Path("/proc") / str(pid) / "stat"
        self.assertTrue(not stat.exists() or stat.read_text().split()[2] == "Z")

    def test_interruption_cleans_the_disposable_checkout_and_records_incomplete(self):
        def interrupt(command, cwd, log, timeout, env):
            if "--exact" in command:
                raise KeyboardInterrupt()
            return self.fake_cargo(command, cwd, log, timeout, env)
        code, report = self.execute(interrupt)
        self.assertEqual(code, 1)
        self.assertEqual(report["status"], "incomplete")
        self.assertIn("KeyboardInterrupt", report["error"])
        self.assertEqual(self.git("worktree", "list", "--porcelain").count("worktree "), 1)


if __name__ == "__main__":
    unittest.main()
