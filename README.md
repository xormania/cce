# cce

Repository for the CCE kernel work.

Start with [AGENTS.md](AGENTS.md) and the
[local development setup specification](docs/local-development.md).

The development target is `ubuntu-dev`, with native Rust tooling, Serena, and
the existing local agent environment. Setup resolves current upstream releases
and verifies the actual harness connections before declaring the environment ready.

This initial change defines the environment and agent guidance. Installation
and acceptance checks must run on `ubuntu-dev`; they have not been performed
by this documentation change.
