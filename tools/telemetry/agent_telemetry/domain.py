"""Immutable observation values, independent of storage and source formats."""

from dataclasses import asdict, dataclass
import hashlib
import json


class TelemetryError(Exception):
    def __init__(self, code: str, message: str):
        super().__init__(message)
        self.code = code


@dataclass(frozen=True)
class CaptureContext:
    project_id: str
    dataset_id: str
    collector_id: str
    producer_id: str
    session_id: str | None = None
    session_generation: str | None = None
    harness: str | None = None
    task_id: str | None = None
    config_digest: str = ""
    attribution: str = "unknown"

    def __post_init__(self):
        for name in ("project_id", "dataset_id", "collector_id", "producer_id", "attribution"):
            value = getattr(self, name)
            if not isinstance(value, str) or not value:
                raise TelemetryError("invalid_event", "context identity must be nonempty text")
        for name in ("session_id", "session_generation", "harness", "task_id"):
            value = getattr(self, name)
            if value is not None and not isinstance(value, str):
                raise TelemetryError("invalid_event", "optional context identity must be text or null")
        if not isinstance(self.config_digest, str):
            raise TelemetryError("invalid_event", "configuration identity must be text")


@dataclass(frozen=True)
class ArtifactRef:
    digest: str
    size_bytes: int
    media_type: str = "application/octet-stream"
    availability: str = "captured"
    reason: str | None = None


@dataclass(frozen=True)
class Event:
    event_id: str
    event_type: str
    payload_json: str
    context: CaptureContext
    observed_at: str | None = None
    basis: str = "observed"
    completeness: str = "complete"
    reason: str | None = None
    source_json: str = "{}"
    links: tuple[tuple[str, str], ...] = ()
    artifacts: tuple[ArtifactRef, ...] = ()
    schema_version: int = 1
    redaction_policy: str = "credentials-v1"
    redacted_paths: tuple[str, ...] = ()

    def __post_init__(self):
        object.__setattr__(self, "links", tuple(tuple(link) for link in self.links))
        object.__setattr__(self, "artifacts", tuple(self.artifacts))
        object.__setattr__(self, "redacted_paths", tuple(self.redacted_paths))


@dataclass(frozen=True)
class Receipt:
    event_id: str
    sequence: int
    digest: str
    captured_at: str


@dataclass(frozen=True)
class CaptureResult:
    status: str
    receipt: Receipt | None = None
    reason: str | None = None


def json_value(text):
    def invalid(value):
        raise ValueError("nonfinite JSON number")

    def pairs(items):
        result = {}
        for key, value in items:
            if key in result:
                raise ValueError("duplicate JSON key")
            result[key] = value
        return result

    return json.loads(text, parse_constant=invalid, object_pairs_hook=pairs)


def canonical(value):
    return json.dumps(value, ensure_ascii=False, sort_keys=True, separators=(",", ":"), allow_nan=False)


def validate(event):
    for value in (event.event_id, event.event_type, event.context.project_id,
                  event.context.dataset_id, event.context.collector_id, event.context.producer_id):
        if not isinstance(value, str) or not value or len(value) > 1000:
            raise TelemetryError("invalid_event", "observation identity must be nonempty text")
    if event.basis not in ("observed", "reported", "inferred", "simulated"):
        raise TelemetryError("invalid_event", "unknown observation basis")
    if event.completeness not in ("complete", "partial", "unavailable", "suppressed"):
        raise TelemetryError("invalid_event", "unknown completeness")
    if type(event.schema_version) is not int or event.schema_version != 1:
        raise TelemetryError("invalid_event", "unknown event schema")
    json_value(event.payload_json)
    json_value(event.source_json)
    for relation, target in event.links:
        if not isinstance(relation, str) or not relation or not isinstance(target, str) or not target:
            raise TelemetryError("invalid_event", "invalid observation link")
    for ref in event.artifacts:
        if ref.availability not in ("captured", "unavailable") or type(ref.size_bytes) is not int or ref.size_bytes < 0:
            raise TelemetryError("invalid_event", "invalid artifact metadata")
    canonical(asdict(event))


def observation_digest(event):
    # Capture time and the database sequence are deliberately outside the observation.
    return hashlib.sha256(canonical(asdict(event)).encode()).hexdigest()
