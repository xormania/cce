"""Scoped reading of the decision ledger: a bounded brief, exact record reads and change checks.

Supersession, conflicts and missing decisions come only from ``drift_ledger.load`` and
``drift_ledger.resolve``; this module never decides anything and never writes the ledger.

A *snapshot* is one parsed ledger text, identified by the SHA-256 of its bytes. Every record has a
digest over its exact fields, its resolved status and its links, so a change in status (for example
an answer becoming superseded) is a change even when the entry itself was not edited.

A *scope* is a small derived descriptor of the session's current work::

    {"objective": "...", "work": ["W-a", "W-b"], "origin": "who or what set it"}

Without a scope the brief covers all open work (a broad coordination view) and says so.

The brief includes, in full, the records that govern the scope: current answers to questions marked
``applies = "always"``, every work item in scope with the current answers to its question, records
linked to those, and every missing decision, conflict and finding. Other current records are
*candidates*: they are named (identity and question title), never loaded in full, because loading one
takes a relevance judgment this brief does not make. A name is a locator, not a loaded record, and
its question has not been judged irrelevant. The budget is a ceiling, not a target.
When the governing records alone exceed the budget, the brief names the ones it could not include
and the read that returns them. The whole delivered text, including the caller's header and footer,
is measured in characters and UTF-8 bytes; no record text is ever cut.

``read`` returns the exact stored fields of the requested records (written as TOML, so every value is
the stored value), in pages bound to the snapshot and the request: a cursor from another snapshot or
another request is refused rather than mixed. ``changes`` compares a caller-held checkpoint with the
snapshot and reports added, changed, superseded, unchanged, missing and unknown records separately.

Command line (all output on stdout; inputs are read only inside ``--root``)::

    python3 -I ledger_context.py brief   --root DIR --ledger PATH [--scope FILE] [--max-chars N]
    python3 -I ledger_context.py read    --root DIR --ledger PATH REF... [--cursor C] [--max-chars N]
    python3 -I ledger_context.py changes --root DIR --ledger PATH [--checkpoint FILE] [--scope FILE]

REF is a record identity or ``inventory`` (every record with its class, in pages).
"""
from __future__ import annotations

import argparse
import hashlib
import json
import os
import re
import sys

if "drift_ledger" not in sys.modules:
    # Only when the ledger module is not already provided (an in-memory loader registers it first).
    sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import drift_ledger as ledger  # noqa: E402

# Hook channels commonly cap injected text near 10,000 characters; the default leaves room for framing.
DEFAULT_MAX_CHARS = 9000
DEFAULT_PAGE_CHARS = 8000
ALWAYS = "always"


class ContextError(ValueError):
    """A request that cannot be answered as asked; ``remedy`` says what to do instead."""

    def __init__(self, code, remedy):
        super().__init__(code)
        self.code, self.remedy = code, remedy


def _digest(value):
    text = json.dumps(value, sort_keys=True, ensure_ascii=False, separators=(",", ":"), default=repr)
    return hashlib.sha256(text.encode("utf-8")).hexdigest()


def snapshot(text, findings=()):
    """Parse and resolve one ledger text. ``findings`` adds caller findings (for example scan results)."""
    model = ledger.load(text)
    view, resolved = ledger.resolve(model)
    snap = {"id": hashlib.sha256(text.encode("utf-8")).hexdigest(), "model": model, "view": view,
            "findings": list(resolved) + list(findings), "records": {}}
    snap["records"] = _records(model, view)
    return snap


