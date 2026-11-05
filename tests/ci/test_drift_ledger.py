"""Paired replays: each recorded failure mode is shown failing and its healthy counterpart passing.

Fixtures are synthetic; they reproduce the behaviours, not any private record.
"""
from __future__ import annotations

from pathlib import Path
import subprocess
import sys
import tempfile
import json
import tomllib
import unittest
from unittest import mock

ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT / "tools/ci"))
import drift_ledger as ledger

BASE = """
version = 2
[[question]]
id = "Q1"
rev = 1
title = "Read-only mode"
asked = "questions.md#1"
[[question]]
id = "Q2"
rev = 1
title = "Naming contract"
asked = "questions.md#2"
[[question]]
id = "Q3"
rev = 1
title = "Bulk export"
asked = "questions.md#3"
affects = "the export request interface"
[[question]]
id = "Q4"
rev = 1
title = "Gate identity"
asked = "questions.md#4"
affects = "making the gate a required check"
[[question]]
id = "Q5"
rev = 1
title = "CI cadence"
asked = "questions.md#5"
[[answer]]
id = "D1"
rev = 1
questions = ["Q1"]
authority = "owner"
recorded_by = "agent-r"
quote = "owner words"
outcome = "decided"
given = "day 1"
source = "decisions/one.md #1"
summary = "Accept read-only mode"
[[answer]]
id = "D2"
rev = 1
questions = ["Q2"]
authority = "owner"
recorded_by = "agent-r"
quote = "owner words"
outcome = "deferred"
given = "day 1"
source = "decisions/one.md #2"
summary = "Deferred; compatible repairs proceed"
forbids = ["ban punctuation in names", "shorter name limits"]
[[answer]]
id = "D5"
rev = 1
questions = ["Q5"]
authority = "owner"
recorded_by = "agent-r"
quote = "owner words"
outcome = "decided"
given = "day 1"
source = "decisions/one.md #5"
summary = "Run CI on every push"
"""

REQUIRED = """
[[answer]]
id = "R3"
rev = 1
questions = ["Q3"]
authority = "requirement"
recorded_by = "agent-r"
outcome = "required"
given = "brief"
source = "brief S7"
summary = "Bulk export is required"
[[work]]
id = "W3"
rev = 1
answer = "R3"
summary = "Implement bulk export"
owner = "agent-a"
state = "open"
"""

# The incident shape: a newer copy of the question list, still asking question 1 by number.
OBSOLETE_LIST = """# Open questions
Answer by number.

## 1. Read-only mode

**Options.** (a) accept (b) decline
**Recommendation.** (a)

## 4. Gate identity

**Ask.** Which identity?
"""


def view(text, documents=None):
    model = ledger.load(text)
    current, findings = ledger.resolve(model)
    if documents is not None:
        findings += ledger.scan(model, current, documents)
    return current, findings


def codes(findings):
    return sorted(f["code"] for f in findings)


def questions(rows):
    return sorted(r["question"] for r in rows)


