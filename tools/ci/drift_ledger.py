"""Resolve owner questions to their recorded answers, work and proposals.

The ledger is versioned data kept outside this repository. It links each
question to the answers that settle it; the questions and decisions stay in
their own documents. This module only reads the ledger and those documents:
it never decides anything, never treats an agent proposal as an answer and never
picks between answers by date. A later answer replaces an earlier one only when
it names it in ``supersedes``.

Every entry carries a revision ``rev`` that advances by one on each change, so a
writer who read an older revision cannot overwrite a newer one even when the
visible value looks the same. Answers are never edited: a change of direction is a
new answer that names the old one in ``supersedes``. An owner answer recorded by
someone else carries the owner's words (``quote``) or says why only a
transcription survives (``quote_unavailable``).

Command line:

    python3 -I drift_ledger.py view --root DIR --ledger PATH [--scan PATH ...]
    python3 -I drift_ledger.py update --base FILE --current FILE --proposed FILE --out FILE

``update`` applies a writer's changes (``proposed`` against the ``base`` they read)
onto the ``current`` published ledger. Unrelated changes by others are kept, a
change to an entry someone else changed since ``base`` is refused as stale, and
re-applying changes that are already published is a no-op.
"""
from __future__ import annotations

import argparse
import datetime
import glob
import json
import os
import re
import sys
import tempfile
import tomllib

AUTHORITIES = frozenset({"owner", "requirement"})
OUTCOMES = frozenset({"decided", "deferred", "declined", "required"})
VERSION = 2
KINDS = (("question", "questions"), ("answer", "answers"), ("work", "work"), ("proposal", "proposals"))
# Work leaves "open" by completion (with evidence), withdrawal (with a reason) or replacement (naming
# its successor); no state is a dead end and none needs history to be deleted.
WORK_STATES = frozenset({"open", "done", "withdrawn", "superseded"})
PROPOSAL_STATES = frozenset({"open", "withdrawn", "adopted"})
MAX_ITEMS = 500
MAX_TEXT = 2000
# An owner quotation is kept whole however long the message was; readers page it instead of cutting it.
MAX_QUOTE = 200_000
APPLIES = frozenset({"", "always"})
ID = re.compile(r"[A-Za-z0-9][A-Za-z0-9._-]{0,63}\Z")
# A line that treats a question as still waiting for the owner.
OPEN_WORDS = re.compile(
    r"\b(open|unanswered|outstanding|awaiting|pending|undecided|answer by number|decision needed|"
    r"needs? (?:your|the owner'?s?|an owner) (?:decision|answer|approval)|blocked on (?:the )?owner)\b", re.I)
# A line that reports a resolution is not a claim that the question is open.
RESOLVED_WORDS = re.compile(r"\b(decided|resolved|answered|superseded|withdrawn|done|declined|deferred)\b", re.I)
QUESTION_REF = re.compile(r"\bQ(\d{1,4})\b")
ASKED_HEADING = re.compile(r"#{2,3}\s+Q?(\d{1,4})[.:]\s")
# A section marked as the original question kept for history is not a current status claim.
HISTORY_MARK = re.compile(r"original question as asked", re.I)
# Only an explicit resolution marker at the start of a line settles a whole section; an incidental
# word such as "done" inside the question text does not.
SECTION_RESOLVED = re.compile(r"^\s*(?:>\s*)?(?:\*\*)?(?:decided|resolved|answered|superseded|withdrawn)\b",
                              re.I | re.M)
ASKING = re.compile(r"\*\*(Options|Recommendation|Ask)\b|^(Options|Ask):", re.M)


class LedgerError(ValueError):
    pass


def _text(row, key, required=True, limit=MAX_TEXT):
    value = row.get(key, "")
    if not isinstance(value, str) or len(value) > limit or (required and not value.strip()):
        raise LedgerError("invalid_field:" + key)
    return value


def _ids(row, key):
    value = row.get(key, [])
    if not isinstance(value, list) or any(not isinstance(v, str) or not ID.fullmatch(v) for v in value):
        raise LedgerError("invalid_field:" + key)
    return value


