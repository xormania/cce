"""Execution witnesses cover producer authenticity and real filtering behavior."""
from __future__ import annotations

import copy
from datetime import datetime, timedelta, timezone
import io
from pathlib import Path
import sys
import tempfile
import unittest
from unittest import mock
import urllib.error

ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT / "tools/ci"))
from drift_contracts import evaluate
from drift_execution import execution_checks

HEAD, OLD, BASE, MERGE, MAIN = (c * 40 for c in "abcde")
REPO = "example/store"
RUNS = "/actions/runs?head_sha=" + HEAD + "&per_page=50"
HISTORY = "/actions/workflows/rust.yml/runs?branch=feature&per_page=20"
SCANS = "/commits/" + HEAD + "/check-runs?check_name=scan&per_page=100"
SCAN_FILES = (".github/workflows/leak-scan.yml", "tools/ci/leak_scan_trusted.sh", "tools/ci/leak_scan.sh")
CLEAN = "no private markers in PR text, commit messages, added lines or file names"
PYTHON_COMMAND = "python3 -m unittest discover -s tests/ci -v"
RUST_COMMAND = "cargo test --workspace --locked"
PYTHON_WORKFLOW = """name: drift-input

on:
  push:
    branches: ['**']
  pull_request:
  workflow_dispatch:

permissions:
  contents: read

concurrency:
  group: drift-input-${{ github.ref }}
  cancel-in-progress: true

jobs:
  test:
    name: Drift tooling tests
    if: github.event_name != 'pull_request' || github.event.pull_request.head.repo.full_name != github.repository
    runs-on: ubuntu-latest
    timeout-minutes: 5
    steps:
      - uses: actions/checkout@3d3c42e5aac5ba805825da76410c181273ba90b1 # v7.0.1
        with:
          persist-credentials: false
      - name: Unit and replay tests
        run: python3 -m unittest discover -s tests/ci -v
"""


def witness_step(name, conclusion="success", number=1, start=10, end=11):
    epoch = datetime(2026, 11, 5, tzinfo=timezone.utc)
    return {"name": name, "conclusion": conclusion, "number": number,
            "started_at": (epoch + timedelta(seconds=start)).isoformat().replace("+00:00", "Z"),
            "completed_at": (epoch + timedelta(seconds=end)).isoformat().replace("+00:00", "Z")}


def command_log(script, output, start=10.1, finish=11.1):
    """Actions prints the script before its output, with second-rounded API times."""
    epoch = datetime(2026, 11, 5, tzinfo=timezone.utc)
    header = ["##[group]Run " + script.splitlines()[0]]
    header += ["\x1b[36;1m" + line + "\x1b[0m" for line in script.splitlines()]
    header += ["shell: /usr/bin/bash -e {0}", "##[endgroup]"]
    rows = [(start + i * 0.00001, line) for i, line in enumerate(header)]
    rows += [(finish + i * 0.00001, line) for i, line in enumerate(output.splitlines())]
    return "".join((epoch + timedelta(seconds=at)).isoformat(timespec="microseconds").replace("+00:00", "Z")
                   + " " + line + "\n" for at, line in rows)


def scan_check(conclusion="success", body=CLEAN, run_id=13, number=501,
               started="2026-11-04T10:00:00Z", app=15368, status="completed"):
    # GitHub replaces details_url on checks created by GitHub Actions; the summary names the run.
    return {"id": number, "app": {"id": app}, "status": status, "conclusion": conclusion, "started_at": started,
            "details_url": "https://github.com/" + REPO + "/runs/" + str(number),
            "output": {"summary": "leak-scan run " + str(run_id) + "\nPR #9 at " + HEAD + "\n\n" + body + "\n"}}


def scan_run(number=13, path=".github/workflows/leak-scan.yml", event="workflow_run"):
    return {"id": number, "name": "leak-scan", "path": path, "event": event, "head_sha": MAIN,
            "head_repository": {"full_name": REPO}, "status": "completed", "conclusion": "success", "run_attempt": 1}


def source():
    return {"repository": REPO, "head_sha": HEAD, "base_sha": BASE,
            "branch": "feature", "pulls": [9]}


def run(producer, number, sha=HEAD, event="push"):
    return {"id": number, "name": producer,
            "path": ".github/workflows/" + producer + ".yml",
            "head_sha": sha, "head_repository": {"full_name": REPO},
            "head_branch": "feature", "event": event, "run_number": number,
            "run_attempt": 1, "status": "completed", "conclusion": "success",
            "pull_requests": [{"number": 9, "head": {"sha": sha}, "base": {"sha": BASE}}] if event == "pull_request" else []}


class Api:
    repository = REPO

    def __init__(self, root):
        self.calls = []
        self.files = {(name, ref): (root / name).read_text()
                      for name in [".github/workflows/" + p + ".yml" for p in ("rust", "drift-input")]
                      for ref in (HEAD, OLD, MERGE)}
        self.files.update({(name, MAIN): (root / name).read_text() for name in SCAN_FILES})
        self.responses = {
            RUNS: {"workflow_runs": [run("drift-input", 11), run("rust", 12)]},
            SCANS: {"check_runs": [scan_check()]},
            "/actions/runs/13": scan_run(),
            "/pulls/9": {"state": "open", "head": {"sha": HEAD}, "base": {"sha": BASE}, "merge_commit_sha": MERGE},
            "/actions/runs/11/jobs?per_page=50": {"jobs": [{"id": 111, "conclusion": "success",
                "steps": [witness_step("Unit and replay tests")]}]},
            "/actions/runs/12/jobs?per_page=50": {"jobs": [{"id": 112, "conclusion": "success",
                "steps": [witness_step(name, number=i + 1) for i, name in enumerate(("Format", "Clippy", "Test"))]}]},
        }
        self.logs = {111: command_log(PYTHON_COMMAND, "Ran 89 tests in 0.1s\nOK\n"),
                     112: command_log(RUST_COMMAND, "test result: ok. 142 passed; 0 failed; 0 ignored\n")}

    def api(self, path):
        self.calls.append(("api", path))
        return copy.deepcopy(self.responses[path])

    def file(self, path, ref):
        self.calls.append(("file", path, ref))
        return self.files[path, ref]

    def log(self, job):
        self.calls.append(("log", job))
        return self.logs[job]


