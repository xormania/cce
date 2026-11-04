"""The qualification tier runner: test attribution, the qualification rules, campaign counts, and
one end-to-end run on a scratch repository."""
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest
from unittest import mock

ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT / "tools/tiers"))
import run_tiers as t

NO_RUN = [
    json.dumps({
        "reason": "compiler-artifact",
        "package_id": "path+file:///w/crates/cce-storage#0.1.0",
        "target": {"name": "cce_storage", "kind": ["lib"]},
        "profile": {"test": True},
        "executable": "/w/target/debug/deps/cce_storage-aaa",
    }),
    json.dumps({
        "reason": "compiler-artifact",
        "package_id": "path+file:///w/crates/cce-storage#0.1.0",
        "target": {"name": "cce_storage", "kind": ["lib"]},
        "profile": {"test": False},
        "executable": None,
    }),
    json.dumps({
        "reason": "compiler-artifact",
        "package_id": "path+file:///w/crates/cce-storage#0.1.0",
        "target": {"name": "workloads", "kind": ["test"]},
        "profile": {"test": True},
        "executable": "/w/target/debug/deps/workloads-bbb",
    }),
    json.dumps({
        "reason": "compiler-artifact",
        "package_id": "path+file:///w/crates/core#cce-core@0.1.0",
        "target": {"name": "cce_core", "kind": ["lib"]},
        "profile": {"test": True},
        "executable": "/w/target/debug/deps/cce_core-ccc",
    }),
    "not json",
    json.dumps({"reason": "build-finished", "success": True}),
]

RUN = """\
    Finished `test` profile [unoptimized + debuginfo] target(s) in 0.1s
     Running unittests src/lib.rs (target/debug/deps/cce_storage-aaa)

running 3 tests
test engine_tests::a ... ok
test engine_tests::b ... FAILED
test fuzz_replay_tests::write_seeds ... ignored, writes the seed files
model report: {"passed": true}

test result: FAILED. 1 passed; 1 failed; 1 ignored; 0 measured; 0 filtered out; finished in 0.1s

     Running tests/workloads.rs (target/debug/deps/workloads-bbb)

running 1 test
test large_content_streams_through_one_segment_buffer ... ignored, workload tier

test result: ok. 0 passed; 0 failed; 1 ignored; 0 measured; 0 filtered out; finished in 0.0s

     Running unittests src/lib.rs (target/debug/deps/cce_core-ccc)

running 0 tests

test result: ok. 0 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.0s

   Doc-tests cce_core

running 1 test
test crates/core/src/lib.rs - Example (line 3) ... ok

test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.0s
"""