def _terms(row, key):
    value = row.get(key, [])
    if not isinstance(value, list) or any(not isinstance(v, str) or not v.strip() or len(v) > 200 for v in value):
        raise LedgerError("invalid_field:" + key)
    return [v.strip() for v in value]


def load(text):
    """Parse and validate the ledger. Structural defects raise LedgerError."""
    try:
        data = tomllib.loads(text)
    except tomllib.TOMLDecodeError as error:
        raise LedgerError("invalid_toml") from error
    if data.get("version") != VERSION:
        raise LedgerError("unsupported_version")
    model = {"questions": {}, "answers": {}, "work": {}, "proposals": {}}
    for kind, key in KINDS:
        rows = data.get(kind, [])
        if not isinstance(rows, list) or len(rows) > MAX_ITEMS:
            raise LedgerError("invalid_section:" + kind)
        for row in rows:
            if not isinstance(row, dict) or not isinstance(row.get("id"), str) or not ID.fullmatch(row["id"]):
                raise LedgerError("invalid_identity:" + kind)
            if any(row["id"] in model[other] for other in model):
                raise LedgerError("duplicate_identity:" + row["id"])
            rev = row.get("rev")
            if type(rev) is not int or not 1 <= rev <= 1_000_000:
                raise LedgerError("invalid_revision:" + row["id"])
            model[key][row["id"]] = row
    for q in model["questions"].values():
        _text(q, "title")
        _text(q, "asked")
        _text(q, "affects", required=False)
        _text(q, "withdrawn", required=False)
        # A short name for lists that only name the question (the title stays the full wording).
        _text(q, "label", required=False, limit=40)
        # "always": a standing direction that applies to every scope (included in every scoped brief).
        if _text(q, "applies", required=False) not in APPLIES:
            raise LedgerError("invalid_field:applies")
    for a in model["answers"].values():
        if a.get("authority") not in AUTHORITIES or a.get("outcome") not in OUTCOMES:
            raise LedgerError("invalid_answer:" + a["id"])
        if a["outcome"] == "required" and a["authority"] != "requirement":
            raise LedgerError("invalid_answer:" + a["id"])
        for key in ("given", "source", "summary", "recorded_by"):
            _text(a, key)
        quote = _text(a, "quote", required=False, limit=MAX_QUOTE)
        unavailable = _text(a, "quote_unavailable", required=False)
        if a["authority"] == "owner" and bool(quote.strip()) == bool(unavailable.strip()):
            # The owner's own words, or an explicit statement that only a transcription survives.
            raise LedgerError("owner_words_missing:" + a["id"])
        _terms(a, "forbids")
        if not _ids(a, "questions"):
            raise LedgerError("answer_without_question:" + a["id"])
        for qid in _ids(a, "questions"):
            if qid not in model["questions"]:
                raise LedgerError("unknown_question:" + qid)
        for old in _ids(a, "supersedes"):
            if old not in model["answers"] or old == a["id"]:
                raise LedgerError("unknown_superseded_answer:" + old)
            if not set(model["answers"][old]["questions"]) & set(a["questions"]):
                raise LedgerError("supersession_across_questions:" + a["id"])
    _acyclic(model["answers"])
    for w in model["work"].values():
        if w.get("answer") not in model["answers"] or w.get("state") not in WORK_STATES:
            raise LedgerError("invalid_work:" + w["id"])
        for key in ("summary", "owner"):
            _text(w, key)
        _text(w, "evidence", required=w["state"] == "done")
        _text(w, "reason", required=w["state"] == "withdrawn")
        successor = w.get("superseded_by")
        if w["state"] == "superseded" or successor is not None:
            if w["state"] != "superseded" or successor not in model["work"] or successor == w["id"]:
                raise LedgerError("invalid_work_successor:" + w["id"])
    for p in model["proposals"].values():
        if p.get("state") not in PROPOSAL_STATES:
            raise LedgerError("invalid_proposal:" + p["id"])
        _text(p, "by")
        _text(p, "summary")
        _terms(p, "restricts")
        for qid in _ids(p, "questions"):
            if qid not in model["questions"]:
                raise LedgerError("unknown_question:" + qid)
        _ids(p, "adopted_by")
    return model


