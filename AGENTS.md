# Agent instructions

## Direction and scope

- xor is the project owner. Current explicit direction takes precedence over
  earlier plans and repository guidance.
- Read [README.md](README.md). For environment work, follow
  [docs/local-development.md](docs/local-development.md).
- Keep environment preparation separate from kernel implementation. Use the
  current implementation requirements supplied by xor when that work begins;
  do not infer kernel architecture or requirements from setup files or old projects.
- State material assumptions, verify missing facts, and distinguish requested
  behavior from behavior actually implemented and tested.

## Local environment and tools

- Target a local Linux development machine. Native setup is the default; Docker is not a prerequisite.
- Inspect and reuse the established local agent configuration. Preserve unrelated
  files, settings, credentials, and other projects' toolchain selections.
- During setup or an explicitly requested update, resolve current upstream
  releases, update outdated installations, verify compatibility, and record exact
  versions. During ordinary development, use the resulting project pins.
- Use Serena for Rust symbol navigation, references, and appropriate edits. Read
  its session instructions and confirm it has activated the intended checkout.
- Honor the active tool permissions and xor's preference for MCP-only agent
  operation. Reuse permitted build, test, and Git capabilities; report concrete
  gaps. Do not bypass an enforced restriction through a shell or unrestricted REPL.

## Changes and validation

- Inspect the working tree before editing and preserve unrelated work.
- Make focused changes. Ask only when a missing decision actually blocks the
  work; continue independent work when one check is blocked.
- Use meaningful checks appropriate to the change. For the initial setup, use
  the disposable-fixture acceptance checks in the setup specification.
- Report what changed, checks run, actual results, and unresolved blockers.
  A written configuration or installed executable alone is not proof of readiness.

## Git workflow

- Use feature branches and pull requests against the repository's default branch.
- Do not directly update the default branch, force-push, delete remote branches,
  change repository rules, or merge a PR unless xor explicitly requests it.
- Follow the repository's actual merge rules; do not import rules from another project.
- Use the established human or neutral automation Git identity. Do not add
  assistant/model co-authorship or generated-by signatures.
- Do parallel work in git worktrees outside the checkout, one branch per
  worktree: `git worktree add <path-outside-the-checkout> -b <branch> origin/master`.
  Remove the worktree after its PR merges.

## Public text

This repository is public. Commit messages, PR titles and bodies, issue and PR
comments, and committed files must stand on their own:

- Describe the change in the diff: what changed, why in terms of the code, and
  how it was checked.
- Do not include, quote, summarize, link, or name planning material, task
  prompts, or reference documents supplied outside the repository, and do not
  describe future work the change does not implement.
- Do not commit files supplied as instructions or reference material.
- If unsure whether something belongs, leave it out and ask xor.
- The `scan` check (trusted `leak-scan` workflow) scans PR text, commit messages,
  added lines and file names. If it fails, rewrite the text; do not work around
  the check.
