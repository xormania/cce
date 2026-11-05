"""Collection contracts using independent repository and health adapters."""
from __future__ import annotations

import ast
from concurrent.futures import ThreadPoolExecutor
from dataclasses import FrozenInstanceError, replace
import json
from pathlib import Path
import sys
import threading
import unittest

PACKAGE_ROOT = Path(__file__).resolve().parents[2] / "tools" / "telemetry"
sys.path.insert(0, str(PACKAGE_ROOT))

from agent_telemetry.application import Collector
from agent_telemetry.domain import (
    ArtifactRef, CaptureContext, CaptureResult, Event, Receipt, TelemetryError,
)


def context(**changes):
    return replace(CaptureContext("sample", "observations", "local", "worker"), **changes)


def event(event_id="item", payload='{"value": 0}', **changes):
    return replace(Event(event_id, "sample.observed", payload, context()), **changes)


class RecordingRepository:
    def __init__(self, failure=None, barrier=None):
        self.failure = failure
        self.barrier = barrier
        self.calls = []
        self.lock = threading.Lock()

    def append(self, observation, captured_at, cursor=None):
        if self.barrier:
            self.barrier.wait(timeout=5)
        if self.failure:
            raise self.failure
        with self.lock:
            self.calls.append((observation, captured_at, cursor))
            return CaptureResult("recorded", Receipt(observation.event_id, len(self.calls), "digest", captured_at))


