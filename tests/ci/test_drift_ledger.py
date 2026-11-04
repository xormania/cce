"""Paired replays: each recorded failure mode is shown failing and its healthy counterpart passing.

Fixtures are synthetic; they reproduce the behaviours, not any private record.
"""
from __future__ import annotations

from pathlib import Path
import subprocess
import sys
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT / "tools/ci"))
import drift_ledger as ledger

BASE = """
version = 1
[[question]]
id = "Q1"
title = "Read-only mode"
asked = "questions.md#1"
[[question]]
id = "Q2"
title = "Naming contract"
asked = "questions.md#2"
[[question]]
id = "Q3"
title = "Bulk export"
asked = "questions.md#3"
affects = "the export request interface"
[[question]]
id = "Q4"
title = "Gate identity"
asked = "questions.md#4"
affects = "making the gate a required check"
[[question]]
id = "Q5"
title = "CI cadence"
asked = "questions.md#5"
[[answer]]
id = "D1"
questions = ["Q1"]
authority = "owner"
outcome = "decided"
given = "day 1"
source = "decisions/one.md #1"
summary = "Accept read-only mode"
[[answer]]
id = "D2"
questions = ["Q2"]
authority = "owner"
outcome = "deferred"
given = "day 1"
source = "decisions/one.md #2"
summary = "Deferred; compatible repairs proceed"
forbids = ["ban punctuation in names", "shorter name limits"]
[[answer]]
id = "D5"
questions = ["Q5"]
authority = "owner"
outcome = "decided"
given = "day 1"
source = "decisions/one.md #5"
summary = "Run CI on every push"
"""

REQUIRED = """
[[answer]]
id = "R3"
questions = ["Q3"]
authority = "requirement"
outcome = "required"
given = "brief"
source = "brief S7"
summary = "Bulk export is required"
[[work]]
id = "W3"
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
answer = "D2"
summary = "Precise errors and boundary tests"
owner = "agent-a"
state = "open"
[[proposal]]
id = "P2b"
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
questions = ["Q5"]
authority = "owner"
outcome = "decided"
given = "day 2"
source = "board"
summary = "Run CI on pushes and fork pull requests only"
supersedes = ["D5"]
[[work]]
id = "W5"
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
questions = ["Q5"]
authority = "owner"
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
            "unknown_question": BASE + '[[answer]]\nid="X"\nquestions=["Q9"]\nauthority="owner"\noutcome="decided"\ngiven="d"\nsource="s"\nsummary="s"\n',
            "supersession_cycle": BASE.replace('summary = "Accept read-only mode"', 'summary = "Accept read-only mode"\nsupersedes = ["D5"]')
                                  .replace('summary = "Run CI on every push"', 'summary = "Run CI on every push"\nsupersedes = ["D1"]'),
            "invalid_answer": BASE.replace('authority = "owner"\noutcome = "decided"\ngiven = "day 1"\nsource = "decisions/one.md #1"',
                                           'authority = "owner"\noutcome = "required"\ngiven = "day 1"\nsource = "decisions/one.md #1"'),
            "duplicate_identity": BASE + '[[question]]\nid="Q1"\ntitle="t"\nasked="a"\n',
            "invalid_field:evidence": BASE + '[[work]]\nid="W9"\nanswer="D1"\nsummary="s"\nowner="o"\nstate="done"\n',
        }
        for code, text in cases.items():
            with self.subTest(code=code):
                with self.assertRaises(ledger.LedgerError) as raised:
                    ledger.load(text)
                self.assertTrue(str(raised.exception).startswith(code), str(raised.exception))

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


if __name__ == "__main__":
    unittest.main()
