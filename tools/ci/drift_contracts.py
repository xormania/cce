"""Deterministic checks over normalized, candidate-scoped evidence.

Collection owns authenticity, applicability and any demonstrated source equivalence.
This module never executes evidence or treats a model answer as a contract verdict.
Only semantic packets contain supplied text; all other output is public-safe enums
and opaque correlation identifiers. Invalid input raises a content-free ValueError.
"""
from __future__ import annotations

import hashlib
import re


MAX_ROWS = 256
MAX_EXCERPTS = 8
MAX_EXCERPT_BYTES = 1600
MAX_ID_BYTES = 4096
QUESTION_KINDS = frozenset({"proof", "guarantee", "witness", "deferral", "resolution"})
VIOLATION_CODES = frozenset({
    "required_check_not_executed", "required_check_failed", "evidence_candidate_mismatch",
    "claim_case_mismatch", "semantic_disposition_conflict", "semantic_catch_not_demonstrated",
})
LIMITATION_CODES = frozenset({"no_evidence", "unscoped_claim", "unscoped_mutation", "missing_evidence_identity", "incomplete_case_inventory"})
_SHA = re.compile(r"[0-9a-f]{40}\Z")
_REPOSITORY = re.compile(r"[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+\Z")
_COMPLETE = frozenset({"green", "complete", "qualified"})
_CLAIM_STATES = _COMPLETE | {"planned", "red", "partial", "deferred", "implemented"}
_CHECK_STATES = frozenset({"pass", "fail", "skipped", "blocked", "cancelled", "not_exercised", "unknown"})
_DISPOSITIONS = frozenset({"caught", "equivalent", "defence-in-depth", "superseded", "planned", "missed", "unviable", "error"})


def _invalid() -> None:
    raise ValueError("Invalid normalized evidence")


def _text(value, limit=MAX_ID_BYTES) -> str:
    if not isinstance(value, str) or not value:
        _invalid()
    try:
        if len(value.encode()) > limit:
            _invalid()
    except UnicodeError:
        _invalid()
    return value


def _boolean(row: dict, key: str, default=False) -> bool:
    value = row.get(key, default)
    if type(value) is not bool:
        _invalid()
    return value


def _rows(value) -> list:
    if not isinstance(value, list) or len(value) > MAX_ROWS or any(not isinstance(row, dict) for row in value):
        _invalid()
    return value


def _subject(kind: str, identity: str) -> str:
    return hashlib.sha256((kind + "\0" + identity).encode()).hexdigest()[:24]


def _excerpts(row: dict) -> list[dict]:
    excerpts = row.get("excerpts", [])
    if not isinstance(excerpts, list) or len(excerpts) > MAX_EXCERPTS:
        _invalid()
    return [{"id": f"e{i+1}", "text": _text(value, MAX_EXCERPT_BYTES)} for i, value in enumerate(excerpts)]


def _identity_matches(row: dict, key: str, head: str, limitations: set[str]) -> bool:
    value = row.get(key)
    equivalent = _boolean(row, "equivalent")
    if value is None:
        limitations.add("missing_evidence_identity")
        return True  # Missing identity is incomplete evidence, not an invented mismatch.
    if not isinstance(value, str) or not _SHA.fullmatch(value):
        _invalid()
    return value == head or equivalent