class Attribution(unittest.TestCase):
    def test_package_names_come_from_either_package_id_form(self):
        self.assertEqual(t.package_name("path+file:///w/crates/cce-storage#0.1.0"), "cce-storage")
        self.assertEqual(t.package_name("path+file:///w/crates/core#cce-core@0.1.0"), "cce-core")
        self.assertEqual(t.package_name("registry+https://x/index#serde@1.0.0"), "serde")

    def test_test_executables_map_to_package_and_target(self):
        exes, crates = t.executables(NO_RUN)
        self.assertEqual(exes, {
            "cce_storage-aaa": ("cce-storage", "lib"),
            "workloads-bbb": ("cce-storage", "workloads"),
            "cce_core-ccc": ("cce-core", "lib"),
        })
        self.assertEqual(crates, {"cce_storage": "cce-storage", "cce_core": "cce-core"})

    def test_a_result_after_captured_output_is_attributed_to_its_test(self):
        text = (
            "     Running tests/workloads.rs (target/debug/deps/workloads-bbb)\n"
            "running 2 tests\n"
            "test large_content_streams_through_one_segment_buffer ... WORKLOAD {\"a\": 1}\n"
            "ok\n"
            "test opening_and_point_and_range_reads_do_not_grow_with_the_store ... noise\n"
            "more noise\n"
            "FAILED\n"
            "test result: FAILED. 1 passed; 1 failed; 0 ignored; 0 measured; 0 filtered out\n"
        )
        tests, _ = t.parse_test_output(text, *t.executables(NO_RUN))
        self.assertEqual(tests, {
            "cce-storage:workloads:large_content_streams_through_one_segment_buffer": "ok",
            "cce-storage:workloads:opening_and_point_and_range_reads_do_not_grow_with_the_store":
                "failed",
        })

    def test_every_test_line_is_attributed_to_the_binary_cargo_was_running(self):
        tests, binaries = t.parse_test_output(RUN, *t.executables(NO_RUN))
        self.assertEqual(tests, {
            "cce-storage:lib:engine_tests::a": "ok",
            "cce-storage:lib:engine_tests::b": "failed",
            "cce-storage:lib:fuzz_replay_tests::write_seeds": "ignored",
            "cce-storage:workloads:large_content_streams_through_one_segment_buffer": "ignored",
            "cce-core:doc:crates/core/src/lib.rs - Example (line 3)": "ok",
        })
        self.assertEqual([b["binary"] for b in binaries],
                         ["cce-storage:lib", "cce-storage:workloads", "cce-core:lib",
                          "cce-core:doc"])
        self.assertEqual(binaries[0], {"binary": "cce-storage:lib", "passed": 1, "failed": 1,
                                       "ignored": 1, "filtered": 0})


class Qualification(unittest.TestCase):
    one = [{"binary": "p:lib", "passed": 1, "failed": 0, "ignored": 0, "filtered": 0}]

    def test_a_clean_run_with_executed_tests_passes(self):
        self.assertEqual(t.qualify(0, {"p:lib:a": "ok"}, self.one), ("passed", []))

    def test_a_failed_test_fails_the_tier(self):
        outcome, problems = t.qualify(101, {"p:lib:a": "failed"}, self.one)
        self.assertEqual(outcome, "failed")
        self.assertEqual(problems, [{"failed": ["p:lib:a"]}])

    def test_a_nonzero_exit_without_a_failed_test_fails_the_tier(self):
        self.assertEqual(t.qualify(101, {"p:lib:a": "ok"}, self.one),
                         ("failed", [{"exit": 101}]))

    one_and_ignored = [{"binary": "p:lib", "passed": 1, "failed": 0, "ignored": 1, "filtered": 0}]

    def test_an_undeclared_ignored_test_does_not_pass(self):
        outcome, problems = t.qualify(0, {"p:lib:a": "ok", "p:lib:b": "ignored"},
                                      self.one_and_ignored, declared={})
        self.assertEqual(outcome, "unexpected-ignored")
        self.assertEqual(problems, [{"unexpected_ignored": ["p:lib:b"]}])

    def test_a_declared_ignored_test_is_accepted(self):
        self.assertEqual(
            t.qualify(0, {"p:lib:a": "ok", "p:lib:b": "ignored"}, self.one_and_ignored,
                      declared={"p:lib:b": "elsewhere"}),
            ("passed", []))

    def test_tests_counted_but_not_attributed_fail_the_tier(self):
        outcome, problems = t.qualify(0, {}, self.one)
        self.assertEqual(outcome, "failed")
        self.assertEqual(problems, [{"unattributed": "1 tests counted, 0 attributed"}])

    def test_zero_executed_tests_is_not_a_pass(self):
        none = [{"binary": "p:lib", "passed": 0, "failed": 0, "ignored": 0, "filtered": 9}]
        self.assertEqual(t.qualify(0, {}, none),
                         ("no-tests", [{"no_tests": "no test executed"}]))

    def test_lint_covers_every_feature(self):
        self.assertIn("--all-features", t.TIERS["lint"]["argv"])

    def test_every_declared_ignored_test_names_its_tier_or_reason(self):
        for test, why in t.DECLARED_IGNORED.items():
            self.assertTrue(why in t.TIERS or why.startswith("maintenance action"), test)


