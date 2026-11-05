#!/usr/bin/env bash
# Trusted leak scan for pull requests, run by the leak-scan workflow (workflow_run) from the default
# branch's code. For each open same-repository pull request of the triggering run it:
#   1. creates the `scan` check on the pull request's current head as in progress;
#   2. reads the title and body through the API and fetches the commits into a bare repository as
#      data (nothing from the pull request is checked out or executed);
#   3. runs leak_scan.sh with MARKERS and completes the check with its verdict.
# Anything that stops a verdict leaves the check failed or in progress, which blocks a merge: an
# unscanned pull request never looks clean.
# Env: GH_TOKEN, REPO (owner/name), PULLS (JSON array of {number} from the triggering run),
#      MARKERS, SERVER (e.g. https://github.com), RUN_ID (this workflow run).
# The check summary starts with "leak-scan run <RUN_ID>": GitHub replaces a details link on checks
# created by GitHub Actions, so the summary is where the producing run is named.
#      LEAK_SCAN_REMOTE: fetch URL override, used by the tests.
set -euo pipefail
here="$(cd "$(dirname "$0")" && pwd)"
work="$(mktemp -d)"; trap 'rm -rf "$work"' EXIT

scan_one() {  # scan_one NUMBER BASE HEAD ; prints the verdict lines
  local pr=$1 base=$2 head=$3 gd="$work/pr-$1.git" remote fetched
  git init -q --bare "$gd"
  remote="${LEAK_SCAN_REMOTE:-$SERVER/$REPO.git}"
  local auth=()
  if [ -z "${LEAK_SCAN_REMOTE:-}" ]; then
    auth=(-c "http.extraHeader=Authorization: basic $(printf 'x-access-token:%s' "$GH_TOKEN" | base64 -w0)")
  fi
  git "${auth[@]}" --git-dir="$gd" fetch -q --no-tags "$remote" "+refs/pull/$pr/head:refs/scan/head" "$base" \
    || { echo "::error::could not fetch the pull request's commits; nothing was scanned"; return 1; }
  fetched="$(git --git-dir="$gd" rev-parse refs/scan/head)"
  if [ "$fetched" != "$head" ]; then
    echo "::error::the pull request head moved during the scan; the run for the new head scans it"; return 1
  fi
  bash "$here/leak_scan.sh" "$gd" "$base" "$head" "$work/pr-$pr.txt"
}

numbers="$(jq -r '.[].number' <<<"${PULLS:-[]}")"
if [ -z "$numbers" ]; then echo "::notice::the triggering run has no same-repository pull request; nothing to scan"; exit 0; fi
status=0
for pr in $numbers; do
  info="$(gh api "repos/$REPO/pulls/$pr")"
  if [ "$(jq -r .state <<<"$info")" != open ]; then echo "PR #$pr is not open; skipped"; continue; fi
  if [ "$(jq -r .head.repo.full_name <<<"$info")" != "$REPO" ]; then
    echo "::notice::PR #$pr comes from a fork; not scanned here (no scan check is published)"; continue
  fi
  head="$(jq -r .head.sha <<<"$info")"; base="$(jq -r .base.sha <<<"$info")"
  jq -r '.title, (.body // "")' <<<"$info" > "$work/pr-$pr.txt"
  check="$(gh api -X POST "repos/$REPO/check-runs" -f name=scan -f head_sha="$head" -f status=in_progress -q .id)"
  set +e; out="$(scan_one "$pr" "$base" "$head" 2>&1)"; rc=$?; set -e
  printf '%s\n' "$out"
  if [ "$rc" -eq 0 ]; then conclusion=success; title="No private markers"
  else conclusion=failure; title="Private marker found or scan incomplete"; status=1; fi
  # Lines are fixed messages and counts from leak_scan.sh; matching text is never printed.
  gh api -X PATCH "repos/$REPO/check-runs/$check" -f status=completed -f conclusion="$conclusion" \
     -f "output[title]=$title" -f "output[summary]=$(printf 'leak-scan run %s\nPR #%s at %s\n\n%s\n' "$RUN_ID" "$pr" "$head" "$out" | head -c 4000)" > /dev/null
  echo "PR #$pr: $conclusion"
done
exit $status
