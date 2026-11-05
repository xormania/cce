"""Authenticate producer definitions and preserve execution applicability/provenance.

Candidate workflows are data. Only definitions byte-identical to the trusted
checkout may supply execution evidence. Rust evidence reused after a Markdown
follow-up remains attributed to the actual older producer commit.
"""
from __future__ import annotations

import hashlib
from pathlib import Path
import re
import urllib.error
from urllib.parse import quote


MAX_RUNS = 50
MAX_HISTORY = 20
MAX_JOBS = 50
MAX_LOG_BYTES = 1_000_000
MAX_COMPARE_FILES = 300
_SHA = re.compile(r"[0-9a-f]{40}\Z")
_DEFINITION_REASONS = frozenset({"producer_definition_unaccepted", "unsupported_producer_definition",
                                 "producer_acceptance_unavailable"})
_KINDS = {
    "drift-input": {"unittest": r"python3?\s+-m\s+unittest\s+discover\b"},
    "rust": {"format": r"cargo\s+fmt\b", "clippy": r"cargo\s+clippy\b",
             "test": r"cargo\s+test\b"},
}
# The required leak scan for a pull request is the `scan` check that the trusted leak-scan workflow
# (workflow_run, default-branch code) publishes on the candidate head. Its producer is named on the
# first line of the check summary ("leak-scan run <id>"; GitHub replaces the details link of checks
# created by GitHub Actions); the definition and scripts at that run's commit must equal the trusted ones.
SCAN_CHECK = "scan"
SCAN_TITLE = "Trusted leak scan"
SCAN_PATH = ".github/workflows/leak-scan.yml"
SCAN_SCRIPTS = ("tools/ci/leak_scan_trusted.sh", "tools/ci/leak_scan.sh")
ACTIONS_APP = 15368
SCAN_CLEAN = "no private markers in PR text, commit messages, added lines or file names"
SCAN_MATCH = "private marker found in PR "


def _row(identity, status="unknown", executed=False, source_sha=None, **metadata):
    return {"id": identity, "required": True, "executed": executed,
            "status": status, "source_sha": source_sha, **metadata}


def _definition_failure(identity, source_sha, reason):
    row = _row(identity, source_sha=source_sha, reason=reason)
    if reason == "producer_definition_unaccepted":
        # This row describes policy validation, not an observed test execution.
        row.update(definition_status="awaiting_acceptance", executed=None)
    return row


def _steps(text, producer):
    """Recognize named direct-command steps from the trusted definition only.

    This deliberately supports a small syntax subset rather than interpreting
    arbitrary YAML. Unknown/wrapper-only producer definitions cannot certify work.
    """
    parts = re.split(r"(?m)^\s*- name:\s*", text)[1:]
    found = {}
    for part in parts:
        title, _, body = part.partition("\n")
        title = title.strip().strip("\"'")
        body = re.split(r"(?m)^\s*- (?:name:|uses:|run:)", body, maxsplit=1)[0]
        for kind, pattern in _KINDS[producer].items():
            if re.search(pattern, body):
                if kind in found or not title:
                    raise ValueError("unsupported_producer_definition")
                found[kind] = title
    if set(found) != set(_KINDS[producer]):
        raise ValueError("unsupported_producer_definition")
    return found


REVIEW_PAGES = 10


