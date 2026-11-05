"""Optional observation preserves context actions and command results."""
from __future__ import annotations

from contextlib import redirect_stderr
import io
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
from types import SimpleNamespace
import unittest
from unittest.mock import patch

PACKAGE_ROOT = Path(__file__).resolve().parents[2] / "tools" / "telemetry"
sys.path.insert(0, str(PACKAGE_ROOT))

import agent_telemetry.cce as cce
from agent_telemetry.cce import observe_emission
from agent_telemetry.domain import CaptureContext, CaptureResult, TelemetryError


class Captures:
    def __init__(self, result=None, error=None):
        self.events = []
        self.result = result or CaptureResult("recorded")
        self.error = error

    def record(self, event):
        self.events.append(event)
        if self.error:
            raise self.error
        return self.result


class ContextObservationContracts(unittest.TestCase):
    def setUp(self):
        self.context = CaptureContext("sample", "observations", "local", "reader", session_id="session")
        self.output = ("λ page one\n", {"included": ["record-a"], "pending": ["record-b"], "next_cursor": "page-two", "complete": False})

    def test_observation_calls_action_once_and_preserves_exact_result(self):
        calls = []
        collector = Captures()
        def action():
            calls.append("called")
            return self.output
        result = observe_emission(action, collector, self.context, event_id="emission")
        self.assertIs(result, self.output)
        self.assertEqual(calls, ["called"])
        self.assertEqual(len(collector.events), 1)
        self.assertEqual(collector.events[0].event_id, "emission")

    def test_observation_retains_actual_bytes_metadata_and_unknown_consumption(self):
        collector = Captures()
        observe_emission(lambda: self.output, collector, self.context)
        observation = collector.events[0]
        payload = json.loads(observation.payload_json)
        self.assertEqual(observation.event_type, "cce.context.emitted")
        self.assertEqual(observation.context, self.context)
        self.assertEqual(payload["text"], self.output[0])
        self.assertEqual(payload["metadata"], self.output[1])
        self.assertEqual(payload["emitted_bytes"], len(self.output[0].encode("utf-8")))
        self.assertIsNone(payload["consumed"])
        self.assertEqual(payload["delivery"], "unknown")
        self.assertIsInstance(payload["duration_ns"], int)
        self.assertGreaterEqual(payload["duration_ns"], 0)

    def test_deferred_telemetry_leaves_context_page_unchanged(self):
        collector = Captures(result=CaptureResult("deferred", reason="busy"))
        result = observe_emission(lambda: self.output, collector, self.context)
        self.assertIs(result, self.output)
        self.assertEqual(self.output[1]["next_cursor"], "page-two")
        self.assertEqual(len(collector.events), 1)

    def test_failed_capture_leaves_context_page_unchanged(self):
        collector = Captures(error=OSError("capture unavailable"))
        self.assertIs(observe_emission(lambda: self.output, collector, self.context), self.output)
        self.assertEqual(len(collector.events), 1)

    def test_failed_typed_result_without_health_callback_is_visible(self):
        collector = Captures(result=CaptureResult("deferred", reason="busy"))
        diagnostic = io.StringIO()
        with redirect_stderr(diagnostic):
            result = observe_emission(lambda: self.output, collector, self.context)
        self.assertIs(result, self.output)
        self.assertTrue(diagnostic.getvalue())
        self.assertLess(len(diagnostic.getvalue()), 8192)

    def test_action_exception_retains_identity_and_is_not_retried(self):
        collector = Captures()
        failure = ValueError("reader failure")
        calls = []
        def action():
            calls.append("called")
            raise failure
        with self.assertRaises(ValueError) as caught:
            observe_emission(action, collector, self.context)
        self.assertIs(caught.exception, failure)
        self.assertEqual(calls, ["called"])