def _records(model, view):
    answers, questions = model["answers"], model["questions"]
    superseded_by = {}
    for a in answers.values():
        for old in a.get("supersedes", []):
            superseded_by.setdefault(old, []).append(a["id"])
    conflicting = {aid for row in view["conflicts"] for aid in row["answers"]}
    current = {row["answer"] for row in view["decisions"] + view["required"]}
    q_status = {row["question"]: row["outcome"] for row in view["decisions"] + view["required"]}
    q_status.update({row["question"]: "missing" for row in view["missing"]})
    q_status.update({row["question"]: "conflict" for row in view["conflicts"]})
    q_status.update({q: "withdrawn" for row in view["historical"] if row["answer"] == "-" for q in row["questions"]})
    work_by_answer = {}
    for w in model["work"].values():
        work_by_answer.setdefault(w["answer"], []).append(w["id"])
    records = {}

    def add(kind, row, status, authority, links):
        records[row["id"]] = {"id": row["id"], "kind": kind, "rev": row["rev"], "status": status,
                              "authority": authority, "links": links, "row": row,
                              "digest": _digest({"kind": kind, "row": row, "status": status, "links": links})}

    for q in questions.values():
        add("question", q, q_status.get(q["id"], "missing"), "question", {
            "answers": sorted(a["id"] for a in answers.values() if q["id"] in a["questions"]),
            "current": sorted(a["id"] for a in answers.values() if q["id"] in a["questions"]
                              and (a["id"] in current or a["id"] in conflicting)),
            "proposals": sorted(p["id"] for p in model["proposals"].values() if q["id"] in p.get("questions", []))})
    for a in answers.values():
        status = ("superseded" if a["id"] in superseded_by else "conflicting" if a["id"] in conflicting
                  else "current" if a["id"] in current else "inactive")
        authority = "owner direction" if a["authority"] == "owner" else "requirement"
        add("answer", a, status, authority, {
            "questions": list(a["questions"]), "supersedes": list(a.get("supersedes", [])),
            "superseded_by": sorted(superseded_by.get(a["id"], [])), "work": sorted(work_by_answer.get(a["id"], []))})
    for w in model["work"].values():
        add("work", w, w["state"], "agent work record", {
            "answer": w["answer"], "superseded_by": w.get("superseded_by", "")})
    for p in model["proposals"].values():
        add("proposal", p, p["state"], "agent proposal", {
            "questions": list(p.get("questions", [])), "adopted_by": list(p.get("adopted_by", []))})
    return records


def scope_fingerprint(scope):
    return _digest(scope or {})


def _governing(snap, scope):
    """Identities the scope requires (in delivery order) and scope findings."""
    model, records = snap["model"], snap["records"]
    notes = []
    if scope is None:
        work = sorted(r["id"] for r in records.values() if r["kind"] == "work" and r["status"] == "open")
    else:
        work = []
        for wid in scope.get("work", []):
            if records.get(wid, {}).get("kind") == "work":
                work.append(wid)
            else:
                notes.append("scope names " + wid + ", which is not a work record in this snapshot")
    order = []

    def take(rid):
        if rid not in order:
            order.append(rid)

    def governing_answers(qid):
        # The current answer(s) of a question govern; a superseded answer never does.
        return records[qid]["links"]["current"]

    for q in model["questions"].values():
        if q.get("applies") == ALWAYS:
            for aid in governing_answers(q["id"]):
                take(aid)
    # Decisions the owner still has to make, and conflicts, come before agent work records.
    for row in snap["view"]["missing"]:
        take(row["question"])
    for row in snap["view"]["conflicts"]:
        for aid in row["answers"]:
            take(aid)
    seen_work = set()
    queue = list(work)
    while queue:
        wid = queue.pop(0)
        if wid in seen_work or records.get(wid, {}).get("kind") != "work":
            continue
        seen_work.add(wid)
        answer = records[records[wid]["links"]["answer"]]
        for qid in answer["links"]["questions"]:
            for aid in governing_answers(qid):
                take(aid)
        take(wid)
        # A superseded work item brings the work that replaced it.
        if records[wid]["status"] == "superseded" and records[wid]["links"]["superseded_by"]:
            queue.append(records[wid]["links"]["superseded_by"])
    missing = {row["question"] for row in snap["view"]["missing"]}
    for row in snap["view"]["proposals"]:
        if row["conflicts_with"] or set(row["questions"]) & (missing | {
                q for rid in order if records[rid]["kind"] == "answer" for q in records[rid]["links"]["questions"]}):
            take(row["proposal"])
    return order, work, notes


def _candidates(snap, governing):
    """Current records not governing the scope: current answers, open proposals and open work."""
    out = []
    for rid, rec in snap["records"].items():
        if rid in governing:
            continue
        if ((rec["kind"] == "answer" and rec["status"] == "current")
                or (rec["kind"] == "proposal" and rec["status"] == "open")
                or (rec["kind"] == "work" and rec["status"] == "open")):
            out.append(rid)
    rank = {"answer": 0, "proposal": 1, "work": 2}

    def key(rid):
        rec = snap["records"][rid]
        name = rec["links"]["questions"][0] if rec["kind"] == "answer" else rid
        # Natural order: Q2 before Q10.
        return rank[rec["kind"]], [int(t) if t.isdigit() else t for t in re.split(r"(\d+)", name)]

    return sorted(out, key=key)


