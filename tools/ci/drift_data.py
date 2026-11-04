"""Read pinned versioned evidence and join it to candidate proof as data only."""
from __future__ import annotations

import hashlib
import json
from pathlib import PurePosixPath
import re
import tomllib
import urllib.error

SHA = re.compile(r"[a-f0-9]{40}\Z")
REPOSITORY = re.compile(r"[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+\Z")
STATUSES = frozenset({"green", "qualified", "complete", "implemented", "partial", "deferred", "planned", "red"})
DISPOSITIONS = frozenset({"caught", "equivalent", "defence-in-depth", "superseded", "planned", "missed", "unviable", "error"})
MAX_RECORDS = 8
MAX_LINKS = 8
MAX_CASES = 1000
MAX_STATUS_GROUPS = 40
MAX_MUTATIONS = 12
MAX_MUTATION_LOGS = 8
MAX_BYTES = 1_000_000
MAX_DOCUMENTS = 24
MAX_DECISION_PACKETS = 12


def _path(value):
    return (isinstance(value, str) and re.fullmatch(r"[A-Za-z0-9_. /-]{1,299}", value)
            and not value.startswith("/") and all(part not in {"", ".", ".."} for part in value.split("/")))


def _canonical(value):
    return json.dumps(value, sort_keys=True, ensure_ascii=False, separators=(",", ":"), allow_nan=False).encode()


def config_key(env):
    """Identify configured selectors without exposing repository names or paths."""
    try:
        repository = env.get("DRIFT_DATA_REPOSITORY", "")
        files = json.loads(env.get("DRIFT_DATA_FILES", ""))
        if not REPOSITORY.fullmatch(repository) or not isinstance(files, dict):
            return None
        return hashlib.sha256(_canonical({"repository": repository, "files": files})).hexdigest()
    except (ValueError, TypeError, UnicodeError):
        return None


def _excerpt(text, needles=(), limit=1500):
    """Keep decisive lines before applying a byte limit, including omission flags."""
    if not isinstance(text, str):
        raise ValueError("invalid_data")
    lines = text.splitlines()
    wanted = []
    if needles:
        for index, line in enumerate(lines):
            if any(needle in line for needle in needles):
                wanted.extend(range(max(0, index - 2), min(len(lines), index + 3)))
    if wanted:
        selected = sorted(set(wanted))
        value = "\n".join(("[lines omitted]\n" if pos and index != selected[pos - 1] + 1 else "")
                          + lines[index] for pos, index in enumerate(selected))
    else:
        value = "\n".join(lines)
    raw = value.encode()
    suffix = "\n[excerpt truncated]" if len(raw) > limit else ""
    return raw[:limit - len(suffix.encode())].decode(errors="ignore") + suffix


def _read(api, path, ref):
    if not _path(path):
        raise ValueError("invalid_data_path")
    value = api.file(path, ref)
    if not isinstance(value, str) or len(value.encode()) > MAX_BYTES:
        raise ValueError("invalid_data_file")
    return value


def _links(path, text):
    """Resolve only ordinary repository-relative Markdown links, never URLs."""
    for target in re.findall(r"\[[^\]]*\]\(([^)\s]+)\)", text):
        target = target.split("#", 1)[0]
        if not target or ":" in target or target.startswith("/") or "\\" in target:
            continue
        parts = list(PurePosixPath(path).parent.parts)
        for part in target.split("/"):
            if part == ".":
                continue
            if part == "..":
                if not parts:
                    parts = []
                    break
                parts.pop()
            elif part:
                parts.append(part)
        resolved = "/".join(parts)
        if resolved.endswith((".md", ".toml", ".txt", ".log")) and _path(resolved):
            yield resolved


def _verified_head(summary, source, public_api):
    match = re.search(r"^verified-at:\s*([a-f0-9]{7,40})\s*$", summary, re.M)
    if not match:
        return None
    ref = match[1]
    if SHA.fullmatch(ref):
        return ref
    if public_api is None:
        return None
    resolved = public_api.api("/commits/" + ref).get("sha")
    return resolved if isinstance(resolved, str) and SHA.fullmatch(resolved) and resolved.startswith(ref) else None