def report(kind, **fields):
    return f"{kind} report: {json.dumps(fields)}"


class Campaigns(unittest.TestCase):
    model_batch = {"test": "m", "report": "model", "cases": 250, "total": "generated_sequences",
                   "env": {}}
    model = dict(passed=True, cases=250, sequences_run=250, operations_per_sequence=[64, 80],
                 attempted_operations=18000, accepted_cases=250, rejected_cases=0)

    def check(self, batch, *lines):
        return t.check_batch(batch, t.reports("\n".join(lines)))

    def test_a_model_batch_that_meets_its_budget_has_no_gap(self):
        line, gaps = self.check(self.model_batch, "noise", report("model", **self.model))
        self.assertEqual(gaps, [])
        self.assertEqual(line["sequences_run"], 250)

    def test_a_model_batch_short_of_its_budget_or_without_a_rejection_count_has_gaps(self):
        short = dict(self.model, cases=200, sequences_run=200, operations_per_sequence=[32, 40])
        del short["rejected_cases"]
        del short["accepted_cases"]
        _, gaps = self.check(self.model_batch, report("model", **short))
        self.assertEqual(gaps, ["cases 200 != 250", "sequences run 200 < 250",
                                "sequences of 32 operations < 64",
                                "accepted generated cases not measured",
                                "rejected generated cases not measured"])
        # Rejections are recorded, not forbidden; they must be a count, and add up when the
        # report states how many cases were attempted.
        _, gaps = self.check(self.model_batch, report("model", **dict(self.model,
                                                                        rejected_cases=3)))
        self.assertEqual(gaps, [])
        _, gaps = self.check(self.model_batch, report("model", **dict(self.model,
                                                                        rejected_cases=-1)))
        self.assertEqual(gaps, ["rejected generated cases -1 is not a count"])
        _, gaps = self.check(self.model_batch, report("model", **dict(
            self.model, rejected_cases=3, attempted_cases=252)))
        self.assertEqual(gaps, ["accepted 250 + rejected 3 != attempted 252"])
        _, gaps = self.check(self.model_batch, report("model", **dict(
            self.model, rejected_cases=3, attempted_cases=253)))
        self.assertEqual(gaps, [])

    def test_a_report_printed_after_the_test_name_is_read(self):
        line = "test model_tests::m ... " + report("model", **self.model)
        self.assertEqual(self.check(self.model_batch, line)[1], [])

    def test_a_missing_or_failed_report_is_a_gap(self):
        self.assertEqual(self.check(self.model_batch, "test result: ok."),
                         (None, ["0 model reports, expected 1"]))
        _, gaps = self.check(self.model_batch, report("model", **dict(self.model, passed=False)))
        self.assertEqual(gaps, ["the report says the campaign failed"])

    def test_crash_batches_require_every_cut_or_exactly_the_sampled_cuts_and_both_outcomes(self):
        every = {"test": "c", "report": "crash", "cuts": "every", "cases": 16, "env": {}}
        sampled = dict(every, cuts=48)
        ok = dict(passed=True, cases=16, cut_points_available=900, cuts_executed=900,
                  recovered_with_step=3, recovered_without_step=4, accepted_cases=16,
                  rejected_cases=0)
        self.assertEqual(self.check(every, report("crash", **ok))[1], [])
        self.assertEqual(self.check(every, report("crash", **dict(ok, cuts_executed=899)))[1],
                         ["cuts executed 899 < cut points 900"])
        self.assertEqual(self.check(sampled, report("crash", **dict(ok, cuts_executed=768)))[1],
                         [])
        self.assertEqual(
            self.check(sampled, report("crash", **dict(ok, cuts_executed=767,
                                                       recovered_with_step=0)))[1],
            ["cuts executed 767 != 768", "no case recovered with step"])

    def test_crash_batches_must_run_their_configured_cases_and_report_cut_points(self):
        every = {"test": "c", "report": "crash", "cuts": "every", "cases": 64, "env": {}}
        sampled = dict(every, cuts=48, cases=16)
        base = dict(passed=True, cut_points_available=900, cuts_executed=900,
                    recovered_with_step=3, recovered_without_step=4, rejected_cases=0)
        _, gaps = self.check(every, report("crash", **dict(base, cases=1, accepted_cases=1)))
        self.assertIn("cases 1 != 64", gaps)
        no_points = dict(base, cases=64, accepted_cases=64)
        del no_points["cut_points_available"]
        _, gaps = self.check(every, report("crash", **no_points))
        self.assertIn("no cut points reported", gaps)
        _, gaps = self.check(sampled, report("crash", **dict(
            base, cases=0, accepted_cases=0, cut_points_available=0, cuts_executed=0)))
        self.assertIn("cases 0 != 16", gaps)
        self.assertIn("no cut points reported", gaps)
        _, gaps = self.check(sampled, report("crash", **dict(
            base, cases=1, accepted_cases=1, cuts_executed=48)))
        self.assertIn("cases 1 != 16", gaps)

    def test_accepted_and_rejected_cases_must_be_measured(self):
        for kind, batch, fields in [
            ("model", self.model_batch, self.model),
            ("crash", {"test": "c", "report": "crash", "cuts": "every", "cases": 16, "env": {}},
             dict(passed=True, cases=16, cut_points_available=9, cuts_executed=9,
                  recovered_with_step=1, recovered_without_step=1, accepted_cases=16,
                  rejected_cases=0)),
        ]:
            self.assertEqual(self.check(batch, report(kind, **fields))[1], [], kind)
            _, gaps = self.check(batch, report(kind, **dict(fields, rejected_cases=None,
                                                            accepted_cases=None)))
            self.assertIn("rejected generated cases not measured", gaps, kind)
            self.assertIn("accepted generated cases not measured", gaps, kind)
            _, gaps = self.check(batch, report(kind, **dict(fields, accepted_cases=3)))
            self.assertIn(f"accepted cases 3 != {fields['cases']}", gaps, kind)

    def test_totals_must_meet_the_minimums(self):
        decoder = {"test": "d", "report": "decoder", "total": "decoder_cases", "env": {}}
        batches = [{"batch": self.model_batch, "report_line": self.model}] * 4 + [
            {"batch": decoder, "report_line": {"passed": True, "cases": 10000}}]
        totals, gaps = t.campaign_totals(batches)
        self.assertEqual(gaps, [])
        self.assertEqual(totals["generated_sequences"], 1000)
        totals, gaps = t.campaign_totals(batches[1:4] + [{"batch": decoder, "report_line": None}])
        self.assertEqual(gaps, ["generated sequences 750 < 1000", "decoder cases 0 < 10000"])

    def test_the_decoder_sweep_must_cover_every_target_at_its_budget(self):
        batch = next(b for b in t.CAMPAIGN_BATCHES if b["report"] == "decoder")
        per = int(batch["env"]["CCE_DECODER_CASES"])
        full = {n: {"cases": per, "valid": 1, "malformed": 1} for n in batch["targets"]}
        line = dict(passed=True, cases=per * len(full), per_target=full)
        self.assertEqual(self.check(batch, report("decoder", **line))[1], [])
        dropped = dict(full)
        missing = sorted(dropped)[0]
        del dropped[missing]
        _, gaps = self.check(batch, report("decoder", **dict(line, per_target=dropped)))
        self.assertEqual(gaps, [f"decoder target {missing} not reported"])
        short = dict(full, **{missing: {"cases": per - 1, "valid": 1, "malformed": 1}})
        _, gaps = self.check(batch, report("decoder", **dict(line, per_target=short)))
        self.assertEqual(gaps, [f"decoder target {missing}: cases {per - 1} != {per}"])
        self.assertEqual(len(batch["targets"]), 7)

    def test_every_crash_batch_states_its_configured_cases(self):
        for b in t.CAMPAIGN_BATCHES:
            if b["report"] == "crash":
                self.assertGreater(b.get("cases", 0), 0, b["test"])

    def test_every_capturing_run_uses_one_test_thread(self):
        for b in t.CAMPAIGN_BATCHES:
            argv = t.campaign_argv(t.TIERS["campaigns"], b)
            self.assertIn("--test-threads=1", argv, b["test"])
        for name, tier in t.TIERS.items():
            if "--nocapture" in tier.get("test_args", []):
                self.assertIn("--test-threads=1", tier["test_args"], name)

    def test_the_selection_stages_its_budgets_in_batches_with_recorded_seeds(self):
        generated = [b for b in t.CAMPAIGN_BATCHES if b.get("total") == "generated_sequences"]
        self.assertGreaterEqual(sum(b["cases"] for b in generated),
                                t.CAMPAIGN_MINIMUMS["generated_sequences"])
        self.assertEqual(len({b["env"]["CCE_MODEL_SEED"] for b in generated}), len(generated))
        self.assertTrue(any(b["report"] == "decoder" for b in t.CAMPAIGN_BATCHES))