def _label(rec):
    row = rec["row"]
    if rec["kind"] != "answer":
        return ""
    if row["authority"] != "owner":
        return "requirement"
    return "owner " + row["outcome"] + (", quoted" if row.get("quote", "").strip() else ", paraphrase")


def _full(snap, rid):
    rec, records = snap["records"][rid], snap["records"]
    row = rec["row"]
    if rec["kind"] == "answer":
        forbids = (" Must not become: " + "; ".join(row["forbids"])) if row.get("forbids") else ""
        return "- {id} on {qs} ({label}): {summary}{forbids}".format(
            id=rid, qs=", ".join(rec["links"]["questions"]), label=_label(rec), summary=row["summary"],
            forbids=forbids)
    if rec["kind"] == "work":
        state = row["state"] + (" by " + row["superseded_by"] if row.get("superseded_by") else "")
        return "- {id} ({state}, owner {owner}, under {answer}): {summary}".format(
            id=rid, state=state, owner=row["owner"], answer=row["answer"], summary=row["summary"])
    if rec["kind"] == "proposal":
        view = next((p for p in snap["view"]["proposals"] if p["proposal"] == rid), None)
        conflict = (" CONFLICTS with deferred " + ", ".join(view["conflicts_with"]) + ": needs an owner decision"
                    if view and view["conflicts_with"] else "")
        return "- {id} by {by} on {qs} (agent proposal, not a decision): {summary}{c}".format(
            id=rid, by=row["by"], qs=", ".join(rec["links"]["questions"]) or "-", summary=row["summary"], c=conflict)
    affects = row.get("affects", "").strip() or "NOT STATED (record the exact operation)"
    return "- {id} (question, {status}): {title}. Affects: {affects}".format(
        id=rid, status=rec["status"], title=row["title"], affects=affects)


def _card(snap, rid, titled=True):
    """A name for a record that is not loaded. An answer is named by its question (``read`` of a
    question returns its answers), with the question's short ``label``, else its title when there is
    room. Question identities that read as words (``T-review-cadence``) need neither."""
    rec, records = snap["records"][rid], snap["records"]
    if rec["kind"] != "answer":
        return rid
    names = []
    for qid in rec["links"]["questions"]:
        row = records[qid]["row"]
        word = row.get("label", "").strip() or (row["title"] if titled and not qid.startswith("T-") else "")
        names.append(qid + (" " + word if word else ""))
    return ", ".join(names)