class ReplayTests(unittest.TestCase):
    # 1. An answered question resurfacing as unanswered.
    def test_answered_question_in_a_newer_copied_list_is_flagged_and_stays_decided(self):
        current, findings = view(BASE, {"questions-copy.md": OBSOLETE_LIST})
        self.assertIn("Q1", questions(current["decisions"]))
        self.assertNotIn("Q1", questions(current["missing"]))
        flagged = [f for f in findings if f["code"] == "settled_question_presented_open"]
        self.assertEqual([(f["question"], f["answer"], f["line"]) for f in flagged], [("Q1", "D1", 4)])
        # Q4 has no answer: asking it is correct, not a finding.
        self.assertNotIn("Q4", [f["question"] for f in flagged])

    def test_incidental_resolution_words_in_the_question_text_do_not_hide_it(self):
        text = OBSOLETE_LIST.replace("**Options.**", "Only short runs were done so far.\n**Options.**", 1)
        _, findings = view(BASE, {"q.md": text})
        self.assertEqual([f["question"] for f in findings], ["Q1"])
        marked = text.replace("**Options.**", "**Decided:** see the ledger.\n**Options.**", 1)
        self.assertEqual(view(BASE, {"q.md": marked})[1], [])

    def test_each_reference_is_judged_on_its_own_clause(self):
        _, findings = view(BASE, {"s.md": "Q4 decided; Q1 pending.\nQ1 decided; Q4 pending.\n"})
        self.assertEqual([(f["question"], f["line"]) for f in findings], [("Q1", 1)])

    def test_supersession_must_share_a_question(self):
        text = BASE + '[[answer]]\nid="D9"\nrev=1\nquestions=["Q5"]\nauthority="owner"\nrecorded_by="r"\nquote="q"\noutcome="decided"\ngiven="d"\nsource="s"\nsummary="s"\nsupersedes=["D1"]\n'
        with self.assertRaises(ledger.LedgerError) as raised:
            ledger.load(text)
        self.assertTrue(str(raised.exception).startswith("supersession_across_questions"))

    def test_adoption_must_come_from_an_answer_to_that_question(self):
        text = BASE + '[[proposal]]\nid="P4"\nrev=1\nquestions=["Q4"]\nby="a"\nsummary="s"\nstate="adopted"\nadopted_by=["D1"]\n'
        current, findings = view(text)
        self.assertIn("proposal_adopted_without_answer", codes(findings))
        self.assertIn("Q4", questions(current["missing"]))

    def test_healthy_list_marks_sections_as_history_and_points_at_the_ledger(self):
        healthy = OBSOLETE_LIST.replace("## 1. Read-only mode\n",
            "## 1. Read-only mode\n\n> Original question as asked (history). Current status: ledger `Q1`.\n")
        _, findings = view(BASE, {"questions.md": healthy, "status.md": "Q1 decided (D1); Q4 still open."})
        self.assertEqual(codes(findings), [])

    def test_status_line_claiming_a_settled_question_is_pending_is_flagged(self):
        _, findings = view(BASE, {"status.md": "Still awaiting the owner on Q1 and Q4."})
        self.assertEqual([(f["question"], f["path"], f["line"]) for f in findings], [("Q1", "status.md", 1)])

    # 2. A deferred decision being converted into a permanent restriction.
    def test_proposal_restricting_a_deferred_decision_stays_a_conflicting_proposal(self):
        text = BASE + """
[[proposal]]
id = "P2"
rev = 1
questions = ["Q2"]
by = "agent-a"
summary = "Forbid punctuation in names"
state = "open"
restricts = ["ban punctuation in names"]
"""
        current, findings = view(text)
        self.assertIn("proposal_restricts_deferred_decision", codes(findings))
        row = [r for r in current["decisions"] if r["question"] == "Q2"][0]
        self.assertEqual(row["outcome"], "deferred")
        self.assertEqual(current["proposals"][0]["conflicts_with"], ["D2"])
        self.assertIn("CONFLICTS with deferred D2", ledger.render(current, findings, "test"))

    def test_compatible_repair_under_a_deferral_raises_nothing(self):
        text = BASE + """
[[work]]
id = "W2"
rev = 1
answer = "D2"
summary = "Precise errors and boundary tests"
owner = "agent-a"
state = "open"
[[proposal]]
id = "P2b"
rev = 1
questions = ["Q2"]
by = "agent-a"
summary = "Better error text"
state = "open"
"""
        current, findings = view(text)
        self.assertEqual(codes(findings), [])
        self.assertEqual(current["proposals"][0]["conflicts_with"], [])

    def test_adopting_a_proposal_needs_a_recorded_answer(self):
        bad = BASE + """
[[proposal]]
id = "P4"
rev = 1
questions = ["Q4"]
by = "agent-a"
summary = "Bind to an app identity"
state = "adopted"
"""
        current, findings = view(bad)
        self.assertIn("proposal_adopted_without_answer", codes(findings))
        self.assertIn("Q4", questions(current["missing"]))

    # 3. Already-required work becoming another approval request.
    def test_required_work_is_work_and_its_approval_request_is_flagged(self):
        current, findings = view(BASE + REQUIRED, {"status.md": "Q3 needs your decision before we start."})
        self.assertIn("Q3", questions(current["required"]))
        self.assertNotIn("Q3", questions(current["missing"]))
        self.assertEqual([w["work"] for w in current["work"]], ["W3"])
        self.assertEqual([(f["code"], f["question"]) for f in findings], [("settled_question_presented_open", "Q3")])

    def test_without_the_requirement_the_same_question_is_a_named_missing_decision(self):
        current, findings = view(BASE)
        row = [r for r in current["missing"] if r["question"] == "Q3"][0]
        self.assertEqual(row["affects"], "the export request interface")
        self.assertEqual(codes(findings), [])

    # 5. A genuinely unanswered question must remain unanswered.
    def test_unanswered_question_stays_missing_despite_an_agent_recommendation(self):
        text = BASE + """
[[proposal]]
id = "P4"
rev = 1
questions = ["Q4"]
by = "agent-a"
summary = "Recommend an app identity"
state = "open"
"""
        current, findings = view(text, {"q.md": OBSOLETE_LIST})
        self.assertIn("Q4", questions(current["missing"]))
        self.assertNotIn("Q4", questions(current["decisions"] + current["required"]))
        self.assertNotIn("Q4", [f.get("question") for f in findings])

    def test_missing_decision_must_name_the_affected_operation(self):
        text = BASE.replace('affects = "making the gate a required check"\n', "")
        current, findings = view(text)
        self.assertIn("missing_decision_without_operation", codes(findings))
        self.assertIn("NOT STATED", ledger.render(current, findings, "test"))

    # 6. A later owner instruction superseding an earlier accepted decision.
    def test_later_instruction_supersedes_only_when_it_says_so(self):
        later = """
[[answer]]
id = "D5b"
rev = 1
questions = ["Q5"]
authority = "owner"
recorded_by = "agent-r"
quote = "owner words"
outcome = "decided"
given = "day 2"
source = "board"
summary = "Run CI on pushes and fork pull requests only"
supersedes = ["D5"]
[[work]]
id = "W5"
rev = 1
answer = "D5"
summary = "Old CI rework"
owner = "agent-a"
state = "open"
"""
        current, findings = view(BASE + later)
        row = [r for r in current["decisions"] if r["question"] == "Q5"][0]
        self.assertEqual(row["answer"], "D5b")
        self.assertEqual([h["answer"] for h in current["historical"]], ["D5"])
        self.assertIn("work_under_superseded_answer", codes(findings))

    def test_two_answers_without_supersession_are_a_conflict_not_ordered_by_date(self):
        later = """
[[answer]]
id = "D5b"
rev = 1
questions = ["Q5"]
authority = "owner"
recorded_by = "agent-r"
quote = "owner words"
outcome = "decided"
given = "day 2"
source = "board"
summary = "Run CI on pushes and fork pull requests only"
"""
        current, findings = view(BASE + later)
        self.assertEqual(current["conflicts"], [{"question": "Q5", "answers": ["D5", "D5b"]}])
        self.assertNotIn("Q5", questions(current["decisions"]))
        self.assertIn("conflicting_answers_without_supersession", codes(findings))