def _acyclic(answers):
    state = {}

    def visit(aid):
        if state.get(aid) == 1:
            raise LedgerError("supersession_cycle:" + aid)
        if state.get(aid) == 2:
            return
        state[aid] = 1
        for old in answers[aid].get("supersedes", []):
            visit(old)
        state[aid] = 2

    for aid in answers:
        visit(aid)


def resolve(model):
    """Return the current view and its consistency findings.

    Findings are structural facts about the records; they never settle a question.
    """
    answers, findings = model["answers"], []
    superseded = {old for a in answers.values() for old in a.get("supersedes", [])}
    current = {qid: [a for a in answers.values() if qid in a["questions"] and a["id"] not in superseded]
               for qid in model["questions"]}
    view = {"decisions": [], "required": [], "work": [], "done": [], "closed": [], "proposals": [],
            "missing": [], "historical": [], "conflicts": []}
    for qid, question in model["questions"].items():
        live = current[qid]
        if question.get("withdrawn", "").strip() and not live:
            # Withdrawn by its asker or made moot; never an owner answer.
            view["historical"].append({"answer": "-", "questions": [qid], "summary": question["title"],
                                       "superseded_by": ["withdrawn: " + question["withdrawn"]]})
            continue
        if len(live) > 1:
            # Two answers without an explicit supersession are not ordered by date.
            findings.append({"code": "conflicting_answers_without_supersession", "question": qid,
                             "answers": sorted(a["id"] for a in live)})
            view["conflicts"].append({"question": qid, "answers": sorted(a["id"] for a in live)})
            continue
        if not live:
            affects = question.get("affects", "").strip()
            if not affects:
                findings.append({"code": "missing_decision_without_operation", "question": qid})
            view["missing"].append({"question": qid, "title": question["title"], "affects": affects})
            continue
        answer = live[0]
        row = {"question": qid, "title": question["title"], "answer": answer["id"],
               "authority": answer["authority"], "outcome": answer["outcome"], "summary": answer["summary"],
               "given": answer["given"], "source": answer["source"], "forbids": answer.get("forbids", []),
               "recorded_by": answer["recorded_by"], "quoted": bool(answer.get("quote", "").strip()),
               "supersedes": answer.get("supersedes", [])}
        (view["required"] if answer["authority"] == "requirement" else view["decisions"]).append(row)
    for aid in sorted(superseded):
        a = answers[aid]
        by = sorted(b["id"] for b in answers.values() if aid in b.get("supersedes", []))
        view["historical"].append({"answer": aid, "questions": a["questions"], "summary": a["summary"],
                                   "superseded_by": by})
    for w in model["work"].values():
        if w["state"] == "open" and w["answer"] in superseded:
            # Open work under a replaced answer: finish, withdraw or supersede it explicitly.
            findings.append({"code": "work_under_superseded_answer", "work": w["id"], "answer": w["answer"]})
        row = {"work": w["id"], "answer": w["answer"], "summary": w["summary"], "owner": w["owner"],
               "evidence": w.get("evidence", ""), "state": w["state"],
               "why": w.get("reason", "") or w.get("superseded_by", "")}
        {"open": view["work"], "done": view["done"]}.get(w["state"], view["closed"]).append(row)
    for p in model["proposals"].values():
        if p["state"] == "withdrawn":
            continue
        adopted_by = [aid for aid in p.get("adopted_by", []) if aid in answers and aid not in superseded]
        if p["state"] == "adopted" and not adopted_by:
            # Only a recorded owner or requirement answer can adopt a proposal.
            findings.append({"code": "proposal_adopted_without_answer", "proposal": p["id"]})
        uncovered = [q for q in p.get("questions", [])
                     if not any(q in answers[aid]["questions"] for aid in adopted_by)]
        if p["state"] == "adopted" and adopted_by and uncovered:
            findings.append({"code": "proposal_adopted_without_answer", "proposal": p["id"],
                             "questions": uncovered})
        if p["state"] == "adopted" and adopted_by and not uncovered:
            continue
        conflicts = []
        for qid in p.get("questions", []):
            for a in current.get(qid, []):
                if a["outcome"] == "deferred":
                    hit = [t for t in p.get("restricts", []) for f in a.get("forbids", [])
                           if t.lower() in f.lower() or f.lower() in t.lower()]
                    if hit:
                        conflicts.append(a["id"])
                        findings.append({"code": "proposal_restricts_deferred_decision", "proposal": p["id"],
                                         "answer": a["id"], "terms": sorted(set(hit))})
        view["proposals"].append({"proposal": p["id"], "questions": p.get("questions", []), "by": p["by"],
                                  "summary": p["summary"], "conflicts_with": sorted(set(conflicts))})
    return view, findings