def _owner_acceptance(api, source):
    """Return the repository owner's approving review of exactly this head, if any.

    Candidate output can never certify a changed producer. Only the repository owner's latest
    review of the candidate's own head commit, on an open PR whose base is the evaluated base,
    can accept it; a later push needs a new review. Every review page is read; an unreadable or
    unbounded review list fails closed.
    """
    owner = api.repository.split("/", 1)[0]
    for number in source.get("pulls", []):
        pull = api.api("/pulls/" + str(int(number)))
        if not isinstance(pull, dict):
            raise ValueError("producer_acceptance_unavailable")
        if (pull.get("state") != "open" or (pull.get("head") or {}).get("sha") != source["head_sha"]
                or (pull.get("base") or {}).get("sha") != source["base_sha"]):
            continue
        reviews = []
        for page in range(1, REVIEW_PAGES + 1):
            rows = api.api("/pulls/" + str(int(number)) + "/reviews?per_page=100&page=" + str(page))
            if not isinstance(rows, list):
                raise ValueError("producer_acceptance_unavailable")
            reviews += rows
            if len(rows) < 100:
                break
        else:
            raise ValueError("producer_acceptance_unavailable")
        decisive = [r for r in reviews
                    if isinstance(r, dict) and (r.get("user") or {}).get("login") == owner
                    and r.get("commit_id") == source["head_sha"]
                    and r.get("state") in {"APPROVED", "CHANGES_REQUESTED", "DISMISSED"}
                    and type(r.get("id")) is int]
        if decisive:
            latest = max(decisive, key=lambda r: (str(r.get("submitted_at") or ""), r["id"]))
            if latest["state"] == "APPROVED":
                return {"pull": int(number), "review": latest["id"]}
    return None


def _definition(api, source, producer, root):
    """Return (path, accepted definition, expected steps, acceptance).

    Distinct failures: ``producer_definition_unaccepted`` (changed, no owner acceptance),
    ``unsupported_producer_definition`` (steps not recognizable).
    """
    path = ".github/workflows/" + producer + ".yml"
    trusted = (root / path).read_text()
    candidate = api.file(path, source["head_sha"])
    if candidate == trusted:
        return path, trusted, _steps(trusted, producer), None
    acceptance = _owner_acceptance(api, source)
    if acceptance is None:
        raise ValueError("producer_definition_unaccepted")
    return path, candidate, _steps(candidate, producer), acceptance


def _markdown_filtered(text, event):
    match = re.search(r"(?m)^  " + re.escape(event) + r":\s*(?:#.*)?$", text)
    if not match:
        return False
    body = re.split(r"(?m)^(?:  [A-Za-z_]|[^\s#])", text[match.end():], maxsplit=1)[0]
    return bool(re.search(r"(?m)^\s+paths-ignore:\s*\[\s*['\"]\*\*\.md['\"]\s*\]\s*$", body))


def _candidate_run(row, source, api, producer, head=None):
    return (row.get("name") == producer
            and row.get("path") == ".github/workflows/" + producer + ".yml"
            and row.get("head_sha") == (head or source["head_sha"])
            and row.get("head_repository", {}).get("full_name") == api.repository
            and row.get("event") in {"push", "pull_request"}
            and row.get("conclusion") != "skipped")


def _checkout_refs(logs):
    lines = list(_emitted_lines(logs))
    refs = set()
    for index, line in enumerate(lines[:-1]):
        if re.search(r"\[command\].*\bgit log -1 --format=%H\s*$", line):
            candidate = lines[index + 1].strip()
            if _SHA.fullmatch(candidate):
                refs.add(candidate)
    return refs


def _workflow_ref(api, source, run, path, trusted, logs):
    """Check the producer blob where GitHub obtained PR workflow code too."""
    head = run["head_sha"]
    if api.file(path, head) != trusted:
        raise ValueError("producer_definition_mismatch")
    if run["event"] != "pull_request":
        return head
    pulls = [row for row in run.get("pull_requests", []) if isinstance(row, dict)
             and row.get("number") in source.get("pulls", [])
             and row.get("head", {}).get("sha") == head
             and row.get("base", {}).get("sha") == source["base_sha"]]
    if not pulls:
        raise ValueError("producer_merge_identity_unavailable")
    for number in (row["number"] for row in pulls):
        row = api.api("/pulls/" + str(int(number)))
        if not isinstance(row, dict):
            raise ValueError("producer_merge_identity_unavailable")
        merge = row.get("merge_commit_sha")
        if (row.get("state") == "open" and row.get("head", {}).get("sha") == head
                and row.get("base", {}).get("sha") == source["base_sha"]
                and isinstance(merge, str) and _SHA.fullmatch(merge)):
            if _checkout_refs(logs) != {merge}:
                raise ValueError("producer_merge_identity_unavailable")
            if api.file(path, merge) != trusted:
                raise ValueError("producer_definition_mismatch")
            return merge
    raise ValueError("producer_merge_identity_unavailable")