class StructureTests(unittest.TestCase):
    def test_structural_defects_are_refused(self):
        cases = {
            "unknown_question": BASE + '[[answer]]\nid="X"\nrev=1\nquestions=["Q9"]\nauthority="owner"\nrecorded_by="r"\nquote="q"\noutcome="decided"\ngiven="d"\nsource="s"\nsummary="s"\n',
            "supersession_cycle": BASE + '[[answer]]\nid="D5c"\nrev=1\nquestions=["Q5"]\nauthority="owner"\nrecorded_by="r"\nquote="q"\noutcome="decided"\ngiven="d"\nsource="s"\nsummary="s"\nsupersedes=["D5d"]\n'
                                  '[[answer]]\nid="D5d"\nrev=1\nquestions=["Q5"]\nauthority="owner"\nrecorded_by="r"\nquote="q"\noutcome="decided"\ngiven="d"\nsource="s"\nsummary="s"\nsupersedes=["D5c"]\n',
            "invalid_answer": BASE.replace('quote = "owner words"\noutcome = "decided"\ngiven = "day 1"\nsource = "decisions/one.md #1"',
                                           'quote = "owner words"\noutcome = "required"\ngiven = "day 1"\nsource = "decisions/one.md #1"'),
            "duplicate_identity": BASE + '[[question]]\nid="Q1"\nrev=1\ntitle="t"\nasked="a"\n',
            "invalid_field:evidence": BASE + '[[work]]\nid="W9"\nrev=1\nanswer="D1"\nsummary="s"\nowner="o"\nstate="done"\n',
            "invalid_revision": BASE + '[[question]]\nid="Q9"\ntitle="t"\nasked="a"\n',
            "owner_words_missing": BASE + '[[answer]]\nid="D9"\nrev=1\nquestions=["Q4"]\nauthority="owner"\nrecorded_by="r"\noutcome="decided"\ngiven="d"\nsource="s"\nsummary="s"\n',
            "invalid_field:reason": BASE + '[[work]]\nid="W9"\nrev=1\nanswer="D1"\nsummary="s"\nowner="o"\nstate="withdrawn"\n',
            "invalid_work_successor": BASE + '[[work]]\nid="W9"\nrev=1\nanswer="D1"\nsummary="s"\nowner="o"\nstate="superseded"\nsuperseded_by="W9"\n',
        }
        for code, text in cases.items():
            with self.subTest(code=code):
                with self.assertRaises(ledger.LedgerError) as raised:
                    ledger.load(text)
                self.assertTrue(str(raised.exception).startswith(code), str(raised.exception))

    def test_owner_quotation_may_exceed_other_text_limits_but_summaries_may_not(self):
        long = "w" * (ledger.MAX_TEXT + 500)
        text = BASE.replace('quote = "owner words"', 'quote = "' + long + '"', 1)
        model = ledger.load(text)
        self.assertIn(long, [a.get("quote") for a in model["answers"].values()])
        with self.assertRaises(ledger.LedgerError):
            ledger.load(BASE.replace('title = "Read-only mode"', 'title = "' + long + '"', 1))

    def test_command_line_view_reads_only_inside_its_root_and_states_precedence(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / "ledger.toml").write_text(BASE + REQUIRED)
            (root / "q.md").write_text(OBSOLETE_LIST)
            done = subprocess.run([sys.executable, "-I", "-B", str(ROOT / "tools/ci/drift_ledger.py"), "view",
                                   "--root", str(root), "--ledger", "ledger.toml", "--scan", "*.md",
                                   "--scan", "../*"], capture_output=True, text=True, check=True)
            self.assertIn("take precedence over this view immediately", done.stdout)
            self.assertIn("Q1 decided by D1", done.stdout)
            self.assertIn("settled_question_presented_open", done.stdout)
            bad = subprocess.run([sys.executable, "-I", "-B", str(ROOT / "tools/ci/drift_ledger.py"), "view",
                                  "--root", str(root), "--ledger", "missing.toml"], capture_output=True, text=True)
            self.assertEqual(bad.returncode, 2)
            self.assertIn("ask the owner", bad.stdout)


