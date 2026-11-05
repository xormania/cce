"""Check durable observation storage through its public repository contract."""

from dataclasses import replace
import json
import os
from pathlib import Path
import selectors
import sqlite3
import stat
import subprocess
import sys
import tempfile
import time
import unittest
from unittest import mock


ROOT = Path(__file__).resolve().parents[2]
PACKAGE_ROOT = ROOT / "tools/telemetry"
sys.path.insert(0, str(PACKAGE_ROOT))

from agent_telemetry.domain import CaptureContext, Event, TelemetryError
from agent_telemetry.sqlite import SQLiteRepository


FIRST_CAPTURE = "2026-01-02T03:04:05+00:00"
LATER_CAPTURE = "2026-01-03T03:04:05+00:00"


# Workers use the same interpreter as unittest, real files and separate connections.
# The pause worker installs its connection adapter before importing the repository.
WORKER = r'''
import json
import re
import sqlite3
import sys

mode, path = sys.argv[1:3]

if mode == "pause":
    original_connect = sqlite3.connect
    paused = False

    def after_statement(connection, statement):
        global paused
        if not paused and re.match(r"\s*INSERT\s+INTO\s+cursors\b", statement, re.IGNORECASE):
            paused = True
            event_rows = connection.execute(
                "SELECT count(*) FROM events WHERE event_id='interrupted'").fetchone()[0]
            cursor_rows = connection.execute(
                "SELECT count(*) FROM cursors WHERE source_id='stream' AND position='position'").fetchone()[0]
            print(json.dumps({"phase": "inserted", "in_transaction": connection.in_transaction,
                              "event_rows": event_rows, "cursor_rows": cursor_rows}), flush=True)
            sys.stdin.readline()

    class PausingCursor(sqlite3.Cursor):
        def execute(self, statement, parameters=()):
            result = super().execute(statement, parameters)
            after_statement(self.connection, statement)
            return result

    class PausingConnection(sqlite3.Connection):
        def execute(self, statement, parameters=()):
            result = super().execute(statement, parameters)
            after_statement(self, statement)
            return result

        def cursor(self, *args, **kwargs):
            kwargs.setdefault("factory", PausingCursor)
            return super().cursor(*args, **kwargs)

    def connect(*args, **kwargs):
        kwargs["factory"] = PausingConnection
        return original_connect(*args, **kwargs)

    sqlite3.connect = connect

from agent_telemetry.domain import CaptureContext, Event
from agent_telemetry.sqlite import SQLiteRepository

context = CaptureContext("project", "dataset", "collector", "worker", session_id="session")

def emit(phase, **values):
    print(json.dumps({"phase": phase, **values}), flush=True)

def append(repository, event_id, payload):
    event = Event(event_id, "sample", json.dumps(payload), context)
    result = repository.append(event, "2026-01-02T03:04:05+00:00", cursor=("stream", "position"))
    emit("receipt", status=result.status, event_id=event_id,
         sequence=None if result.receipt is None else result.receipt.sequence)

if mode == "reader":
    repository = SQLiteRepository.inspect(path)
    emit("ready")
    sys.stdin.readline()
    samples = []
    for _ in range(30):
        rows = repository.query(limit=100)
        sequences = [row["sequence"] for row in rows]
        if sequences != sorted(set(sequences)):
            raise AssertionError("reader observed repeated or unordered persisted sequence")
        samples.append(len(rows))
    emit("read", samples=samples)
    repository.close()
else:
    repository = SQLiteRepository(path, timeout_ms=3000)
    emit("ready")
    sys.stdin.readline()
    if mode == "batch":
        producer = sys.argv[3]
        append(repository, "shared", {"shared": True})
        for index in range(10):
            append(repository, producer + ":" + str(index), {"producer": producer, "index": index})
        repository.close()
    else:
        append(repository, "interrupted", {"value": "retained"})
        sys.stdin.readline()
        repository.close()
'''


