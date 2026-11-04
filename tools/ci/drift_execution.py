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
_KINDS = {
    "drift-input": {"unittest": r"python3?\s+-m\s+unittest\s+discover\b"},
    "rust": {"format": r"cargo\s+fmt\b", "clippy": r"cargo\s+clippy\b",
             "test": r"cargo\s+test\b"},
    "leak-guard": {"scan": r"git\s+diff\b"},
}


def _row(identity, status="unknown", executed=False, source_sha=None, **metadata):
    return {"id": identity, "required": True, "executed": executed,
            "status": status, "source_sha": source_sha, **metadata}


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


def _definition(api, source, producer, root):
    path = ".github/workflows/" + producer + ".yml"
    trusted = (root / path).read_text()
    if api.file(path, source["head_sha"]) != trusted:
        raise ValueError("producer_definition_mismatch")
    return path, trusted, _steps(trusted, producer)


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


def _scan_skipped(logs):
    return any(re.search(r"(?:PRIVATE_MARKERS.*(?:skipping|not available|not set|invalid)|nothing was scanned)", line, re.I)
               for line in _emitted_lines(logs))


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
    if source.get("pulls"):
        producers.append("leak-guard")
    for producer in producers:
        try:
            path, trusted, expected = _definition(api, source, producer, root)
        except (ValueError, KeyError, OSError, UnicodeError, urllib.error.URLError):
            checks.append(_row(producer + ":trusted-definition", source_sha=source["head_sha"]))
            limits.append("producer_definition_unverified")
            continue
        candidates = [r for r in runs if _candidate_run(r, source, api, producer)
                      and (r.get("event") == "pull_request"
                           if producer == "leak-guard" or producer == "rust" and rust_pr_required else True)]
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
        logs, job_logs, log_available = "", {}, True
        try:
            job_logs = {job["id"]: api.log(job["id"]) for job in jobs if job.get("conclusion") != "skipped"}
            values = list(job_logs.values())
            if any(not isinstance(log, str) or len(log.encode("utf-8")) > MAX_LOG_BYTES for log in values):
                raise ValueError("execution_log_bound")
            logs = "\n".join(values)
            if not logs.strip() or len(logs.encode("utf-8")) > MAX_LOG_BYTES:
                raise ValueError("execution_log_bound")
        except (ValueError, KeyError, OSError, urllib.error.URLError):
            limits.append("execution_log_unavailable")
            waiting = True
            log_available = False
        try:
            workflow_ref = _workflow_ref(api, source, run, path, trusted, logs)
        except (ValueError, KeyError, OSError, UnicodeError, urllib.error.URLError):
            checks.append(_row(producer + ":trusted-definition", source_sha=source["head_sha"]))
            limits.append("producer_definition_unverified")
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
            ran = status in {"success", "failure"} and log_available and bool(witness.strip())
            if status == "success" and log_available:
                if kind == "test":
                    ran = any(int(n) > 0 for n in re.findall(r"test result: ok\. (\d+) passed", witness))
                elif kind == "unittest":
                    ran = any(int(n) > 0 for n in re.findall(r"Ran (\d+) tests?", witness))
                elif kind == "scan":
                    ran = ran and not _scan_skipped(witness)
            checks.append(_row(title, "pass" if status == "success" else "fail" if status == "failure" else "skipped",
                ran, run["head_sha"], equivalent=equivalent, run_id=run["id"], job_id=job_id, attempt=run.get("run_attempt", 1),
                workflow_ref=workflow_ref, workflow_sha256=hashlib.sha256(trusted.encode("utf-8")).hexdigest()))
    return checks, sorted(set(limits)), waiting