WORK = """
[[work]]
id = "W1"
rev = 1
answer = "D1"
summary = "Implement read-only mode"
owner = "agent-a"
state = "open"
"""


def entry(text, eid):
    return ledger._entries(ledger.load(text))[eid][1]


class RecordTests(unittest.TestCase):
    def test_open_work_under_a_replaced_answer_closes_without_deleting_history(self):
        replaced = BASE + """
[[answer]]
id = "D5b"
rev = 1
questions = ["Q5"]
authority = "owner"
recorded_by = "agent-r"
quote = "pushes and forks only"
outcome = "decided"
given = "day 2"
source = "board"
summary = "Run CI on pushes and fork pull requests only"
supersedes = ["D5"]
[[work]]
id = "W5"
rev = 1
answer = "D5"
summary = "Old CI rework"
owner = "agent-a"
state = "open"
"""
        _, findings = view(replaced)
        self.assertIn("work_under_superseded_answer", codes(findings))
        withdrawn = replaced.replace('state = "open"', 'state = "withdrawn"\nreason = "direction changed"')
        current, findings = view(withdrawn)
        self.assertEqual(findings, [])
        self.assertEqual([(r["work"], r["state"]) for r in current["closed"]], [("W5", "withdrawn")])
        successor = replaced.replace('state = "open"', 'state = "superseded"\nsuperseded_by = "W5b"') + """
[[work]]
id = "W5b"
rev = 1
answer = "D5b"
summary = "CI rework for the new direction"
owner = "agent-a"
state = "open"
"""
        current, findings = view(successor)
        self.assertEqual(findings, [])
        self.assertEqual([r["work"] for r in current["work"]], ["W5b"])
        self.assertEqual([(r["work"], r["why"]) for r in current["closed"]], [("W5", "W5b")])

    def test_owner_answers_show_who_recorded_them_and_whether_the_words_survive(self):
        text = BASE.replace('quote = "owner words"\noutcome = "decided"\ngiven = "day 1"\nsource = "decisions/one.md #1"',
                            'quote_unavailable = "only the summary was kept"\noutcome = "decided"\ngiven = "day 1"\n'
                            'source = "decisions/one.md #1"') + REQUIRED
        current, findings = view(text)
        self.assertEqual(findings, [])
        out = ledger.render(current, findings, "test")
        self.assertIn("recorded by agent-r; transcription only]", out.splitlines()[[i for i, l in enumerate(out.splitlines()) if l.startswith("- Q1 ")][0]])
        self.assertNotIn("transcription only", [l for l in out.splitlines() if l.startswith("- Q5 ")][0])
        both = BASE.replace('quote = "owner words"\noutcome = "decided"\ngiven = "day 1"\nsource = "decisions/one.md #1"',
                            'quote = "owner words"\nquote_unavailable = "x"\noutcome = "decided"\ngiven = "day 1"\n'
                            'source = "decisions/one.md #1"')
        with self.assertRaises(ledger.LedgerError):
            ledger.load(both)