def _inventory(mapping):
    cases = mapping.get("cases", [])
    requirements = mapping.get("requirements", [])
    if not isinstance(cases, list) or len(cases) > MAX_CASES or not isinstance(requirements, list):
        raise ValueError("invalid_data_inventory")
    for rows in (cases, requirements):
        ids = []
        for row in rows:
            if not isinstance(row, dict) or not isinstance(row.get("id"), str) or not row["id"]:
                raise ValueError("invalid_data_identity")
            ids.append(row["id"])
        if len(set(ids)) != len(ids):
            raise ValueError("duplicate_data_identity")
    for case in cases:
        if case.get("status") not in STATUSES or not isinstance(case.get("slice"), str):
            raise ValueError("invalid_case")
        tests = case.get("tests", [])
        if not isinstance(tests, list) or len(tests) > 64 or any(not isinstance(test, str) for test in tests):
            raise ValueError("invalid_case_tests")
    case_ids = {case["id"] for case in cases}
    for requirement in requirements:
        acceptance = requirement.get("acceptance", [])
        if not isinstance(acceptance, list) or any(not isinstance(identity, str) for identity in acceptance):
            raise ValueError("invalid_requirement_reference")
        if any(identity not in case_ids for identity in acceptance):
            raise ValueError("missing_requirement_case")
    enriched = []
    for case in cases:
        row = dict(case)
        row["requirements"] = [requirement for requirement in requirements
                               if case["id"] in requirement.get("acceptance", [])]
        enriched.append(row)
    return enriched


def _answer_record(row):
    """The minimized answer fields sent for comparison: no free-form private notes."""
    return json.dumps({key: row.get(key) for key in ("answer", "question", "authority", "outcome", "given",
                                                     "summary", "forbids", "supersedes", "source")
                       if row.get(key) not in (None, [], "")}, ensure_ascii=False)


def decision_packets(api, ref, ledger_path, documents):
    """Join each disputed claim with its applicable answer and any supersession in one packet.

    Deterministic findings come from drift_ledger; the packet asks Jev only whether the claim is
    reconciled (advisory). Returns (semantic rows, limitations). Nothing here can fail a candidate.
    """
    from drift_ledger import LedgerError, load, resolve, scan
    limits = []
    try:
        model = load(_read(api, ledger_path, ref))
    except LedgerError:
        return [], ["decision_ledger_invalid"]
    except (ValueError, KeyError, TypeError, OSError, urllib.error.URLError):
        return [], ["decision_ledger_unavailable"]
    texts = {}
    for path in documents:
        try:
            texts[path] = _read(api, path, ref)
        except (ValueError, KeyError, TypeError, OSError, urllib.error.URLError):
            limits.append("decision_document_unavailable")
    if not documents:
        limits.append("decision_documents_not_configured")
    view, findings = resolve(model)
    findings += scan(model, view, texts)
    if findings:
        limits.append("decision_record_findings")
    rows = {row["question"]: row for row in view["decisions"] + view["required"]}
    historical = {row["answer"]: row for row in view["historical"]}
    packets = []
    for finding in findings:
        if len(packets) >= MAX_DECISION_PACKETS:
            limits.append("decision_packet_inventory_truncated")
            break
        excerpts = []
        if "path" in finding:
            lines = texts[finding["path"]].splitlines()
            at = finding["line"] - 1
            excerpts.append(_excerpt("\n".join(lines[max(0, at - 2):at + 6])))
        question = finding.get("question")
        if question in rows:
            excerpts.append(_excerpt(_answer_record(rows[question])))
            for old in rows[question].get("supersedes", []):
                if old in historical:
                    excerpts.append(_excerpt(json.dumps(historical[old], ensure_ascii=False)))
        for key in ("proposal", "work"):
            if key in finding:
                table = model["proposals" if key == "proposal" else "work"]
                excerpts.append(_excerpt(json.dumps({k: v for k, v in table[finding[key]].items()
                                                     if k in ("id", "questions", "answer", "summary", "state",
                                                              "restricts")}, ensure_ascii=False)))
        if "answer" in finding and not question:
            excerpts.append(_excerpt(_answer_record(dict(model["answers"][finding["answer"]],
                                                         answer=finding["answer"]))))
        excerpts.append(_excerpt(json.dumps(finding, sort_keys=True)))
        packets.append({"id": "decision:" + finding["code"] + ":" + ":".join(
                            str(finding.get(k, "")) for k in ("question", "answer", "proposal", "work", "path", "line")),
                        "excerpts": excerpts[:6], "questions": ["resolution", "deferral"]})
    return packets, limits


def _runs(raw):
    """Normalize fields, keeping aborts distinct from an assertion witness."""
    if not isinstance(raw, list) or len(raw) > 256:
        raise ValueError("invalid_mutation_runs")
    runs = []
    for row in raw:
        if not isinstance(row, dict) or type(row.get("exit")) is not int:
            raise ValueError("invalid_mutation_run")
        for field in ("executed", "failed_by_assertion", "aborted"):
            if field in row and type(row[field]) is not bool:
                raise ValueError("invalid_mutation_flags")
        exit_code = row["exit"]
        runs.append({"exit": exit_code, "executed": row.get("executed", False),
                     "failed_by_assertion": row.get("failed_by_assertion", False),
                     "aborted": row.get("aborted", False) or exit_code in {-6, 134}})
    return runs


