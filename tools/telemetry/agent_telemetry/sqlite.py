"""SQLite adapter with explicit initialization and read-only inspection."""

from dataclasses import asdict
from pathlib import Path
import os
import sqlite3
import stat
from .domain import (CaptureResult, Receipt, TelemetryError, canonical,
                     observation_digest, validate)

_SCHEMA = (
    "CREATE TABLE metadata (key TEXT PRIMARY KEY, value TEXT NOT NULL)",
    "CREATE TABLE events (sequence INTEGER PRIMARY KEY AUTOINCREMENT, event_id TEXT UNIQUE NOT NULL, digest TEXT NOT NULL, captured_at TEXT NOT NULL, project_id TEXT NOT NULL, event_type TEXT NOT NULL, observation TEXT NOT NULL)",
    "CREATE INDEX events_scope ON events(project_id,event_type,sequence)",
    "CREATE TABLE links (event_id TEXT NOT NULL REFERENCES events(event_id), relation TEXT NOT NULL, target_id TEXT NOT NULL)",
    "CREATE TABLE artifacts (event_id TEXT NOT NULL REFERENCES events(event_id), digest TEXT NOT NULL, metadata TEXT NOT NULL)",
    "CREATE TABLE cursors (source_id TEXT PRIMARY KEY, position TEXT NOT NULL)",
    "CREATE TRIGGER events_no_update BEFORE UPDATE ON events BEGIN SELECT RAISE(ABORT, 'immutable observation'); END",
    "CREATE TRIGGER events_no_delete BEFORE DELETE ON events BEGIN SELECT RAISE(ABORT, 'immutable observation'); END",
)


def require_sqlite():
    version = sqlite3.sqlite_version_info
    fixed = (version >= (3, 51, 3) or (3, 50, 7) <= version < (3, 51, 0)
             or (3, 44, 6) <= version < (3, 45, 0))
    if not fixed:
        raise TelemetryError("sqlite_version", "SQLite runtime lacks the required WAL fix")


def check_path(path):
    # Metadata inspection avoids opening/closing extra DB or WAL-index descriptors.
    for item in (path, *path.parents):
        try:
            info = item.lstat()
        except FileNotFoundError:
            continue
        if stat.S_ISLNK(info.st_mode):
            raise TelemetryError("unsafe_path", "symbolic links are not supported for storage")
    for item in (path, Path(str(path) + "-wal"), Path(str(path) + "-shm")):
        try:
            info = item.lstat()
        except FileNotFoundError:
            continue
        if not stat.S_ISREG(info.st_mode) or info.st_uid != os.getuid() or info.st_mode & 0o077:
            raise TelemetryError("unsafe_path", "database files must be private regular files")


def version_check(db):
    if db.execute("PRAGMA user_version").fetchone()[0] != 1:
        raise TelemetryError("schema_version", "unsupported database schema")
    metadata = dict(db.execute("SELECT key,value FROM metadata"))
    if metadata.get("format") != "agent-telemetry-v1":
        raise TelemetryError("schema_version", "unsupported database format")
    return metadata


def settings(db, timeout_ms):
    db.execute(f"PRAGMA busy_timeout={timeout_ms}")
    db.execute("PRAGMA foreign_keys=ON")
    db.execute("PRAGMA synchronous=FULL")
    if db.execute("PRAGMA journal_mode").fetchone()[0].lower() != "wal":
        raise TelemetryError("storage_settings", "database is not in WAL mode")
    if db.execute("PRAGMA synchronous").fetchone()[0] != 2 or db.execute("PRAGMA foreign_keys").fetchone()[0] != 1:
        raise TelemetryError("storage_settings", "required settings unavailable")


def translate(error):
    code = getattr(error, "sqlite_errorcode", 0) & 255
    return TelemetryError("busy" if code in (sqlite3.SQLITE_BUSY, sqlite3.SQLITE_LOCKED) else "storage_error",
                          "database operation failed")


