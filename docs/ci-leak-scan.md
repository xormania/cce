# Trusted leak scan

The required `scan` check fails a pull request whose title, body, commit
messages, added lines or changed file names match the private marker pattern.
The pattern lives in the `PRIVATE_MARKERS` repository secret.

## Where the secret is used

`.github/workflows/leak-scan.yml` produces `scan` for same-repository pull
requests. It runs on `workflow_run` after the `leak-guard` pull request workflow
completes, so it always runs the default branch's code, never the pull
request's. `leak-guard` itself only starts it: it runs the pull request's own
definition, so it uses no secrets and produces no `scan` check. `tools/ci/leak_scan_trusted.sh` then:

1. reads the pull request's current title, body, head and base through the API;
2. creates the `scan` check on that head as in progress;
3. fetches the pull request's commits into a bare repository as data, without
   checking them out or running anything from them;
4. runs `tools/ci/leak_scan.sh` and completes the check with its verdict.

The check reports which input matched and how many lines. It never shows the
matching text.

## Failing closed

An unscanned pull request never passes:

- A missing secret, an invalid pattern, a failed fetch or a head that moved
  during the scan publishes `scan` as failed.
- A run that stops before its verdict leaves `scan` in progress, which also
  blocks a merge.

Runs for one branch are serialized, and each run reads the pull request's
current state when it starts. An edit made while a scan is running is covered by
the next run.

## Limits

- Pull requests from forks are not scanned by this workflow and get no `scan`
  check from it.
- PR comments and review comments are not scanned.
- A workflow file added or changed by a pull request takes effect for scans only
  after it is merged.

## Drift evidence

The drift observer treats the latest `scan` check from GitHub Actions on the
candidate head as the leak scan's evidence. The check must:

- name, on the first line of its summary (`leak-scan run <id>`), a completed
  `leak-scan` `workflow_run` in this repository (GitHub replaces the details link
  of checks created by GitHub Actions, so the summary carries it);
- come from a run whose workflow definition and scan scripts, at the commit it
  ran, equal the trusted ones.

A pass counts as executed only when the check reports a clean scan, and a failure
only when it reports a match. An observer run that starts before the scan has
finished waits for a later recheck. The Checks API cannot tie a check to the run
that created it, so a check from the same app that copies a genuine run's summary
is not detected.

## Tests

`python3 -m unittest discover -s tests/ci -p test_leak_scan.py` uses synthetic
repositories and a stub `gh` to check:

- each scanned input, including added lines that start with `++` and binary
  content;
- that matches are never echoed;
- the fail-closed cases above;
- that forks, closed pull requests and runs without a pull request publish
  nothing.