class ExecutionTests(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory()
        self.addCleanup(self.directory.cleanup)
        self.root = Path(self.directory.name)
        target = self.root / ".github/workflows"
        target.mkdir(parents=True)
        for name in ("rust", "drift-input", "leak-scan"):
            text = (PYTHON_WORKFLOW if name == "drift-input" else
                    (ROOT / ".github/workflows" / (name + ".yml")).read_text())
            (target / (name + ".yml")).write_text(text)
        (self.root / "tools/ci").mkdir(parents=True)
        for name in SCAN_FILES[1:]:
            (self.root / name).write_text((ROOT / name).read_text())
        self.api = Api(self.root)

    def collect(self, markdown=False, observations=None):
        return execution_checks(self.api, source(), markdown, observations, self.root)

    def violations(self, checks):
        return evaluate({"source": source(), "checks": checks})["violations"]

    def reuse(self, files=None):
        self.api.responses[RUNS]["workflow_runs"] = [run("drift-input", 11)]
        self.api.responses[HISTORY] = {"workflow_runs": [run("rust", 12, OLD)]}
        self.api.responses["/compare/" + OLD + "..." + HEAD] = {
            "status": "ahead", "merge_base_commit": {"sha": OLD},
            "files": files if files is not None else [{"filename": "docs/readers.md"}]}

    def test_current_success_records_authentic_producer_and_candidate(self):
        observations = []
        checks, limits, waiting = self.collect(observations=observations)
        self.assertEqual(len(checks), 5)
        self.assertEqual((limits, waiting, self.violations(checks)), ([], False, []))
        self.assertTrue(all(row["source_sha"] == HEAD for row in checks))
        self.assertTrue(all(row["workflow_sha256"] for row in checks))
        self.assertEqual(checks[-1]["id"], "Trusted leak scan")
        self.assertEqual(checks[-1]["workflow_ref"], MAIN)
        self.assertEqual(len(observations), 2)

    def test_markdown_followup_reuses_real_previous_rust_run_on_mixed_pr(self):
        self.reuse()
        checks, limits, waiting = self.collect(markdown=False)
        rust = [row for row in checks if row["id"] in {"Format", "Clippy", "Test"}]
        self.assertEqual((limits, waiting, self.violations(checks)), ([], False, []))
        self.assertEqual(len(rust), 3)
        self.assertTrue(all(row["source_sha"] == OLD and row["equivalent"] for row in rust))
        self.assertTrue(all(row["run_id"] == 12 and row["attempt"] == 1 for row in rust))

    def test_changed_code_cannot_reuse_prior_green_rust(self):
        self.reuse([{"filename": "crates/store/src/lib.rs"}])
        checks, limits, waiting = self.collect()
        self.assertTrue(waiting)
        self.assertIn("required_execution_unavailable", limits)
        self.assertFalse(any(row["id"] == "Test" for row in checks))

    def test_renaming_code_to_markdown_is_not_unchanged_rust_input(self):
        self.reuse([{"filename": "ignored.md", "previous_filename": "crates/store/src/lib.rs"}])
        _, limits, waiting = self.collect()
        self.assertTrue(waiting)
        self.assertIn("required_execution_unavailable", limits)

    def test_nonancestor_or_truncated_comparison_cannot_prove_reuse(self):
        for field, value in (("status", "diverged"), ("total_files", 301),
                             ("merge_base_commit", {"sha": BASE})):
            with self.subTest(field=field):
                self.reuse()
                self.api.responses["/compare/" + OLD + "..." + HEAD][field] = value
                self.assertTrue(self.collect()[2])

    def test_history_from_another_branch_cannot_supply_reused_evidence(self):
        self.reuse()
        self.api.responses[HISTORY]["workflow_runs"][0]["head_branch"] = "unrelated"
        self.assertTrue(self.collect()[2])

    # A legitimate producer change: refused until the owner accepts this exact head, then continued.
    OWNER = REPO.split("/")[0]

    def change_rust(self):
        path = ".github/workflows/rust.yml"
        changed = self.api.files[path, HEAD].replace("cargo clippy --locked --workspace --all-targets -- --deny warnings",
            "cargo clippy --locked --workspace --all-targets -- --deny warnings\n          cargo clippy --locked --workspace --all-targets --all-features -- --deny warnings")
        self.assertNotEqual(changed, self.api.files[path, HEAD])
        for ref in (HEAD, MERGE):
            self.api.files[path, ref] = changed
        self.api.responses["/pulls/9/reviews?per_page=100&page=1"] = []

    def pr_rust(self):
        """Make rust a required pull request producer whose run tested the PR's merge commit."""
        path = ".github/workflows/rust.yml"
        trusted = (self.root / path).read_text().replace("  pull_request:\n    paths-ignore: ['**.md']", "  pull_request:")
        (self.root / path).write_text(trusted)
        for ref in (HEAD, OLD, MERGE):
            self.api.files[path, ref] = trusted
        self.api.responses[RUNS]["workflow_runs"] = [run("drift-input", 11), run("rust", 12, event="pull_request")]
        self.api.logs[112] = ("2026-11-05T00:00:09.100000Z [command]/usr/bin/git log -1 --format=%H\n"
                              "2026-11-05T00:00:09.100001Z " + MERGE + "\n" + self.api.logs[112])

    def review(self, login, state="APPROVED", commit=HEAD, number=1):
        return {"id": number, "user": {"login": login}, "state": state, "commit_id": commit,
                "submitted_at": "2026-11-04T10:%02d:00Z" % number}

    def test_changed_producer_without_owner_acceptance_is_refused_as_unaccepted(self):
        self.change_rust()
        checks, limits, _ = self.collect()
        self.assertIn("producer_definition_unaccepted", limits)
        self.assertIn("rust:trusted-definition", [row["id"] for row in checks])
        policy = next(row for row in checks if row["id"] == "rust:trusted-definition")
        self.assertEqual(policy.get("definition_status"), "awaiting_acceptance")
        self.assertIsNone(policy["executed"])
        self.assertEqual(policy["status"], "unknown")
        self.assertTrue(policy["required"])
        self.assertEqual(policy["source_sha"], HEAD)
        self.assertEqual([v["code"] for v in self.violations(checks)],
                         ["workflow_definition_awaiting_acceptance"])
        self.assertNotIn("producer_merge_identity_unavailable", limits)

    def test_unaccepted_definition_does_not_hide_another_producers_failed_check(self):
        self.change_rust()
        self.api.responses["/actions/runs/11/jobs?per_page=50"]["jobs"][0]["steps"][0]["conclusion"] = "failure"
        checks, limits, waiting = self.collect()
        self.assertEqual(sorted(v["code"] for v in self.violations(checks)),
                         ["required_check_failed", "workflow_definition_awaiting_acceptance"])
        self.assertIn("producer_definition_unaccepted", limits)
        self.assertFalse(waiting)

    def test_unaccepted_definition_does_not_hide_another_producers_skipped_check(self):
        self.change_rust()
        self.api.responses["/actions/runs/11/jobs?per_page=50"]["jobs"][0]["steps"][0]["conclusion"] = "skipped"
        checks, limits, waiting = self.collect()
        self.assertEqual(sorted(v["code"] for v in self.violations(checks)),
                         ["required_check_not_executed", "workflow_definition_awaiting_acceptance"])
        self.assertIn("producer_definition_unaccepted", limits)
        self.assertFalse(waiting)

    def test_owner_acceptance_of_this_head_lets_the_changed_producer_certify(self):
        self.change_rust()
        self.api.responses["/pulls/9/reviews?per_page=100&page=1"] = [self.review(self.OWNER)]
        checks, limits, waiting = self.collect()
        self.assertEqual(self.violations(checks), [])
        self.assertFalse(waiting)
        self.assertIn("producer_definition_owner_accepted_rust", limits)
        rust = [row for row in checks if row["id"] in {"Format", "Clippy", "Test"}]
        self.assertEqual(len(rust), 3)
        self.assertTrue(all(row["producer_acceptance"] == {"pull": 9, "review": 1} for row in rust))

    def test_changed_merge_workflow_without_acceptance_is_reported_as_policy(self):
        self.pr_rust()
        path = ".github/workflows/rust.yml"
        self.api.files[path, MERGE] = self.api.files[path, MERGE].replace(
            "cargo clippy --locked --workspace --all-targets -- --deny warnings",
            "cargo clippy --locked --workspace --all-targets -- --deny warnings\n          cargo clippy --locked --workspace --all-targets --all-features -- --deny warnings")
        self.api.responses["/pulls/9/reviews?per_page=100&page=1"] = []
        checks, limits, waiting = self.collect()
        self.assertEqual([v["code"] for v in self.violations(checks)],
                         ["workflow_definition_awaiting_acceptance"])
        self.assertIn("producer_definition_unaccepted", limits)
        self.assertFalse(waiting)

    def test_owner_accepted_definition_still_reports_a_skipped_check(self):
        self.change_rust()
        self.api.responses["/pulls/9/reviews?per_page=100&page=1"] = [self.review(self.OWNER)]
        self.api.responses["/actions/runs/12/jobs?per_page=50"]["jobs"][0]["steps"][2]["conclusion"] = "skipped"
        checks, limits, waiting = self.collect()
        self.assertEqual([v["code"] for v in self.violations(checks)], ["required_check_not_executed"])
        self.assertIn("producer_definition_owner_accepted_rust", limits)
        self.assertFalse(waiting)

    def test_acceptance_by_another_account_stale_head_or_later_change_request_is_refused(self):
        for reviews in ([self.review("helper-bot")],
                        [self.review(self.OWNER, commit=OLD)],
                        [self.review(self.OWNER), self.review(self.OWNER, "CHANGES_REQUESTED", number=2)],
                        [self.review(self.OWNER, "COMMENTED")]):
            with self.subTest(reviews=reviews):
                self.change_rust()
                self.api.responses["/pulls/9/reviews?per_page=100&page=1"] = reviews
                checks, limits, _ = self.collect()
                self.assertIn("producer_definition_unaccepted", limits)
                self.assertTrue(self.violations(checks))

    def test_a_later_change_request_on_a_second_page_is_not_missed(self):
        self.change_rust()
        first = [self.review(self.OWNER, number=1)] + [self.review("helper-bot", "COMMENTED", number=n)
                                                        for n in range(2, 101)]
        self.api.responses["/pulls/9/reviews?per_page=100&page=1"] = first
        self.api.responses["/pulls/9/reviews?per_page=100&page=2"] = [
            self.review(self.OWNER, "CHANGES_REQUESTED", number=101)]
        self.assertIn("producer_definition_unaccepted", self.collect()[1])

    def test_unbounded_reviews_or_a_moved_base_fail_closed(self):
        self.change_rust()
        full = [self.review(self.OWNER)] * 100
        for page in range(1, 11):
            self.api.responses["/pulls/9/reviews?per_page=100&page=" + str(page)] = full
        self.assertIn("producer_acceptance_unavailable", self.collect()[1])
        self.change_rust()
        self.api.responses["/pulls/9/reviews?per_page=100&page=1"] = [self.review(self.OWNER)]
        self.api.responses["/pulls/9"]["base"]["sha"] = OLD
        self.assertNotIn("producer_definition_owner_accepted_rust", self.collect()[1])

    def test_malformed_pull_response_is_recorded_as_unavailable(self):
        self.change_rust()
        self.api.responses["/pulls/9"] = ["not", "an", "object"]
        self.assertIn("producer_acceptance_unavailable", self.collect()[1])
        self.pr_rust()
        self.api.responses["/pulls/9"] = ["not", "an", "object"]
        self.assertIn("producer_merge_identity_unavailable", self.collect()[1])

    def test_latest_review_is_ordered_by_time_not_list_position(self):
        self.change_rust()
        self.api.responses["/pulls/9/reviews?per_page=100&page=1"] = [
            self.review(self.OWNER, "CHANGES_REQUESTED", number=5), self.review(self.OWNER, number=2)]
        self.assertIn("producer_definition_unaccepted", self.collect()[1])

    def test_accepted_producer_still_fails_on_failed_or_missing_execution(self):
        self.change_rust()
        self.api.responses["/pulls/9/reviews?per_page=100&page=1"] = [self.review(self.OWNER)]
        self.api.responses["/actions/runs/12/jobs?per_page=50"]["jobs"][0]["steps"][2]["conclusion"] = "failure"
        checks, _, _ = self.collect()
        self.assertEqual([v["code"] for v in self.violations(checks)], ["required_check_failed"])
        self.change_rust()
        self.api.responses["/pulls/9/reviews?per_page=100&page=1"] = [self.review(self.OWNER)]
        self.api.responses[RUNS]["workflow_runs"] = [run("drift-input", 11)]
        _, limits, waiting = self.collect()
        self.assertTrue(waiting)
        self.assertIn("required_execution_unavailable", limits)

    def test_conflicting_pr_reports_merge_identity_then_recovers_after_update(self):
        self.pr_rust()
        self.api.responses["/pulls/9"]["merge_commit_sha"] = None
        checks, limits, _ = self.collect()
        self.assertIn("producer_merge_identity_unavailable", limits)
        self.assertNotIn("producer_definition_unverified", limits)
        self.assertIn("rust:merge-identity", [row["id"] for row in checks])
        self.api.responses["/pulls/9"]["merge_commit_sha"] = MERGE
        checks, limits, _ = self.collect()
        self.assertEqual((self.violations(checks), limits), ([], []))

    def test_fake_same_named_workflow_cannot_certify_tests(self):
        path = ".github/workflows/drift-input.yml"
        self.api.files[path, HEAD] = "name: drift-input\nsteps:\n - name: Unit and replay tests\n   run: echo 'Ran 89 tests'\n"
        checks, limits, _ = self.collect()
        self.assertIn("producer_definition_unverified", limits)
        self.assertIn("required_check_not_executed", [v["code"] for v in self.violations(checks)])
        self.assertNotIn(("log", 111), self.api.calls)

    def test_pr_merge_workflow_must_match_trusted_definition_too(self):
        self.pr_rust()
        self.api.files[".github/workflows/rust.yml", MERGE] = "name: rust\n"
        checks, limits, _ = self.collect()
        self.assertIn("producer_definition_unverified", limits)
        self.assertEqual([v["code"] for v in self.violations(checks)],
                         ["workflow_definition_awaiting_acceptance"])
        self.assertIn(("log", 112), self.api.calls)

    # The leak scan: the trusted producer's `scan` check on the candidate head.
    def scan_row(self, checks):
        return [row for row in checks if row["id"] == "Trusted leak scan"]

    def test_pr_merge_identity_is_checked_for_a_healthy_pr_producer(self):
        self.pr_rust()
        checks, limits, waiting = self.collect()
        self.assertEqual((limits, waiting, self.violations(checks)), ([], False, []))
        self.assertEqual([row["workflow_ref"] for row in checks if row["id"] == "Test"], [MERGE])

    def test_scan_that_reports_it_could_not_run_is_not_executed(self):
        for conclusion, summary in (("success", "something else"),
                                    ("failure", "::error::PRIVATE_MARKERS secret is not available; nothing was scanned"),
                                    ("failure", "::error::the pull request head moved during the scan")):
            with self.subTest(summary=summary):
                self.api.responses[SCANS] = {"check_runs": [scan_check(conclusion, summary)]}
                checks, _, waiting = self.collect()
                self.assertFalse(self.scan_row(checks)[0]["executed"])
                self.assertFalse(waiting)
                self.assertIn("required_check_not_executed", [v["code"] for v in self.violations(checks)])

    def test_reported_match_is_an_executed_failure(self):
        self.api.responses[SCANS] = {"check_runs": [scan_check("failure", "::error::private marker found in PR added (rewrite it; matches not echoed): 1 line(s)")]}
        checks, _, _ = self.collect()
        self.assertTrue(self.scan_row(checks)[0]["executed"])
        self.assertIn("required_check_failed", [v["code"] for v in self.violations(checks)])

    def test_missing_or_running_scan_waits(self):
        self.api.responses[SCANS] = {"check_runs": []}
        _, limits, waiting = self.collect()
        self.assertTrue(waiting)
        self.assertIn("required_execution_unavailable", limits)
        self.api.responses[SCANS] = {"check_runs": [scan_check(), scan_check(None, "", number=502,
                                     started="2026-11-04T11:00:00Z", status="in_progress")]}
        checks, _, waiting = self.collect()
        self.assertTrue(waiting)
        self.assertFalse(self.scan_row(checks))

    def test_scan_from_another_app_or_an_untrusted_producer_cannot_certify(self):
        cases = {
            "other app": lambda: self.api.responses.__setitem__(SCANS, {"check_runs": [scan_check(app=1)]}),
            "no run named": lambda: self.api.responses[SCANS]["check_runs"][0]["output"].__setitem__(
                "summary", CLEAN),
            "run named later": lambda: self.api.responses[SCANS]["check_runs"][0]["output"].__setitem__(
                "summary", CLEAN + "\nleak-scan run 13\n"),
            "other workflow": lambda: self.api.responses.__setitem__("/actions/runs/13", scan_run(path=".github/workflows/leak-guard.yml")),
            "pull request event": lambda: self.api.responses.__setitem__("/actions/runs/13", scan_run(event="pull_request")),
            "changed definition": lambda: self.api.files.__setitem__((".github/workflows/leak-scan.yml", MAIN), "name: leak-scan\n"),
            "changed script": lambda: self.api.files.__setitem__(("tools/ci/leak_scan.sh", MAIN), "exit 0\n"),
        }
        for name, plant in cases.items():
            with self.subTest(case=name):
                self.setUp()
                plant()
                checks, limits, _ = self.collect()
                self.assertFalse(self.scan_row(checks))
                self.assertTrue(set(limits) & {"leak_scan_producer_unverified", "required_execution_unavailable"})
                self.assertTrue(self.violations(checks) or "required_execution_unavailable" in limits)

    def test_latest_scan_check_decides(self):
        self.api.responses[SCANS] = {"check_runs": [
            scan_check("failure", "private marker found in PR pr (rewrite it; matches not echoed): 1 line(s)",
                       number=502, started="2026-11-04T09:00:00Z"), scan_check()]}
        checks, _, _ = self.collect()
        self.assertEqual(self.scan_row(checks)[0]["check_run_id"], 501)
        self.assertEqual(self.violations(checks), [])

    def test_split_rust_jobs_use_commands_and_names_from_trusted_definition(self):
        path = ".github/workflows/rust.yml"
        definition = "name: rust\non:\n  push:\n    paths-ignore: ['**.md']\njobs:\n"
        for kind, command in (("fmt", "fmt --all -- --check"),
                              ("clippy", "clippy --locked --workspace"), ("test", "test --locked --workspace")):
            definition += "  " + kind + ":\n    steps:\n      - name: Check " + kind + "\n        run: cargo " + command + "\n"
        (self.root / path).write_text(definition)
        for ref in (HEAD, OLD, MERGE):
            self.api.files[path, ref] = definition
        self.api.responses["/actions/runs/12/jobs?per_page=50"] = {"jobs": [
            {"id": 112 + i, "conclusion": "success", "steps": [witness_step("Check " + kind)]}
            for i, kind in enumerate(("fmt", "clippy", "test"))]}
        self.api.logs[112] = command_log("cargo fmt --all -- --check", "Format finished\n")
        self.api.logs[113] = command_log("cargo clippy --locked --workspace", "Clippy finished\n")
        self.api.logs[114] = command_log("cargo test --locked --workspace", "test result: ok. 3 passed; 0 failed\n")
        self.api.responses[RUNS]["workflow_runs"][1] = run("rust", 12, event="pull_request")
        self.api.logs[112] = ("2026-11-05T00:00:09.100000Z [command]/usr/bin/git log -1 --format=%H\n"
                              "2026-11-05T00:00:09.100001Z " + MERGE + "\n" + self.api.logs[112])
        checks, _, waiting = self.collect()
        self.assertFalse(waiting)
        self.assertEqual(self.violations(checks), [])

    def test_zero_test_execution_fails_even_when_steps_succeed(self):
        self.api.logs[111] = command_log(PYTHON_COMMAND, "Ran 0 tests in 0s\nOK\n")
        self.api.logs[112] = command_log(RUST_COMMAND, "test result: ok. 0 passed; 0 failed; 0 ignored\n")
        checks, _, _ = self.collect()
        self.assertEqual(len(self.violations(checks)), 2)

    def python_witness(self, output, script=PYTHON_COMMAND):
        self.api.responses["/actions/runs/11/jobs?per_page=50"]["jobs"][0]["steps"] = [
            witness_step("Unit and replay tests")]
        self.api.logs[111] = command_log(script, output)

    def test_all_skipped_unittest_cases_do_not_establish_execution(self):
        class Skipped(unittest.TestCase):
            @unittest.skip("unavailable fixture")
            def runTest(self):
                self.fail("skipped body must not run")
        stream = io.StringIO()
        result = unittest.TextTestRunner(stream=stream, verbosity=2).run(
            unittest.TestSuite(Skipped() for _ in range(3)))
        self.assertEqual((result.testsRun, len(result.skipped)), (3, 3))
        for diagnostic in ("", "runtime ... ok\n"):
            with self.subTest(diagnostic=diagnostic):
                self.python_witness(diagnostic + stream.getvalue())
                checks, _, _ = self.collect()
                row = next(row for row in checks if row["id"] == "Unit and replay tests")
                self.assertFalse(row["executed"])
                self.assertIn("required_check_not_executed", [v["code"] for v in self.violations(checks)])

    def test_skipped_subtests_do_not_erase_a_successful_test(self):
        class Mixed(unittest.TestCase):
            def test_pass(self):
                """A description uses the next line of verbose unittest output."""
                self.assertTrue(True)

            def test_subtests(self):
                for number in range(2):
                    with self.subTest(number=number):
                        self.skipTest("unavailable fixture")
        stream = io.StringIO()
        result = unittest.TextTestRunner(stream=stream, verbosity=2).run(
            unittest.defaultTestLoader.loadTestsFromTestCase(Mixed))
        self.assertEqual((result.testsRun, len(result.skipped)), (2, 2))
        self.python_witness(stream.getvalue())
        checks, _, waiting = self.collect()
        self.assertTrue(next(row for row in checks if row["id"] == "Unit and replay tests")["executed"])
        self.assertFalse(waiting)

    def test_earlier_or_later_same_job_summary_cannot_certify_zero_rust_tests(self):
        zero = "test result: ok. 0 passed; 0 failed; 0 ignored\n"
        positive = "test result: ok. 3 passed; 0 failed; 0 ignored\n"
        for helper_before in (True, False):
            with self.subTest(helper_before=helper_before):
                self.api.responses["/actions/runs/12/jobs?per_page=50"]["jobs"] = [{
                    "id": 112, "conclusion": "success", "steps": [
                        witness_step("Format"), witness_step("Clippy"), witness_step("Test", number=4),
                        witness_step("Helper", number=3 if helper_before else 5,
                                     start=10 if helper_before else 11, end=10 if helper_before else 11)]}]
                main = command_log(RUST_COMMAND, zero, start=10.4, finish=11.1)
                helper = command_log("python3 tools/diagnostics.py", positive,
                                     start=10.1 if helper_before else 11.4,
                                     finish=10.2 if helper_before else 11.5)
                self.api.logs[112] = helper + main if helper_before else main + helper
                checks, _, waiting = self.collect()
                row = next(row for row in checks if row["id"] == "Test")
                self.assertFalse(row["executed"])
                self.assertFalse(waiting)

    def test_final_unittest_summary_cannot_borrow_an_earlier_invocations_count(self):
        self.python_witness("Ran 3 tests in 0.1s\nOK\nRan 0 tests in 0.0s\nOK\n")
        checks, _, _ = self.collect()
        self.assertFalse(next(row for row in checks if row["id"] == "Unit and replay tests")["executed"])

    def test_following_action_without_inputs_cannot_supply_a_test_summary(self):
        self.python_witness("Ran 0 tests in 0.0s\nOK\n")
        self.api.logs[111] += (
            "2026-11-05T00:00:11.400000Z ##[group]Run example/diagnostic@abc123\n"
            "2026-11-05T00:00:11.400001Z ##[endgroup]\n"
            "2026-11-05T00:00:11.500000Z Ran 3 tests in 0.0s\n"
            "2026-11-05T00:00:11.500001Z OK\n")
        checks, _, waiting = self.collect()
        self.assertFalse(next(row for row in checks if row["id"] == "Unit and replay tests")["executed"])
        self.assertFalse(waiting)

    def test_unrecognized_or_duplicate_preamble_is_incomplete(self):
        for suffix in (
            "2026-11-05T00:00:11.400000Z ##[group]Run opaque wrapper\n"
            "2026-11-05T00:00:11.500000Z Ran 3 tests in 0.0s\n"
            "2026-11-05T00:00:11.500001Z OK\n",
            command_log(PYTHON_COMMAND, "Ran 3 tests in 0.0s\nOK\n", start=11.4, finish=11.5),
        ):
            with self.subTest(suffix=suffix):
                self.python_witness("Ran 0 tests in 0.0s\nOK\n")
                self.api.logs[111] += suffix
                checks, limits, waiting = self.collect()
                self.assertTrue(waiting)
                self.assertIn("execution_step_output_unavailable", limits)
                self.assertNotIn("Unit and replay tests", [row["id"] for row in checks])

    def test_delayed_preamble_within_the_step_interval_is_accepted(self):
        self.python_witness("Ran 3 tests in 0.1s\nOK\n")
        step = self.api.responses["/actions/runs/11/jobs?per_page=50"]["jobs"][0]["steps"][0]
        step["completed_at"] = "2026-11-05T00:00:12Z"
        self.api.logs[111] = command_log(PYTHON_COMMAND, "Ran 3 tests in 0.1s\nOK\n", start=11.1, finish=12.1)
        checks, limits, waiting = self.collect()
        self.assertTrue(next(row for row in checks if row["id"] == "Unit and replay tests")["executed"])
        self.assertEqual((limits, waiting), ([], False))

    def test_failed_named_step_remains_failed_without_a_downloadable_log(self):
        self.api.responses["/actions/runs/11/jobs?per_page=50"]["jobs"][0]["steps"][0]["conclusion"] = "failure"
        self.api.logs[111] = ""
        checks, limits, waiting = self.collect()
        self.assertIn("required_check_failed", [v["code"] for v in self.violations(checks)])
        self.assertIn("execution_log_unavailable", limits)
        self.assertTrue(waiting)

    def test_printed_script_is_not_test_output(self):
        script = "# Ran 3 tests in 0.0s\n" + PYTHON_COMMAND
        path = ".github/workflows/drift-input.yml"
        text = (self.root / path).read_text().replace("run: " + PYTHON_COMMAND,
            "run: |\n          " + script.replace("\n", "\n          "))
        (self.root / path).write_text(text)
        self.api.files[path, HEAD] = text
        self.python_witness("Ran 0 tests in 0.0s\nOK\n", script)
        checks, _, waiting = self.collect()
        self.assertFalse(next(row for row in checks if row["id"] == "Unit and replay tests")["executed"])
        self.assertFalse(waiting)

    def test_missing_step_identity_is_incomplete_not_zero_execution(self):
        self.python_witness("Ran 3 tests in 0.1s\nOK\n")
        self.api.responses["/actions/runs/11/jobs?per_page=50"]["jobs"][0]["steps"][0].pop("started_at")
        checks, limits, waiting = self.collect()
        self.assertTrue(waiting)
        self.assertIn("execution_step_output_unavailable", limits)
        self.assertNotIn("Unit and replay tests", [row["id"] for row in checks])

    def test_uv_multiline_script_and_nested_output_groups_retain_the_result(self):
        script = "uv python install 3.12.16\nuv run --no-project --python 3.12.16 " + PYTHON_COMMAND
        path = ".github/workflows/drift-input.yml"
        text = (self.root / path).read_text().replace("run: " + PYTHON_COMMAND,
            "run: |\n          " + script.replace("\n", "\n          "))
        (self.root / path).write_text(text)
        self.api.files[path, HEAD] = text
        self.python_witness("##[group]nested diagnostic\nvalue\n##[endgroup]\nRan 3 tests in 0.1s\nOK\n", script)
        checks, limits, waiting = self.collect()
        self.assertTrue(next(row for row in checks if row["id"] == "Unit and replay tests")["executed"])
        self.assertEqual((limits, waiting), ([], False))

    def test_zero_doc_tests_do_not_erase_executed_rust_tests(self):
        self.api.responses["/actions/runs/12/jobs?per_page=50"]["jobs"][0]["steps"][2] = witness_step("Test")
        self.api.logs[112] = command_log(RUST_COMMAND,
            "test result: ok. 3 passed; 0 failed; 0 ignored\n"
            "test result: ok. 0 passed; 0 failed; 0 ignored\n")
        checks, _, waiting = self.collect()
        self.assertTrue(next(row for row in checks if row["id"] == "Test")["executed"])
        self.assertFalse(waiting)

    def test_another_jobs_test_count_cannot_certify_zero_test_execution(self):
        self.api.responses["/actions/runs/12/jobs?per_page=50"] = {"jobs": [
            {"id": 112, "conclusion": "success", "steps": [
                {"name": "Format", "conclusion": "success"}, {"name": "Clippy", "conclusion": "success"}]},
            {"id": 114, "conclusion": "success", "steps": [witness_step("Test")]}]}
        self.api.logs[112] = "test result: ok. 142 passed; 0 failed; 0 ignored\n"
        self.api.logs[114] = command_log(RUST_COMMAND, "test result: ok. 0 passed; 0 failed; 0 ignored\n")
        checks, _, _ = self.collect()
        test = [row for row in checks if row["id"] == "Test"][0]
        self.assertEqual(test["job_id"], 114)
        self.assertFalse(test["executed"])
        self.assertIn("required_check_not_executed", [row["code"] for row in self.violations(checks)])

    def test_missing_log_is_incomplete_and_cannot_certify_execution(self):
        self.api.logs[112] = ""
        checks, limits, waiting = self.collect()
        self.assertTrue(waiting)
        self.assertIn("execution_log_unavailable", limits)
        self.assertTrue(self.violations(checks))

    def test_synthetic_contract_check_has_no_runner_log_to_download(self):
        # GitHub includes checks created through the Checks API in the producer's
        # jobs inventory, although they have no steps or downloadable runner log.
        self.api.responses["/actions/runs/12/jobs?per_page=50"]["jobs"].append({
            "id": 114, "name": "CCE drift contracts", "conclusion": "failure", "steps": []})
        def log(job):
            if job == 114:
                raise urllib.error.HTTPError(
                    "https://api.github.com/jobs/114/logs", 404, "Not Found", {}, None)
            return self.api.logs[job]
        self.api.log = mock.Mock(side_effect=log)
        checks, limits, waiting = self.collect()
        self.assertEqual((limits, waiting, self.violations(checks)), ([], False, []))
        self.assertNotIn(mock.call(114), self.api.log.call_args_list)

    def test_unrelated_job_logs_cannot_invalidate_required_execution(self):
        for log in (None, "x" * 1_000_001):
            with self.subTest(log="missing" if log is None else "oversized"):
                self.api.responses["/actions/runs/12/jobs?per_page=50"]["jobs"] = [
                    {"id": 112, "conclusion": "success", "steps": [
                        witness_step(name, number=i + 1) for i, name in enumerate(("Format", "Clippy", "Test"))]},
                    {"id": 114, "conclusion": "failure", "steps": [
                        {"name": "Publish optional artifact", "conclusion": "failure"}]}]
                if log is not None:
                    self.api.logs[114] = log
                checks, limits, waiting = self.collect()
                self.assertEqual((limits, waiting, self.violations(checks)), ([], False, []))
                self.assertNotIn(("log", 114), self.api.calls)

    def test_unavailable_required_log_does_not_discard_other_jobs_witnesses(self):
        for log in (None, "", "x" * 1_000_001):
            with self.subTest(log="missing" if log is None else "empty" if not log else "oversized"):
                self.api.responses["/actions/runs/12/jobs?per_page=50"]["jobs"] = [
                    {"id": 112, "conclusion": "success", "steps": [
                        {"name": "Format", "conclusion": "success"},
                        {"name": "Clippy", "conclusion": "success"}]},
                    {"id": 114, "conclusion": "success", "steps": [witness_step("Test")]}]
                self.api.logs[112] = "Format and Clippy finished\n"
                if log is not None:
                    self.api.logs[114] = log
                checks, limits, waiting = self.collect()
                self.assertTrue(waiting)
                self.assertIn("execution_log_unavailable", limits)
                rows = {row["id"]: row for row in checks}
                self.assertTrue(rows["Format"]["executed"])
                self.assertTrue(rows["Clippy"]["executed"])
                self.assertNotIn("Test", rows)
                self.assertIn("execution_step_output_unavailable", limits)
                self.assertEqual(self.violations(checks), [])

    def test_duplicate_required_steps_remain_ambiguous(self):
        self.api.responses["/actions/runs/12/jobs?per_page=50"]["jobs"].append({
            "id": 114, "conclusion": "success", "steps": [{"name": "Test", "conclusion": "success"}]})
        self.api.logs[114] = self.api.logs[112]
        checks, _, _ = self.collect()
        test = next(row for row in checks if row["id"] == "Test")
        self.assertFalse(test["executed"])
        self.assertTrue(self.violations(checks))

    def test_cumulative_markdown_pr_does_not_require_filtered_rust(self):
        self.api.responses[RUNS]["workflow_runs"] = [run("drift-input", 11)]
        checks, limits, waiting = self.collect(markdown=True)
        self.assertEqual((limits, waiting, self.violations(checks)), ([], False, []))
        self.assertNotIn(("api", HISTORY), self.api.calls)
        self.assertEqual(len(checks), 2)

    def test_unfiltered_pr_producer_still_requires_current_rust_on_markdown(self):
        path = ".github/workflows/rust.yml"
        trusted = (self.root / path).read_text().replace("  pull_request:\n    paths-ignore: ['**.md']", "  pull_request:")
        (self.root / path).write_text(trusted)
        for ref in (HEAD, OLD, MERGE):
            self.api.files[path, ref] = trusted
        self.api.responses[RUNS]["workflow_runs"] = [run("drift-input", 11)]
        checks, limits, waiting = self.collect(markdown=True)
        self.assertTrue(waiting)
        self.assertIn("required_execution_unavailable", limits)
        self.assertNotIn(("api", HISTORY), self.api.calls)

    def test_push_rust_cannot_satisfy_required_pr_merge_validation(self):
        path = ".github/workflows/rust.yml"
        trusted = (self.root / path).read_text().replace("  pull_request:\n    paths-ignore: ['**.md']", "  pull_request:")
        (self.root / path).write_text(trusted)
        for ref in (HEAD, OLD, MERGE):
            self.api.files[path, ref] = trusted
        self.assertTrue(self.collect()[2])

    def test_old_pr_run_cannot_certify_a_new_base_with_unchanged_head(self):
        self.pr_rust()
        self.api.responses[RUNS]["workflow_runs"][-1]["pull_requests"][0]["base"]["sha"] = OLD
        checks, limits, _ = self.collect()
        self.assertIn("producer_merge_identity_unavailable", limits)
        self.assertNotIn("producer_definition_unverified", limits)
        self.assertTrue(self.violations(checks))

    def test_current_merge_pointer_cannot_relabel_an_older_tested_merge(self):
        self.pr_rust()
        self.api.logs[112] = self.api.logs[112].replace(MERGE, OLD)
        checks, limits, _ = self.collect()
        self.assertIn("producer_merge_identity_unavailable", limits)
        self.assertNotIn("producer_definition_unverified", limits)
        self.assertTrue(self.violations(checks))

    def test_newer_applicable_failed_run_is_not_hidden_by_older_green(self):
        self.reuse()
        newer = run("rust", 14, BASE)
        newer["conclusion"] = "failure"
        self.api.responses[HISTORY]["workflow_runs"].insert(0, newer)
        self.api.responses["/compare/" + BASE + "..." + HEAD] = {
            "status": "ahead", "merge_base_commit": {"sha": BASE}, "files": [{"filename": "README.md"}]}
        self.api.files[".github/workflows/rust.yml", BASE] = self.api.files[".github/workflows/rust.yml", HEAD]
        self.api.responses["/actions/runs/14/jobs?per_page=50"] = {"jobs": [{"id": 114, "conclusion": "failure", "steps": [
            {"name": "Format", "conclusion": "success"}, {"name": "Clippy", "conclusion": "success"},
            {"name": "Test", "conclusion": "failure"}]}]}
        self.api.logs[114] = "test result: FAILED. 1 passed; 1 failed\n"
        checks, _, _ = self.collect()
        self.assertIn("required_check_failed", [v["code"] for v in self.violations(checks)])

    def test_branch_without_pr_does_not_invent_required_leak_scan(self):
        candidate = source()
        candidate["pulls"] = []
        checks, _, waiting = execution_checks(self.api, candidate, False, trusted_root=self.root)
        self.assertFalse(waiting)
        self.assertEqual(len(checks), 4)


if __name__ == "__main__":
    unittest.main()
