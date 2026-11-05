"""Configuration and command behavior in disposable independent projects."""
from __future__ import annotations

from dataclasses import FrozenInstanceError
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest

PACKAGE_ROOT = Path(__file__).resolve().parents[2] / "tools" / "telemetry"
sys.path.insert(0, str(PACKAGE_ROOT))

from agent_telemetry.config import load_config
from agent_telemetry.domain import TelemetryError
from agent_telemetry.sqlite import SQLiteRepository


def profile_text(project="example", dataset="measurements", collector="local", extra=""):
    return f'''schema_version = 1
project_id = "{project}"
dataset_id = "{dataset}"
collector_id = "{collector}"
{extra}
[storage]
runtime_root = "state"
database = "events.sqlite3"
capture_lock_timeout_ms = 80

[artifacts]
root = "objects"
enabled = true
'''


class TemporaryProfile(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name)
        self.profile = self.root / "profile.toml"
        self.profile.write_text(profile_text())
        self.decoy = self.root / "different-working-directory"
        self.decoy.mkdir()

    def command(self, *arguments, payload=None):
        environment = dict(os.environ, PYTHONDONTWRITEBYTECODE="1", PYTHONPATH=str(PACKAGE_ROOT))
        return subprocess.run(
            [sys.executable, "-B", "-m", "agent_telemetry", "--config", str(self.profile), *arguments],
            input=payload, text=True, encoding="utf-8", capture_output=True,
            cwd=self.decoy, env=environment, timeout=15,
        )


class ProfileContracts(TemporaryProfile):
    def test_relative_paths_follow_profile_location_and_ignore_caller_cwd(self):
        first = load_config(self.profile)
        previous = Path.cwd()
        try:
            os.chdir(self.decoy)
            second = load_config(self.profile)
        finally:
            os.chdir(previous)
        self.assertEqual(first.runtime_root, self.root / "state")
        self.assertEqual(first.database, self.root / "state" / "events.sqlite3")
        self.assertEqual(first.artifact_root, self.root / "state" / "objects")
        self.assertEqual(first.digest, second.digest)
        self.assertEqual(first, second)
        self.assertFalse((self.root / "state").exists())

    def test_effective_configuration_is_frozen(self):
        configuration = load_config(self.profile)
        with self.assertRaises((FrozenInstanceError, AttributeError, TypeError)):
            configuration.project_id = "different"
        self.assertEqual(configuration.project_id, "example")

    def test_effective_identity_changes_with_capture_configuration(self):
        original = load_config(self.profile)
        self.profile.write_text(profile_text(project="second", dataset="other", collector="other-writer"))
        changed = load_config(self.profile)
        self.assertNotEqual(original.digest, changed.digest)
        self.assertEqual((changed.project_id, changed.dataset_id, changed.collector_id), ("second", "other", "other-writer"))

    def test_unknown_keys_are_visible_configuration_errors(self):
        original = profile_text()
        cases = [profile_text(extra='unexpected = "unused"'), original.replace('runtime_root = "state"', 'runtime_root = "state"\nunexpected = 1'), original + 'unexpected = "unused"\n']
        for text in cases:
            with self.subTest(text=text):
                self.profile.write_text(text)
                with self.assertRaises(TelemetryError):
                    load_config(self.profile)

    def test_invalid_types_and_unsupported_versions_are_rejected(self):
        original = profile_text()
        replacements = [
            ('schema_version = 1', 'schema_version = 2'),
            ('schema_version = 1', 'schema_version = true'),
            ('project_id = "example"', 'project_id = 7'),
            ('capture_lock_timeout_ms = 80', 'capture_lock_timeout_ms = true'),
            ('capture_lock_timeout_ms = 80', 'capture_lock_timeout_ms = -1'),
            ('enabled = true', 'enabled = "true"'),
        ]
        for before, after in replacements:
            with self.subTest(replacement=after):
                self.profile.write_text(original.replace(before, after))
                with self.assertRaises(TelemetryError):
                    load_config(self.profile)

    def test_database_and_artifact_paths_cannot_escape_runtime_root(self):
        original = profile_text()
        for before, after in [('database = "events.sqlite3"', 'database = "../outside.sqlite3"'), ('root = "objects"', 'root = "../outside"')]:
            with self.subTest(path=after):
                self.profile.write_text(original.replace(before, after))
                with self.assertRaises(TelemetryError):
                    load_config(self.profile)
        self.assertFalse((self.root / "state").exists())

    def test_symlinked_subdirectory_cannot_redirect_storage_outside_root(self):
        state = self.root / "state"
        state.mkdir()
        outside = self.root / "outside"
        outside.mkdir()
        (state / "redirect").symlink_to(outside, target_is_directory=True)
        self.profile.write_text(profile_text().replace('database = "events.sqlite3"', 'database = "redirect/events.sqlite3"'))
        with self.assertRaises(TelemetryError):
            load_config(self.profile)
        self.assertEqual(list(outside.iterdir()), [])

    def test_runtime_root_symlink_is_refused_before_resolving_it(self):
        outside = self.root / "outside"
        outside.mkdir()
        (self.root / "state").symlink_to(outside, target_is_directory=True)
        with self.assertRaises(TelemetryError):
            load_config(self.profile)
        self.assertEqual(list(outside.iterdir()), [])