def scan(model, view, documents):
    """Find document lines that present a settled question as still waiting for the owner.

    ``documents`` maps a path to its text. Returns findings with path and line.
    """
    settled = {row["question"]: row["answer"] for row in view["decisions"] + view["required"]}
    numbers = {}
    for qid in model["questions"]:
        match = re.fullmatch(r"Q(\d{1,4})", qid)
        if match:
            numbers[int(match[1])] = qid
    findings = []
    for path in sorted(documents):
        lines = documents[path].splitlines()
        # A numbered question section that still asks for an answer (options, a recommendation or an
        # "ask") and records no resolution presents the question as open.
        heads = [(i, int(m[1])) for i, line in enumerate(lines)
                 if (m := ASKED_HEADING.match(line)) and int(m[1]) in numbers]
        for k, (start, n) in enumerate(heads):
            end = heads[k + 1][0] if k + 1 < len(heads) else len(lines)
            body = "\n".join(lines[start:end])
            qid = numbers[n]
            if (qid in settled and ASKING.search(body) and not SECTION_RESOLVED.search(body)
                    and not HISTORY_MARK.search(body)):
                findings.append({"code": "settled_question_presented_open", "question": qid,
                                 "answer": settled[qid], "path": path, "line": start + 1})
        for number, line in enumerate(lines, 1):
            if ASKED_HEADING.match(line):
                continue
            refs = [m for m in QUESTION_REF.finditer(line) if int(m[1]) in numbers]
            if not refs:
                continue
            prefix_open = bool(OPEN_WORDS.search(line[:refs[0].start()]))
            for k, m in enumerate(refs):
                # Each reference is judged on its own clause: from it to the next reference.
                clause = line[m.start():refs[k + 1].start() if k + 1 < len(refs) else len(line)]
                if RESOLVED_WORDS.search(clause) or not (OPEN_WORDS.search(clause) or prefix_open):
                    continue
                qid = numbers[int(m[1])]
                if qid in settled:
                    findings.append({"code": "settled_question_presented_open", "question": qid,
                                     "answer": settled[qid], "path": path, "line": number})
    return findings


