"""Public diagnostic identity for supplied evidence references and retained failures."""
from __future__ import annotations

import copy
import importlib.util
import io
import json
from pathlib import Path
import sys
import unittest


ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT / "tools/ci"))
SPEC = importlib.util.spec_from_file_location("support_diagnostic_client", ROOT / "tools/ci/jev_client.py")
client = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(client)
import drift_runner as runner  # noqa: E402

MODEL = "jev-1.14.0"
PRIVATE_MARKER = "private-provider-detail"


def packet():
    return {
        "subject": "a" * 16,
        "source": {"repository": "xormania/cce", "head_sha": "b" * 40, "base_sha": "c" * 40},
        "excerpts": [{"id": "e1", "text": "The check compares reopened bytes with their expected contents."},
                     {"id": "e2", "text": "The result is reported as incomplete."}],
        "questions": ["proof", "witness"],
    }


def response():
    # The expected answer inventory is specified independently of the client request.
    return {
        "model": MODEL,
        "answers": {
            "proof": {"type": "choice", "choice": "supported"},
            "witness": {"type": "choice", "choice": "semantic_behavior"},
            "support": {"type": "choice", "choice": "e1"},
        },
        "usage": {"input_tokens": 120, "output_tokens": 30, "total_tokens": 150},
    }


def diagnostic(*, stage="schema", reason="choice_not_allowed", question=None):
    value = {
        "version": 1, "stage": stage, "reason": reason,
        "http_status": 200, "response_bytes_read": 321, "response_truncated": False,
    }
    if question is not None:
        value["question"] = question
    return value


def historical_diagnostics():
    return [
        diagnostic(stage="policy", reason="duplicate_support"),
        dict(diagnostic(question="support_1"), expected_answer_count=7,
             answer_count=7, missing_answer_count=0, unexpected_answer_count=0),
        dict(diagnostic(question="support_2"), expected_answer_count=7,
             answer_count=7, missing_answer_count=0, unexpected_answer_count=0),
    ]


def observation(failure):
    return {"fingerprint": "d" * 64, "record": {
        "subject": "a" * 16, "source": packet()["source"], "status": "malformed_response",
        "requested_model": MODEL, "observed_model": MODEL, "question_version": 1,
        "provider_request_attempted": True, "provider_request_attempts": 1,
        "diagnostic": failure,
    }}


class Reply(io.BytesIO):
    status = 200


class Opener:
    def __init__(self, value):
        self.body = json.dumps(value).encode("utf-8")
        self.calls = []

    def open(self, request, timeout):
        self.calls.append((request, timeout))
        return Reply(self.body)