def data_evidence(source, env, api_factory, normalized_mutations=None, public_api=None,
                  data_ref=None, metadata=None, subjects=None):
    """Return claims, private relationship rows, limitations and required-input failure.

    ``metadata`` contains only a pinned revision and an opaque configuration key.
    Raw paths, private excerpts and received credentials never enter that projection.
    A caller may pin ``data_ref`` to reconstruct earlier budget-deferred work.
    """
    repository = env.get("DRIFT_DATA_REPOSITORY", "")
    token = env.get("DRIFT_DATA_TOKEN", "")
    paths = env.get("DRIFT_DATA_FILES", "")
    required = env.get("DRIFT_DATA_REQUIRED", "false").lower() == "true"
    if not repository and not paths:
        return [], [], ["data_not_configured", "decision_data_not_configured"], required
    if not token:
        return [], [], ["data_credentials_missing"], required
    limits = []
    try:
        files = json.loads(paths)
        if (not REPOSITORY.fullmatch(repository) or not isinstance(files, dict)
                or not {"requirements", "summary"} <= files.keys()
                or files.keys() - {"requirements", "summary", "records", "ledger", "documents"}
                or not _path(files["requirements"]) or not _path(files["summary"])
                or ("ledger" in files and not _path(files["ledger"]))):
            raise ValueError("invalid_data_configuration")
        documents = files.get("documents", [])
        if (not isinstance(documents, list) or len(documents) > MAX_DOCUMENTS
                or any(not _path(path) for path in documents) or len(set(documents)) != len(documents)):
            raise ValueError("invalid_document_inventory")
        records = files.get("records", [])
        if (not isinstance(records, list) or len(records) > MAX_RECORDS
                or any(not _path(path) for path in records) or len(set(records)) != len(records)):
            raise ValueError("invalid_record_inventory")
        api = api_factory(repository, token)
        ref = data_ref if data_ref is not None else api.api("/commits/HEAD")["sha"]
        if not isinstance(ref, str) or not SHA.fullmatch(ref):
            raise ValueError("invalid_data_revision")
        key = config_key(env)
        if metadata is not None:
            metadata.update({"data_ref": ref, "config_key": key})
        map_text = _read(api, files["requirements"], ref)
        summary = _read(api, files["summary"], ref)
        mapping = tomllib.loads(map_text)
        cases = _inventory(mapping)
        try:
            verified = _verified_head(summary, source, public_api)
        except (ValueError, KeyError, TypeError, OSError, urllib.error.URLError):
            verified = None
            limits.append("data_candidate_resolution_unavailable")
        applicable = verified == source["head_sha"]
        if not applicable:
            limits.append("data_candidate_not_proven")
        context = {files["requirements"]: map_text, files["summary"]: summary}
        for path in records:
            try:
                context[path] = _read(api, path, ref)
            except (ValueError, KeyError, TypeError, OSError, urllib.error.URLError):
                limits.append("data_record_unavailable")
        linked = []
        for path, text in list(context.items()):
            for target in _links(path, text):
                if target not in context and target not in linked:
                    linked.append(target)
        if len(linked) > MAX_LINKS:
            limits.append("data_link_inventory_truncated")
        for path in linked[:MAX_LINKS]:
            try:
                context[path] = _read(api, path, ref)
            except (ValueError, KeyError, TypeError, OSError, urllib.error.URLError):
                limits.append("data_link_unavailable")
        grouped = {}
        for case in cases:
            grouped.setdefault(case["slice"], []).append(case["status"])
        if len(grouped) > MAX_STATUS_GROUPS:
            # Never silently retain only early groups and call that a full check.
            return [], [], sorted(set(limits + ["data_summary_inventory_bound"])), required
        claims, semantic, seen = [], [], set()
        for line in summary.splitlines():
            if not line.startswith("|"):
                continue
            columns = line.split("|")[1:-1]
            if len(columns) < 2:
                continue
            identity = columns[0].strip(" *`")
            if identity not in grouped:
                continue
            if identity in seen:
                raise ValueError("duplicate_summary_identity")
            seen.add(identity)
            statement = columns[1].strip()
            count = re.search(r"(?<!\d)(\d+)\s*/\s*\1\s+green\b", statement)
            complete = bool(count)
            if count and int(count[1]) != len(grouped[identity]):
                limits.append("data_case_total_mismatch")
            claims.append({"id": identity, "applicable": applicable,
                           "status": "complete" if complete else "partial",
                           "evidence_sha": verified if applicable else ref,
                           "case_statuses": grouped[identity], "excerpts": []})
            semantic.append({"id": "status:" + identity,
                             "excerpts": [_excerpt(line), _excerpt(json.dumps([case for case in cases if case["slice"] == identity]))],
                             "questions": ["proof", "deferral", "resolution"]})
        if public_api is not None:
            from drift_relationships import mapped_relationships
            related, more = mapped_relationships(public_api, source, cases, summary, context,
                                                 data_ref=ref, subjects=subjects)
            semantic.extend(related)
            limits.extend(more)
        elif any(case.get("tests") for case in cases):
            limits.append("data_public_proof_unavailable")
        for path in records:
            if path in context:
                if context[path].strip():
                    semantic.append({"id": "record:" + path,
                                     "excerpts": [_excerpt(context[path], ("assert", "FAILED", "panicked", "SIGABRT", "allocation", "disposition"))],
                                     "questions": ["witness", "proof", "resolution"]})
                else:
                    limits.append("data_record_empty")
        mutations = mapping.get("mutations", [])
        if not isinstance(mutations, list) or len(mutations) > 1000:
            raise ValueError("invalid_mutation_inventory")
        mutation_ids = [row.get("id") for row in mutations if isinstance(row, dict)]
        if len(mutation_ids) != len(mutations) or any(not isinstance(identity, str) or not identity for identity in mutation_ids) or len(set(mutation_ids)) != len(mutation_ids):
            raise ValueError("duplicate_mutation_identity")
        current = [row for row in mutations if isinstance(row.get("source_head"), str)
                   and re.fullmatch(r"[a-f0-9]{7,40}", row["source_head"])
                   and source["head_sha"].startswith(row["source_head"])]
        if len(current) > MAX_MUTATIONS:
            limits.append("mutation_inventory_truncated")
        log_reads = 0
        for mutation in current[:MAX_MUTATIONS]:
            evidence = mutation.get("evidence", "")
            if not _path(evidence) or mutation.get("disposition") not in DISPOSITIONS:
                raise ValueError("invalid_mutation_reference")
            try:
                raw = _read(api, evidence + "/result.json", ref)
                result = json.loads(raw)
            except (ValueError, KeyError, TypeError, OSError, urllib.error.URLError):
                limits.append("mutation_evidence_unavailable")
                continue
            if not isinstance(result, dict) or type(result.get("source_dirty")) is not bool:
                raise ValueError("invalid_mutation_receipt")
            if result["source_dirty"] or result.get("source_head") != source["head_sha"]:
                limits.append("mutation_candidate_not_proven")
                continue
            runs = _runs(result.get("runs", []))
            disposition = mutation["disposition"]
            if normalized_mutations is not None:
                normalized_mutations.append({"id": mutation["id"], "applicable": True,
                    "disposition": disposition, "raw_outcome": result.get("outcome"),
                    "claimed_semantic": disposition == "caught", "runs": runs, "excerpts": []})
            excerpts = [_excerpt(json.dumps(mutation)), _excerpt(raw, ("outcome", "failed_by_assertion", "aborted", '"exit"'))]
            for index in range(min(len(runs), 3)):
                if log_reads >= MAX_MUTATION_LOGS:
                    limits.append("mutation_log_inventory_truncated")
                    break
                log_reads += 1
                try:
                    log = _read(api, evidence + "/test-" + str(index) + ".log", ref)
                    if not log.strip():
                        raise ValueError("empty_mutation_log")
                    excerpts.append(_excerpt(log, ("assert", "FAILED", "panicked", "SIGABRT", "allocation", "left:", "right:")))
                except (ValueError, KeyError, TypeError, OSError, urllib.error.URLError):
                    limits.append("mutation_log_unavailable")
            semantic.append({"id": "mutation:" + mutation["id"],
                             "excerpts": excerpts,
                             "questions": ["witness", "proof", "resolution"]})
        if "ledger" in files:
            decided, more = decision_packets(api, ref, files["ledger"], documents)
            semantic.extend(decided)
            limits.extend(more)
        else:
            limits.append("decision_data_not_configured")
        for row in semantic:
            row.pop("public_path", None)
            row.pop("public_line", None)
            row["private"] = True
            row["reconstruction"] = {"kind": "data", "data_ref": ref, "config_key": key}
        incomplete = any(code in limits for code in (
            "data_record_unavailable", "data_record_empty", "data_link_unavailable", "data_link_inventory_truncated",
            "mutation_inventory_truncated", "mutation_evidence_unavailable", "mutation_candidate_not_proven",
            "mutation_log_inventory_truncated", "mutation_log_unavailable", "data_case_total_mismatch"))
        return claims, semantic, sorted(set(limits)), required and (not applicable or incomplete)
    except (ValueError, KeyError, TypeError, UnicodeError, OSError, urllib.error.URLError):
        return [], [], ["data_source_unavailable"], required
