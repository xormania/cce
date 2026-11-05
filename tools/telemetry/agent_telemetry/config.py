"""Load a collector profile without opening storage or consulting the environment."""

from __future__ import annotations

from dataclasses import dataclass
import hashlib
import json
from pathlib import Path
import stat
import tomllib

from .domain import TelemetryError


@dataclass(frozen=True)
class Config:
    project_id: str
    dataset_id: str
    collector_id: str
    producer_id: str
    runtime_root: Path
    database: Path
    artifact_root: Path
    artifacts_enabled: bool
    timeout_ms: int
    digest: str


def _fail(message: str) -> None:
    raise TelemetryError("configuration", message)


def _keys(data: dict, allowed: set[str], label: str) -> None:
    if data.keys() - allowed:
        _fail(f"Unknown {label} setting")


def _text(value: object, label: str) -> str:
    if not isinstance(value, str) or not value.strip():
        _fail(f"{label} must be nonempty text")
    if any(ord(char) < 32 for char in value):
        _fail(f"{label} contains a control character")
    return value


def _section(data: dict, name: str, allowed: set[str]) -> dict:
    section = data.get(name, {})
    if not isinstance(section, dict):
        _fail(f"{name} must be a table")
    _keys(section, allowed, name)
    return section


def _no_symlinks(path: Path, label: str) -> None:
    for component in (path, *path.parents):
        try:
            mode = component.lstat().st_mode
        except FileNotFoundError:
            continue
        except OSError:
            _fail(f"{label} cannot be inspected")
        if stat.S_ISLNK(mode):
            _fail(f"{label} cannot contain symbolic links")


def _child(root: Path, value: object, label: str) -> Path:
    relative = Path(_text(value, label))
    if relative.is_absolute() or ".." in relative.parts:
        _fail(f"{label} must be a relative path inside runtime_root")
    candidate = root / relative
    _no_symlinks(candidate, label)
    try:
        resolved = candidate.resolve()
    except (OSError, RuntimeError):
        _fail(f"{label} cannot be resolved")
    if not resolved.is_relative_to(root) or resolved == root:
        _fail(f"{label} must be inside runtime_root")
    return resolved


def load_config(path: str | Path) -> Config:
    """Validate the complete profile and freeze its effective storage locations."""
    try:
        profile = Path(path).absolute()
        with profile.open("rb") as stream:
            data = tomllib.load(stream)
    except (OSError, ValueError, RuntimeError):
        _fail("Cannot read a valid TOML profile")
    _keys(data, {"schema_version", "project_id", "dataset_id", "collector_id",
                 "producer_id", "storage", "artifacts"}, "profile")
    if type(data.get("schema_version")) is not int or data["schema_version"] != 1:
        _fail("Unsupported profile schema_version")
    identity = {
        name: _text(data.get(name), name)
        for name in ("project_id", "dataset_id", "collector_id")
    }
    identity["producer_id"] = _text(data.get("producer_id", "collector"), "producer_id")
    storage = _section(data, "storage", {"runtime_root", "database", "adapter",
                                       "synchronous", "capture_lock_timeout_ms"})
    artifacts = _section(data, "artifacts", {"root", "enabled", "adapter"})
    if storage.get("adapter", "sqlite") != "sqlite":
        _fail("Unsupported storage adapter")
    if storage.get("synchronous", "FULL") != "FULL":
        _fail("Storage requires FULL synchronization")
    if artifacts.get("adapter", "filesystem") != "filesystem":
        _fail("Unsupported artifact adapter")
    timeout = storage.get("capture_lock_timeout_ms", 250)
    if type(timeout) is not int or not 0 <= timeout <= 60000:
        _fail("capture_lock_timeout_ms must be an integer between 0 and 60000")
    enabled = artifacts.get("enabled", True)
    if type(enabled) is not bool:
        _fail("artifacts.enabled must be boolean")
    root_input = profile.parent / _text(storage.get("runtime_root"), "runtime_root")
    _no_symlinks(root_input, "runtime_root")
    try:
        root = root_input.resolve()
    except (OSError, RuntimeError):
        _fail("runtime_root cannot be resolved")
    database = _child(root, storage.get("database", "live.sqlite3"), "database")
    artifact_root = _child(root, artifacts.get("root", "artifacts"), "artifacts.root")
    if database == artifact_root or database.is_relative_to(artifact_root) or artifact_root.is_relative_to(database):
        _fail("Database and artifact paths must not overlap")
    effective = {"schema_version": 1, **identity, "runtime_root": str(root),
                 "database": str(database), "artifact_root": str(artifact_root),
                 "artifacts_enabled": enabled, "timeout_ms": timeout,
                 "storage_adapter": "sqlite", "artifact_adapter": "filesystem",
                 "synchronous": "FULL"}
    digest = hashlib.sha256(json.dumps(effective, sort_keys=True, separators=(",", ":")).encode()).hexdigest()
    return Config(**identity, runtime_root=root, database=database,
                  artifact_root=artifact_root, artifacts_enabled=enabled,
                  timeout_ms=timeout, digest=digest)