class UpdateTests(unittest.TestCase):
    """A writer's change is applied onto the latest ledger by entry identity and revision."""

    def merged(self, base, current, proposed):
        text, conflicts = ledger.update(base, current, proposed)
        self.assertEqual(conflicts, [])
        return text

    def conflict(self, base, current, proposed):
        text, conflicts = ledger.update(base, current, proposed)
        self.assertIsNone(text)
        return [c["code"] for c in conflicts]

    def test_unrelated_changes_by_two_writers_both_survive(self):
        base = BASE + WORK
        theirs = base + REQUIRED
        ours = base.replace('state = "open"', 'state = "done"\nevidence = "pr 1"').replace('id = "W1"\nrev = 1', 'id = "W1"\nrev = 2')
        text = self.merged(base, theirs, ours)
        self.assertEqual(entry(text, "W1")["state"], "done")
        self.assertIn("W3", ledger._entries(ledger.load(text)))
        current, findings = view(text)
        self.assertEqual(findings, [])

    def test_stale_revision_is_refused_even_when_the_value_changed_back(self):
        base = BASE + WORK
        # Another writer changed W1 and changed it back (A -> B -> A): same visible value, newer revision.
        theirs = base.replace('id = "W1"\nrev = 1', 'id = "W1"\nrev = 3')
        ours = base.replace('state = "open"', 'state = "withdrawn"\nreason = "r"').replace('id = "W1"\nrev = 1', 'id = "W1"\nrev = 2')
        self.assertEqual(self.conflict(base, theirs, ours), ["stale_revision"])
        # Remedy: re-read the current ledger and reapply.
        again = theirs.replace('state = "open"', 'state = "withdrawn"\nreason = "r"').replace('id = "W1"\nrev = 3', 'id = "W1"\nrev = 4')
        self.assertEqual(entry(self.merged(theirs, theirs, again), "W1")["state"], "withdrawn")

    def test_a_change_must_advance_the_revision(self):
        base = BASE + WORK
        ours = base.replace('state = "open"', 'state = "withdrawn"\nreason = "r"')
        self.assertEqual(self.conflict(base, base, ours), ["revision_not_advanced"])

    def test_answers_are_replaced_by_supersession_not_edited(self):
        edited = BASE.replace('summary = "Accept read-only mode"', 'summary = "Decline read-only mode"')
        self.assertEqual(self.conflict(BASE, BASE, edited), ["answer_modified"])
        replacement = BASE + """
[[answer]]
id = "D1b"
rev = 1
questions = ["Q1"]
authority = "owner"
recorded_by = "agent-r"
quote = "decline it"
outcome = "decided"
given = "day 2"
source = "session"
summary = "Decline read-only mode"
supersedes = ["D1"]
"""
        current, _ = view(self.merged(BASE, BASE, replacement))
        self.assertEqual([r["answer"] for r in current["decisions"] if r["question"] == "Q1"], ["D1b"])
        self.assertEqual([h["answer"] for h in current["historical"]], ["D1"])

    def test_entries_are_never_removed(self):
        self.assertEqual(self.conflict(BASE + WORK, BASE + WORK, BASE), ["entry_removed"])

    def test_retry_after_a_lost_response_is_a_no_op_and_a_reused_identity_is_refused(self):
        published = self.merged(BASE, BASE, BASE + WORK)
        self.assertEqual(ledger.load(self.merged(BASE, published, BASE + WORK)), ledger.load(published))
        other = BASE + WORK.replace("Implement read-only mode", "Something else")
        self.assertEqual(self.conflict(BASE, published, other), ["identity_reused"])
        self.assertEqual(self.conflict(BASE, BASE, BASE + WORK.replace("rev = 1", "rev = 2")), ["new_entry_revision"])

    def test_serialized_ledger_reads_back_identically(self):
        text = BASE + REQUIRED + WORK + '[[question]]\nid = "Q8"\nrev = 1\ntitle = "Quote \\" and \\\\ and \\u00e9"\nasked = "a"\n'
        model = ledger.load(text)
        again = ledger.load(ledger.serialize(list(ledger._entries(model).values()), "# kept header"))
        self.assertEqual(again, model)

    def test_command_line_update_reports_conflicts_and_no_ops(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / "base.toml").write_text(BASE + WORK)
            (root / "current.toml").write_text((BASE + WORK).replace('id = "W1"\nrev = 1', 'id = "W1"\nrev = 2'))
            (root / "ours.toml").write_text((BASE + WORK).replace('state = "open"', 'state = "withdrawn"\nreason = "r"')
                                            .replace('id = "W1"\nrev = 1', 'id = "W1"\nrev = 2'))
            command = [sys.executable, "-I", "-B", str(ROOT / "tools/ci/drift_ledger.py"), "update",
                       "--base", str(root / "base.toml"), "--current", str(root / "current.toml"),
                       "--proposed", str(root / "ours.toml"), "--out", str(root / "out.toml")]
            stale = subprocess.run(command, capture_output=True, text=True)
            self.assertEqual(stale.returncode, 3)
            self.assertIn("code=stale_revision, id=W1, expected_rev=1, found=rev 2", stale.stdout)
            self.assertFalse((root / "out.toml").exists())
            (root / "current.toml").write_text((root / "ours.toml").read_text())
            done = subprocess.run(command, capture_output=True, text=True, check=True)
            self.assertEqual(done.stdout.strip(), "unchanged")