class CommandContracts(TemporaryProfile):
    def test_status_and_query_do_not_initialize_missing_storage(self):
        for command in ("status", "query"):
            with self.subTest(command=command):
                result = self.command(command)
                self.assertEqual(result.returncode, 2, result.stderr)
                self.assertFalse((self.root / "state").exists())

    def test_capture_and_query_work_from_an_unrelated_working_directory(self):
        initialized = self.command("init")
        self.assertEqual(initialized.returncode, 0, initialized.stderr)
        payload = ' {"unknown": ["λ", null, 0], "input_tokens": 31}\n'
        recorded = self.command("record", "--event-id", "first", "--event-type", "task.observed", "--session-id", "session-a", payload=payload)
        self.assertEqual(recorded.returncode, 0, recorded.stderr)
        json.loads(recorded.stdout)
        queried = self.command("query", "--event-type", "task.observed", "--limit", "1", "--after", "0")
        self.assertEqual(queried.returncode, 0, queried.stderr)
        json.loads(queried.stdout)
        repository = SQLiteRepository.inspect(self.root / "state" / "events.sqlite3")
        try:
            rows = repository.query()
        finally:
            repository.close()
        self.assertEqual(len(rows), 1)
        self.assertEqual(rows[0]["event_id"], "first")
        self.assertEqual(rows[0]["payload_json"], payload)
        self.assertEqual(rows[0]["context"]["session_id"], "session-a")
        self.assertEqual(list(self.decoy.iterdir()), [])

    def test_second_project_profile_uses_the_same_generic_commands(self):
        self.profile.write_text(profile_text(project="garden", dataset="field-notes", collector="station"))
        initialized = self.command("init")
        self.assertEqual(initialized.returncode, 0, initialized.stderr)
        recorded = self.command("record", "--event-id", "reading", "--event-type", "sensor.observed", payload='{"rain":0,"unit":"mm"}')
        self.assertEqual(recorded.returncode, 0, recorded.stderr)
        repository = SQLiteRepository.inspect(self.root / "state" / "events.sqlite3")
        try:
            rows = repository.query(project_id="garden")
            status = repository.status()
        finally:
            repository.close()
        self.assertEqual(len(rows), 1)
        self.assertEqual(rows[0]["event_type"], "sensor.observed")
        self.assertEqual(rows[0]["context"]["project_id"], "garden")
        self.assertEqual(status["dataset_id"], "field-notes")
        self.assertEqual(status["collector_id"], "station")

    def test_identical_cli_retry_succeeds_and_conflict_preserves_original(self):
        self.assertEqual(self.command("init").returncode, 0)
        arguments = ("record", "--event-id", "stable", "--event-type", "task.observed")
        first = self.command(*arguments, payload='{"counter":7}')
        retry = self.command(*arguments, payload='{"counter":7}')
        conflict = self.command(*arguments, payload='{"counter":8}')
        self.assertEqual(first.returncode, 0, first.stderr)
        self.assertEqual(retry.returncode, 0, retry.stderr)
        self.assertEqual(conflict.returncode, 2, conflict.stderr)
        repository = SQLiteRepository.inspect(self.root / "state" / "events.sqlite3")
        try:
            rows = repository.query()
        finally:
            repository.close()
        self.assertEqual([json.loads(row["payload_json"]) for row in rows], [{"counter": 7}])

    def test_invalid_profile_reports_error_without_creating_storage(self):
        self.profile.write_text(profile_text(extra='secret_option = "fixture-sensitive-value"'))
        result = self.command("record", "--event-id", "unused", "--event-type", "sample.observed", payload="{}")
        self.assertEqual(result.returncode, 2)
        self.assertNotIn("fixture-sensitive-value", result.stderr)
        self.assertLess(len(result.stderr), 8192)
        self.assertFalse((self.root / "state").exists())


if __name__ == "__main__":
    unittest.main()