class SQLiteRepository:
    def __init__(self, path, *, timeout_ms=250):
        require_sqlite()
        self._open(path, timeout_ms=timeout_ms, readonly=False)

    def _open(self, path, *, timeout_ms, readonly):
        if type(timeout_ms) is not int or not 0 <= timeout_ms <= 60000:
            raise TelemetryError("invalid_config", "invalid lock timeout")
        self.path = Path(os.path.abspath(path))
        self.readonly = readonly
        check_path(self.path)
        if not self.path.exists():
            raise TelemetryError("absent", "database does not exist")
        self.db = None
        try:
            probe = sqlite3.connect(self.path.as_uri() + "?mode=ro", uri=True,
                                    isolation_level=None, timeout=timeout_ms / 1000)
            try:
                self.metadata = version_check(probe)
            except BaseException:
                probe.close()
                raise
            if readonly:
                self.db = probe
                self.db.execute("PRAGMA query_only=ON")
            else:
                # Keep the probe alive across the writable open and version recheck.
                try:
                    self.db = sqlite3.connect(self.path.as_uri() + "?mode=rw", uri=True,
                                              isolation_level=None, timeout=timeout_ms / 1000)
                    self.metadata = version_check(self.db)
                    settings(self.db, timeout_ms)
                finally:
                    probe.close()
            self.db.row_factory = sqlite3.Row
        except sqlite3.Error as error:
            self.close()
            raise translate(error) from error
        except BaseException:
            self.close()
            raise

    @classmethod
    def inspect(cls, path):
        result = cls.__new__(cls)
        result._open(path, timeout_ms=250, readonly=True)
        return result

    @classmethod
    def initialize(cls, path, *, dataset_id, collector_id, timeout_ms=250):
        require_sqlite()
        if not all(isinstance(value, str) and value for value in (dataset_id, collector_id)):
            raise TelemetryError("invalid_config", "missing dataset identity")
        path = Path(os.path.abspath(path))
        check_path(path)
        if path.exists():
            existing = cls(path, timeout_ms=timeout_ms)
            if existing.metadata["dataset_id"] != dataset_id or existing.metadata["collector_id"] != collector_id:
                existing.close()
                raise TelemetryError("identity_mismatch", "different collector already initialized")
            return existing
        path.parent.mkdir(parents=True, exist_ok=True, mode=0o700)
        check_path(path)
        try:
            fd = os.open(path, os.O_CREAT | os.O_EXCL | os.O_WRONLY | os.O_NOFOLLOW, 0o600)
        except FileExistsError:
            return cls.initialize(path, dataset_id=dataset_id, collector_id=collector_id, timeout_ms=timeout_ms)
        os.close(fd)
        db = None
        try:
            db = sqlite3.connect(path.as_uri() + "?mode=rw", uri=True, isolation_level=None,
                                 timeout=timeout_ms / 1000)
            db.execute("PRAGMA journal_mode=WAL")
            settings(db, timeout_ms)
            db.execute("BEGIN IMMEDIATE")
            for statement in _SCHEMA:
                db.execute(statement)
            db.executemany("INSERT INTO metadata VALUES (?,?)", (
                ("format", "agent-telemetry-v1"), ("dataset_id", dataset_id), ("collector_id", collector_id)))
            db.execute("PRAGMA user_version=1")
            db.commit()
            parent = os.open(path.parent, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW)
            try:
                os.fsync(parent)
            finally:
                os.close(parent)
        except sqlite3.Error as error:
            raise translate(error) from error
        finally:
            if db is not None:
                db.close()
        return cls(path, timeout_ms=timeout_ms)

    def close(self):
        if getattr(self, "db", None) is not None:
            self.db.close()
            self.db = None

    def append(self, event, captured_at, cursor=None):
        try:
            return self._append(event, captured_at, cursor)
        except TelemetryError as error:
            status = "deferred" if error.code == "busy" else "unavailable"
            if error.code in ("invalid_event", "identity_mismatch", "conflict"):
                status = "rejected"
            return CaptureResult(status, reason=error.code)
        except (ValueError, TypeError, OverflowError):
            return CaptureResult("rejected", reason="invalid_event")

    def _append(self, event, captured_at, cursor=None):
        validate(event)
        if self.readonly:
            raise TelemetryError("readonly", "inspection connection cannot append")
        if (event.context.dataset_id != self.metadata["dataset_id"] or
                event.context.collector_id != self.metadata["collector_id"]):
            raise TelemetryError("identity_mismatch", "observation belongs to another collector")
        if cursor is not None and (len(cursor) != 2 or not all(isinstance(x, str) and x for x in cursor)):
            raise TelemetryError("invalid_event", "invalid source cursor")
        digest = observation_digest(event)
        try:
            self.db.execute("BEGIN IMMEDIATE")
            previous = self.db.execute("SELECT event_id,sequence,digest,captured_at FROM events WHERE event_id=?",
                                       (event.event_id,)).fetchone()
            if previous is not None:
                if previous["digest"] != digest:
                    self.db.rollback()
                    return CaptureResult("rejected", reason="conflict")
                receipt = Receipt(**dict(previous))
                status = "duplicate"
            else:
                row = self.db.execute("INSERT INTO events(event_id,digest,captured_at,project_id,event_type,observation) VALUES(?,?,?,?,?,?)",
                                      (event.event_id, digest, captured_at, event.context.project_id,
                                       event.event_type, canonical(asdict(event))))
                receipt = Receipt(event.event_id, row.lastrowid, digest, captured_at)
                self.db.executemany("INSERT INTO links VALUES(?,?,?)", ((event.event_id, rel, target) for rel, target in event.links))
                self.db.executemany("INSERT INTO artifacts VALUES(?,?,?)",
                                    ((event.event_id, ref.digest, canonical(asdict(ref))) for ref in event.artifacts))
                status = "recorded"
            if cursor is not None:
                self.db.execute("INSERT INTO cursors VALUES(?,?) ON CONFLICT(source_id) DO UPDATE SET position=excluded.position", cursor)
            self.db.commit()
            return CaptureResult(status, receipt)
        except sqlite3.Error as error:
            self.db.rollback()
            raise translate(error) from error
        except BaseException:
            self.db.rollback()
            raise

    def query(self, *, project_id=None, event_type=None, after=0, limit=100):
        import json
        if type(limit) is not int or not 1 <= limit <= 1000 or type(after) is not int or after < 0:
            raise TelemetryError("invalid_query", "invalid query bounds")
        where = ["sequence>?"]
        parameters = [after]
        for name, value in (("project_id", project_id), ("event_type", event_type)):
            if value is not None:
                where.append(name + "=?")
                parameters.append(value)
        rows = self.db.execute("SELECT * FROM events WHERE " + " AND ".join(where) + " ORDER BY sequence LIMIT ?",
                               parameters + [limit]).fetchall()
        result = []
        for row in rows:
            item = json.loads(row["observation"])
            item.update({key: row[key] for key in ("sequence", "digest", "captured_at")})
            item["context_json"] = canonical(item["context"])
            item["unresolved_links"] = [dict(link) for link in self.db.execute(
                "SELECT relation,target_id FROM links WHERE event_id=? AND NOT EXISTS(SELECT 1 FROM events WHERE event_id=links.target_id)",
                (row["event_id"],))]
            result.append(item)
        return result

    def status(self):
        row = self.db.execute("SELECT count(*),max(sequence),max(captured_at) FROM events").fetchone()
        return {"event_count": row[0], "max_sequence": row[1], "last_capture": row[2],
                "coverage": "observed" if row[0] else "unknown", "publication": "unknown",
                "schema_version": 1, "dataset_id": self.metadata["dataset_id"],
                "collector_id": self.metadata["collector_id"],
                "cursors": dict(self.db.execute("SELECT source_id,position FROM cursors")),
                "sqlite_version": sqlite3.sqlite_version}