class CommandObservationContracts(unittest.TestCase):
    def test_unavailable_diagnostic_stream_preserves_child_exit(self):
        class BrokenDiagnostic:
            def write(self, _text):
                raise OSError("diagnostic stream unavailable")
        completed = subprocess.CompletedProcess(["fixture"], 37, stdout=b"child output\n")
        output = SimpleNamespace(buffer=io.BytesIO())
        with patch.object(cce.subprocess, "run", return_value=completed), patch.object(cce, "load_config", side_effect=TelemetryError("configuration", "profile unavailable")), patch.object(cce.sys, "stdout", output), patch.object(cce.sys, "stderr", BrokenDiagnostic()):
            try:
                status = cce.main(["--config", "unavailable.toml", "--", "fixture"])
            except OSError as error:
                self.fail(f"diagnostic failure replaced the child outcome: {error}")
        self.assertEqual(status, 37)
        self.assertEqual(output.buffer.getvalue(), b"child output\n")

    def test_repository_close_failure_preserves_child_exit(self):
        class BrokenClose:
            def close(self):
                raise OSError("close unavailable")
        configuration = SimpleNamespace(database="fixture.sqlite3", timeout_ms=10, artifacts_enabled=False, project_id="sample", dataset_id="observations", collector_id="local", producer_id="reader", digest="config")
        completed = subprocess.CompletedProcess(["fixture"], 0, stdout=b"child output\n")
        output = SimpleNamespace(buffer=io.BytesIO())
        with patch.object(cce.subprocess, "run", return_value=completed), patch.object(cce, "load_config", return_value=configuration), patch.object(cce, "SQLiteRepository", return_value=BrokenClose()), patch.object(cce, "Collector", return_value=Captures()), patch.object(cce.sys, "stdout", output):
            try:
                status = cce.main(["--config", "fixture.toml", "--", "fixture"])
            except OSError as error:
                self.fail(f"collector cleanup replaced the child outcome: {error}")
        self.assertEqual(status, 0)
        self.assertEqual(output.buffer.getvalue(), b"child output\n")

    def test_missing_or_invalid_profile_preserves_binary_stdout_stderr_and_exit(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            missing = root / "missing.toml"
            invalid = root / "invalid.toml"
            invalid.write_text("this is not valid TOML = [")
            script = "import os,sys; os.write(1,b'\\x00\\xffpayload\\n'); os.write(2,b'child stderr\\n'); sys.exit(37)"
            for profile in (missing, invalid):
                with self.subTest(profile=profile.name):
                    result = subprocess.run(
                        [sys.executable, "-B", "-m", "agent_telemetry.cce", "--config", str(profile), "--", sys.executable, "-B", "-c", script],
                        cwd=root, env=dict(os.environ, PYTHONDONTWRITEBYTECODE="1", PYTHONPATH=str(PACKAGE_ROOT)),
                        capture_output=True, timeout=15,
                    )
                    self.assertEqual(result.returncode, 37, result.stderr)
                    self.assertEqual(result.stdout, b"\x00\xffpayload\n")
                    self.assertIn(b"child stderr\n", result.stderr)
                    self.assertLess(len(result.stderr), 8192)
                    self.assertEqual(sorted(path.name for path in root.iterdir()), ["invalid.toml"])

    def test_failed_capture_does_not_convert_successful_command_to_failure(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            profile = root / "profile.toml"
            profile.write_text('schema_version=1\nproject_id="sample"\ndataset_id="observations"\ncollector_id="local"\n[storage]\nruntime_root="missing-state"\n')
            result = subprocess.run(
                [sys.executable, "-B", "-m", "agent_telemetry.cce", "--config", str(profile), "--", sys.executable, "-B", "-c", "print('action succeeded')"],
                cwd=root, env=dict(os.environ, PYTHONDONTWRITEBYTECODE="1", PYTHONPATH=str(PACKAGE_ROOT)),
                capture_output=True, timeout=15,
            )
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertEqual(result.stdout, b"action succeeded\n")
            self.assertFalse((root / "missing-state").exists())


if __name__ == "__main__":
    unittest.main()