class CollectionContracts(unittest.TestCase):
    def test_context_and_envelope_reject_mutation(self):
        scope = context(session_id="session-a", task_id="task-a")
        observation = event(context=scope)
        for target, name, value in ((scope, "task_id", "task-b"), (observation, "payload_json", "null")):
            with self.subTest(target=type(target).__name__):
                with self.assertRaises((FrozenInstanceError, AttributeError, TypeError)):
                    setattr(target, name, value)
        self.assertEqual(observation.context.task_id, "task-a")
        self.assertEqual(observation.payload_json, '{"value": 0}')

    def test_context_rejects_mutable_or_nontext_optional_identity(self):
        for field in ("task_id", "session_id", "session_generation", "harness"):
            for value in (["mutable"], {"identity": "mutable"}, 17):
                with self.subTest(field=field, value=value):
                    with self.assertRaises(TelemetryError):
                        context(**{field: value})

    def test_unfiltered_json_is_retained_exactly(self):
        payload = ' { "vendor_extra": {"glyph": "λ", "huge": "90071992547409930001"}, "zero": 0, "null": null }\n'
        source = '{ "adapter": "fixture", "position": "00009", "extra": [null,0] }'
        repository = RecordingRepository()
        observation = event(payload=payload, source_json=source)
        result = Collector(repository, clock=lambda: "2030-01-02T03:04:05Z").record(observation)
        self.assertEqual(result.status, "recorded")
        stored, captured_at, cursor = repository.calls[0]
        self.assertEqual(stored.payload_json, payload)
        self.assertEqual(stored.source_json, source)
        self.assertEqual(captured_at, "2030-01-02T03:04:05Z")
        self.assertIsNone(cursor)
        decoded = json.loads(stored.payload_json)
        self.assertEqual(decoded["zero"], 0)
        self.assertIsNone(decoded["null"])
        self.assertNotIn("absent", decoded)

    def test_json_values_need_not_be_objects(self):
        for payload in ('[null, 0, "text"]', '"text"', "null", "0", "true"):
            with self.subTest(payload=payload):
                repository = RecordingRepository()
                result = Collector(repository).record(event(payload=payload))
                self.assertEqual(result.status, "recorded")
                self.assertEqual(repository.calls[0][0].payload_json, payload)

    def test_invalid_json_is_rejected_before_repository_append(self):
        for payload in ("{", '{"value": NaN}', '{"value": Infinity}'):
            with self.subTest(payload=payload):
                repository = RecordingRepository()
                result = Collector(repository).record(event(payload=payload))
                self.assertEqual(result.status, "rejected")
                self.assertEqual(repository.calls, [])

    def test_credentials_are_removed_while_token_counts_survive(self):
        marker = "credential-fixture-never-retain"
        payload = json.dumps({
            "api_key": marker,
            "usage": {"input_tokens": 17, "output_tokens": 0, "tokens": 23},
            "nested": [{"authorization": "Bearer " + marker, "detail": None}],
            "vendor_extra": "kept",
        })
        observation = event(payload=payload)
        repository = RecordingRepository()
        self.assertEqual(Collector(repository).record(observation).status, "recorded")
        captured = repository.calls[0][0]
        self.assertNotIn(marker, captured.payload_json)
        self.assertEqual(captured.redaction_policy, "credentials-v1")
        self.assertIn("payload/api_key", captured.redacted_paths)
        self.assertIn("payload/nested/0/authorization", captured.redacted_paths)
        self.assertEqual(json.loads(captured.payload_json)["usage"], {"input_tokens": 17, "output_tokens": 0, "tokens": 23})
        self.assertEqual(json.loads(captured.payload_json)["vendor_extra"], "kept")
        self.assertEqual(observation.payload_json, payload)

    def test_source_credentials_are_filtered_without_mutating_input(self):
        source = '{"source":"fixture","password":"source-secret-marker","input_tokens":9}'
        repository = RecordingRepository()
        original = event(source_json=source)
        self.assertEqual(Collector(repository).record(original).status, "recorded")
        captured = repository.calls[0][0]
        self.assertNotIn("source-secret-marker", captured.source_json)
        self.assertIn("source/password", captured.redacted_paths)
        self.assertEqual(json.loads(captured.source_json)["input_tokens"], 9)
        self.assertEqual(original.source_json, source)

    def test_cursor_and_event_are_forwarded_in_one_append(self):
        repository = RecordingRepository()
        cursor = ("stream", "opaque-position-42")
        result = Collector(repository).record(event(), cursor=cursor)
        self.assertEqual(result.status, "recorded")
        self.assertEqual(len(repository.calls), 1)
        self.assertEqual(repository.calls[0][2], cursor)

    def test_concurrent_requests_keep_their_own_context(self):
        repository = RecordingRepository(barrier=threading.Barrier(2))
        collector = Collector(repository)
        observations = [event(str(index), context=context(task_id=f"task-{index}", session_id=f"session-{index}", session_generation=str(index))) for index in range(2)]
        with ThreadPoolExecutor(max_workers=2) as executor:
            results = list(executor.map(collector.record, observations))
        self.assertEqual([result.status for result in results], ["recorded", "recorded"])
        actual = {item.event_id: (item.context.task_id, item.context.session_id, item.context.session_generation) for item, _, _ in repository.calls}
        self.assertEqual(actual, {"0": ("task-0", "session-0", "0"), "1": ("task-1", "session-1", "1")})

    def test_storage_failure_is_visible_without_escaping(self):
        health = []
        repository = RecordingRepository(failure=TelemetryError("locked", "writer busy"))
        result = Collector(repository, health=health.append).record(event())
        self.assertIn(result.status, {"deferred", "unavailable"})
        self.assertIsNone(result.receipt)
        self.assertTrue(health)

    def test_failed_health_channel_preserves_capture_failure_result(self):
        def broken_health(_message):
            raise OSError("diagnostics unavailable")
        repository = RecordingRepository(failure=OSError("disk unavailable"))
        result = Collector(repository, health=broken_health).record(event())
        self.assertIn(result.status, {"deferred", "unavailable"})
        self.assertIsNone(result.receipt)

    def test_health_is_bounded_and_does_not_echo_secret_exception_text(self):
        health = []
        marker = "secret-fixture-health-value"
        repository = RecordingRepository(failure=OSError(marker + "x" * 20000))
        result = Collector(repository, health=health.append).record(event())
        self.assertIn(result.status, {"deferred", "unavailable"})
        self.assertTrue(health)
        rendered = str(health)
        self.assertLessEqual(len(rendered), 8192)
        self.assertNotIn(marker, rendered)
        self.assertNotIn(marker, result.reason or "")

    def test_missing_captured_artifact_prevents_present_reference(self):
        class MissingArtifactStore:
            def verify(self, _reference):
                return False
        repository = RecordingRepository()
        reference = ArtifactRef("a" * 64, 1)
        result = Collector(repository, artifacts=MissingArtifactStore()).record(event(artifacts=(reference,)))
        self.assertIn(result.status, {"rejected", "unavailable"})
        self.assertEqual(repository.calls, [])


class CoreBoundaries(unittest.TestCase):
    def test_core_does_not_import_concrete_infrastructure(self):
        forbidden = {"sqlite3", "subprocess", "ledger_context", "drift_ledger", "config", "sqlite", "artifacts"}
        for filename in ("domain.py", "ports.py", "application.py"):
            with self.subTest(module=filename):
                tree = ast.parse((PACKAGE_ROOT / "agent_telemetry" / filename).read_text())
                imports = []
                for node in ast.walk(tree):
                    if isinstance(node, ast.Import):
                        imports.extend(alias.name for alias in node.names)
                    elif isinstance(node, ast.ImportFrom) and node.module:
                        imports.append(node.module)
                self.assertEqual([name for name in imports if set(name.split(".")) & forbidden], [])


if __name__ == "__main__":
    unittest.main()
