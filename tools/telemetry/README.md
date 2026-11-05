# Agent telemetry

This Python package records JSON observations with project, producer and session
identity. SQLite stores the event text and query fields. Optional artifacts use a
separate content-addressed directory. Collection is local; the status command does
not assert remote publication or that an emitted message was consumed.

Install with `python -m pip install ./tools/telemetry` using Python 3.11 or newer.
The writer requires SQLite 3.51.3 or newer, or the fixed 3.44.6 or 3.50.7 releases.
Check the library loaded by Python with
`python -c 'import sqlite3; print(sqlite3.sqlite_version)'`.

A profile supplies the identities and storage locations:

```toml
schema_version = 1
project_id = "example"
dataset_id = "work-observations"
collector_id = "primary"

[storage]
runtime_root = "state"
database = "events.sqlite3"
capture_lock_timeout_ms = 250

[artifacts]
enabled = true
root = "artifacts"
```

Choose a runtime directory outside version-controlled files. A relative
`runtime_root` is resolved from the profile's directory. Database and artifact
paths are relative to that root and cannot escape it. These paths cannot overlap
or traverse symbolic links. The loader accepts only the documented settings.
`producer_id` defaults to `collector`; storage uses `adapter = "sqlite"` and
`synchronous = "FULL"`; artifacts use `adapter = "filesystem"`.

Initialize explicitly, then record and inspect observations:

```sh
telemetry --config profile.toml init
printf '%s' '{"input_tokens":0,"output_tokens":null}' | telemetry --config profile.toml record --event-id event-1 --event-type usage.sample --session-id session-1
telemetry --config profile.toml status
telemetry --config profile.toml query --event-type usage.sample --limit 20 --after 0
```

Initialization installs a complete database without replacing existing storage.
An interrupted attempt can be retried; an unrelated database is still rejected.
Termination before installation can leave a private temporary file, but it does
not occupy the configured database path.

The same interface is available as `python -m agent_telemetry`. Commands emit JSON
and failures use standard error with exit status 2. `status` and `query` open an
existing database for reading; they do not create one. The query cursor is the
last returned collection sequence. Recording the same ID and observation again
returns its original receipt. Changing the observation under that ID is rejected.

The `record` command accepts `--basis observed`, `reported`, `inferred` or
`simulated`. Select the basis that describes the source; reporting an outcome is
not evidence that it was independently measured. JSON fields retain absent, null
and zero values. Credential fields are filtered before persistence; unmodified
payloads retain their supplied JSON text.

Library callers construct a frozen `CaptureContext` and an `Event`, then call
`Collector.record`. Repositories and artifact stores are injected through ports.
Callers receive a capture result and can supply a separate health callback. A
failed capture must be handled separately from the action being observed. The
configured timeout bounds database lock waiting, not filesystem synchronization.

An empty database means coverage is unknown. Failed capture can lose observations
whose source cannot replay them. Keep the live database and its journal sidecars
together; a copy of an open database file is not a verified snapshot.


Context producers can call `agent_telemetry.cce.observe_emission` with an action
returning `(text, metadata)`. The original pair is returned unchanged. The command
adapter also wraps an existing context command:

```sh
python -m agent_telemetry.cce --config profile.toml --session-id session-1 -- context-command
```

It forwards the command's output and exit status and records its observed emission.
Capture and cleanup failures use a separate diagnostic. Consumption and delivery
remain unknown. Integrations are opt-in: invoke the original command to disable
capture. This package does not install hooks or collect native transcripts.

Credentials are filtered by structured field names, including `authorization`,
`api_key`, `password`, and access/refresh token fields. Usage fields such as
`input_tokens` are retained. Redaction policy and affected JSON paths accompany
each event. This filter does not discover secrets embedded in arbitrary text or
binary artifacts; sources must supply approved content. Artifact names are hashes,
not user-supplied paths. Missing captured content fails verification. Orphan scans
only report unreferenced objects.

The SQLite adapter rejects observed symbolic links and nonprivate database files.
Its path checks and SQLite's file open are separate operations: protection against
hostile concurrent replacement of database directories is not enforced. Artifact
operations instead traverse directories through descriptors without following
links. Durability depends on local filesystem synchronization guarantees; process
termination checks do not establish power-loss behavior. A lock budget does not
bound every filesystem operation. Querying a live WAL database uses SQLite's normal
reader coordination and may require existing or writable journal sidecars.