def _markdown_reuse(api, source, trusted):
    """Find the latest ancestor push verdict preserved by the explicit filter."""
    # No inferred filter: this is the actual supported trusted producer policy.
    filters = re.findall(r"(?m)^\s+paths-ignore:\s*\[\s*['\"]\*\*\.md['\"]\s*\]\s*$", trusted)
    if not filters:
        return None, []
    suffix = "/actions/workflows/rust.yml/runs?branch=" + quote(source["branch"], safe="") + "&per_page=" + str(MAX_HISTORY)
    inventory = api.api(suffix)
    rows = inventory.get("workflow_runs", [])
    limits = ["execution_history_truncated"] if inventory.get("total_count", len(rows)) > MAX_HISTORY else []
    for run in rows[:MAX_HISTORY]:
        old = run.get("head_sha")
        if (not isinstance(old, str) or not _SHA.fullmatch(old) or old == source["head_sha"]
                or run.get("head_branch") != source["branch"] or run.get("event") != "push"
                or not _candidate_run(run, source, api, "rust", old)):
            continue
        comparison = api.api("/compare/" + old + "..." + source["head_sha"])
        files = comparison.get("files")
        if (comparison.get("status") not in {"ahead", "identical"}
                or comparison.get("merge_base_commit", {}).get("sha") != old
                or not isinstance(files, list) or len(files) >= MAX_COMPARE_FILES
                or comparison.get("total_files", len(files)) != len(files)):
            continue
        if all(isinstance(row.get("filename"), str) and row["filename"].endswith(".md")
               and (not row.get("previous_filename") or row["previous_filename"].endswith(".md"))
               for row in files):
            return run, limits
    return None, limits


def _emitted_lines(logs):
    for line in logs.splitlines():
        line = re.sub(r"^\d{4}-\d\d-\d\dT\S+\s+", "", line).strip()
        line = re.sub(r"\x1b\[[0-?]*[ -/]*[@-~]", "", line).strip()
        # Actions prints the shell script before running it. That printed source
        # is not an emitted skipped-scan signal.
        if line.startswith(("if ", "elif ", "echo ", "#", "for ", "run:", "rc=")):
            continue
        yield line


def _trusted_scan(api, source, root):
    """Return (check rows, limits, waiting) for the trusted leak scan of a pull request candidate.

    The latest `scan` check from GitHub Actions on the candidate head is evidence only when its
    summary names a completed leak-scan workflow_run in this repository whose definition and scripts,
    at the commit it ran, equal the trusted ones. A pass counts as executed only when the check
    reports a clean scan; a failure only when it reports a match. Anything else did not scan.
    Binding a check to the run that created it is not possible through the Checks API, so a
    same-app check that copies a genuine run's summary is not detected here.
    """
    head = source["head_sha"]
    try:
        listing = api.api("/commits/" + head + "/check-runs?check_name=" + SCAN_CHECK + "&per_page=100")
        if not isinstance(listing, dict):
            raise ValueError("execution_inventory_unavailable")
    except (ValueError, KeyError, OSError, urllib.error.URLError):
        return [], ["required_execution_unavailable"], True
    runs = [row for row in listing.get("check_runs", [])
            if isinstance(row, dict) and (row.get("app") or {}).get("id") == ACTIONS_APP]
    if not runs:
        return [], ["required_execution_unavailable"], True
    check = max(runs, key=lambda row: (str(row.get("started_at") or ""), row.get("id") or 0))
    if check.get("status") != "completed":
        return [], [], True
    summary = str((check.get("output") or {}).get("summary") or "")
    link = re.match(r"leak-scan run (\d{1,20})\n", summary)
    trusted = (root / SCAN_PATH).read_text()
    try:
        if not link:
            raise ValueError("leak_scan_producer_unverified")
        run = api.api("/actions/runs/" + link[1])
        if (not isinstance(run, dict) or run.get("path") != SCAN_PATH or run.get("event") != "workflow_run"
                or (run.get("head_repository") or {}).get("full_name") != api.repository
                or run.get("status") != "completed" or not _SHA.fullmatch(str(run.get("head_sha") or ""))):
            raise ValueError("leak_scan_producer_unverified")
        ref = run["head_sha"]
        if api.file(SCAN_PATH, ref) != trusted or any(
                api.file(path, ref) != (root / path).read_text() for path in SCAN_SCRIPTS):
            raise ValueError("leak_scan_producer_unverified")
    except (ValueError, KeyError, OSError, UnicodeError, urllib.error.URLError):
        return ([_row("leak-scan:trusted-producer", source_sha=head, reason="leak_scan_producer_unverified",
                      check_run_id=check.get("id"))],
                ["producer_definition_unverified", "leak_scan_producer_unverified"], False)
    conclusion = check.get("conclusion")
    executed = ((conclusion == "success" and SCAN_CLEAN in summary)
                or (conclusion == "failure" and SCAN_MATCH in summary))
    status = "pass" if conclusion == "success" else "fail" if conclusion == "failure" else "skipped"
    return ([_row(SCAN_TITLE, status, executed, head, run_id=run["id"], check_run_id=check.get("id"),
                  attempt=run.get("run_attempt", 1), workflow_ref=ref,
                  workflow_sha256=hashlib.sha256(trusted.encode("utf-8")).hexdigest(), equivalent=False,
                  producer_acceptance=None)], [], False)