def render(view, findings, provenance):
    out = ["# Owner decisions and remaining work (generated)", "",
           provenance,
           "Direct instructions from the owner in the current session take precedence over this view "
           "immediately, even before they are recorded. Work and proposal entries are agent records, "
           "not owner decisions.", ""]

    def section(title, rows, fmt):
        out.append("## " + title + " (" + str(len(rows)) + ")")
        out.extend("- " + fmt(row) for row in rows) if rows else out.append("- none")
        out.append("")

    section("Owner decisions", view["decisions"], lambda r: "{q} {o} by {a} ({g}): {s} [{src}; recorded by {rb}{t}]{f}".format(
        q=r["question"], o=r["outcome"], a=r["answer"], g=r["given"], s=r["summary"], src=r["source"],
        rb=r["recorded_by"], t="" if r["quoted"] else "; transcription only",
        f=(" Must not become: " + "; ".join(r["forbids"])) if r["forbids"] else ""))
    section("Required by the owner's requirements (implement; no approval needed)", view["required"],
            lambda r: "{q} {a}: {s} [{src}]".format(q=r["question"], a=r["answer"], s=r["summary"], src=r["source"]))
    section("Authorized work still to implement or verify", view["work"],
            lambda r: "{w} ({a}, {o}): {s}".format(w=r["work"], a=r["answer"], o=r["owner"], s=r["summary"]))
    section("Agent proposals (not decisions)", view["proposals"], lambda r: "{p} by {b} on {q}: {s}{c}".format(
        p=r["proposal"], b=r["by"], q=", ".join(r["questions"]) or "-", s=r["summary"],
        c=(" CONFLICTS with deferred " + ", ".join(r["conflicts_with"]) + ": needs an owner decision")
        if r["conflicts_with"] else ""))
    section("Missing owner decisions", view["missing"], lambda r: "{q}: {t}. Affects: {a}".format(
        q=r["question"], t=r["title"], a=r["affects"] or "NOT STATED (record the exact operation)"))
    section("Conflicting answers (no supersession recorded; do not pick by date)", view["conflicts"],
            lambda r: "{q}: {a}".format(q=r["question"], a=", ".join(r["answers"])))
    section("Historical / superseded", view["historical"], lambda r: "{a} on {q} superseded by {b}: {s}".format(
        a=r["answer"], q=", ".join(r["questions"]), b=", ".join(r["superseded_by"]), s=r["summary"]))
    section("Completed work", view["done"], lambda r: "{w} ({a}): {s} [{e}]".format(
        w=r["work"], a=r["answer"], s=r["summary"], e=r["evidence"]))
    section("Closed work (withdrawn or superseded)", view["closed"], lambda r: "{w} ({a}) {st}: {s} [{y}]".format(
        w=r["work"], a=r["answer"], st=r["state"], s=r["summary"], y=r["why"]))
    section("Record findings", findings, lambda r: ", ".join(k + "=" + (",".join(v) if isinstance(v, list) else str(v))
                                                            for k, v in r.items()))
    return "\n".join(out)


def _entries(model):
    """Map each identity to (kind, row), in file order within each kind."""
    return {row["id"]: (kind, row) for kind, key in KINDS for row in model[key].values()}


KINDS_NAMES = frozenset(kind for kind, _ in KINDS)
BARE_KEY = re.compile(r"[A-Za-z0-9_-]+\Z")
TRIPLE = ('"' * 3, "'" * 3)


def _same(a, b):
    """Value equality as TOML means it: types must match and nan equals nan."""
    if isinstance(a, float) and isinstance(b, float) and a != a and b != b:
        return True
    if isinstance(a, dict) and isinstance(b, dict):
        return a.keys() == b.keys() and all(_same(a[k], b[k]) for k in a)
    if isinstance(a, (list, tuple)) and isinstance(b, (list, tuple)):
        return len(a) == len(b) and all(_same(x, y) for x, y in zip(a, b))
    return type(a) is type(b) and a == b


def _string(value):
    # A JSON string is a TOML basic string, except that TOML also forbids a raw DEL character.
    return json.dumps(value, ensure_ascii=False).replace("\x7f", "\\u007f")


def _key(key):
    return key if BARE_KEY.fullmatch(key) else _string(key)


def _value(value):
    """Write any value tomllib can return so that reading it back gives the identical value."""
    if isinstance(value, bool):
        return "true" if value else "false"
    if isinstance(value, int):
        return str(value)
    if isinstance(value, float):
        if value != value:
            return "nan"
        if value in (float("inf"), float("-inf")):
            return "inf" if value > 0 else "-inf"
        return repr(value)
    if isinstance(value, str):
        return _string(value)
    if isinstance(value, (datetime.datetime, datetime.date, datetime.time)):
        return value.isoformat()
    if isinstance(value, list):
        return "[" + ", ".join(_value(v) for v in value) + "]"
    if isinstance(value, dict):
        if not value:
            return "{}"
        return "{ " + ", ".join(_key(k) + " = " + _value(v) for k, v in value.items()) + " }"
    raise LedgerError("unserializable_value")


