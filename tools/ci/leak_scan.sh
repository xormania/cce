#!/usr/bin/env bash
# leak_scan.sh GIT_DIR BASE HEAD PR_TEXT_FILE
# Scans a pull request for private markers: its title and body (PR_TEXT_FILE), the commit messages in
# BASE..HEAD, the lines added in BASE...HEAD and the changed file names. The commits are read as git
# data from GIT_DIR (a bare repository is enough); nothing from the pull request is checked out or run.
# The marker pattern comes from MARKERS (case-insensitive extended regex). Output names only which
# input matched and how many lines, never the matching text.
# Exit 0: clean. Exit 1: a marker matched, or the scan could not run (missing or invalid pattern,
# unreadable input): an unscanned pull request must never look clean.
set -euo pipefail
gd=$1 base=$2 head=$3 prtext=$4
t="$(mktemp -d)"; trap 'rm -rf "$t"' EXIT
if [ -z "${MARKERS:-}" ]; then echo "::error::PRIVATE_MARKERS secret is not available; nothing was scanned"; exit 1; fi
# grep exits 2 on an invalid pattern; never treat that as "no match".
rc=0; grep -Ei -- "$MARKERS" /dev/null || rc=$?
if [ "$rc" -ge 2 ]; then echo "::error::PRIVATE_MARKERS is not a valid extended regex; nothing was scanned"; exit 1; fi
cp -- "$prtext" "$t/pr.txt"
git --git-dir="$gd" log --format='%B' "$base..$head" > "$t/commits.txt"
# Mark added lines with '>' so file headers ("+++ b/...") can never be confused with real additions
# such as a source line that itself starts with "++". --text and the two --no-* flags make every
# added line visible even if attributes mark a file binary or filter it.
git --git-dir="$gd" diff --text --no-ext-diff --no-textconv --output-indicator-new='>' "$base...$head" -U0 > "$t/diff.txt"
# -a: a diff containing binary content (NUL bytes) must still be read line by line; without it grep
# reports only "binary file matches" and extracts nothing, so nothing is scanned.
rc=0; grep -a '^>' "$t/diff.txt" | cut -c2- > "$t/added.txt" || rc=$?
if [ "$rc" -ge 2 ]; then echo "::error::could not extract added lines (grep exit $rc)"; exit 1; fi
if [ "$(grep -ac '^>' "$t/diff.txt" || true)" -gt 0 ] && [ ! -s "$t/added.txt" ]; then
  echo "::error::the diff adds lines but none were extracted; nothing was scanned"; exit 1
fi
git --git-dir="$gd" diff --name-only "$base...$head" > "$t/files.txt"
fail=0
for f in pr commits added files; do
  rc=0; grep -naEi -- "$MARKERS" "$t/$f.txt" > "$t/$f.hits" || rc=$?
  if [ "$rc" -eq 0 ]; then
    echo "::error::private marker found in PR $f (rewrite it; matches not echoed): $(wc -l < "$t/$f.hits") line(s)"
    fail=1
  elif [ "$rc" -ge 2 ]; then
    echo "::error::scan of PR $f failed (grep exit $rc)"
    fail=1
  fi
done
[ "$fail" -eq 0 ] && echo "no private markers in PR text, commit messages, added lines or file names"
exit $fail