class SQLiteStorageTests(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name)
        self.path = self.root / "observations.sqlite3"
        self.context = CaptureContext(
            "project", "dataset", "collector", "producer",
            session_id="session", session_generation="generation", task_id="task",
        )

    def initialize(self, path=None, **kwargs):
        repository = SQLiteRepository.initialize(
            path or self.path, dataset_id="dataset", collector_id="collector", **kwargs,
        )
        self.addCleanup(repository.close)
        return repository

    def inspect(self, path=None):
        repository = SQLiteRepository.inspect(path or self.path)
        self.addCleanup(repository.close)
        return repository

    def event(self, event_id="event", payload='{"value":1}', **kwargs):
        return Event(event_id, "sample", payload, kwargs.pop("context", self.context), **kwargs)

    def worker(self, mode, *args):
        environment = dict(os.environ)
        environment["PYTHONPATH"] = os.pathsep.join(filter(None, (
            str(PACKAGE_ROOT), environment.get("PYTHONPATH"),
        )))
        process = subprocess.Popen(
            [sys.executable, "-u", "-c", WORKER, mode, str(self.path), *args],
            stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
            text=True, env=environment,
        )
        self.addCleanup(self.reap, process)
        return process

    @staticmethod
    def reap(process):
        if process.poll() is None:
            process.kill()
        process.communicate(timeout=10)

    def packet(self, process):
        with selectors.DefaultSelector() as selector:
            selector.register(process.stdout, selectors.EVENT_READ)
            self.assertTrue(selector.select(10), "worker did not reach its explicit barrier")
        line = process.stdout.readline()
        self.assertTrue(line, "worker exited before its explicit barrier")
        return json.loads(line)

    @staticmethod
    def release(process):
        process.stdin.write("continue\n")
        process.stdin.flush()

    def test_retry_reuses_original_receipt_after_capture_clock_changes_and_reopen(self):
        repository = self.initialize()
        first = repository.append(self.event(), FIRST_CAPTURE)
        self.assertEqual(first.status, "recorded")
        self.assertIsNotNone(first.receipt)
        repository.close()
        reopened = SQLiteRepository(self.path)
        self.addCleanup(reopened.close)
        replay = reopened.append(self.event(), LATER_CAPTURE)
        self.assertEqual(replay.status, "duplicate")
        self.assertEqual(replay.receipt, first.receipt)
        self.assertEqual(len(reopened.query()), 1)
        self.assertEqual(reopened.query()[0]["captured_at"], FIRST_CAPTURE)

    def test_conflicting_payload_and_context_leave_original_observation_unchanged(self):
        repository = self.initialize()
        original = self.event()
        first = repository.append(original, FIRST_CAPTURE)
        before = repository.query()
        alternatives = (
            replace(original, payload_json='{"value":2}'),
            replace(original, context=replace(self.context, task_id="other-task")),
            replace(original, context=replace(self.context, session_id="other-session")),
            replace(original, event_type="other-kind"),
            replace(original, source_json='{"revision":"new"}'),
        )
        for event in alternatives:
            with self.subTest(event=event):
                result = repository.append(event, LATER_CAPTURE)
                self.assertEqual(result.status, "rejected")
                self.assertTrue(result.reason)
                self.assertEqual(repository.query(), before)
        self.assertEqual(repository.append(original, LATER_CAPTURE).receipt, first.receipt)

    def test_dataset_and_collector_mismatch_are_rejected(self):
        repository = self.initialize()
        for context in (replace(self.context, dataset_id="other-dataset"),
                        replace(self.context, collector_id="other-collector")):
            with self.subTest(context=context):
                result = repository.append(self.event(context=context), FIRST_CAPTURE)
                self.assertEqual(result.status, "rejected")
                self.assertTrue(result.reason)
        self.assertEqual(repository.query(), [])

    def test_json_text_retains_spacing_unknown_fields_unicode_and_value_distinctions(self):
        repository = self.initialize()
        payload = '{ "unknown" : {"méasure": "雪"}, "large": "900719925474099312345", "null": null, "zero": 0 }\n'
        source = '{ "locator": "α", "position": "0000012345678901234567890" }'
        result = repository.append(self.event(payload=payload, source_json=source), FIRST_CAPTURE)
        self.assertEqual(result.status, "recorded")
        repository.close()
        row = self.inspect().query()[0]
        self.assertEqual(row["payload_json"], payload)
        self.assertEqual(row["source_json"], source)
        retained = json.loads(row["payload_json"])
        self.assertNotIn("absent", retained)
        self.assertIsNone(retained["null"])
        self.assertEqual(retained["zero"], 0)
        self.assertEqual(retained["large"], "900719925474099312345")
        self.assertEqual(retained["unknown"], {"méasure": "雪"})

    def test_query_filters_and_cursor_follow_persisted_sequence(self):
        repository = self.initialize()
        events = [
            self.event("first"),
            replace(self.event("second"), event_type="other"),
            self.event("third", context=replace(self.context, project_id="second-project")),
        ]
        receipts = [repository.append(event, FIRST_CAPTURE).receipt for event in events]
        self.assertEqual([row["event_id"] for row in repository.query(project_id="project")],
                         ["first", "second"])
        self.assertEqual([row["event_id"] for row in repository.query(event_type="sample")],
                         ["first", "third"])
        self.assertEqual([row["event_id"] for row in repository.query(after=receipts[0].sequence, limit=1)],
                         ["second"])
        self.assertEqual([row["sequence"] for row in repository.query()],
                         [receipt.sequence for receipt in receipts])

    def test_unknown_link_target_is_visible_until_target_arrives(self):
        repository = self.initialize()
        first = self.event("dependent", links=(("parent", "future"),))
        self.assertEqual(repository.append(first, FIRST_CAPTURE).status, "recorded")
        row = repository.query()[0]
        self.assertEqual(len(row["unresolved_links"]), 1)
        self.assertIn("future", json.dumps(row["unresolved_links"]))
        self.assertEqual(repository.append(self.event("future"), LATER_CAPTURE).status, "recorded")
        self.assertEqual(repository.query()[0]["unresolved_links"], [])

    def test_status_reports_empty_coverage_and_explicit_dataset_identity(self):
        repository = self.initialize()
        status = repository.status()
        self.assertEqual(status["event_count"], 0)
        self.assertEqual(status["coverage"], "unknown")
        self.assertEqual(status["publication"], "unknown")
        self.assertIsNone(status["last_capture"])
        self.assertEqual(status["dataset_id"], "dataset")
        self.assertEqual(status["collector_id"], "collector")
        self.assertEqual(status["schema_version"], 1)
        repository.append(self.event(), FIRST_CAPTURE)
        status = repository.status()
        self.assertEqual(status["event_count"], 1)
        self.assertEqual(status["last_capture"], FIRST_CAPTURE)
        self.assertEqual(status["publication"], "unknown")

    def test_inspection_and_writer_open_do_not_initialize_missing_database(self):
        missing = self.root / "absent" / "observations.sqlite3"
        for operation in (SQLiteRepository.inspect, SQLiteRepository):
            with self.subTest(operation=operation):
                with self.assertRaises(TelemetryError):
                    operation(missing)
                self.assertFalse(missing.exists())
                self.assertFalse(missing.parent.exists())

    def test_inspection_uses_read_only_database_connections(self):
        repository = self.initialize()
        repository.append(self.event(), FIRST_CAPTURE)
        repository.close()
        environment = dict(os.environ)
        environment["PYTHONPATH"] = str(PACKAGE_ROOT)
        script = (
            "import json, sqlite3, sys\n"
            "observed = []\n"
            "original_connect = sqlite3.connect\n"
            "def connect(database, *args, **kwargs):\n"
            "    observed.append((str(database), kwargs.get('uri', False)))\n"
            "    return original_connect(database, *args, **kwargs)\n"
            "sqlite3.connect = connect\n"
            "from agent_telemetry.sqlite import SQLiteRepository\n"
            "repository = SQLiteRepository.inspect(sys.argv[1])\n"
            "status = repository.status()\n"
            "rows = repository.query()\n"
            "repository.close()\n"
            "print(json.dumps({'connections': observed, 'events': status['event_count'], 'rows': len(rows)}))\n"
        )
        result = subprocess.run([sys.executable, "-c", script, str(self.path)],
                                capture_output=True, text=True, timeout=10, env=environment)
        self.assertEqual(result.returncode, 0, result.stderr)
        report = json.loads(result.stdout)
        self.assertEqual(report["events"], 1)
        self.assertEqual(report["rows"], 1)
        observed = report["connections"]
        self.assertTrue(observed, "inspection did not open its SQLite read boundary")
        self.assertTrue(all(uri and "mode=ro" in database for database, uri in observed), observed)

    def test_lock_budget_exhaustion_returns_explicit_deferred_result(self):
        repository = self.initialize(timeout_ms=40)
        lock = sqlite3.connect(self.path, isolation_level=None)
        self.addCleanup(lock.close)
        lock.execute("BEGIN IMMEDIATE")
        try:
            started = time.monotonic()
            result = repository.append(self.event(), FIRST_CAPTURE)
            elapsed = time.monotonic() - started
            self.assertEqual(result.status, "deferred")
            self.assertIsNone(result.receipt)
            self.assertTrue(result.reason)
            self.assertLess(elapsed, 2, "lock failure exceeded the configured wait by seconds")
        finally:
            lock.execute("ROLLBACK")
        self.assertEqual(repository.query(), [])
        self.assertEqual(repository.append(self.event(), LATER_CAPTURE).status, "recorded")

    def test_independent_process_writers_and_reader_preserve_unique_receipts(self):
        self.initialize().close()
        processes = [self.worker("batch", str(index)) for index in range(4)]
        reader = self.worker("reader")
        for process in [*processes, reader]:
            self.assertEqual(self.packet(process)["phase"], "ready")
        for process in [reader, *processes]:
            self.release(process)
        receipts = []
        for process in processes:
            output, errors = process.communicate(timeout=20)
            self.assertEqual(process.returncode, 0, errors)
            receipts.extend(json.loads(line) for line in output.splitlines())
        output, errors = reader.communicate(timeout=20)
        self.assertEqual(reader.returncode, 0, errors)
        self.assertEqual(json.loads(output)["phase"], "read")
        self.assertEqual(len(receipts), 44)
        self.assertTrue(all(row["status"] in ("recorded", "duplicate") for row in receipts))
        shared = [row for row in receipts if row["event_id"] == "shared"]
        self.assertEqual(sum(row["status"] == "recorded" for row in shared), 1)
        self.assertEqual(len({row["sequence"] for row in shared}), 1)
        rows = self.inspect().query(limit=100)
        self.assertEqual(len(rows), 41)
        self.assertEqual({row["event_id"] for row in rows}, {row["event_id"] for row in receipts})
        self.assertEqual(len({row["sequence"] for row in rows}), len(rows))
        self.assertEqual([row["sequence"] for row in rows], sorted(row["sequence"] for row in rows))

    def test_killed_writer_leaves_no_uncommitted_observation(self):
        self.initialize().close()
        process = self.worker("pause")
        self.assertEqual(self.packet(process)["phase"], "ready")
        self.release(process)
        inserted = self.packet(process)
        self.assertEqual(inserted["phase"], "inserted")
        self.assertTrue(inserted["in_transaction"], "INSERT must execute inside the uncommitted transaction")
        self.assertEqual(inserted["event_rows"], 1, "event row was not inserted before the crash barrier")
        self.assertEqual(inserted["cursor_rows"], 1, "cursor update was not applied before the crash barrier")
        process.kill()
        output, errors = process.communicate(timeout=10)
        self.assertNotEqual(process.returncode, 0)
        self.assertNotIn('"phase": "receipt"', output, errors)
        self.assertEqual(self.inspect().query(), [])
        self.assertEqual(self.inspect().status()["cursors"], {})
        reopened = SQLiteRepository(self.path)
        self.addCleanup(reopened.close)
        replay = Event("interrupted", "sample", '{"value": "retained"}',
                       CaptureContext("project", "dataset", "collector", "worker", session_id="session"))
        recorded = reopened.append(replay, LATER_CAPTURE, cursor=("stream", "position"))
        duplicate = reopened.append(replay, FIRST_CAPTURE, cursor=("stream", "position"))
        self.assertEqual(recorded.status, "recorded")
        self.assertEqual(duplicate.status, "duplicate")
        self.assertEqual(duplicate.receipt, recorded.receipt)
        self.assertEqual([row["event_id"] for row in reopened.query()], ["interrupted"])
        self.assertEqual(reopened.status()["cursors"], {"stream": "position"})

    def test_acknowledged_observation_survives_killed_writer_and_fresh_process_read(self):
        self.initialize().close()
        process = self.worker("ack")
        self.assertEqual(self.packet(process)["phase"], "ready")
        self.release(process)
        receipt = self.packet(process)
        self.assertEqual(receipt["phase"], "receipt")
        self.assertEqual(receipt["status"], "recorded")
        process.kill()
        process.communicate(timeout=10)
        reader = self.worker("reader")
        self.assertEqual(self.packet(reader)["phase"], "ready")
        self.release(reader)
        output, errors = reader.communicate(timeout=10)
        self.assertEqual(reader.returncode, 0, errors)
        self.assertEqual(set(json.loads(output)["samples"]), {1})
        rows = self.inspect().query()
        self.assertEqual(rows[0]["event_id"], "interrupted")
        self.assertEqual(rows[0]["sequence"], receipt["sequence"])
        self.assertEqual(self.inspect().status()["cursors"], {"stream": "position"})

    def test_future_schema_in_uncheckpointed_wal_is_refused_without_mutation(self):
        self.initialize().close()
        script = (
            "import os, sqlite3, sys\n"
            "connection = sqlite3.connect(sys.argv[1], isolation_level=None)\n"
            "connection.execute('PRAGMA journal_mode=WAL')\n"
            "connection.execute('PRAGMA wal_autocheckpoint=0')\n"
            "connection.execute('CREATE TABLE future_format(value TEXT)')\n"
            "connection.execute(\"INSERT INTO future_format VALUES ('retained')\")\n"
            "connection.execute('PRAGMA user_version=2147483000')\n"
            "os._exit(0)\n"
        )
        subprocess.run([sys.executable, "-c", script, str(self.path)], check=True, timeout=10)
        files = [self.path, Path(str(self.path) + "-wal"), Path(str(self.path) + "-shm")]
        self.assertTrue(all(path.exists() for path in files))
        self.assertGreater(files[1].stat().st_size, 0)
        before = [path.read_bytes() for path in files[:2]] + [files[2].stat().st_size]
        for operation in (SQLiteRepository, SQLiteRepository.inspect):
            with self.subTest(operation=operation):
                with self.assertRaises(TelemetryError):
                    operation(self.path)
                self.assertTrue(all(path.exists() for path in files))
                self.assertEqual([path.read_bytes() for path in files[:2]] + [files[2].stat().st_size], before)
        connection = sqlite3.connect(self.path.as_uri() + "?mode=ro", uri=True)
        try:
            self.assertEqual(connection.execute("SELECT value FROM future_format").fetchall(), [("retained",)])
            self.assertEqual(connection.execute("PRAGMA user_version").fetchone()[0], 2147483000)
        finally:
            connection.close()

    def test_writer_refuses_loaded_sqlite_version_without_wal_reset_fix(self):
        environment = dict(os.environ)
        environment["PYTHONPATH"] = str(PACKAGE_ROOT)
        script = (
            "import json, sqlite3, sys\n"
            "sqlite3.sqlite_version_info = (3, 51, 2)\n"
            "sqlite3.sqlite_version = '3.51.2'\n"
            "from agent_telemetry.domain import TelemetryError\n"
            "from agent_telemetry.sqlite import SQLiteRepository\n"
            "try:\n"
            "    repository = SQLiteRepository.initialize(sys.argv[1], dataset_id='dataset', collector_id='collector')\n"
            "except TelemetryError as error:\n"
            "    print(json.dumps({'refused': True, 'code': error.code}))\n"
            "else:\n"
            "    repository.close()\n"
            "    print(json.dumps({'refused': False}))\n"
        )
        result = subprocess.run([sys.executable, "-c", script, str(self.path)],
                                capture_output=True, text=True, timeout=10, env=environment)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertTrue(json.loads(result.stdout)["refused"])

    def test_initialization_persists_every_new_directory_entry(self):
        path = self.root / "outer" / "middle" / "inner" / "observations.sqlite3"
        synchronized = set()
        original_fsync = os.fsync

        def sync(descriptor):
            information = os.fstat(descriptor)
            if stat.S_ISDIR(information.st_mode):
                synchronized.add((information.st_dev, information.st_ino))
            return original_fsync(descriptor)

        with mock.patch.object(os, "fsync", side_effect=sync):
            repository = self.initialize(path)
            repository.close()
        for directory in (self.root, self.root / "outer", self.root / "outer/middle", path.parent):
            with self.subTest(directory=directory.name):
                information = directory.stat()
                self.assertIn((information.st_dev, information.st_ino), synchronized,
                              "a newly created directory entry was not durably synchronized")

    def test_last_capture_follows_persistence_order_when_clock_moves_backward(self):
        repository = self.initialize()
        first = repository.append(self.event("earlier-append"), LATER_CAPTURE)
        second = repository.append(self.event("later-append"), FIRST_CAPTURE)
        self.assertEqual(first.status, "recorded")
        self.assertEqual(second.status, "recorded")
        self.assertLess(first.receipt.sequence, second.receipt.sequence)
        self.assertEqual(repository.status()["last_capture"], FIRST_CAPTURE)

    def test_supported_schema_without_collector_metadata_is_refused_with_typed_error(self):
        for missing_key in ("dataset_id", "collector_id"):
            path = self.root / (missing_key + ".sqlite3")
            self.initialize(path).close()
            connection = sqlite3.connect(path)
            try:
                connection.execute("DELETE FROM metadata WHERE key=?", (missing_key,))
                connection.commit()
                self.assertEqual(connection.execute("PRAGMA user_version").fetchone()[0], 1)
            finally:
                connection.close()
            operations = (
                SQLiteRepository,
                SQLiteRepository.inspect,
                lambda candidate: SQLiteRepository.initialize(
                    candidate, dataset_id="dataset", collector_id="collector"),
            )
            for operation in operations:
                with self.subTest(missing_key=missing_key, operation=operation):
                    try:
                        opened = operation(path)
                    except Exception as error:
                        self.assertIsInstance(error, TelemetryError,
                                              "schema refusal must expose a typed storage error")
                    else:
                        self.addCleanup(opened.close)
                        self.fail("schema with missing collector metadata was accepted")

    def test_invalid_timeout_is_rejected_before_creating_database_or_directories(self):
        for index, timeout in enumerate((-1, True, "250", None)):
            path = self.root / ("invalid-" + str(index)) / "observations.sqlite3"
            with self.subTest(timeout=timeout):
                with self.assertRaises(TelemetryError):
                    SQLiteRepository.initialize(
                        path, dataset_id="dataset", collector_id="collector", timeout_ms=timeout)
                self.assertFalse(path.exists(), "invalid configuration created a database")
                self.assertFalse(path.parent.exists(), "invalid configuration created directories")

    def test_every_writer_connection_enforces_durable_connection_settings(self):
        observed = []
        original_connect = sqlite3.connect

        def connect(database, *args, **kwargs):
            connection = original_connect(database, *args, **kwargs)
            observed.append((str(database), connection))
            return connection

        def assert_settings(connections, expected_timeout):
            live_writers = []
            for database, connection in connections:
                if "mode=ro" in database:
                    continue
                try:
                    mode = connection.execute("PRAGMA journal_mode").fetchone()[0]
                except sqlite3.ProgrammingError:
                    continue
                self.assertEqual(mode.lower(), "wal")
                self.assertEqual(connection.execute("PRAGMA synchronous").fetchone()[0], 2)
                self.assertEqual(connection.execute("PRAGMA foreign_keys").fetchone()[0], 1)
                self.assertEqual(connection.execute("PRAGMA busy_timeout").fetchone()[0], expected_timeout)
                live_writers.append((database, connection))
            self.assertTrue(live_writers, "no live writer connection was observed")
            return live_writers

        with mock.patch.object(sqlite3, "connect", side_effect=connect):
            self.initialize(timeout_ms=73)
            first_connections = assert_settings(observed, 73)
            boundary = len(observed)
            reopened = SQLiteRepository(self.path, timeout_ms=113)
            self.addCleanup(reopened.close)
            assert_settings(observed[boundary:], 113)
            assert_settings(first_connections, 73)

    def test_event_and_source_cursor_commit_together_and_rejection_preserves_cursor(self):
        repository = self.initialize()
        original = self.event("first", payload='{"value":"original"}')
        first = repository.append(original, FIRST_CAPTURE, cursor=("source", "position-a"))
        self.assertEqual(first.status, "recorded")
        self.assertEqual(repository.status()["cursors"], {"source": "position-a"})
        alternatives = (
            replace(original, payload_json='{"value":"conflict"}'),
            replace(original, context=replace(self.context, dataset_id="foreign-dataset")),
        )
        for event in alternatives:
            with self.subTest(event=event):
                rejected = repository.append(event, LATER_CAPTURE, cursor=("source", "position-b"))
                self.assertEqual(rejected.status, "rejected")
                self.assertEqual(repository.status()["cursors"], {"source": "position-a"})
                self.assertEqual([row["event_id"] for row in repository.query()], ["first"])
        second = repository.append(self.event("second"), LATER_CAPTURE,
                                   cursor=("source", "position-c"))
        self.assertEqual(second.status, "recorded")
        self.assertEqual(repository.status()["cursors"], {"source": "position-c"})
        self.assertEqual([row["event_id"] for row in repository.query()], ["first", "second"])


if __name__ == "__main__":
    unittest.main()