def _scan_line(line, quote):
    """Scan one line outside of strings.

    Returns (quote still open at the end of the line, True if a comment follows content on the
    line, net change in bracket depth from [ ] { } outside strings).
    """
    i, depth = 0, 0
    while i < len(line):
        if quote is None:
            if line.startswith(TRIPLE, i):
                quote = line[i:i + 3]
                i += 3
                continue
            if line[i] in "\"'":
                quote = line[i]
            elif line[i] == "#":
                return None, True, depth
            elif line[i] in "[{":
                depth += 1
            elif line[i] in "]}":
                depth -= 1
        elif quote in TRIPLE:
            if quote == TRIPLE[0] and line[i] == "\\":
                i += 2
                continue
            if line.startswith(quote, i):
                quote = None
                i += 3
                continue
        else:
            if quote == '"' and line[i] == "\\":
                i += 2
                continue
            if line[i] == quote:
                quote = None
        i += 1
    return (quote if quote in TRIPLE else None), False, depth


def _comments(text):
    """Full-line comments by anchor, and whether any comment follows content on the same line.

    Anchors: ("header",) for comments before the version line; ("top", key) for comments before
    or inside a top-level key or before a [table] line; ("top", table, key) inside a table;
    (entry id,) for comments just before an entry's [[kind]] line; (entry id, key) for comments
    before or inside an entry's key; ("trailer",) after the last content. Lines that continue a
    multi-line string or array are never read as keys or table headers.
    """
    anchors, pending, inline, entries, current = {}, [], False, [], None
    open_quote, depth, field, table = None, 0, None, None
    name = lambda key: next(iter(tomllib.loads(key + " = 0")))
    for line in text.splitlines():
        stripped = line.strip()
        inside = open_quote is not None or depth > 0
        if open_quote is None and stripped.startswith("#"):
            pending.append(line)
            continue
        if open_quote is None and not stripped:
            continue
        open_quote, has_comment, delta = _scan_line(line, open_quote)
        inline = inline or has_comment
        if inside:
            # A comment inside a multi-line value is kept with the field that holds it.
            if pending:
                field.extend(pending)
                pending = []
            depth += delta
            continue
        depth += delta
        if stripped.startswith("[["):
            current = {"comments": pending, "keys": {}}
            entries.append(current)
            field = []
        elif stripped.startswith("["):
            current = None
            table = next(iter(tomllib.loads(stripped)))
            field = anchors.setdefault(("top", table), [])
            field.extend(pending)
        else:
            key = stripped.split("=", 1)[0].strip()
            if current is None:
                anchor = (("header",) if table is None and name(key) == "version" else
                          ("top", name(key)) if table is None else ("top", table, name(key)))
                field = anchors.setdefault(anchor, [])
                field.extend(pending)
            else:
                field = current["keys"].setdefault(key, [])
                field.extend(pending)
                if key == "id":
                    try:
                        current["id"] = tomllib.loads(stripped)["id"]
                    except (tomllib.TOMLDecodeError, KeyError):
                        pass
        pending = []
    for entry in entries:
        if "id" not in entry:
            continue
        if entry["comments"]:
            anchors[(entry["id"],)] = entry["comments"]
        for key, block in entry["keys"].items():
            if block:
                anchors[(entry["id"], name(key))] = block
    if pending:
        anchors[("trailer",)] = pending
    return {anchor: block for anchor, block in anchors.items() if block}, inline


def _extras(text):
    """Top-level data outside the version and the entry arrays (accepted by the reader)."""
    return {k: v for k, v in tomllib.loads(text).items() if k != "version" and k not in KINDS_NAMES}


def serialize(entries, header="", extras=None, comments=None):
    """Write a ledger; reading it back gives the same entries, top-level data and full-line comments.

    ``header`` is the leading comment block for callers that pass only that; a ("header",) anchor
    in ``comments`` takes its place.
    """
    comments = dict(comments or {})
    head = comments.pop(("header",), None)
    out = list(head) if head is not None else ([header.rstrip("\n")] if header.strip() else [])
    out.append("version = " + str(VERSION))
    extras = extras or {}
    for k, v in extras.items():
        if not isinstance(v, dict):
            out.extend(comments.get(("top", k), []))
            out.append(_key(k) + " = " + _value(v))
    for name, table in extras.items():
        if isinstance(table, dict):
            out.append("")
            out.extend(comments.get(("top", name), []))
            out.append("[" + _key(name) + "]")
            for k, v in table.items():
                out.extend(comments.get(("top", name, k), []))
                out.append(_key(k) + " = " + _value(v))
    for kind, _ in KINDS:
        for k, row in entries:
            if k != kind:
                continue
            out.append("")
            out.extend(comments.get((row["id"],), []))
            out.append("[[" + kind + "]]")
            for key, val in row.items():
                out.extend(comments.get((row["id"], key), []))
                out.append(_key(key) + " = " + _value(val))
    trailer = comments.get(("trailer",))
    if trailer:
        out += [""] + trailer
    return "\n".join(out) + "\n"


