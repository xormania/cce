"""Collector composition and JSON command-line interface."""

from __future__ import annotations

import argparse
from dataclasses import asdict
import json
import sqlite3
import sys

from .application import Collector
from .artifacts import FileArtifactStore
from .config import load_config
from .domain import CaptureContext, Event, TelemetryError
from .sqlite import SQLiteRepository


def _emit(value: object, *, error: bool = False) -> None:
    print(json.dumps(value, ensure_ascii=True, allow_nan=False),
          file=sys.stderr if error else sys.stdout)


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description="Capture and query JSON observations")
    parser.add_argument("--config", required=True, help="TOML collector profile")
    commands = parser.add_subparsers(dest="command", required=True)
    commands.add_parser("init", help="Explicitly initialize the configured database")
    commands.add_parser("status", help="Inspect existing collection state")
    query = commands.add_parser("query", help="Read a bounded page of events")
    query.add_argument("--event-type")
    query.add_argument("--limit", type=int, default=100)
    query.add_argument("--after", type=int, default=0)
    record = commands.add_parser("record", help="Capture a JSON payload from standard input")
    record.add_argument("--event-id", required=True)
    record.add_argument("--event-type", required=True)
    record.add_argument("--session-id")
    record.add_argument("--basis", choices=("observed", "reported", "inferred", "simulated"),
                        default="observed")
    args = parser.parse_args(argv)
    repository = None
    try:
        config = load_config(args.config)
        if args.command == "init":
            repository = SQLiteRepository.initialize(config.database, dataset_id=config.dataset_id,
                                                     collector_id=config.collector_id,
                                                     timeout_ms=config.timeout_ms)
            _emit(repository.status())
        elif args.command in ("query", "status"):
            repository = SQLiteRepository.inspect(config.database)
            if args.command == "status":
                _emit(repository.status())
            else:
                _emit(repository.query(project_id=config.project_id, event_type=args.event_type,
                                       after=args.after, limit=args.limit))
        else:
            repository = SQLiteRepository(config.database, timeout_ms=config.timeout_ms)
            artifacts = FileArtifactStore(config.artifact_root) if config.artifacts_enabled else None
            context = CaptureContext(project_id=config.project_id, dataset_id=config.dataset_id,
                                     collector_id=config.collector_id, producer_id=config.producer_id,
                                     config_digest=config.digest, session_id=args.session_id,
                                     attribution="explicit")
            event = Event(event_id=args.event_id, event_type=args.event_type,
                          payload_json=sys.stdin.read(), context=context, basis=args.basis)
            result = Collector(repository, artifacts=artifacts).record(event)
            _emit(asdict(result))
            if result.status not in ("recorded", "duplicate"):
                _emit({"status": result.status, "reason": result.reason}, error=True)
                return 2
        return 0
    except TelemetryError as error:
        _emit({"status": "unavailable", "reason": error.code}, error=True)
        return 2
    except (OSError, ValueError, sqlite3.Error):
        _emit({"status": "unavailable", "reason": "operation_failed"}, error=True)
        return 2
    finally:
        if repository is not None:
            repository.close()


if __name__ == "__main__":
    raise SystemExit(main())
