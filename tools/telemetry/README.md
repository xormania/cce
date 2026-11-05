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