def _header(text):
    lines = []
    for line in text.splitlines():
        if not line.startswith("#"):
            break
        lines.append(line)
    return "\n".join(lines)


def _three_way(base, current, proposed):
    """Merge one keyed collection the way entries merge. Returns (merged, conflicting keys)."""
    merged, conflicts = dict(current), []
    for key in list(base) + [k for k in proposed if k not in base]:
        b, p, c = base.get(key), proposed.get(key), current.get(key)
        if _same(b, p):
            continue
        if _same(c, b) or _same(c, p):
            if p is None:
                merged.pop(key, None)
            else:
                merged[key] = p
        else:
            conflicts.append(key)
    return merged, conflicts


def _mismatch():
    return None, [{"code": "serialization_mismatch", "id": "-",
                   "remedy": "nothing was written; report this ledger content as a writer defect"}]


def update(base_text, current_text, proposed_text):
    """Apply a writer's changes onto the current ledger.

    ``base`` is the ledger the writer read, ``proposed`` the writer's edited copy of it and
    ``current`` the latest published ledger. Returns (merged text, conflicts). With conflicts the
    merged text is None and nothing may be published; each conflict names the entry and remedy.
    The comparison is by entry identity and full content, including ``rev``, so an entry that
    changed and changed back (A -> B -> A) still counts as changed. Every value the reader accepts
    is written back unchanged, as are top-level data outside the entries and full-line comments.
    The written text is read back and must match exactly; otherwise nothing is returned.
    """
    base, current, proposed = (_entries(load(t)) for t in (base_text, current_text, proposed_text))
    conflicts = []
    changed = {}
    for eid in list(base) + [e for e in proposed if e not in base]:
        b, p = base.get(eid), proposed.get(eid)
        if _same(b, p):
            continue
        c = current.get(eid)
        if p is None:
            conflicts.append({"code": "entry_removed", "id": eid,
                              "remedy": "keep the entry; close it with a state or supersede it"})
            continue
        if b is None:
            if p[1]["rev"] != 1:
                conflicts.append({"code": "new_entry_revision", "id": eid, "remedy": "a new entry starts at rev 1"})
            elif c is not None and not _same(c, p):
                conflicts.append({"code": "identity_reused", "id": eid,
                                  "remedy": "the identity is already published with other content; use a new one"})
            else:
                changed[eid] = p
            continue
        if b[0] != p[0]:
            conflicts.append({"code": "kind_changed", "id": eid, "remedy": "record a new entry"})
        elif b[0] == "answer":
            conflicts.append({"code": "answer_modified", "id": eid,
                              "remedy": "record a new answer that names this one in supersedes"})
        elif p[1]["rev"] != b[1]["rev"] + 1:
            conflicts.append({"code": "revision_not_advanced", "id": eid,
                              "remedy": "set rev to " + str(b[1]["rev"] + 1)})
        elif _same(c, p):
            changed[eid] = p
        elif not _same(c, b):
            conflicts.append({"code": "stale_revision", "id": eid, "expected_rev": b[1]["rev"],
                              "found": "removed" if c is None else "rev " + str(c[1]["rev"]),
                              "remedy": "re-read the current ledger and reapply this change"})
        else:
            changed[eid] = p
    extras, extra_conflicts = _three_way(*(_extras(t) for t in (base_text, current_text, proposed_text)))
    conflicts += [{"code": "top_level_conflict", "id": key,
                   "remedy": "re-read the current ledger and reapply this change"} for key in extra_conflicts]
    (base_notes, _), (current_notes, _), (proposed_notes, inline) = (
        _comments(t) for t in (base_text, current_text, proposed_text))
    if inline:
        conflicts.append({"code": "inline_comment_not_kept", "id": "-",
                          "remedy": "move the comment onto its own line above the value"})
    notes, note_conflicts = _three_way(base_notes, current_notes, proposed_notes)
    conflicts += [{"code": "comment_conflict", "id": ":".join(map(str, key)),
                   "remedy": "re-read the current ledger and reapply the comment change"} for key in note_conflicts]
    if conflicts:
        return None, conflicts
    merged = [changed.get(eid, entry) for eid, entry in current.items()]
    merged += [changed[eid] for eid in changed if eid not in current]
    try:
        text = serialize(merged, extras=extras, comments=notes)
        back = load(text)
    except LedgerError:
        return _mismatch()
    written = _entries(back)
    if (set(written) != {row["id"] for _, row in merged}
            or any(not _same(written[row["id"]], (kind, row)) for kind, row in merged)
            or not _same(_extras(text), extras) or _comments(text)[0] != notes):
        return _mismatch()
    try:
        resolve(back)
    except LedgerError as error:
        return None, [{"code": "merged_ledger_invalid", "id": str(error),
                       "remedy": "re-read the current ledger; a change you rely on was altered"}]
    return text, []


