"""Observe context emission without assigning delivery or consumption semantics."""

from dataclasses import asdict
import json
import subprocess
import sys
import time
import uuid
from .application import Collector
from .artifacts import FileArtifactStore
from .config import load_config
from .domain import CaptureContext, Event
from .sqlite import SQLiteRepository


def observe_emission(action, collector, context, *, event_id=None):
    """Return the original (text, metadata) pair, even when capture fails."""
    start = time.monotonic_ns()
    result = action()
    elapsed = time.monotonic_ns() - start
    try:
        text, metadata = result
        collector.record(Event(
            event_id or str(uuid.uuid4()), "cce.context.emitted",
            json.dumps({"text": text, "metadata": metadata, "duration_ns": elapsed,
                        "emitted_bytes": len(text.encode()), "consumed": None,
                        "delivery": "unknown"}, ensure_ascii=False), context))
    except Exception:
        try:
            print("telemetry: emission capture unavailable", file=sys.stderr)
        except Exception:
            pass
    return result


def main(argv=None):
    """Run an existing context command, forwarding bytes and preserving its status."""
    import argparse
    parser = argparse.ArgumentParser(description="Observe a context command's emitted output")
    parser.add_argument("--config", required=True)
    parser.add_argument("--session-id")
    parser.add_argument("command", nargs=argparse.REMAINDER)
    args = parser.parse_args(argv)
    command = args.command[1:] if args.command[:1] == ["--"] else args.command
    if not command:
        parser.error("a context command is required")
    start = time.monotonic_ns()
    result = subprocess.run(command, stdout=subprocess.PIPE)
    duration = time.monotonic_ns() - start
    sys.stdout.buffer.write(result.stdout)
    sys.stdout.buffer.flush()
    repository = None
    try:
        config = load_config(args.config)
        repository = SQLiteRepository(config.database, timeout_ms=config.timeout_ms)
        collector = Collector(repository,
                              FileArtifactStore(config.artifact_root) if config.artifacts_enabled else None,
                              health=lambda status: print("telemetry: " + json.dumps(status), file=sys.stderr))
        context = CaptureContext(config.project_id, config.dataset_id, config.collector_id,
                                 config.producer_id, session_id=args.session_id,
                                 config_digest=config.digest, attribution="explicit")
        text = result.stdout.decode("utf-8", errors="strict")
        collector.record(Event(str(uuid.uuid4()), "cce.context.emitted", json.dumps({
            "text": text, "exit_code": result.returncode, "duration_ns": duration,
            "emitted_bytes": len(result.stdout), "consumed": None, "delivery": "unknown",
        }, ensure_ascii=False), context, completeness="complete" if result.returncode == 0 else "partial"))
    except Exception:
        print("telemetry: emission capture unavailable", file=sys.stderr)
    finally:
        if repository is not None:
            repository.close()
    return result.returncode if result.returncode >= 0 else 128 - result.returncode


if __name__ == "__main__":
    raise SystemExit(main())