# Values the reader accepts beyond the fields it validates. Every one must come back unchanged.
EXOTIC = r"""
[[question]]
id = "Q7"
rev = 1
title = "Exotic values"
asked = "q.md#7"
text = "tab\tnew\nline quote\" backslash\\ del\u007f \u00e9 \u2603 #not-a-comment"
literal = 'C:\path\x'
multiline = '''
first
second'''
yes = true
no = false
big = 9007199254740993
negative = -42
small = 1.5e-3
negzero = -0.0
plusinf = inf
minusinf = -inf
notanumber = nan
when = 2026-11-04T10:00:00Z
offset = 2026-11-04T10:00:00.123456-05:00
local = 2026-11-04T10:00:00.5
day = 2026-11-04
clock = 07:32:00
mixed = [1, "two", 3.0, true, [4], { five = 5 }]
empty_list = []
empty_table = {}
nested = { inner = { deep = "x", "dotted.key" = 1 }, "with space" = 2 }
"note.a" = "a quoted key with a dot"
"key with space" = "spaced"
"del\u007fkey" = "a key with DEL"
"""


def _same(a, b):
    if isinstance(a, float) and isinstance(b, float) and a != a and b != b:
        return True
    if isinstance(a, dict) and isinstance(b, dict):
        return a.keys() == b.keys() and all(_same(a[k], b[k]) for k in a)
    if isinstance(a, (list, tuple)) and isinstance(b, (list, tuple)) and type(a) is type(b):
        return len(a) == len(b) and all(_same(x, y) for x, y in zip(a, b))
    return type(a) is type(b) and a == b


