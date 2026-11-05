"""Question revisions invalidate answer reuse while preserving evidence identity."""

from __future__ import annotations

import copy
import os
from pathlib import Path
import sys
import tempfile
import unittest
from unittest import mock


ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT / "tools/ci"))
import drift_backlog as backlog
import drift_runner as runner
import jev_client as client
import test_drift_backlog_integration as artifact_fixtures


# Source identities captured from the previous public question contract.
PREVIOUS_ENGINE_HASH = "e6d035513716ad9eb4ac93226e235f55bf06b38bd97006ab0ca41d8ace9d0c04"
PREVIOUS_QUESTION_VERSION = 1
PACKET_FINGERPRINT = "65384ad251f21974184b0cde204b9384f20bd79fb11f25d4b3dc8271738ecd49"
PACKET = {
    "subject": "f629954ba554ef21c8e96d78",
    "source": {"repository": "xormania/cce", "head_sha": "a" * 40, "base_sha": "b" * 40,
               "branch": "feature", "pulls": [15], "run_id": 7, "attempt": 1},
    "excerpts": [{"id": "e1", "text": "Original incomplete assertion"}],
    "questions": ["proof"],
}
RECIPE = {
    "fingerprint": PACKET_FINGERPRINT, "subject": "f629954ba554ef21c8e96d78",
    "scope": "57b65703204b045f594222dfb5ea0377ddddee762791c54605d7f8a9e8399219",
    "kind": "public", "source": copy.deepcopy(PACKET["source"]), "state": "deferred",
}


class ReservationApi(artifact_fixtures.ArtifactApi):
    def api(self, path):
        value = super().api(path)
        if "artifacts" in value:
            value["artifacts"][0]["name"] = "drift-reservation"
        return value


