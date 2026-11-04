"""Resolve owner questions to their recorded answers, work and proposals.

The ledger is versioned data kept outside this repository. It links each
question to the answers that settle it; the questions and decisions stay in
their own documents. This module only reads the ledger and those documents:
it never decides anything, never treats an agent proposal as an answer and never
picks between answers by date. A later answer replaces an earlier one only when
it names it in ``supersedes``.

Command line (prints the current view):

    python3 -I drift_ledger.py view --root DIR --ledger PATH [--scan PATH ...]
"""
from __future__ import annotations

import argparse
import glob
import os
import re
import sys
import tomllib

AUTHORITIES = frozenset({"owner", "requirement"})
OUTCOMES = frozenset({"decided", "deferred", "declined", "required"})
WORK_STATES = frozenset({"open", "done"})
PROPOSAL_STATES = frozenset({"open", "withdrawn", "adopted"})
MAX_ITEMS = 500
MAX_TEXT = 2000
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


def _text(row, key, required=True):
    value = row.get(key, "")
    if not isinstance(value, str) or len(value) > MAX_TEXT or (required and not value.strip()):
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
    if data.get("version") != 1:
        raise LedgerError("unsupported_version")
    model = {"questions": {}, "answers": {}, "work": {}, "proposals": {}}
    for kind, key in (("question", "questions"), ("answer", "answers"), ("work", "work"), ("proposal", "proposals")):
        rows = data.get(kind, [])
        if not isinstance(rows, list) or len(rows) > MAX_ITEMS:
            raise LedgerError("invalid_section:" + kind)
        for row in rows:
            if not isinstance(row, dict) or not isinstance(row.get("id"), str) or not ID.fullmatch(row["id"]):
                raise LedgerError("invalid_identity:" + kind)
            if any(row["id"] in model[other] for other in model):
                raise LedgerError("duplicate_identity:" + row["id"])
            model[key][row["id"]] = row
    for q in model["questions"].values():
        _text(q, "title")
        _text(q, "asked")
        _text(q, "affects", required=False)
        _text(q, "withdrawn", required=False)
    for a in model["answers"].values():
        if a.get("authority") not in AUTHORITIES or a.get("outcome") not in OUTCOMES:
            raise LedgerError("invalid_answer:" + a["id"])
        if a["outcome"] == "required" and a["authority"] != "requirement":
            raise LedgerError("invalid_answer:" + a["id"])
        for key in ("given", "source", "summary"):
            _text(a, key)
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
    view = {"decisions": [], "required": [], "work": [], "done": [], "proposals": [],
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
               "supersedes": answer.get("supersedes", [])}
        (view["required"] if answer["authority"] == "requirement" else view["decisions"]).append(row)
    for aid in sorted(superseded):
        a = answers[aid]
        by = sorted(b["id"] for b in answers.values() if aid in b.get("supersedes", []))
        view["historical"].append({"answer": aid, "questions": a["questions"], "summary": a["summary"],
                                   "superseded_by": by})
    for w in model["work"].values():
        if w["answer"] in superseded:
            findings.append({"code": "work_under_superseded_answer", "work": w["id"], "answer": w["answer"]})
        (view["work"] if w["state"] == "open" else view["done"]).append(
            {"work": w["id"], "answer": w["answer"], "summary": w["summary"], "owner": w["owner"],
             "evidence": w.get("evidence", "")})
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

    section("Owner decisions", view["decisions"], lambda r: "{q} {o} by {a} ({g}): {s} [{src}]{f}".format(
        q=r["question"], o=r["outcome"], a=r["answer"], g=r["given"], s=r["summary"], src=r["source"],
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
    section("Record findings", findings, lambda r: ", ".join(k + "=" + (",".join(v) if isinstance(v, list) else str(v))
                                                            for k, v in r.items()))
    return "\n".join(out)


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
    args = parser.parse_args(argv)
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


if __name__ == "__main__":
    sys.exit(main())