def brief(snap, scope=None, head="", foot="", max_chars=DEFAULT_MAX_CHARS, max_bytes=None, reader="read"):
    """Render the scoped brief. Returns (text, meta); ``text`` includes ``head`` and ``foot``.

    ``meta``: chars, bytes, tokens_estimate (bytes / 4, an estimate only), selection, scope fingerprint,
    governing identities not delivered (``pending``) and candidates delivered only as cards.
    ``reader`` is the command a session runs to read records (shown in locators).
    """
    max_bytes = max_bytes or max_chars
    frame = (head.rstrip("\n") + "\n" if head else "") + (foot.rstrip("\n") + "\n" if foot else "")
    if len(frame) > max_chars or len(frame.encode("utf-8")) > max_bytes:
        raise ContextError("frame_exceeds_budget", "shorten the header and footer or raise the budget")
    governing, work, notes = _governing(snap, scope)
    candidates = _candidates(snap, governing)
    view = snap["view"]
    if scope is None:
        scope_line = "Scope: none set; broad coordination view of all open work (" + str(len(work)) + " items)."
    else:
        scope_line = ("Scope: " + scope.get("objective", "-") + " | work: " + (", ".join(work) or "none")
                      + " | set by: " + scope.get("origin", "not stated"))
    top = ["# Applicable owner decisions and work (generated)", "",
           "Snapshot " + snap["id"][:12] + "; scope " + scope_fingerprint(scope)[:12]
           + "; selection: deterministic (scope links and standing directions).",
           scope_line,
           "Direct instructions from the owner in the current session take precedence over this view "
           "immediately, even before they are recorded. Each line is the recorder's summary: 'quoted' means "
           "the owner's own words are stored with it (" + reader + " ID), 'paraphrase' that only the "
           "recorder's wording survives. Work and proposal entries are agent records, not decisions.", ""]
    top += ["Scope note: " + n for n in notes] + ([""] if notes else [])
    findings = ["- " + ", ".join(k + "=" + (",".join(v) if isinstance(v, list) else str(v)) for k, v in f.items())
                for f in snap["findings"]]
    counts = ("Not shown: {h} superseded or withdrawn, {d} completed work, {c} closed work "
              "(" + reader + " inventory).").format(h=len(view["historical"]), d=len(view["done"]),
                                                     c=len(view["closed"]))

    def size(text):
        return len(text), len(text.encode("utf-8"))

    titled = [True]

    def assemble(full_ids, card_ids, pending, more_cards):
        lines = [head.rstrip("\n")] if head else []
        lines += top
        lines.append("## Governing this scope (" + str(len([r for r in governing if r in full_ids])) + " of "
                     + str(len(governing)) + ")")
        lines += [_full(snap, rid) for rid in governing if rid in full_ids] or ["- none"]
        if pending:
            lines.append("NOT LOADED: " + str(len(pending)) + " governing records did not fit; read them before "
                         "acting on what they govern: " + reader + " " + " ".join(pending))
        lines += ["", "## Findings (" + str(len(findings)) + ")"] + (findings or ["- none"])
        shown = [rid for rid in candidates if rid in full_ids]
        lines += ["", "## Other current records (" + str(len(candidates)) + "; not judged for this scope)"]
        lines += [_full(snap, rid) for rid in shown]
        if card_ids:
            lines.append("- Named only; " + reader + " ID before acting on what it covers: "
                         + "; ".join(_card(snap, rid, titled[0]) for rid in card_ids))
        if more_cards:
            lines.append("- " + str(more_cards) + " more not listed: " + reader + " inventory")
        if not candidates:
            lines.append("- none")
        lines += ["", counts]
        if foot:
            lines.append(foot.rstrip("\n"))
        return "\n".join(lines) + "\n"

    def fits(text):
        chars, nbytes = size(text)
        return chars <= max_chars and nbytes <= max_bytes

    # The longest prefix of the governing records that fits with the rest named as NOT LOADED.
    for k in range(len(governing), -1, -1):
        full, pending = governing[:k], governing[k:]
        if fits(assemble(full, [], pending, len(candidates))):
            break
    cards = []
    if not pending:
        # Candidates are named, never expanded: the budget is a ceiling, not a target. Loading a
        # candidate in full takes a relevance judgment, which this deterministic brief does not make.
        for style in (True, False):
            titled[0], cards = style, []
            for rid in candidates:
                if fits(assemble(full, cards + [rid], [], len(candidates) - len(cards) - 1)):
                    cards.append(rid)
                else:
                    break
            more = len(candidates) - len(cards)
            if not more:
                break
    else:
        more = len(candidates)
    text = assemble(full, cards, pending, more)
    if not fits(text):
        # Even the frame does not fit: name what is missing instead of cutting text, with the
        # identities when they fit and only their count and the inventory read when they do not.
        def overflow(names):
            lines = [head.rstrip("\n")] if head else []
            lines.append("Context too large for this channel: nothing loaded. " + str(len(governing))
                         + " governing records" + (": " + ", ".join(names) if names else "")
                         + ". Read them: " + reader + " inventory")
            if foot:
                lines.append(foot.rstrip("\n"))
            return "\n".join(lines) + "\n"

        text = overflow(governing)
        if not fits(text):
            text = overflow([])
        if not fits(text):
            raise ContextError("frame_exceeds_budget", "shorten the header and footer or raise the budget")
        pending, more, cards = list(governing), len(candidates), []
    chars, nbytes = size(text)
    meta = {"chars": chars, "bytes": nbytes, "tokens_estimate": (nbytes + 3) // 4, "selection": "deterministic",
            "snapshot": snap["id"], "scope": scope_fingerprint(scope), "pending": pending,
            "cards": cards, "unlisted": more, "fits": fits(text)}
    return text, meta


