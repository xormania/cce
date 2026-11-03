# Local development setup

## Goal and completion

Prepare `https://github.com/xormania/cce` on a local Linux machine so xor can begin the
Rust kernel work with Serena and the standard local agent environment ready.
Native installation is the default. Docker is optional, not a prerequisite.

This document defines the setup task. It does not assert that the machine has
been inspected, tools installed, MCP connected, or acceptance checks passed.
The initial PR adds guidance; the local agent executes and records the setup.

Complete the preparation, resolve routine installation/configuration issues,
and verify the result on the actual host. Preserve the requested capabilities
when investigating a problem. Report a real blocker precisely and continue
independent work. Kernel design and implementation remain the subsequent task.

## 1. Inspect the local baseline

1. Read `AGENTS.md` and the applicable local/user agent instructions.
2. Locate or clone `xormania/cce` using xor's existing directory conventions.
   Verify the remote, branch, working tree, resolved path, ownership, and mount.
   Prefer a native Linux filesystem for the working copy. Do not relocate existing
   work or assume a pathname establishes filesystem durability.
3. Inspect the installed harnesses, MCP registrations, relevant hooks, permission
   settings, and existing Rust/Python tooling. Record versions and executable
   paths without collecting credentials or dumping the full environment.
4. Identify the permitted mechanisms for installation, file operations, building,
   testing, and Git/GitHub operations. Reuse them rather than introducing a new
   orchestration service.
5. Check the connectivity and native build prerequisites actually needed for the
   selected tools: source/package downloads, compiler/linker, and required libraries.
   Inspect the host rather than assuming its installed packages from another setup.

Reuse working installations and configuration, but update outdated tools as
specified below. Keep changes to shared settings narrow and preserve unrelated
MCP services, credentials, hooks, and other projects' toolchain selections.

## 2. Resolve current versions at setup time

**Already installed does not mean current.** Consult official upstream release
and installation sources when the setup runs. Use current stable releases by
default; record the release channel and any demonstrated compatibility issue.
Do not silently retain an old release, downgrade, or substitute an old image pin.