def _write_atomic(path, text):
    """Replace ``path`` with ``text`` in one step: readers see the old file or the new one, never a part."""
    directory = os.path.dirname(os.path.abspath(path))
    fd, temporary = tempfile.mkstemp(dir=directory, prefix=".ledger-", suffix=".tmp")
    try:
        with os.fdopen(fd, "w", encoding="utf-8") as stream:
            stream.write(text)
            stream.flush()
            os.fsync(stream.fileno())
        os.replace(temporary, path)
    except BaseException:
        if os.path.exists(temporary):
            os.unlink(temporary)
        raise


def _inside(root, path):
    full = os.path.realpath(os.path.join(root, path))
    return full if full.startswith(os.path.realpath(root) + os.sep) else None


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    sub = parser.add_subparsers(dest="command", required=True)
    view_cmd = sub.add_parser("view")
    view_cmd.add_argument("--root", required=True)
    view_cmd.add_argument("--ledger", required=True)
    view_cmd.add_argument("--scan", action="append", default=[])
    view_cmd.add_argument("--provenance", default="")
    update_cmd = sub.add_parser("update")
    for name in ("--base", "--current", "--proposed", "--out"):
        update_cmd.add_argument(name, required=True)
    args = parser.parse_args(argv)
    if args.command == "update":
        return _update_command(args)
    ledger = _inside(args.root, args.ledger)
    if not ledger:
        parser.error("ledger must be inside --root")
    try:
        model = load(open(ledger, encoding="utf-8").read())
    except (OSError, LedgerError) as error:
        print("Owner decision ledger unavailable or invalid: " + str(error) +
              ". Do not treat any question as open or settled from other files; ask the owner.")
        return 2
    documents = {}
    for pattern in args.scan:
        for match in sorted(glob.glob(os.path.join(args.root, pattern))):
            full = _inside(args.root, os.path.relpath(match, args.root))
            if full and os.path.isfile(full):
                documents[os.path.relpath(full, os.path.realpath(args.root))] = open(full, encoding="utf-8").read()
    view, findings = resolve(model)
    findings += scan(model, view, documents)
    print(render(view, findings, args.provenance or "Source: " + args.ledger))
    return 0


def _update_command(args):
    """Exit 0: merged ledger written ("updated", or "unchanged" when already published); 3: conflicts; 2: invalid input."""
    try:
        texts = [open(path, encoding="utf-8").read() for path in (args.base, args.current, args.proposed)]
        merged, conflicts = update(*texts)
    except (OSError, LedgerError) as error:
        print("invalid ledger input: " + str(error))
        return 2
    if conflicts:
        for c in conflicts:
            print("conflict: " + ", ".join(k + "=" + str(v) for k, v in c.items()))
        return 3
    _write_atomic(args.out, merged)
    # A retry after a lost publication response finds its changes already present.
    print("unchanged" if tomllib.loads(merged) == tomllib.loads(texts[1]) else "updated")
    return 0


if __name__ == "__main__":
    sys.exit(main())