def _toml(kind, row):
    lines = ["[[" + kind + "]]"]
    lines += [ledger._key(k) + " = " + ledger._value(v) for k, v in row.items()]
    return "\n".join(lines)


def _expand(snap, rid):
    """A record and the records needed to read it in context (question with all its answers, work with
    its answer, answer with its question)."""
    records = snap["records"]
    rec = records[rid]
    if rec["kind"] == "question":
        return [rid] + rec["links"]["answers"] + rec["links"]["proposals"]
    if rec["kind"] == "work":
        return [rid, rec["links"]["answer"]]
    if rec["kind"] == "answer":
        return [rid] + rec["links"]["questions"]
    return [rid] + rec["links"]["questions"]


def _inventory_text(snap, scope):
    governing, _, _ = _governing(snap, scope)
    candidates = set(_candidates(snap, governing))
    lines = ["# Inventory: every record in snapshot " + snap["id"][:12] + " (complete: "
             + str(len(snap["records"])) + " records)"]
    for rid in sorted(snap["records"]):
        rec = snap["records"][rid]
        cls = "governing" if rid in governing else "candidate" if rid in candidates else "history"
        lines.append("- {id} {kind} rev {rev} {status} [{cls}] {digest}".format(
            id=rid, kind=rec["kind"], rev=rec["rev"], status=rec["status"], cls=cls, digest=rec["digest"][:12]))
    return "\n".join(lines) + "\n"


def _records_text(snap, refs):
    seen, parts, records = [], [], snap["records"]
    for ref in refs:
        if ref not in records:
            raise ContextError("unknown_record:" + ref, "read inventory for the identities in this snapshot")
        for rid in _expand(snap, ref):
            if rid in seen:
                continue
            seen.append(rid)
            rec = records[rid]
            parts.append("## {id}: {kind}, {authority}, rev {rev}, {status}, digest {digest}".format(
                id=rid, kind=rec["kind"], authority=rec["authority"], rev=rec["rev"], status=rec["status"],
                digest=rec["digest"][:12]))
            links = {k: v for k, v in rec["links"].items() if v}
            if links:
                parts.append("links: " + json.dumps(links, ensure_ascii=False))
            if rec["kind"] == "answer" and rec["row"]["authority"] == "owner":
                parts.append("summary is the recorder's wording; quote is the owner's words as stored"
                             if rec["row"].get("quote", "").strip() else
                             "summary is the recorder's wording; no owner quotation survives (quote_unavailable)")
            parts.append(_toml(rec["kind"], rec["row"]))
            parts.append("")
    return "\n".join(parts)


def read(snap, refs, cursor=None, max_chars=DEFAULT_PAGE_CHARS, scope=None):
    """Exact stored fields of ``refs`` (or ``["inventory"]``), one page at a time.

    Returns {"snapshot", "request", "offset", "total", "text", "next"}; ``next`` is the cursor for the
    following page or None. A cursor from another snapshot or request is refused.
    """
    if max_chars < 200:
        raise ContextError("page_too_small", "use at least 200 characters per page")
    request = _digest({"refs": list(refs), "scope": scope_fingerprint(scope)})[:16]
    text = _inventory_text(snap, scope) if list(refs) == ["inventory"] else _records_text(snap, refs)
    offset = 0
    if cursor is not None:
        parts = cursor.split(":")
        if len(parts) != 3 or not re.fullmatch(r"[0-9]+", parts[2]):
            raise ContextError("invalid_cursor", "read again without a cursor")
        if parts[0] != snap["id"][:16]:
            raise ContextError("snapshot_changed", "the ledger changed since that page; read again without a cursor")
        if parts[1] != request:
            raise ContextError("cursor_for_another_request", "use the cursor with the same references")
        offset = int(parts[2])
        if offset >= len(text):
            raise ContextError("invalid_cursor", "read again without a cursor")
    end = min(len(text), offset + max_chars)
    if end < len(text):
        # Prefer a line boundary; a single longer line is split by characters (never lost).
        newline = text.rfind("\n", offset, end)
        if newline > offset + max_chars // 2:
            end = newline + 1
    return {"snapshot": snap["id"], "request": request, "offset": offset, "total": len(text),
            "text": text[offset:end], "next": (snap["id"][:16] + ":" + request + ":" + str(end)) if end < len(text) else None}