| Component | Required outcome | Official source |
| --- | --- | --- |
| Rust and rustup | Current stable Rust and current rustup; native Cargo builds work | [Rust installation](https://rust-lang.org/tools/install/) and [rustup](https://rust-lang.github.io/rustup/basics.html) |
| Rust components | Matching Cargo, rustfmt, Clippy, rust-analyzer, and rust-src for the selected toolchain | [rustup components](https://rust-lang.github.io/rustup/concepts/components.html) |
| uv | Current stable uv, updated through its supported installation method | [uv installation and updates](https://docs.astral.sh/uv/getting-started/installation/) |
| Python for Serena | A current patched Python release in a version line supported by the selected Serena release | [Serena installation](https://oraios.github.io/serena/02-usage/010_installation.html) |
| Serena | Current published Serena release with the Rust language-server backend | [Serena releases](https://github.com/oraios/serena/releases/latest) and [configuration](https://oraios.github.io/serena/02-usage/050_configuration.html) |
| Agent harnesses | Current supported Codex/Claude Code installations used by xor, with working Serena connections | [Codex CLI](https://developers.openai.com/codex/cli) and [Claude Code setup](https://code.claude.com/docs/en/setup) |
| Supporting tools | Current supported Git, native build tools, and the existing MCP tools needed for this workflow | Their official installation/update documentation |

Update existing installations as well as installing missing components. Inspect
how each was installed before updating it. Use the release's documented Python
compatibility instead of forcing the newest Python major/minor onto Serena, and
avoid replacing Ubuntu's system Python to satisfy a tool dependency.

Check for stale executable paths, package version constraints, Rust overrides,
cached launch commands, and MCP registrations that still select an old version.
A package-manager success message is insufficient: verify the executable and
version used by the actual client and server process after restart.

Resolve versions first, then record reproducible project pins after the smoke
checks succeed. Record the date, source URL, exact version, executable path,
installation method, and release channel. Use an exact stable Rust toolchain
in the resulting project configuration; do not change another project's pin
or the user's default merely to select this project's toolchain.

These instructions request an update during setup. They do not request automatic
upgrades on every session startup. Later updates repeat the same verification.

## 3. Prepare Rust and Serena

### Rust

Make native Cargo build/test, formatting, and Clippy work from the checkout's
local execution environment. Install rust-analyzer and Rust sources for the
same toolchain and verify the language server can see Cargo, the toolchain,
sources, and any needed dependency caches.

Use a disposable Rust 2024 fixture for setup validation. Do not choose the
kernel's crate layout, interfaces, storage implementation, or dependencies as a
side effect of proving that the toolchain works.

### Serena

Follow the selected release's official native installation/update instructions.
The published package is `serena-agent`, managed with uv. Confirm both package
and executable identity, initialize the language-server backend, and configure
Rust for this project using the schema supported by the installed release.

Configure project identity as `cce`; bind activation to the actual checkout or
worktree. Keep generated indexes, caches, transient memories, logs, and fixture
output out of commits. Preserve useful existing configuration when upgrading.

Do not copy the old Xorder PHP-specific Serena configuration into this Rust
project. Xorder may provide useful conventions, but it is not a mandatory runtime
or source of fixed versions for this native setup.

Serena and rust-analyzer must see the same source paths and the intended Rust
toolchain. If indexing reports no Cargo project before implementation exists,
distinguish that expected empty-repository condition from a failed fixture test.
Prove Rust semantics against the disposable Cargo fixture and restore `cce`
activation afterward; repeat against real crates when implementation begins.

## 4. Connect the standard local agent environment

Inspect the actual local configurations, then connect each Codex/Claude Code
harness used for this project through Serena's documented stdio MCP launch.
Prefer project scope when supported; otherwise make a narrow per-project
registration using the client's supported mechanism.

| Surface | Setup responsibility |
| --- | --- |
| `AGENTS.md` | Shared project instructions; keep it concise and consistent with xor's current direction |
| Claude instructions | Reuse the existing convention; if a project `CLAUDE.md` adapter is needed, have it refer to `AGENTS.md` rather than duplicate policy |
| Codex MCP | Inspect user configuration and trusted-project `.codex/config.toml`; add or update only the intended Serena entry |
| Claude Code MCP | Inspect existing scopes; use project `.mcp.json` or the established per-project registration and verify which entry wins |
| Serena project configuration | Use the installed version's schema, the correct checkout, and Rust backend |
| Local-only settings and state | Keep credentials, host-specific overrides, caches, and raw session logs untracked |

Use [Serena's client guidance](https://oraios.github.io/serena/02-usage/030_clients.html),
[Codex MCP documentation](https://developers.openai.com/codex/mcp), and
[Claude Code MCP documentation](https://code.claude.com/docs/en/mcp) for the
installed versions. Verify the launch command, working directory, PATH,
project activation, context, and startup behavior. Do not assume a shell's
PATH or a user-level registration is what the harness actually uses.

Read Serena's session instructions after connecting. Preserve xor's active
tool-access policy and preference for MCP-only agent operation. Confirm permitted
capabilities for source editing, Cargo commands, diagnostics, Git, and PR work.
A server exposed through MCP does not by itself restrict what its execution
tools can do. Do not bypass enforced restrictions through a shell or REPL.
Identify any missing capability and the existing configuration or tool needed
to supply it; do not turn this setup into a new control-plane project.

Inspect existing hooks before changing them. Keep unrelated hooks and approval
settings intact. New optional reminder or auto-approval hooks are not required
to prove the Serena connection works.

## 5. Verify from the actual local harness sessions

Use a disposable fixture with at least two Rust source modules, a named symbol,
a cross-file caller, and a test that observes the symbol's behavior. Keep it
separate from the kernel's eventual production workspace and requirements.

Run these checks through the configured clients and permitted local tools.
For every result, record the tool/session used, command or MCP operation,
exit status where applicable, and concise evidence. Do not mark unexecuted
checks as passed.

| Check | Required observation |
| --- | --- |
| Identity | Remote and actual checkout path match `xormania/cce`; selected versions and executable paths are recorded |
| Native build | The fixture builds with the selected Rust toolchain |
| Tests | Fixture tests execute with a nonzero test count and pass |
| Formatting | `cargo fmt --all -- --check` succeeds on the fixture |
| Linting | `cargo clippy --all-targets --all-features -- -D warnings` succeeds on the fixture |
| MCP connection | Every configured harness connects to the intended Serena instance and receives its instructions |
| Project activation | Serena reports the intended checkout/fixture during each respective check |
| Symbol lookup | Serena finds the fixture symbol with the expected definition and source file |
| References | Serena finds the known caller in another source module |
| Edit and reread | An intentional edit through Serena is visible on disk and through a fresh semantic read; a test confirms the changed behavior |
| Diagnostics | A temporary Rust type error appears through the configured diagnostics route; after restoration, diagnostics and tests are clean |
| Restart | Restarting the MCP server/harness reconnects using the selected versions and correct project; source edits persist |
| Agent instructions | Each harness loads the intended shared instructions, directly or through its adapter |
| Tool access | Required build/test/Git operations are reachable through permitted mechanisms; gaps are named rather than hidden |

A basic Cargo check sequence for the fixture is:

```sh
cargo build
cargo test
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
```

These describe command behavior, not permission to bypass the host's tool policy.
Use the approved local execution mechanism. Restore the fixture after deliberate
faults, rerun the checks, and confirm that setup left no fixture or generated
output staged for the production repository.

If xor supplies an implementation brief with additional tool requirements,
identify and prepare them before claiming readiness for those checks. Tools
that need a separately pinned toolchain (for example, nightly-only tooling)
stay isolated from product builds, which use stable. Record such capabilities
separately. This does not authorize implementing the
kernel or making nightly the user's default as part of environment setup.

## 6. Record the handoff

Produce a concise setup record with:

- The resolved checkout path, branch, remote, host facts, and relevant mount.
- Tool versions, installation methods, executable paths, release sources, and
  the date checked; any compatibility exception with its actual failure evidence.
- Configuration files changed and the effective MCP launch settings for each
  harness, with credentials omitted.
- How to start the local sessions, activate this project, and rerun checks.
- Acceptance results marked passed, failed, blocked, or not applicable, with
  enough evidence to reproduce each result.
- Any remaining setup action and exactly what prevents it.

Commit portable project configuration and a redacted setup summary through a
feature branch/PR. Keep private machine details and raw local logs untracked.
Do not substitute cloud tests or written instructions for local results.
The environment is ready when required tools and connections work locally and
the applicable checks above have actually passed.