def _excerpt(logs):
    decisive = [line[:300] for line in _emitted_lines(logs) if any(word in line for word in (
        "test result:", "FAILED", "panicked at", "SIGABRT", "memory allocation of", "Ran ",
        "nothing was scanned", "skipping", "PRIVATE_MARKERS"))]
    raw = "\n".join(decisive).encode("utf-8")
    return raw[:1400].decode("utf-8", errors="ignore") + ("\n[excerpt truncated]" if len(raw) > 1400 else "")


def execution_checks(api, source, markdown_only, observations=None, trusted_root=None):
    """Return authenticated checks, public-safe limitations and incomplete status.

    ``markdown_only`` describes a complete cumulative comparison. For a mixed PR,
    a newer Markdown-only push can reuse an older successful push producer after
    proving that all intervening filenames match the trusted workflow's filter.
    """
    root = Path(trusted_root) if trusted_root is not None else Path(__file__).resolve().parents[2]
    inventory = api.api("/actions/runs?head_sha=" + source["head_sha"] + "&per_page=" + str(MAX_RUNS))
    runs = inventory.get("workflow_runs", [])
    limits = ["execution_inventory_truncated"] if inventory.get("total_count", len(runs)) > MAX_RUNS else []
    checks, waiting = [], bool(limits)
    rust_text = (root / ".github/workflows/rust.yml").read_text()
    rust_pr_required = bool(source.get("pulls")) and not _markdown_filtered(rust_text, "pull_request")
    producers = ["drift-input"] + (["rust"] if not markdown_only or rust_pr_required else [])
    for producer in producers:
        try:
            path, trusted, expected, acceptance = _definition(api, source, producer, root)
        except (ValueError, KeyError, OSError, UnicodeError, urllib.error.URLError) as error:
            reason = str(error) if str(error) in _DEFINITION_REASONS else "producer_definition_unavailable"
            checks.append(_definition_failure(producer + ":trusted-definition", source["head_sha"], reason))
            limits += ["producer_definition_unverified", reason]
            continue
        if acceptance is not None:
            limits.append("producer_definition_owner_accepted_" + producer.replace("-", "_"))
        candidates = [r for r in runs if _candidate_run(r, source, api, producer)
                      and (r.get("event") == "pull_request"
                           if producer == "rust" and rust_pr_required else True)]
        equivalent = False
        if candidates:
            run = max(candidates, key=lambda r: (r.get("run_number", 0), r.get("run_attempt", 1)))
        elif producer == "rust" and not rust_pr_required:
            try:
                run, history_limits = _markdown_reuse(api, source, trusted)
                limits += history_limits
                equivalent = run is not None
            except (ValueError, KeyError, OSError, urllib.error.URLError):
                run = None
                limits.append("execution_history_unavailable")
        else:
            run = None
        if run is None:
            limits.append("required_execution_unavailable")
            waiting = True
            continue
        if run.get("status") != "completed":
            waiting = True
            continue
        jobs_inventory = api.api("/actions/runs/" + str(int(run["id"])) + "/jobs?per_page=" + str(MAX_JOBS))
        jobs = jobs_inventory.get("jobs", [])
        if jobs_inventory.get("total_count", len(jobs)) > MAX_JOBS:
            limits.append("execution_jobs_truncated")
            waiting = True
        # The jobs endpoint can include synthetic Checks API entries with no
        # runner steps or log endpoint. Only jobs containing expected steps from
        # the authenticated producer definition can supply required witnesses.
        job_logs, log_bytes = {}, 0
        for job in jobs:
            if (job.get("conclusion") == "skipped"
                    or not any(step.get("name") in expected.values() for step in job.get("steps", []))):
                continue
            try:
                job_id = job["id"]
                if job_id in job_logs:
                    continue
                log = api.log(job_id)
                if not isinstance(log, str) or not log.strip():
                    raise ValueError("execution_log_unavailable")
                size = len(log.encode("utf-8")) + bool(job_logs)
                if log_bytes + size > MAX_LOG_BYTES:
                    raise ValueError("execution_log_bound")
                job_logs[job_id] = log
                log_bytes += size
            except (ValueError, KeyError, OSError, urllib.error.URLError):
                limits.append("execution_log_unavailable")
                waiting = True
        logs = "\n".join(job_logs.values())
        try:
            workflow_ref = _workflow_ref(api, source, run, path, trusted, logs)
        except (ValueError, KeyError, OSError, UnicodeError, urllib.error.URLError) as error:
            if str(error) == "producer_merge_identity_unavailable":
                # The run happened; its merge commit cannot be identified (for example the PR now
                # conflicts with its base). Remedy: update the branch so a current merge run exists.
                checks.append(_row(producer + ":merge-identity", source_sha=source["head_sha"],
                                   reason="producer_merge_identity_unavailable"))
                limits.append("producer_merge_identity_unavailable")
            else:
                reason = ("producer_definition_unaccepted" if str(error) == "producer_definition_mismatch"
                          else "producer_definition_unavailable")
                checks.append(_definition_failure(producer + ":trusted-definition", source["head_sha"], reason))
                limits += ["producer_definition_unverified", reason]
            continue
        if observations is not None and logs:
            excerpt = _excerpt(logs)
            if excerpt:
                observations.append({"id": "execution-" + producer,
                    "excerpts": [excerpt], "questions": ["witness", "proof"]})
        for kind, title in expected.items():
            actual = [(job, step) for job in jobs for step in job.get("steps", []) if step.get("name") == title]
            status = actual[0][1].get("conclusion") if len(actual) == 1 else None
            job_id = actual[0][0]["id"] if len(actual) == 1 else None
            witness = job_logs.get(job_id, "")
            ran = status in {"success", "failure"} and bool(witness.strip())
            if status == "success" and ran:
                if kind == "test":
                    ran = any(int(n) > 0 for n in re.findall(r"test result: ok\. (\d+) passed", witness))
                elif kind == "unittest":
                    ran = any(int(n) > 0 for n in re.findall(r"Ran (\d+) tests?", witness))
            checks.append(_row(title, "pass" if status == "success" else "fail" if status == "failure" else "skipped",
                ran, run["head_sha"], equivalent=equivalent, run_id=run["id"], job_id=job_id, attempt=run.get("run_attempt", 1),
                workflow_ref=workflow_ref, workflow_sha256=hashlib.sha256(trusted.encode("utf-8")).hexdigest(),
                producer_acceptance=acceptance))
    if source.get("pulls"):
        rows, scan_limits, scan_waiting = _trusted_scan(api, source, root)
        checks += rows
        limits += scan_limits
        waiting = waiting or scan_waiting
    return checks, sorted(set(limits)), waiting