class EngineCompatibilityTests(unittest.TestCase):
    def setUp(self):
        provider = mock.patch.object(runner, "diagnose")
        self.provider = provider.start()
        self.addCleanup(provider.stop)
        self.addCleanup(self.provider.assert_not_called)
        opener = mock.patch.object(client.urllib.request.OpenerDirector, "open")
        self.opener = opener.start()
        self.addCleanup(opener.stop)
        self.addCleanup(self.opener.assert_not_called)

    def baseline(self, api):
        with mock.patch.dict(os.environ, {"GITHUB_RUN_ID": "99"}):
            return runner.GitHub.baseline(api, "d" * 40)

    def previous_entry(self):
        return {"fingerprint": PACKET_FINGERPRINT, "record": {
            "subject": PACKET["subject"], "source": copy.deepcopy(PACKET["source"]),
            "status": "diagnosed", "question_version": PREVIOUS_QUESTION_VERSION,
            "diagnosis": {"answers": {"proof": {"choice": "overstated"}},
                          "supporting_excerpt_ids": ["e1"], "abstained": False}}}

    def test_question_revision_changes_engine_without_changing_packet_or_recipe_identity(self):
        self.assertEqual(client.QUESTION_VERSION, 2)
        self.assertNotEqual(runner.engine_hash(), PREVIOUS_ENGINE_HASH)
        self.assertEqual(runner.VERSION, 2)
        self.assertEqual(runner.fingerprint(copy.deepcopy(PACKET)), PACKET_FINGERPRINT)
        self.assertEqual(backlog.descriptor(copy.deepcopy(PACKET), PACKET_FINGERPRINT), RECIPE)

    def test_previous_question_answer_is_not_reused_or_counted_as_a_new_request(self):
        api = artifact_fixtures.ArtifactApi({11: {
            "version": 2, "engine_hash": PREVIOUS_ENGINE_HASH,
            "entries": [self.previous_entry()], "pending": [PACKET_FINGERPRINT], "backlog": [],
        }})
        previous = self.baseline(api)
        self.assertFalse(previous["baseline_missing"])
        self.assertEqual(previous["entries"], [])
        self.assertEqual(previous["pending"], [])
        self.assertEqual(previous["reserved"], [])
        selected, reused, deferred = runner.choose([copy.deepcopy(PACKET)], previous)
        self.assertEqual(selected, [{"fingerprint": PACKET_FINGERPRINT, "packet": PACKET}])
        self.assertEqual((reused, deferred), ([], []))

    def test_previous_question_reservation_reconstructs_original_recipe_without_provider_use(self):
        reserved = dict(copy.deepcopy(RECIPE), state="reserved")
        api = ReservationApi({11: {
            "version": 2, "engine_hash": PREVIOUS_ENGINE_HASH,
            "fingerprints": [PACKET_FINGERPRINT], "backlog": [reserved],
        }})
        previous = self.baseline(api)
        self.assertFalse(previous["baseline_missing"])
        self.assertEqual(previous["reserved"], [])
        self.assertEqual(previous["backlog"], [RECIPE])
        collected = []

        def original_context(source, kind, data_ref, data_key):
            collected.append((copy.deepcopy(source), kind, data_ref, data_key))
            return [copy.deepcopy(PACKET)]

        packets, retained, limitations = backlog.reconstruct(
            previous["backlog"], original_context, runner.fingerprint)
        self.assertEqual(collected, [(PACKET["source"], "public", None, None)])
        self.assertEqual(packets, [PACKET])
        self.assertEqual(retained, [RECIPE])
        self.assertEqual(limitations, [])
        selected, reused, deferred = runner.choose(packets, previous)
        self.assertEqual(selected, [{"fingerprint": PACKET_FINGERPRINT, "packet": PACKET}])
        self.assertEqual((reused, deferred), ([], []))

    def test_historical_response_observations_publish_with_legacy_diagnostics_and_usage(self):
        rows = []
        record = {
            "subject": PACKET["subject"], "source": copy.deepcopy(PACKET["source"]),
            "status": "diagnosed", "question_version": PREVIOUS_QUESTION_VERSION,
            "provider_request_attempted": True, "provider_request_attempts": 1,
            "requested_model": "jev-1.14.0", "observed_model": "jev-1.14.0",
            "request_sha256": "1" * 64, "response_sha256": "2" * 64,
            "latency_seconds": 0.25, "usage": {"input_tokens": 10, "output_tokens": 3, "total_tokens": 13},
            "diagnosis": {"answers": {"proof": {"choice": "overstated"}},
                          "supporting_excerpt_ids": ["e1"], "abstained": False},
        }
        rows.append({"fingerprint": PACKET_FINGERPRINT, "record": copy.deepcopy(record)})
        for question in ("support_1", "support_2", None):
            diagnostic = {
                "version": 1, "stage": "schema" if question else "policy",
                "reason": "choice_not_allowed" if question else "duplicate_support",
                "http_status": 200, "response_bytes_read": 180, "response_truncated": False,
                "expected_answer_count": 7, "answer_count": 7,
                "missing_answer_count": 0, "unexpected_answer_count": 0,
            }
            if question:
                diagnostic["question"] = question
            failed = dict(copy.deepcopy(record), status="malformed_response", diagnostic=diagnostic)
            failed.pop("diagnosis")
            rows.append({"fingerprint": PACKET_FINGERPRINT, "record": failed})
        original = copy.deepcopy(rows)
        control = {
            "version": 2, "source": copy.deepcopy(PACKET["source"]), "trusted_sha": "d" * 40,
            "engine_hash": runner.engine_hash(), "violations": [], "data_failed": False,
            "waiting": False, "selected": 0, "pending": [], "limitations": [],
            "previous_entries": [], "backlog": [], "backlog_overflow": 0,
            "current_fingerprints": [], "results": rows, "provider_status": "completed",
        }
        with tempfile.TemporaryDirectory() as directory, \
                mock.patch.object(runner, "current_candidate", return_value=True), \
                mock.patch.object(runner, "begin_check", return_value=123), \
                mock.patch.object(runner, "complete_check", return_value=True), \
                mock.patch.dict(os.environ, {"GITHUB_STEP_SUMMARY": ""}):
            runner.save(Path(directory) / "control.json", control)
            self.assertEqual(runner.publish(None, directory), 0)
            state = runner.load(Path(directory) / "public/state.json")
        observations = state["observations"]
        self.assertEqual(state["provider_status"], "completed")
        self.assertEqual(len(observations), 4)
        self.assertEqual(observations[0]["record"]["status"], "diagnosed")
        for observed, previous in zip(observations, original):
            self.assertEqual(observed["fingerprint"], PACKET_FINGERPRINT)
            retained = observed["record"]
            self.assertEqual(retained["question_version"], 1)
            self.assertEqual(retained["usage"], {"input_tokens": 10, "output_tokens": 3, "total_tokens": 13})
            self.assertEqual(retained["provider_request_attempts"], 1)
            self.assertEqual(retained["request_sha256"], "1" * 64)
            self.assertEqual(retained["response_sha256"], "2" * 64)
            self.assertEqual(retained.get("diagnostic"), previous["record"].get("diagnostic"))
        self.assertEqual(rows, original)


if __name__ == "__main__":
    unittest.main()