def checkpoint(snap, scope=None):
    return {"snapshot": snap["id"], "scope": scope_fingerprint(scope),
            "records": {rid: rec["digest"] for rid, rec in snap["records"].items()}}


def changes(snap, held=None, scope=None):
    """Compare a caller-held checkpoint with ``snap`` (None: the source could not be read).

    Without a valid checkpoint every record is ``unknown``: never reported as unchanged.
    """
    result = {"state": "ok", "added": [], "changed": [], "superseded": [], "unchanged": [], "missing": [],
              "unknown": [], "unavailable": [], "scope_changed": None, "checkpoint": None}
    known = held.get("records") if isinstance(held, dict) else None
    if snap is None:
        result["state"] = "unavailable"
        result["unavailable"] = sorted(known) if isinstance(known, dict) else []
        return result
    result["checkpoint"] = checkpoint(snap, scope)
    if not isinstance(known, dict) or not all(isinstance(v, str) for v in known.values()):
        result["unknown"] = sorted(snap["records"])
        return result
    result["scope_changed"] = held.get("scope") != scope_fingerprint(scope)
    for rid, rec in sorted(snap["records"].items()):
        if rid not in known:
            result["added"].append(rid)
        elif known[rid] == rec["digest"]:
            result["unchanged"].append(rid)
        else:
            result["changed"].append(rid)
            if rec["status"] == "superseded":
                result["superseded"].append(rid)
    result["missing"] = sorted(set(known) - set(snap["records"]))
    return result


def _read_inside(root, path):
    """Read ``path`` (relative paths are relative to ``root``) through the canonical path that passed
    the root check, so a same-named file elsewhere (for example in the current directory) is never read."""
    full = ledger._inside(root, path)
    if not full:
        raise SystemExit("path outside --root: " + path)
    with open(full, encoding="utf-8") as stream:
        return stream.read()


def _checkpoint_file(root, path):
    """A held checkpoint, or None (every record then reads as unknown) when it is absent or unreadable."""
    if not path:
        return None
    try:
        return json.loads(_read_inside(root, path))
    except (OSError, UnicodeError, ValueError):
        return None


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__.split("\n", 1)[0])
    sub = parser.add_subparsers(dest="command", required=True)
    for name in ("brief", "read", "changes"):
        p = sub.add_parser(name)
        p.add_argument("--root", required=True)
        p.add_argument("--ledger", required=True)
        p.add_argument("--scope")
        if name == "brief":
            p.add_argument("--max-chars", type=int, default=DEFAULT_MAX_CHARS)
        if name == "read":
            p.add_argument("refs", nargs="+")
            p.add_argument("--cursor")
            p.add_argument("--max-chars", type=int, default=DEFAULT_PAGE_CHARS)
        if name == "changes":
            p.add_argument("--checkpoint")
    args = parser.parse_args(argv)
    root = os.path.realpath(args.root)
    scope = json.loads(_read_inside(root, args.scope)) if args.scope else None
    try:
        snap = snapshot(_read_inside(root, args.ledger))
    except (ledger.LedgerError, OSError, UnicodeError) as error:
        if args.command == "changes":
            held = _checkpoint_file(root, args.checkpoint)
            print(json.dumps(changes(None, held, scope), indent=1))
            return 0
        print("ledger did not load: " + str(error), file=sys.stderr)
        return 2
    try:
        if args.command == "brief":
            text, meta = brief(snap, scope, max_chars=args.max_chars)
            sys.stdout.write(text)
            print(json.dumps(meta), file=sys.stderr)
        elif args.command == "read":
            page = read(snap, args.refs, args.cursor, args.max_chars, scope)
            sys.stdout.write(page["text"])
            print("-- page {o}-{e} of {t} characters; snapshot {s}; {n}".format(
                o=page["offset"], e=page["offset"] + len(page["text"]), t=page["total"], s=page["snapshot"][:12],
                n=("next: --cursor " + page["next"]) if page["next"] else "complete"))
        else:
            held = _checkpoint_file(root, args.checkpoint)
            print(json.dumps(changes(snap, held, scope), indent=1))
    except ContextError as error:
        print(error.code + ": " + error.remedy, file=sys.stderr)
        return 2
    return 0


if __name__ == "__main__":
    sys.exit(main())
