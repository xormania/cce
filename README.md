# cce

Repository for the CCE kernel work.

Start with [AGENTS.md](AGENTS.md) and the
[local development setup specification](docs/local-development.md).

The development target is a local Linux machine, with native Rust tooling, Serena, and
the existing local agent environment. Setup resolves current upstream releases
and verifies the actual harness connections before declaring the environment ready.

This initial change defines the environment and agent guidance. Installation
and acceptance checks must run on that local machine; they have not been performed
by this documentation change.