class EndToEnd(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.repo = self.root / "repo"
        self.repo.mkdir()
        for args in (["init", "-q"], ["config", "user.name", "Fixture"],
                     ["config", "user.email", "fixture@example.test"]):
            self.git(*args)
        (self.repo / "a.txt").write_text("a\n")
        self.git("add", "a.txt")
        self.git("commit", "-q", "-m", "fixture")
        tiers = {
            "good": {"kind": "command", "argv": [sys.executable, "-c", "print('fine')"],
                     "default": True},
            "bad": {"kind": "command", "argv": [sys.executable, "-c", "raise SystemExit(3)"],
                    "default": False},
        }
        patch = mock.patch.object(t, "TIERS", tiers)
        patch.start()
        self.addCleanup(patch.stop)

    def git(self, *args):
        subprocess.run(["git", *args], cwd=self.repo, check=True, capture_output=True)

    def run_tiers(self, *args):
        return t.main(["--repo", str(self.repo), *args])

    def test_results_record_the_candidate_and_each_tier_with_its_log_digest(self):
        out = self.root / "run"
        code = self.run_tiers("--output", str(out))
        self.assertEqual(code, 0)
        r = json.loads((out / "results.json").read_text())
        head = subprocess.run(["git", "rev-parse", "HEAD"], cwd=self.repo, capture_output=True,
                              text=True).stdout.strip()
        self.assertEqual(r["candidate"], {"commit": head, "dirty": False})
        self.assertEqual([x["name"] for x in r["tiers"]], ["good"])
        cmd = r["tiers"][0]["commands"][0]
        self.assertEqual(r["tiers"][0]["outcome"], "passed")
        log = out / cmd["log"]
        self.assertEqual(log.read_text(), "fine\n")
        self.assertEqual(cmd["log_sha256"], t.sha256_file(log))
        self.assertTrue(r["passed"])

    def test_a_failing_tier_fails_the_run(self):
        code = self.run_tiers("--output", str(self.root / "run"), "--tier", "good",
                              "--tier", "bad")
        self.assertEqual(code, 1)
        r = json.loads((self.root / "run/results.json").read_text())
        self.assertEqual([x["outcome"] for x in r["tiers"]], ["passed", "failed"])
        self.assertFalse(r["passed"])

    def test_a_dirty_working_copy_is_recorded_with_its_files(self):
        (self.repo / "a.txt").write_text("changed\n")
        self.run_tiers("--output", str(self.root / "run"))
        c = json.loads((self.root / "run/results.json").read_text())["candidate"]
        self.assertTrue(c["dirty"])
        self.assertEqual(set(c["files_sha256"]), {"a.txt"})

    def test_a_fresh_clone_runs_the_committed_candidate_without_ignored_working_files(self):
        # Ignored files (like build output) leave the working copy clean but stay out of a clone.
        (self.repo / ".gitignore").write_text("ignored.txt\n")
        self.git("add", ".gitignore")
        self.git("commit", "-q", "-m", "ignore")
        (self.repo / "ignored.txt").write_text("build output\n")
        tiers = {"look": {"kind": "command", "default": True, "argv": [
            sys.executable, "-c",
            "import os; print(sorted(f for f in os.listdir('.') if not f.startswith('.')))"]}}
        with mock.patch.object(t, "TIERS", tiers):
            self.assertEqual(self.run_tiers("--output", str(self.root / "run"),
                                            "--fresh-clone"), 0)
        r = json.loads((self.root / "run/results.json").read_text())
        self.assertTrue(r["fresh_clone"])
        self.assertFalse(r["candidate"]["dirty"])
        log = self.root / "run" / r["tiers"][0]["commands"][0]["log"]
        self.assertEqual(log.read_text(), "['a.txt']\n")

    def test_a_fresh_clone_of_a_dirty_working_copy_is_refused(self):
        (self.repo / "a.txt").write_text("changed\n")
        with self.assertRaises(SystemExit):
            self.run_tiers("--output", str(self.root / "run"), "--fresh-clone")
        self.assertFalse((self.root / "run").exists())

    def test_every_rust_and_cargo_setting_in_the_environment_is_recorded(self):
        env = {"RUSTFLAGS": "-C debug-assertions", "CARGO_PROFILE_RELEASE_LTO": "true",
               "RUSTUP_TOOLCHAIN": "stable", "CCE_MODEL_CASES": "7", "HOME_ISH": "no"}
        with mock.patch.dict(os.environ, env):
            self.run_tiers("--output", str(self.root / "run"))
        got = json.loads((self.root / "run/results.json").read_text())["environment"]
        for k in ("RUSTFLAGS", "CARGO_PROFILE_RELEASE_LTO", "RUSTUP_TOOLCHAIN", "CCE_MODEL_CASES"):
            self.assertEqual(got.get(k), env[k], k)
        self.assertNotIn("HOME_ISH", got)

    def test_an_existing_output_directory_is_refused(self):
        (self.root / "run").mkdir()
        with self.assertRaises(SystemExit):
            self.run_tiers("--output", str(self.root / "run"))


class Timeouts(unittest.TestCase):
    def test_a_timeout_kills_the_whole_process_group(self):
        with tempfile.TemporaryDirectory() as d:
            pid_file = Path(d) / "grandchild.pid"
            script = (
                "import subprocess, sys, time\n"
                "c = subprocess.Popen([sys.executable, '-c', 'import time; time.sleep(60)'])\n"
                f"open({str(pid_file)!r}, 'w').write(str(c.pid))\n"
                "time.sleep(60)\n"
            )
            # Long enough for the grandchild to start and record itself, even on a busy host.
            code, _ = t.run_logged([sys.executable, "-c", script], d, dict(os.environ),
                                   Path(d) / "log", 8)
            self.assertIsNone(code)
            self.assertTrue(pid_file.is_file(), "the grandchild never started; raise the timeout")
            pid = int(pid_file.read_text())
            # The grandchild was killed; at most a zombie remains until its parent is reaped.
            deadline = __import__("time").monotonic() + 5
            alive = True
            while alive and __import__("time").monotonic() < deadline:
                try:
                    state = Path(f"/proc/{pid}/stat").read_text().split()[2]
                    alive = state != "Z"
                except FileNotFoundError:
                    alive = False
                if alive:
                    __import__("time").sleep(0.1)
            self.assertFalse(alive, "the test binary's child kept running after the timeout")


if __name__ == "__main__":
    unittest.main()
