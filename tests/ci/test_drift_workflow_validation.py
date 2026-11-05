"""Bounded runtime provisioning approval preserves workflow and execution evidence."""
from __future__ import annotations

import hashlib
import unittest

import test_drift_execution as fixtures


PATH = ".github/workflows/drift-input.yml"
PIN = "94527f2e458b27549849d47d273a16bec83a01e9"
POLICY = {"policy": "python-runtime-v1"}
AUTO = "producer_definition_auto_validated_drift_input"
STEP = "      - name: Unit and replay tests\n"


def setup_action(version="0.13.7", pin=PIN):
    return (f"      - uses: astral-sh/setup-uv@{pin}\n"
            "        with:\n"
            f"          version: '{version}'\n"
            "          enable-cache: false\n")


def runtime_script(python="3.12.16"):
    return f"uv python install {python}\nuv run --no-project --python {python} " + fixtures.PYTHON_COMMAND


class RuntimeWorkflowTests(unittest.TestCase):
    def setUp(self):
        self.fixture = fixtures.ExecutionTests()
        self.fixture.setUp()
        self.addCleanup(self.fixture.doCleanups)
        self.api = self.fixture.api
        self.root = self.fixture.root
        self.trusted = (self.root / PATH).read_text()
        self.api.responses["/pulls/9/reviews?per_page=100&page=1"] = []

    def workflow(self, uv="0.13.7", python="3.12.16", script=None):
        self.assertEqual(self.trusted.count(STEP), 1)
        self.assertEqual(self.trusted.count("        run: " + fixtures.PYTHON_COMMAND), 1)
        text = self.trusted.replace(STEP, setup_action(uv) + STEP)
        command = runtime_script(python) if script is None else script
        return text.replace("        run: " + fixtures.PYTHON_COMMAND,
                            "        run: |\n          " + command.replace("\n", "\n          "))

    def configure(self, text=None, script=None, output="Ran 3 tests in 0.1s\nOK\n"):
        text = self.workflow() if text is None else text
        script = runtime_script() if script is None else script
        for reference in (fixtures.HEAD, fixtures.MERGE):
            self.api.files[PATH, reference] = text
        self.fixture.python_witness(output, script)
        return text

    def collect(self):
        checks, limits, waiting = self.fixture.collect()
        return checks, limits, waiting, self.fixture.violations(checks)

    def python_row(self, checks):
        return next(row for row in checks if row["id"] == "Unit and replay tests")

    def assert_auto_success(self, candidate):
        checks, limits, waiting, violations = self.collect()
        self.assertEqual(violations, [])
        self.assertFalse(waiting)
        self.assertIn(AUTO, limits)
        self.assertNotIn("producer_definition_owner_accepted_drift_input", limits)
        row = self.python_row(checks)
        self.assertEqual(row["producer_acceptance"], POLICY)
        self.assertTrue(row["executed"])
        self.assertEqual(row["status"], "pass")
        self.assertEqual(row["source_sha"], fixtures.HEAD)
        self.assertEqual(row["workflow_sha256"], hashlib.sha256(candidate.encode()).hexdigest())
        self.assertFalse(any(call[:2] == ("api", "/pulls/9/reviews?per_page=100&page=1") for call in self.api.calls))

    def assert_not_automatic(self, text, script=None):
        self.configure(text, script)
        checks, limits, waiting, violations = self.collect()
        self.assertNotIn(AUTO, limits)
        self.assertFalse(any(row.get("producer_acceptance") == POLICY for row in checks))
        self.assertTrue(waiting or violations, "unsupported workflow certified without owner acceptance")

    def test_pinned_runtime_provisioning_certifies_without_review_and_keeps_actual_hash(self):
        candidate = self.configure()
        self.assertNotEqual(candidate, self.trusted)
        self.assert_auto_success(candidate)

    def test_managed_runtime_versions_can_change_again_without_review(self):
        old = self.workflow(uv="0.13.6", python="3.12.15")
        (self.root / PATH).write_text(old)
        candidate = self.configure()
        self.assertNotEqual(old, candidate)
        self.assert_auto_success(candidate)

    def test_managed_runtime_can_revert_to_the_trusted_direct_test_command(self):
        old = self.workflow()
        (self.root / PATH).write_text(old)
        candidate = self.configure(self.trusted, fixtures.PYTHON_COMMAND)
        self.assertNotEqual(old, candidate)
        self.assert_auto_success(candidate)

    def test_python_runtime_change_preserves_an_observed_test_failure(self):
        self.configure(output="Ran 3 tests in 0.1s\nFAILED (failures=1)\n")
        self.api.responses["/actions/runs/11/jobs?per_page=50"]["jobs"][0]["steps"][0]["conclusion"] = "failure"
        checks, limits, waiting, violations = self.collect()
        self.assertIn(AUTO, limits)
        self.assertEqual(self.python_row(checks)["producer_acceptance"], POLICY)
        self.assertEqual([row["code"] for row in violations], ["required_check_failed"])
        self.assertFalse(waiting)

    def test_python_runtime_change_does_not_turn_a_skipped_step_into_execution(self):
        self.configure()
        self.api.responses["/actions/runs/11/jobs?per_page=50"]["jobs"][0]["steps"][0]["conclusion"] = "skipped"
        checks, limits, waiting, violations = self.collect()
        self.assertIn(AUTO, limits)
        self.assertFalse(self.python_row(checks)["executed"])
        self.assertEqual([row["code"] for row in violations], ["required_check_not_executed"])
        self.assertFalse(waiting)

    def test_valid_runtime_change_still_requires_nonzero_test_execution(self):
        self.configure(output="Ran 0 tests in 0.0s\nOK\n")
        checks, limits, waiting, violations = self.collect()
        self.assertIn(AUTO, limits)
        self.assertFalse(self.python_row(checks)["executed"])
        self.assertEqual([row["code"] for row in violations], ["required_check_not_executed"])
        self.assertFalse(waiting)

    def test_valid_runtime_change_cannot_certify_an_all_skipped_suite(self):
        self.configure(output="test_one (sample.Tests.test_one) ... skipped 'unavailable'\n\nRan 1 test in 0.0s\n\nOK (skipped=1)\n")
        checks, limits, waiting, violations = self.collect()
        self.assertIn(AUTO, limits)
        self.assertFalse(self.python_row(checks)["executed"])
        self.assertEqual([row["code"] for row in violations], ["required_check_not_executed"])
        self.assertFalse(waiting)

    def test_changed_workflow_requires_its_actual_script_in_the_job_log(self):
        self.configure()
        self.api.logs[111] = fixtures.command_log(fixtures.PYTHON_COMMAND, "Ran 3 tests in 0.1s\nOK\n")
        checks, limits, waiting, _ = self.collect()
        self.assertIn(AUTO, limits)
        self.assertTrue(waiting)
        self.assertIn("execution_step_output_unavailable", limits)
        self.assertNotIn("Unit and replay tests", [row["id"] for row in checks])

    def test_another_step_cannot_supply_the_changed_workflows_test_count(self):
        self.configure(output="Ran 0 tests in 0.0s\nOK\n")
        self.api.responses["/actions/runs/11/jobs?per_page=50"]["jobs"][0]["steps"].append(
            fixtures.witness_step("Helper", number=2, start=12, end=13))
        self.api.logs[111] += fixtures.command_log("python3 tools/diagnostics.py", "Ran 9 tests in 0.1s\nOK\n", start=12.1, finish=13.1)
        checks, limits, waiting, violations = self.collect()
        self.assertIn(AUTO, limits)
        self.assertFalse(self.python_row(checks)["executed"])
        self.assertEqual([row["code"] for row in violations], ["required_check_not_executed"])
        self.assertFalse(waiting)

    def test_runtime_approval_does_not_bypass_tested_merge_identity(self):
        self.configure()
        self.api.responses[fixtures.RUNS]["workflow_runs"][0] = fixtures.run("drift-input", 11, event="pull_request")
        self.api.logs[111] = ("2026-11-05T00:00:09.100000Z [command]/usr/bin/git log -1 --format=%H\n"
                              "2026-11-05T00:00:09.100001Z " + fixtures.OLD + "\n" + self.api.logs[111])
        checks, limits, _, violations = self.collect()
        self.assertIn(AUTO, limits)
        self.assertIn("producer_merge_identity_unavailable", limits)
        self.assertTrue(violations)
        self.assertNotIn("Unit and replay tests", [row["id"] for row in checks])

    def test_only_the_allowlisted_pinned_setup_action_is_automatic(self):
        candidate = self.workflow()
        alternatives = [candidate.replace(PIN, "1" * 40),
                        candidate.replace(PIN, "v7"),
                        candidate.replace("astral-sh/setup-uv@", "example/setup-uv@")]
        for changed in alternatives:
            with self.subTest(changed=changed):
                self.assert_not_automatic(changed)

    def test_runtime_versions_must_be_exact_numeric_pins(self):
        for uv, python in (("latest", "3.12.16"), ("0.13", "3.12.16"),
                           ("0.13.7", "latest"), ("0.13.7", "3.12")):
            with self.subTest(uv=uv, python=python):
                self.assert_not_automatic(self.workflow(uv=uv, python=python), runtime_script(python))

    def test_install_and_run_must_select_the_same_python_version(self):
        script = "uv python install 3.12.15\nuv run --no-project --python 3.12.16 " + fixtures.PYTHON_COMMAND
        self.assert_not_automatic(self.workflow(script=script), script)

    def test_provisioning_action_must_occur_once_with_the_expected_options(self):
        candidate = self.workflow()
        alternatives = [candidate.replace(setup_action(), setup_action() + setup_action()),
                        candidate.replace("          enable-cache: false", "          enable-cache: true"),
                        candidate.replace("          enable-cache: false\n", ""),
                        candidate.replace("          enable-cache: false", "          enable-cache: false\n          extra-option: true")]
        for changed in alternatives:
            with self.subTest(changed=changed):
                self.assert_not_automatic(changed)

    def test_automatic_runtime_change_cannot_disable_or_soften_the_required_step(self):
        candidate = self.workflow()
        command = "        run: |\n          " + runtime_script().replace("\n", "\n          ") + "\n"
        alternatives = [candidate.replace(STEP + command, "")]
        alternatives += [candidate.replace(STEP, STEP + setting) for setting in
                         ("        if: false\n", "        continue-on-error: true\n")]
        for changed in alternatives:
            with self.subTest(changed=changed):
                self.assertNotEqual(changed, candidate)
                self.assert_not_automatic(changed)

    def test_automatic_runtime_change_cannot_narrow_test_discovery(self):
        for command in (fixtures.PYTHON_COMMAND.replace("tests/ci", "tests/ci/subset"),
                        fixtures.PYTHON_COMMAND + " -p test_selected.py",
                        "python3 -m unittest test_selected"):
            with self.subTest(command=command):
                script = runtime_script().replace(fixtures.PYTHON_COMMAND, command)
                self.assert_not_automatic(self.workflow(script=script), script)

    def test_printed_guarded_or_short_circuited_commands_are_not_automatic(self):
        command = runtime_script().splitlines()[1]
        for replacement in ('echo "' + command + '"', "# " + command,
                            "if false; then " + command + "; fi",
                            "exit 0\n" + command, command + " || true"):
            with self.subTest(replacement=replacement):
                script = "uv python install 3.12.16\n" + replacement
                self.assert_not_automatic(self.workflow(script=script), script)

    def test_remaining_workflow_must_match_the_trusted_definition(self):
        candidate = self.workflow()
        alternatives = [candidate.replace("branches: ['**']", "branches: ['selected-only']"),
                        candidate.replace("    runs-on: ubuntu-latest", "    continue-on-error: true\n    runs-on: ubuntu-latest"),
                        candidate.replace("      - uses: actions/checkout@", "      - uses: example/checkout@")]
        for changed in alternatives:
            with self.subTest(changed=changed):
                self.assert_not_automatic(changed)

    def test_python_automatic_approval_does_not_approve_a_changed_rust_definition(self):
        self.configure()
        self.fixture.change_rust()
        checks, limits, waiting, violations = self.collect()
        self.assertIn(AUTO, limits)
        self.assertEqual(self.python_row(checks)["producer_acceptance"], POLICY)
        self.assertEqual([row["code"] for row in violations], ["workflow_definition_awaiting_acceptance"])
        self.assertIn("producer_definition_unaccepted", limits)
        self.assertFalse(waiting)

    def test_other_workflow_changes_keep_explicit_owner_acceptance(self):
        candidate = self.trusted.replace(STEP, "      - name: Additional diagnostics\n        run: python3 --version\n" + STEP)
        self.configure(candidate, fixtures.PYTHON_COMMAND)
        self.api.responses["/pulls/9/reviews?per_page=100&page=1"] = [self.fixture.review(self.fixture.OWNER)]
        checks, limits, waiting, violations = self.collect()
        self.assertEqual(violations, [])
        self.assertFalse(waiting)
        self.assertNotIn(AUTO, limits)
        self.assertIn("producer_definition_owner_accepted_drift_input", limits)
        self.assertEqual(self.python_row(checks)["producer_acceptance"], {"pull": 9, "review": 1})


if __name__ == "__main__":
    unittest.main()
