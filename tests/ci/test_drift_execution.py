"""Execution witnesses cover producer authenticity and real filtering behavior."""
from __future__ import annotations

import copy
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
                "steps": [{"name": "Unit and replay tests", "conclusion": "success"}]}]},
            "/actions/runs/12/jobs?per_page=50": {"jobs": [{"id": 112, "conclusion": "success",
                "steps": [{"name": name, "conclusion": "success"} for name in ("Format", "Clippy", "Test")]}]},
        }
        self.logs = {111: "Ran 89 tests in 0.1s\nOK\n",
                     112: "test result: ok. 142 passed; 0 failed; 0 ignored\n"}

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
            (target / (name + ".yml")).write_text((ROOT / ".github/workflows" / (name + ".yml")).read_text())
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
        self.api.logs[112] = "[command]/usr/bin/git log -1 --format=%H\n" + MERGE + "\n" + self.api.logs[112]

    def review(self, login, state="APPROVED", commit=HEAD, number=1):
        return {"id": number, "user": {"login": login}, "state": state, "commit_id": commit,
                "submitted_at": "2026-11-04T10:%02d:00Z" % number}

    def test_changed_producer_without_owner_acceptance_is_refused_as_unaccepted(self):
        self.change_rust()
        checks, limits, _ = self.collect()
        self.assertIn("producer_definition_unaccepted", limits)
        self.assertIn("rust:trusted-definition", [row["id"] for row in checks])
        self.assertIn("required_check_not_executed", [v["code"] for v in self.violations(checks)])
        self.assertNotIn("producer_merge_identity_unavailable", limits)

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
        self.assertIn("required_check_failed", [v["code"] for v in self.violations(checks)])
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
        self.assertIn("required_check_not_executed", [v["code"] for v in self.violations(checks)])
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
            {"id": 112, "conclusion": "success", "steps": [{"name": "Check " + kind, "conclusion": "success"}]}
            for kind in ("fmt", "clippy", "test")]}
        self.api.responses[RUNS]["workflow_runs"][1] = run("rust", 12, event="pull_request")
        self.api.logs[112] += "[command]/usr/bin/git log -1 --format=%H\n" + MERGE + "\n"
        checks, _, waiting = self.collect()
        self.assertFalse(waiting)
        self.assertEqual(self.violations(checks), [])

    def test_zero_test_execution_fails_even_when_steps_succeed(self):
        self.api.logs[111] = "Ran 0 tests in 0s\nOK\n"
        self.api.logs[112] = "test result: ok. 0 passed; 0 failed; 0 ignored\n"
        checks, _, _ = self.collect()
        self.assertEqual(len(self.violations(checks)), 2)

    def test_another_jobs_test_count_cannot_certify_zero_test_execution(self):
        self.api.responses["/actions/runs/12/jobs?per_page=50"] = {"jobs": [
            {"id": 112, "conclusion": "success", "steps": [
                {"name": "Format", "conclusion": "success"}, {"name": "Clippy", "conclusion": "success"}]},
            {"id": 114, "conclusion": "success", "steps": [{"name": "Test", "conclusion": "success"}]}]}
        self.api.logs[112] = "test result: ok. 142 passed; 0 failed; 0 ignored\n"
        self.api.logs[114] = "test result: ok. 0 passed; 0 failed; 0 ignored\n"
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
                        {"name": name, "conclusion": "success"} for name in ("Format", "Clippy", "Test")]},
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
                    {"id": 114, "conclusion": "success", "steps": [
                        {"name": "Test", "conclusion": "success"}]}]
                self.api.logs[112] = "Format and Clippy finished\n"
                if log is not None:
                    self.api.logs[114] = log
                checks, limits, waiting = self.collect()
                self.assertTrue(waiting)
                self.assertIn("execution_log_unavailable", limits)
                rows = {row["id"]: row for row in checks}
                self.assertTrue(rows["Format"]["executed"])
                self.assertTrue(rows["Clippy"]["executed"])
                self.assertFalse(rows["Test"]["executed"])
                self.assertEqual([v["code"] for v in self.violations(checks)], ["required_check_not_executed"])

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