class WriterRoundTripTests(unittest.TestCase):
    """read -> update -> write -> read keeps every accepted value, unrelated record and comment."""

    def merged(self, base, current, proposed):
        text, conflicts = ledger.update(base, current, proposed)
        self.assertEqual(conflicts, [])
        return text

    def row(self, text, eid):
        return ledger._entries(ledger.load(text))[eid][1]

    def test_every_value_the_reader_accepts_survives_an_update(self):
        proposed = BASE + EXOTIC
        ledger.load(proposed)  # accepted by the reader
        text = self.merged(BASE, BASE, proposed)
        self.assertTrue(_same(self.row(text, "Q7"), self.row(proposed, "Q7")))
        self.assertTrue(_same(tomllib.loads(text)["question"][-1], tomllib.loads(proposed)["question"][-1]))

    def test_unrelated_records_with_such_values_are_kept_unchanged(self):
        base = BASE + EXOTIC
        proposed = base + WORK
        text = self.merged(base, base, proposed)
        before, after = ledger._entries(ledger.load(base)), ledger._entries(ledger.load(text))
        for eid in before:
            self.assertTrue(_same(before[eid], after[eid]), eid)
        self.assertIn("W1", after)
        # An unchanged entry holding nan is not a change (nan != nan must not demand a new revision).
        self.assertEqual(ledger.update(base, base, base)[1], [])

    def test_owner_quotations_are_written_back_verbatim(self):
        quote = 'He said: "keep it" \u2014 tab\there, backslash \\ and del \u007f. # not a comment'
        answer = ('[[answer]]\nid = "D4"\nrev = 1\nquestions = ["Q4"]\nauthority = "owner"\nrecorded_by = "r"\n'
                  'quote = ' + json.dumps(quote) + '\noutcome = "decided"\ngiven = "d"\nsource = "s"\nsummary = "s"\n')
        text = self.merged(BASE, BASE, BASE + answer)
        self.assertEqual(self.row(text, "D4")["quote"], quote)

    def test_top_level_data_and_tables_are_kept_and_merged(self):
        base = BASE.replace("version = 2\n", 'version = 2\nowner = "x"\n', 1) + '[meta]\nnote = "keep"\n"a.b" = 1\n'
        self.assertNotIn("[meta]", base.split("[[")[0])  # a table after entries is still top-level data
        text = self.merged(base, base, base + WORK)
        data = tomllib.loads(text)
        self.assertEqual((data["owner"], data["meta"]), ("x", {"note": "keep", "a.b": 1}))
        changed = base.replace('note = "keep"', 'note = "changed"')
        text = self.merged(base, base, changed)
        self.assertEqual(tomllib.loads(text)["meta"]["note"], "changed")
        theirs = base.replace('note = "keep"', 'note = "theirs"')
        _, conflicts = ledger.update(base, theirs, changed)
        self.assertEqual([c["code"] for c in conflicts], ["top_level_conflict"])

    def test_full_line_comments_are_kept_and_an_inline_comment_is_refused(self):
        base = "# header line\n" + BASE + "\n# section: work\n" + WORK.replace(
            'owner = "agent-a"', '# owner line note\nowner = "agent-a"')
        proposed = base.replace("# header line", "# header line (edited)") + "\n# new section\n" + REQUIRED
        current = base.replace("# section: work", "# section: work (theirs)")
        text = self.merged(base, current, proposed)
        for comment in ("# header line (edited)", "# section: work (theirs)", "# owner line note", "# new section"):
            self.assertIn(comment, text)
        inline = proposed.replace('summary = "Implement bulk export"', 'summary = "Implement bulk export" # why')
        merged, conflicts = ledger.update(base, base, inline)
        self.assertIsNone(merged)
        self.assertEqual([c["code"] for c in conflicts], ["inline_comment_not_kept"])
        # A hash inside a string is not a comment.
        hashed = proposed.replace('summary = "Implement bulk export"', 'summary = "Export #3 # not a comment"')
        self.assertIn('"Export #3 # not a comment"', self.merged(base, base, hashed))

    def test_multi_line_values_with_comments_inside_are_kept(self):
        # Review finding: a comment line inside a multi-line array made the writer crash.
        entry = ('[[question]]\nid = "Q8"\nrev = 1\ntitle = "t"\nasked = "a"\nvalues = [\n'
                 '# note inside the array\n  1,\n  [[2], 3],\n  "x # not a comment",\n]\nafter = "kept"\n')
        top = '# about the list\nlist = [\n# why\n  "a",\n]\n'
        text = BASE.replace("version = 2\n", "version = 2\n" + top, 1) + entry
        ledger.load(text)
        self.assertEqual(ledger.update(text, text, text)[1], [])
        merged = self.merged(text, text, text + WORK)
        self.assertTrue(_same(self.row(merged, "Q8"), self.row(text, "Q8")))
        self.assertEqual(tomllib.loads(merged)["list"], ["a"])
        lines = merged.splitlines()
        for comment, field in (("# note inside the array", "values = "), ("# why", "list = "),
                               ("# about the list", "list = ")):
            # Each comment stays with its field: it comes just before that field's line.
            self.assertLess(lines.index(comment), [i for i, l in enumerate(lines) if l.startswith(field)][0])
        self.assertNotIn(("Q8", "1,"), ledger._comments(text)[0])

    def test_a_failed_write_publishes_nothing(self):
        # The writer re-reads what it wrote; any difference refuses the whole update.
        with mock.patch.object(ledger, "_value", side_effect=lambda v: '"damaged"' if v == "x" else ORIGINAL_VALUE(v)):
            merged, conflicts = ledger.update(BASE, BASE, BASE + EXOTIC.replace('deep = "x"', 'deep = "x"'))
        self.assertIsNone(merged)
        self.assertEqual([c["code"] for c in conflicts], ["serialization_mismatch"])
        # Damaged top-level data or a lost comment in the written text is caught the same way.
        base = "# keep this note\n" + BASE + '[meta]\nnote = "keep"\n'
        real = ledger.serialize
        for damage in (lambda t: t.replace('note = "keep"', 'note = "lost"'), lambda t: t.replace("# keep this note\n", "")):
            with mock.patch.object(ledger, "serialize", side_effect=lambda *a, **k: damage(real(*a, **k))):
                merged, conflicts = ledger.update(base, base, base + WORK)
            self.assertIsNone(merged)
            self.assertEqual([c["code"] for c in conflicts], ["serialization_mismatch"])
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / "base.toml").write_text(BASE)
            (root / "proposed.toml").write_text(BASE + EXOTIC)
            (root / "out.toml").write_text("previous output\n")
            command = [sys.executable, "-I", "-B", str(ROOT / "tools/ci/drift_ledger.py"), "update",
                       "--base", str(root / "base.toml"), "--current", str(root / "base.toml"),
                       "--proposed", str(root / "proposed.toml"), "--out", str(root / "out.toml")]
            # Interrupted replace: the target keeps its previous content and no partial file is left.
            with mock.patch.object(ledger.os, "replace", side_effect=OSError("disk full")):
                with self.assertRaises(OSError):
                    ledger._write_atomic(str(root / "out.toml"), "new content\n")
            self.assertEqual((root / "out.toml").read_text(), "previous output\n")
            self.assertEqual(sorted(p.name for p in root.iterdir()),
                             ["base.toml", "out.toml", "proposed.toml"])
            done = subprocess.run(command, capture_output=True, text=True, check=True)
            self.assertEqual(done.stdout.strip(), "updated")
            self.assertTrue(_same(self.row((root / "out.toml").read_text(), "Q7"), self.row(BASE + EXOTIC, "Q7")))


ORIGINAL_VALUE = ledger._value if hasattr(ledger, "_value") else None


if __name__ == "__main__":
    unittest.main()
