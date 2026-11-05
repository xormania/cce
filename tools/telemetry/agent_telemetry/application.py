"""Capture orchestration; source actions retain their own outcomes."""

from dataclasses import replace
from datetime import datetime, timezone
from .domain import CaptureResult, TelemetryError, canonical, json_value, validate
from .ports import ArtifactStore, EventRepository


# Credential names are exact normalized keys; usage counters are not credentials.
_CREDENTIALS = frozenset(("authorization", "password", "passwd", "secret", "apikey",
                          "accesstoken", "refreshtoken", "idtoken", "credential",
                          "credentials", "cookie", "setcookie", "clientsecret", "token"))


def sanitize(text, prefix):
    paths = []

    def walk(value, path):
        if isinstance(value, dict):
            result = {}
            for key, item in value.items():
                child = path + "/" + key.replace("~", "~0").replace("/", "~1")
                normalized = key.lower().replace("_", "").replace("-", "")
                if normalized in _CREDENTIALS:
                    result[key] = "[REDACTED]"
                    paths.append(child)
                else:
                    result[key] = walk(item, child)
            return result
        if isinstance(value, list):
            return [walk(item, path + "/" + str(i)) for i, item in enumerate(value)]
        return value

    value = walk(json_value(text), prefix)
    return (canonical(value) if paths else text), paths


class Collector:
    def __init__(self, repository: EventRepository, artifacts: ArtifactStore | None = None,
                 clock=None, health=None):
        self.repository = repository
        self.artifacts = artifacts
        self.clock = clock or (lambda: datetime.now(timezone.utc).isoformat())
        self.health = health
        self.failures = 0

    def record(self, event, *, cursor=None):
        try:
            validate(event)
            payload, payload_paths = sanitize(event.payload_json, "payload")
            source, source_paths = sanitize(event.source_json, "source")
            event = replace(event, payload_json=payload, source_json=source,
                            redacted_paths=tuple(sorted(set(event.redacted_paths + tuple(payload_paths + source_paths)))))
            for ref in event.artifacts:
                if ref.availability == "captured" and (self.artifacts is None or not self.artifacts.verify(ref)):
                    raise TelemetryError("artifact_integrity", "captured artifact cannot be verified")
            result = self.repository.append(event, self.clock(), cursor=cursor)
        except TelemetryError as error:
            status = "deferred" if error.code == "busy" else "unavailable"
            if error.code in ("invalid_event", "conflict", "invalid_json", "identity_mismatch"):
                status = "rejected"
            result = CaptureResult(status, reason=error.code)
        except (ValueError, TypeError, OverflowError):
            result = CaptureResult("rejected", reason="invalid_json")
        except Exception:
            # Never expose exception text: it may contain source payload or secrets.
            result = CaptureResult("unavailable", reason="capture_error")
        if result.status not in ("recorded", "duplicate"):
            self.failures += 1
            if self.health is not None:
                try:
                    self.health({"status": result.status, "reason": result.reason,
                                 "failures": self.failures})
                except Exception:
                    pass
        return result