def evaluate(bundle: dict) -> dict:
    """Return violations, bounded semantic packets and explicit coverage limitations.

    ``equivalent`` must be established by trusted collection against the current
    candidate's effective inputs. A claimed equivalence from an evidence file or
    a provider answer must not be forwarded as a trusted boolean.
    """
    if not isinstance(bundle, dict) or not isinstance(bundle.get("source"), dict):
        _invalid()
    source = bundle["source"]
    head = source.get("head_sha")
    if not isinstance(head, str) or not _SHA.fullmatch(head):
        _invalid()
    if not isinstance(source.get("repository"), str) or not _REPOSITORY.fullmatch(source["repository"]):
        _invalid()
    if source.get("base_sha") is not None and (not isinstance(source["base_sha"], str) or not _SHA.fullmatch(source["base_sha"])):
        _invalid()

    checks = _rows(bundle.get("checks", []))
    claims = _rows(bundle.get("claims", []))
    mutations = _rows(bundle.get("mutations", []))
    semantic = _rows(bundle.get("semantic", []))
    for rows in (checks, claims, mutations, semantic):
        identities = [_text(row.get("id")) for row in rows]
        if len(set(identities)) != len(identities):
            _invalid()
    violations: set[tuple[str, str]] = set()
    packets = []
    limitations: set[str] = set()
    if not any((checks, claims, mutations, semantic)):
        limitations.add("no_evidence")

    def fail(code: str, subject: str) -> None:
        violations.add((code, subject))

    def packet(row: dict, subject: str, questions: list[str]) -> None:
        excerpts = _excerpts(row)
        if excerpts:
            packets.append({"subject": subject, "excerpts": excerpts, "questions": questions})

    for row in checks:
        subject = _subject("check", _text(row.get("id")))
        status = row.get("status")
        if not isinstance(status, str) or status not in _CHECK_STATES:
            _invalid()
        executed = _boolean(row, "executed")
        matches = _identity_matches(row, "source_sha", head, limitations)
        if not _boolean(row, "required"):
            continue
        if not executed or status in {"skipped", "blocked", "cancelled", "not_exercised", "unknown"}:
            fail("required_check_not_executed", subject)
        elif status == "fail":
            fail("required_check_failed", subject)
        if not matches:
            fail("evidence_candidate_mismatch", subject)

    for row in claims:
        subject = _subject("claim", _text(row.get("id")))
        status = row.get("status")
        if not isinstance(status, str) or status not in _CLAIM_STATES:
            _invalid()
        states = row.get("case_statuses", [])
        if not isinstance(states, list) or len(states) > MAX_ROWS or any(not isinstance(s, str) or s not in _CLAIM_STATES for s in states):
            _invalid()
        matches = _identity_matches(row, "evidence_sha", head, limitations)
        applicable = _boolean(row, "applicable")
        if not applicable:
            limitations.add("unscoped_claim")
        elif status in _COMPLETE:
            if not states:
                limitations.add("incomplete_case_inventory")
            elif any(state not in _COMPLETE for state in states):
                fail("claim_case_mismatch", subject)
            if not matches:
                fail("evidence_candidate_mismatch", subject)
        packet(row, subject, ["proof", "guarantee", "resolution"])

    for row in mutations:
        subject = _subject("mutation", _text(row.get("id")))
        disposition = row.get("disposition")
        if not isinstance(disposition, str) or disposition not in _DISPOSITIONS:
            _invalid()
        runs = _rows(row.get("runs", []))
        valid_catch = False
        for run in runs:
            executed = _boolean(run, "executed")
            assertion = _boolean(run, "failed_by_assertion")
            aborted = _boolean(run, "aborted")
            exit_code = run.get("exit")
            if type(exit_code) is not int:
                _invalid()
            valid_catch |= executed and assertion and not aborted and exit_code not in {0, -6, 134}
        applicable = _boolean(row, "applicable")
        claimed_semantic = _boolean(row, "claimed_semantic")
        if not applicable:
            limitations.add("unscoped_mutation")
        elif claimed_semantic:
            if disposition != "caught":
                fail("semantic_disposition_conflict", subject)
            elif not valid_catch:
                fail("semantic_catch_not_demonstrated", subject)
        if claimed_semantic:
            packet(row, subject, ["witness", "proof", "resolution"])

    for row in semantic:
        subject = _subject("semantic", _text(row.get("id")))
        questions = row.get("questions", ["proof", "guarantee"])
        if (not isinstance(questions, list) or not questions or len(questions) > len(QUESTION_KINDS)
                or any(not isinstance(q, str) or q not in QUESTION_KINDS for q in questions)
                or len(set(questions)) != len(questions)):
            _invalid()
        packet(row, subject, questions)

    return {
        "violations": [{"code": code, "subject": subject} for code, subject in sorted(violations)],
        "packets": packets,
        "limitations": sorted(limitations),
    }
