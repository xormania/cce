"""Candidate-scoped evidence behavior and safe output boundaries."""
from __future__ import annotations

import copy
import importlib.util
import json
from pathlib import Path
import unittest


ROOT = Path(__file__).resolve().parents[2]
SPEC = importlib.util.spec_from_file_location("drift_contracts", ROOT / "tools/ci/drift_contracts.py")
contracts = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(contracts)
HEAD, OLD = "1" * 40, "2" * 40


def bundle(**rows):
    return {"source": {"repository": "example/store", "head_sha": HEAD, "base_sha": OLD}, **rows}


def resolve(value):
    if value == "$head":
        return HEAD
    if value == "$old":
        return OLD
    if isinstance(value, dict):
        return {key: resolve(item) for key, item in value.items()}
    if isinstance(value, list):
        return [resolve(item) for item in value]
    return value


class ContractTests(unittest.TestCase):
    def test_replay_pairs(self):
        fixture = json.loads((ROOT / "tests/ci/fixtures/drift_cases.json").read_text())
        self.assertEqual(fixture["origin"], "historical-transformed")
        for case in fixture["cases"]:
            with self.subTest(case=case["name"]):
                result = contracts.evaluate(bundle(**resolve(case["bundle"])))
                self.assertEqual(sorted(row["code"] for row in result["violations"]), sorted(case["codes"]))
                self.assertEqual(result["limitations"], case.get("limitations", []))

    def test_known_defect_fails_and_correction_clears_same_subject(self):
        row = {"id": "complete-case", "applicable": True, "status": "complete", "case_statuses": ["partial"], "evidence_sha": HEAD}
        failed = contracts.evaluate(bundle(claims=[row]))
        row["case_statuses"] = ["green"]
        passed = contracts.evaluate(bundle(claims=[row]))
        self.assertEqual([v["code"] for v in failed["violations"]], ["claim_case_mismatch"])
        self.assertEqual(passed["violations"], [])

    def test_subject_and_text_cannot_leak_through_public_result(self):
        private = "DO_NOT_PUBLISH_private_fixture_marker"
        row = {"id": private, "applicable": True, "status": "green", "case_statuses": ["partial"], "evidence_sha": HEAD, "excerpts": [private]}
        result = contracts.evaluate(bundle(claims=[row]))
        public = {"violations": result["violations"], "limitations": result["limitations"]}
        self.assertNotIn(private, json.dumps(public))
        self.assertEqual(result["packets"][0]["excerpts"][0]["text"], private)
        for violation in result["violations"]:
            self.assertRegex(violation["subject"], r"^[0-9a-f]{24}$")
            self.assertIn(violation["code"], contracts.VIOLATION_CODES)

    def test_subject_stays_stable_across_unrelated_source_change(self):
        row = {"id": "a", "applicable": True, "status": "green", "case_statuses": ["partial"], "evidence_sha": HEAD, "excerpts": ["A claim."]}
        before = contracts.evaluate(bundle(claims=[row]))
        changed = bundle(claims=[copy.deepcopy(row)])
        changed["source"]["head_sha"] = OLD
        changed["claims"][0]["evidence_sha"] = OLD
        after = contracts.evaluate(changed)
        self.assertEqual(before["packets"], after["packets"])

    def test_raw_outcome_does_not_override_accepted_disposition(self):
        row = {"id": "a", "applicable": True, "disposition": "equivalent", "raw_outcome": "caught", "runs": [], "claimed_semantic": False}
        self.assertEqual(contracts.evaluate(bundle(mutations=[row]))["violations"], [])

    def test_explicit_abort_never_counts_even_if_other_flags_are_true(self):
        for exit_code in [101, -6, 134]:
            with self.subTest(exit=exit_code):
                row = {"id": "a", "applicable": True, "disposition": "caught", "claimed_semantic": True,
                       "runs": [{"exit": exit_code, "executed": True, "failed_by_assertion": True, "aborted": True}]}
                result = contracts.evaluate(bundle(mutations=[row]))
                self.assertEqual([v["code"] for v in result["violations"]], ["semantic_catch_not_demonstrated"])

    def test_successful_exit_is_not_a_catch(self):
        row = {"id": "a", "applicable": True, "disposition": "caught", "claimed_semantic": True,
               "runs": [{"exit": 0, "executed": True, "failed_by_assertion": True}]}
        self.assertEqual(contracts.evaluate(bundle(mutations=[row]))["violations"][0]["code"], "semantic_catch_not_demonstrated")

    def test_signal_exits_cannot_certify_an_assertion_catch_without_normalization(self):
        for exit_code in [-9, -11, -15, 137, 139, 143]:
            with self.subTest(exit=exit_code):
                row = {"id": "a", "applicable": True, "disposition": "caught", "claimed_semantic": True,
                       "runs": [{"exit": exit_code, "executed": True, "failed_by_assertion": True}]}
                result = contracts.evaluate(bundle(mutations=[row]))
                self.assertEqual([v["code"] for v in result["violations"]], ["semantic_catch_not_demonstrated"])

    def test_explicit_subprocess_convention_distinguishes_positive_application_exit(self):
        row = {"id": "a", "applicable": True, "disposition": "caught", "claimed_semantic": True,
               "runs": [{"exit": 134, "exit_convention": "subprocess", "executed": True,
                         "failed_by_assertion": True}]}
        self.assertEqual(contracts.evaluate(bundle(mutations=[row]))["violations"], [])
        row["runs"][0]["aborted"] = True
        self.assertEqual(contracts.evaluate(bundle(mutations=[row]))["violations"][0]["code"],
                         "semantic_catch_not_demonstrated")

    def test_claim_case_limit_is_distinct_from_evidence_row_limit(self):
        row = {"id": "a", "applicable": True, "status": "green", "evidence_sha": HEAD,
               "case_statuses": ["green"] * 999 + ["partial"]}
        result = contracts.evaluate(bundle(claims=[row]))
        self.assertEqual([v["code"] for v in result["violations"]], ["claim_case_mismatch"])
        row["case_statuses"].append("green")
        with self.assertRaisesRegex(ValueError, "^Invalid normalized evidence$"):
            contracts.evaluate(bundle(claims=[row]))
        with self.assertRaisesRegex(ValueError, "^Invalid normalized evidence$"):
            contracts.evaluate(bundle(semantic=[{"id": str(index)} for index in range(257)]))

    def test_missing_identity_and_case_inventory_are_explicit_limitations(self):
        result = contracts.evaluate(bundle(claims=[{"id": "a", "applicable": True, "status": "green", "case_statuses": []}]))
        self.assertEqual(result["violations"], [])
        self.assertEqual(result["limitations"], ["incomplete_case_inventory", "missing_evidence_identity"])

    def test_required_failed_check_preserves_failure(self):
        result = contracts.evaluate(bundle(checks=[{"id": "compile", "status": "fail", "required": True, "executed": True, "source_sha": HEAD}]))
        self.assertEqual(result["violations"][0]["code"], "required_check_failed")

    def test_unaccepted_workflow_is_a_policy_violation_without_execution_claim(self):
        row = {"id": "producer:trusted-definition", "status": "unknown", "required": True,
               "executed": None, "source_sha": HEAD, "definition_status": "awaiting_acceptance"}
        result = contracts.evaluate(bundle(checks=[row]))
        self.assertEqual([v["code"] for v in result["violations"]],
                         ["workflow_definition_awaiting_acceptance"])
        self.assertIn("workflow_definition_awaiting_acceptance", contracts.VIOLATION_CODES)

    def test_policy_marker_does_not_hide_a_separate_executed_failure(self):
        rows = [
            {"id": "definition", "status": "unknown", "required": True, "executed": None,
             "source_sha": HEAD, "definition_status": "awaiting_acceptance"},
            {"id": "tests", "status": "fail", "required": True, "executed": True, "source_sha": HEAD},
        ]
        result = contracts.evaluate(bundle(checks=rows))
        self.assertEqual(sorted(v["code"] for v in result["violations"]),
                         ["required_check_failed", "workflow_definition_awaiting_acceptance"])

    def test_policy_marker_does_not_hide_wrong_candidate_identity(self):
        row = {"id": "definition", "status": "unknown", "required": True, "executed": None,
               "source_sha": OLD, "definition_status": "awaiting_acceptance"}
        result = contracts.evaluate(bundle(checks=[row]))
        codes = [v["code"] for v in result["violations"]]
        self.assertIn("workflow_definition_awaiting_acceptance", codes)
        self.assertIn("evidence_candidate_mismatch", codes)
        self.assertNotIn("required_check_not_executed", codes)

    def test_legacy_execution_rows_retain_their_actual_failure_meaning(self):
        for status, executed, expected in (("fail", True, ["required_check_failed"]),
                                           ("unknown", False, ["required_check_not_executed"]),
                                           ("pass", True, [])):
            with self.subTest(status=status, executed=executed):
                row = {"id": "tests", "status": status, "required": True,
                       "executed": executed, "source_sha": HEAD}
                self.assertEqual([v["code"] for v in contracts.evaluate(bundle(checks=[row]))["violations"]], expected)

    def test_malformed_policy_marker_is_rejected_without_echoing_values(self):
        row = {"id": "definition", "status": "unknown", "required": True, "executed": None,
               "source_sha": HEAD, "definition_status": "awaiting_acceptance"}
        changes = [{"definition_status": value} for value in
                   ("untrusted-private-policy-marker", "accepted", None, True, ["awaiting_acceptance"])]
        changes += [{"executed": True}, {"executed": False}, {"status": "fail"}, {"status": "pass"}]
        changes.append({"definition_status": None, "executed": False})
        for changed in changes:
            with self.subTest(changed=changed):
                with self.assertRaises(ValueError) as caught:
                    contracts.evaluate(bundle(checks=[dict(row, **changed)]))
                self.assertEqual(str(caught.exception), "Invalid normalized evidence")

    def test_policy_violation_subject_and_reason_remain_private(self):
        marker = "untrusted-private-definition-marker"
        row = {"id": marker, "status": "unknown", "required": True, "executed": None,
               "source_sha": HEAD, "definition_status": "awaiting_acceptance", "reason": marker}
        result = contracts.evaluate(bundle(checks=[row]))
        public = {"violations": result["violations"], "limitations": result["limitations"]}
        self.assertEqual([v["code"] for v in result["violations"]],
                         ["workflow_definition_awaiting_acceptance"])
        self.assertNotIn(marker, json.dumps(public))
        self.assertRegex(result["violations"][0]["subject"], r"^[0-9a-f]{24}$")

    def test_no_evidence_is_not_a_success_claim(self):
        self.assertEqual(contracts.evaluate(bundle())["limitations"], ["no_evidence"])

    def test_packet_questions_and_evidence_are_bounded(self):
        result = contracts.evaluate(bundle(semantic=[{"id": "x", "questions": ["deferral", "resolution"], "excerpts": ["Original obligation.", "Receiving owner is recorded."]}]))
        self.assertEqual(result["packets"][0]["questions"], ["deferral", "resolution"])
        self.assertEqual([e["id"] for e in result["packets"][0]["excerpts"]], ["e1", "e2"])
        for row in [{"id": "x", "questions": ["execute-shell"]}, {"id": "x", "questions": ["proof", "proof"]}, {"id": "x", "excerpts": ["a" * 1601]}]:
            with self.subTest(row=row):
                with self.assertRaisesRegex(ValueError, "^Invalid normalized evidence$"):
                    contracts.evaluate(bundle(semantic=[row]))

    def test_input_validation_does_not_echo_input(self):
        marker = "NEVER_PRINT_THIS"
        invalid = [None, {}, bundle(checks="invalid"), bundle(checks=[{"id": marker, "status": marker}]), bundle(semantic=[{"id": marker, "excerpts": [marker] * 9}])]
        for value in invalid:
            with self.subTest(value=type(value).__name__):
                with self.assertRaises(ValueError) as raised:
                    contracts.evaluate(value)
                self.assertEqual(str(raised.exception), "Invalid normalized evidence")

    def test_bools_are_not_accepted_as_exit_codes_or_strings_as_flags(self):
        for row in [
            {"id": "x", "applicable": "true", "status": "green", "case_statuses": ["green"]},
            {"id": "x", "applicable": True, "disposition": "caught", "runs": [{"exit": True}]},
        ]:
            key = "claims" if "status" in row else "mutations"
            with self.assertRaises(ValueError):
                contracts.evaluate(bundle(**{key: [row]}))

    def test_source_validation(self):
        for change in [{"head_sha": "a"}, {"repository": "https://arbitrary.invalid/store"}, {"base_sha": 12}]:
            value = bundle()
            value["source"].update(change)
            with self.assertRaises(ValueError):
                contracts.evaluate(value)

    def test_duplicate_ids_and_invalid_unicode_are_rejected_safely(self):
        for value in [
            bundle(semantic=[{"id": "same"}, {"id": "same"}]),
            bundle(semantic=[{"id": "invalid\ud800"}]),
            bundle(semantic=[{"id": "text", "excerpts": ["invalid\ud800"]}]),
        ]:
            with self.assertRaisesRegex(ValueError, "^Invalid normalized evidence$"):
                contracts.evaluate(value)


if __name__ == "__main__":
    unittest.main()