class SupportDiagnosticTests(unittest.TestCase):
    def diagnose(self, value):
        opener = Opener(value)
        result = client.diagnose(packet(), key="synthetic-key", opener=opener)
        self.assertEqual(result["status"], "malformed_response")
        self.assertEqual(len(opener.calls), 1)
        self.assertEqual(result["provider_request_attempts"], 1)
        self.assertEqual(result["usage"], {"input_tokens": 120, "output_tokens": 30, "total_tokens": 150})
        self.assertNotIn(PRIVATE_MARKER, json.dumps(result))
        self.assertIsInstance(result.get("diagnostic"), dict)
        self.assertEqual(result["diagnostic"]["response_bytes_read"], len(opener.body))
        self.assertFalse(result["diagnostic"]["response_truncated"])
        return result["diagnostic"]

    def assert_support_rejection(self, answer, reason):
        value = response()
        value["answers"]["support"] = answer
        failure = self.diagnose(value)
        self.assertEqual((failure["stage"], failure["reason"], failure.get("question")),
                         ("schema", reason, "support"))

    def test_standalone_support_question_can_identify_a_public_diagnostic(self):
        value = diagnostic(question="support")
        value["known_field_types"] = {"model": "string", "answers": "object"}
        clean = client.public_diagnostic(value)
        self.assertEqual(clean, value)
        self.assertIsNot(clean, value)
        self.assertIsNot(clean["known_field_types"], value["known_field_types"])
        self.assertIsNone(client.public_diagnostic(dict(value, question=PRIVATE_MARKER)))
        self.assertIsNone(client.public_diagnostic(dict(value, provider_detail=PRIVATE_MARKER)))

    def test_support_choice_rejection_names_only_the_known_question(self):
        self.assert_support_rejection({"type": "choice", "choice": PRIVATE_MARKER}, "choice_not_allowed")

    def test_support_confidence_rejections_name_only_the_known_question(self):
        for confidence in (True, -0.1, 1.1, PRIVATE_MARKER):
            with self.subTest(confidence=confidence):
                self.assert_support_rejection(
                    {"type": "choice", "choice": "e1", "confidence": confidence}, "confidence_invalid")

    def test_support_probability_rejections_name_only_the_known_question(self):
        cases = [
            ({"e1": 0.8, "none": 0.2}, "probability_inventory_mismatch"),
            ({"e1": 0.8, "e2": 0.1, PRIVATE_MARKER: 0.1}, "probability_inventory_mismatch"),
            ({"e1": True, "e2": 0, "none": 0}, "probability_value_invalid"),
            ({"e1": PRIVATE_MARKER, "e2": 0, "none": 0}, "probability_value_invalid"),
            ({"e1": -0.1, "e2": 0.1, "none": 1}, "probability_value_invalid"),
            ({"e1": 0.6, "e2": 0.6, "none": 0.6}, "probability_total_invalid"),
        ]
        for probabilities, reason in cases:
            with self.subTest(reason=reason, probabilities=probabilities):
                self.assert_support_rejection(
                    {"type": "choice", "choice": "e1", "probabilities": probabilities}, reason)

    def test_new_inventory_diagnostics_count_one_support_answer(self):
        value = response()
        value["answers"].pop("proof")
        value[PRIVATE_MARKER] = PRIVATE_MARKER
        value["error"] = {PRIVATE_MARKER: PRIVATE_MARKER}
        failure = self.diagnose(value)
        self.assertEqual((failure["stage"], failure["reason"]), ("schema", "answer_inventory_mismatch"))
        self.assertEqual(failure["expected_answer_count"], 3)
        self.assertEqual(failure["answer_count"], 2)
        self.assertEqual(failure["missing_answer_count"], 1)
        self.assertEqual(failure["unexpected_answer_count"], 0)
        self.assertEqual(failure["unknown_field_count"], 1)
        self.assertEqual(failure["known_field_types"], {
            "model": "string", "answers": "object", "usage": "object", "error": "object",
        })

    def test_historical_support_diagnostics_keep_their_public_projection(self):
        for value in historical_diagnostics():
            with self.subTest(value=value):
                before = copy.deepcopy(value)
                self.assertEqual(client.public_diagnostic(value), before)
                self.assertEqual(value, before)
                self.assertIsNone(client.public_diagnostic(dict(value, expected_answer_count=8)))

    def test_public_observations_retain_standalone_support_diagnostics(self):
        value = diagnostic(question="support")
        source = observation(value)
        before = copy.deepcopy(source)
        rows = runner.public_observations([source])
        self.assertEqual(len(rows), 1)
        self.assertIn("diagnostic", rows[0]["record"])
        self.assertEqual(rows[0]["record"]["diagnostic"], value)
        self.assertEqual(rows[0]["fingerprint"], source["fingerprint"])
        self.assertEqual(source, before)

    def test_public_observations_retain_historical_support_diagnostics(self):
        for value in historical_diagnostics():
            with self.subTest(value=value):
                source = observation(value)
                rows = runner.public_observations([source])
                self.assertEqual(len(rows), 1)
                self.assertIn("diagnostic", rows[0]["record"])
                self.assertEqual(rows[0]["record"]["diagnostic"], value)


if __name__ == "__main__":
    unittest.main()
