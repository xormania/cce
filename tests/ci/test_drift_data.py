"""Versioned data is pinned, applicable, linked and never a public payload."""
from __future__ import annotations

import copy
import json
from pathlib import Path
import sys
import unittest
from unittest.mock import patch

ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT / "tools" / "ci"))
import drift_data as data
from drift_contracts import evaluate

HEAD, BASE, DATA = "a" * 40, "b" * 40, "c" * 40
SOURCE = {"repository": "example/engine", "head_sha": HEAD, "base_sha": BASE}


class FakeApi:
    def __init__(self, files=None, commits=None):
        self.files = files or {}
        self.commits = commits or {"HEAD": DATA}
        self.calls = []

    def api(self, path):
        self.calls.append(("api", path))
        if not path.startswith("/commits/"):
            raise AssertionError(path)
        return {"sha": self.commits[path.removeprefix("/commits/")]}

    def file(self, path, ref):
        self.calls.append((path, ref))
        value = self.files[path]
        if isinstance(value, Exception):
            raise value
        return value


class DataTests(unittest.TestCase):
    def env(self, required=False, records=None):
        files = {"requirements": "records/map.toml", "summary": "records/status.md"}
        if records is not None:
            files["records"] = records
        return {"DRIFT_DATA_REPOSITORY": "private-team/DO_NOT_PUBLISH_REPOSITORY",
                "DRIFT_DATA_TOKEN": "DO_NOT_PUBLISH_TOKEN", "DRIFT_DATA_FILES": json.dumps(files),
                "DRIFT_DATA_REQUIRED": str(required).lower()}

    def api(self, state="partial", verified=HEAD, extra=""):
        mapping = ('[[requirements]]\nid="contract-a"\nacceptance=["case-b"]\n'
                   '[[cases]]\nid="case-a"\nslice="slice-a"\nstatus="green"\n'
                   '[[cases]]\nid="case-b"\nslice="slice-a"\nstatus="' + state + '"\n' + extra)
        summary = ('---\nverified-at: ' + verified + '\n---\n'
                   '| Slice | State |\n| slice-a | **2/2 green** |\n')
        return FakeApi({"records/map.toml": mapping, "records/status.md": summary})

    def collect(self, api=None, env=None, **kwargs):
        api = api or self.api()
        return data.data_evidence(copy.deepcopy(SOURCE), env or self.env(), lambda _r, _t: api, **kwargs)

    def test_real_shaped_map_and_summary_contradiction_and_correction(self):
        claims, semantic, limits, failed = self.collect()
        self.assertFalse(failed)
        self.assertEqual(limits, ["decision_data_not_configured"])
        verdict = evaluate({"source": SOURCE, "claims": claims})
        self.assertEqual([row["code"] for row in verdict["violations"]], ["claim_case_mismatch"])
        healthy, _, _, _ = self.collect(self.api("green"))
        self.assertEqual(evaluate({"source": SOURCE, "claims": healthy})["violations"], [])
        self.assertTrue(all(row["private"] for row in semantic))

    def test_pinned_revision_override_and_public_safe_configuration_identity(self):
        api = self.api()
        metadata = {}
        self.collect(api, data_ref=BASE, metadata=metadata)
        self.assertTrue(all(ref == BASE for path, ref in api.calls if path != "api"))
        self.assertFalse(any(path == "api" for path, _ref in api.calls))
        self.assertEqual(metadata["data_ref"], BASE)
        self.assertEqual(metadata["config_key"], data.config_key(self.env()))
        encoded = json.dumps(metadata)
        self.assertNotIn("DO_NOT_PUBLISH", encoded)
        self.assertNotIn("records/", encoded)
        changed = self.env()
        changed["DRIFT_DATA_TOKEN"] = "changed credential"
        self.assertEqual(data.config_key(changed), metadata["config_key"])

    def test_short_verified_sha_requires_resolution_to_exact_public_candidate(self):
        api = self.api(verified=HEAD[:7])
        claims, _, limits, _ = self.collect(api)
        self.assertFalse(claims[0]["applicable"])
        self.assertIn("data_candidate_not_proven", limits)
        public = FakeApi(commits={HEAD[:7]: HEAD})
        with patch("drift_relationships.mapped_relationships", return_value=([], [])):
            claims, _, limits, _ = self.collect(api, public_api=public)
        self.assertTrue(claims[0]["applicable"])
        self.assertNotIn("data_candidate_not_proven", limits)
        public.commits[HEAD[:7]] = HEAD[:7] + "d" * 33
        with patch("drift_relationships.mapped_relationships", return_value=([], [])):
            claims, _, _, _ = self.collect(api, public_api=public)
        self.assertFalse(claims[0]["applicable"])

    def test_missing_required_or_optional_input_has_distinct_failure(self):
        self.assertEqual(data.data_evidence(SOURCE, {}, lambda *_: None), ([], [], ["data_not_configured", "decision_data_not_configured"], False))
        env = {"DRIFT_DATA_REQUIRED": "true"}
        self.assertTrue(data.data_evidence(SOURCE, env, lambda *_: None)[-1])
        for required in (False, True):
            result = self.collect(self.api(verified=BASE), self.env(required))
            self.assertEqual(result[-1], required)
            self.assertFalse(result[0][0]["applicable"])

    def test_duplicate_case_or_requirement_identity_cannot_appear_clean(self):
        for extra in ('[[cases]]\nid="case-a"\nslice="slice-a"\nstatus="green"\n',
                      '[[requirements]]\nid="contract-a"\nacceptance=[]\n'):
            result = self.collect(self.api(extra=extra), self.env(True))
            self.assertEqual(result, ([], [], ["data_source_unavailable"], True))

    def test_actual_complete_total_must_match_case_inventory(self):
        api = self.api("green")
        api.files["records/status.md"] = api.files["records/status.md"].replace("2/2", "3/3")
        _, _, limits, failed = self.collect(api, self.env(True))
        self.assertIn("data_case_total_mismatch", limits)
        self.assertTrue(failed)

    def test_summary_group_bound_cannot_silently_report_full_completion(self):
        api = self.api()
        api.files["records/map.toml"] = "\n".join(
            '[[cases]]\nid="case-' + str(index) + '"\nslice="slice-' + str(index) + '"\nstatus="green"'
            for index in range(data.MAX_STATUS_GROUPS + 1))
        api.files["records/status.md"] = "verified-at: " + HEAD + "\n" + "\n".join(
            "| slice-" + str(index) + " | 1/1 green |" for index in range(data.MAX_STATUS_GROUPS + 1))
        for required in (False, True):
            claims, semantic, limits, failed = self.collect(api, self.env(required))
            self.assertEqual((claims, semantic), ([], []))
            self.assertEqual(limits, ["data_summary_inventory_bound"])
            self.assertEqual(failed, required)

    def case_inventory(self, count, groups=1, last_state="green"):
        api = self.api()
        api.files["records/map.toml"] = "\n".join(
            '[[cases]]\nid="case-' + str(index) + '"\nslice="slice-' + str(index % groups)
            + '"\nstatus="' + (last_state if index == count - 1 else "green") + '"'
            for index in range(count))
        totals = [len(range(group, count, groups)) for group in range(groups)]
        api.files["records/status.md"] = "verified-at: " + HEAD + "\n" + "\n".join(
            "| slice-" + str(group) + " | " + str(total) + "/" + str(total) + " green |"
            for group, total in enumerate(totals))
        return api

    def test_supported_case_inventories_reach_contract_evaluation(self):
        for count in (256, 257, 1000):
            for groups in (1, 4):
                for required in (False, True):
                    with self.subTest(count=count, groups=groups, required=required):
                        claims, _, limits, failed = self.collect(self.case_inventory(count, groups), self.env(required))
                        self.assertFalse(failed)
                        self.assertEqual(limits, ["decision_data_not_configured"])
                        self.assertEqual(sum(len(row["case_statuses"]) for row in claims), count)
                        self.assertEqual(len(claims), groups)
                        self.assertEqual(evaluate({"source": SOURCE, "claims": claims})["violations"], [])

    def test_large_group_preserves_a_late_contradiction(self):
        claims, _, _, failed = self.collect(self.case_inventory(1000, last_state="partial"))
        self.assertFalse(failed)
        result = evaluate({"source": SOURCE, "claims": claims})
        self.assertEqual([v["code"] for v in result["violations"]], ["claim_case_mismatch"])

    def test_case_inventory_above_supported_bound_keeps_optional_semantics(self):
        for groups in (1, 4):
            for required in (False, True):
                with self.subTest(groups=groups, required=required):
                    result = self.collect(self.case_inventory(1001, groups), self.env(required))
                    self.assertEqual(result, ([], [], ["data_source_unavailable"], required))

    def test_relative_private_decision_link_is_fetched_at_same_snapshot(self):
        api = self.api()
        api.files["records/status.md"] += "\n[decision](../decisions/retained.md)\n[external](https://arbitrary.invalid/no)\n"
        api.files["decisions/retained.md"] = "case-b: retain the original reader bytes."
        captured = {}
        def related(_public, _source, cases, _summary, context, **_kwargs):
            captured.update({"cases": cases, "context": context})
            return [{"id": "private-relation", "excerpts": [context["decisions/retained.md"]],
                     "questions": ["proof"], "public_path": "DO_NOT_PUBLISH_PATH"}], []
        with patch("drift_relationships.mapped_relationships", side_effect=related):
            _, semantic, limits, _ = self.collect(api, public_api=FakeApi())
        self.assertEqual(limits, ["decision_data_not_configured"])
        self.assertIn("decisions/retained.md", captured["context"])
        self.assertEqual(captured["cases"][1]["requirements"][0]["id"], "contract-a")
        self.assertTrue(all(ref == DATA for path, ref in api.calls if path != "api"))
        self.assertFalse(any("public_path" in row for row in semantic))

    def test_missing_record_reports_scope_without_inventing_optional_failure(self):
        env = self.env(records=["records/missing.log"])
        _, _, limits, failed = self.collect(env=env)
        self.assertIn("data_record_unavailable", limits)
        self.assertFalse(failed)
        env["DRIFT_DATA_REQUIRED"] = "true"
        self.assertTrue(self.collect(env=env)[-1])

    def test_existing_map_joins_recorded_contract_and_real_reader_assertion(self):
        api = self.api(extra='tests=["store:readers:retained_reader_bytes"]\n')
        api.files["records/status.md"] += "\n[contract](../contracts/reader.md)\n"
        api.files["contracts/reader.md"] = "setup\n" * 700 + "contract-a: Original bytes remain available while the reader is held.\n"
        proof = "// unrelated setup\n" * 700 + '#[test]\nfn retained_reader_bytes() {\n    assert_eq!(held.read(), original);\n}\n'
        public = FakeApi(files={"crates/store/tests/readers.rs": proof})
        _, semantic, _, _ = self.collect(api, public_api=public)
        relation = next(row for row in semantic if row["id"] == "mapped-relationship:case-b")
        evidence = "\n".join(relation["excerpts"])
        self.assertIn("Original bytes remain available", evidence)
        self.assertIn("assert_eq!(held.read(), original)", evidence)
        self.assertTrue(relation["private"])
        self.assertNotIn("public_path", relation)
        self.assertTrue(all(ref == HEAD for path, ref in public.calls if path != "api"))
        claim_header = relation["excerpts"][0].splitlines()[0]
        self.assertIn(DATA, claim_header)
        self.assertNotIn(HEAD, claim_header)

    def test_reconstruction_subjects_and_pinned_ref_reach_relationship_collector(self):
        target = "d" * 24
        api = self.api(extra='tests=["store:readers:retained_reader_bytes"]\n')
        with patch("drift_relationships.mapped_relationships", return_value=([], [])) as mapped:
            self.collect(api, public_api=FakeApi(), data_ref=BASE, subjects=[target])
        self.assertEqual(mapped.call_args.kwargs, {"data_ref": BASE, "subjects": [target]})

    def mutation(self, disposition="caught", exit_code=101, assertion=True, **run_metadata):
        extra = ('[[mutations]]\nid="mutant-a"\nsource_head="' + HEAD[:7] + '"\n'
                 'disposition="' + disposition + '"\nevidence="records/mutation-a"\n')
        api = self.api(extra=extra)
        api.files["records/mutation-a/result.json"] = json.dumps({
            "source_head": HEAD, "source_dirty": False, "outcome": "caught",
            "runs": [{"exit": exit_code, "executed": True, "failed_by_assertion": assertion, **run_metadata}]})
        api.files["records/mutation-a/test-0.log"] = "thread test panicked: assertion left == right failed\n"
        return api

    def test_raw_and_accepted_mutation_outcomes_remain_distinct(self):
        mutations = []
        self.collect(self.mutation("equivalent"), normalized_mutations=mutations)
        self.assertEqual(mutations[0]["raw_outcome"], "caught")
        self.assertEqual(mutations[0]["disposition"], "equivalent")
        self.assertFalse(mutations[0]["claimed_semantic"])
        self.assertEqual(evaluate({"source": SOURCE, "mutations": mutations})["violations"], [])

    def test_per_run_abort_is_not_normalized_as_assertion_catch(self):
        mutations = []
        self.collect(self.mutation(exit_code=134), normalized_mutations=mutations)
        self.assertTrue(mutations[0]["runs"][0]["aborted"])
        result = evaluate({"source": SOURCE, "mutations": mutations})
        self.assertEqual(result["violations"][0]["code"], "semantic_catch_not_demonstrated")

    def test_signal_and_application_exits_keep_their_recorded_convention(self):
        for convention in (None, "shell", "subprocess"):
            metadata = {} if convention is None else {"exit_convention": convention}
            for exit_code in (-6, -9, -11, -15, 0, 101, 128, 129, 134, 137, 139, 143, 192, 193):
                with self.subTest(convention=convention, exit=exit_code):
                    mutations = []
                    self.collect(self.mutation(exit_code=exit_code, **metadata), normalized_mutations=mutations)
                    run = mutations[0]["runs"][0]
                    self.assertEqual(run["exit"], exit_code)
                    self.assertEqual(run.get("exit_convention"), convention)
                    valid = exit_code in (101, 128, 193) or convention == "subprocess" and exit_code > 0
                    expected = [] if valid else ["semantic_catch_not_demonstrated"]
                    result = evaluate({"source": SOURCE, "mutations": mutations})
                    self.assertEqual([v["code"] for v in result["violations"]], expected)

    def test_unknown_exit_convention_is_not_silently_discarded(self):
        for convention in ("unknown", True, {}, None):
            for required in (False, True):
                with self.subTest(convention=convention, required=required):
                    result = self.collect(self.mutation(exit_convention=convention), self.env(required))
                    self.assertEqual(result, ([], [], ["data_source_unavailable"], required))

    def test_mutation_diagnosis_contains_decisive_real_log_and_accepted_disposition(self):
        api = self.mutation("equivalent")
        api.files["records/mutation-a/test-0.log"] = ("preamble\n" * 800
            + "assertion failed: no faulty open made cached bytes durable\n")
        _, semantic, limits, _ = self.collect(api)
        relation = next(row for row in semantic if row["id"] == "mutation:mutant-a")
        evidence = "\n".join(relation["excerpts"])
        self.assertIn("no faulty open made cached bytes durable", evidence)
        self.assertIn("equivalent", evidence)
        self.assertNotIn("mutation_log_unavailable", limits)

    def test_missing_mutation_evidence_is_visible(self):
        api = self.mutation()
        del api.files["records/mutation-a/result.json"]
        _, _, limits, failed = self.collect(api, self.env(True))
        self.assertIn("mutation_evidence_unavailable", limits)
        self.assertTrue(failed)

    def test_decisive_record_lines_survive_long_prefix_and_byte_budget(self):
        api = self.api()
        api.files["records/replay.log"] = "preamble\n" * 1000 + "assertion held_reader_bytes == original_bytes FAILED\n"
        _, semantic, _, _ = self.collect(api, self.env(records=["records/replay.log"]))
        excerpt = next(row for row in semantic if row["id"].startswith("record:"))["excerpts"][0]
        self.assertIn("held_reader_bytes == original_bytes", excerpt)
        self.assertLessEqual(len(excerpt.encode()), 1600)

    def test_fetched_script_text_is_never_executed_and_errors_are_static(self):
        api = self.api()
        marker = 'raise RuntimeError("DO_NOT_PUBLISH_PRIVATE_MARKER")'
        api.files["records/status.md"] += marker
        _, semantic, _, _ = self.collect(api)
        self.assertTrue(semantic)
        bad_env = self.env(True)
        bad_env["DRIFT_DATA_FILES"] = '{"requirements":"../secret","summary":"records/status.md"}'
        result = self.collect(api, bad_env)
        self.assertEqual(result, ([], [], ["data_source_unavailable"], True))


if __name__ == "__main__":
    unittest.main()


LEDGER = """
version = 2
[[question]]
id = "Q1"
rev = 1
title = "Read-only mode"
asked = "records/questions.md#1"
[[question]]
id = "Q2"
rev = 1
title = "Gate identity"
asked = "records/questions.md#2"
affects = "making the gate required"
[[answer]]
id = "D1"
rev = 1
questions = ["Q1"]
authority = "owner"
recorded_by = "agent-r"
quote = "owner words"
outcome = "decided"
given = "day 1"
source = "records/decisions.md #1"
summary = "Decline read-only mode"
[[answer]]
id = "D1b"
rev = 1
questions = ["Q1"]
authority = "owner"
recorded_by = "agent-r"
quote = "owner words"
outcome = "decided"
given = "day 2"
source = "records/board.md"
summary = "Accept read-only mode"
supersedes = ["D1"]
"""


class DecisionDataTests(unittest.TestCase):
    def env(self, documents=("records/status-a.md",)):
        files = {"requirements": "records/map.toml", "summary": "records/status.md",
                 "ledger": "records/ledger.toml", "documents": list(documents)}
        return {"DRIFT_DATA_REPOSITORY": "private-team/DO_NOT_PUBLISH_REPOSITORY",
                "DRIFT_DATA_TOKEN": "DO_NOT_PUBLISH_TOKEN", "DRIFT_DATA_FILES": json.dumps(files)}

    def api(self, claim):
        base = DataTests.api(DataTests())
        base.files.update({"records/ledger.toml": LEDGER, "records/status-a.md": claim})
        return base

    def collect(self, api, env=None):
        return data.data_evidence(copy.deepcopy(SOURCE), env or self.env(), lambda _r, _t: api)

    def test_disputed_claim_its_answer_and_the_supersession_share_one_packet(self):
        api = self.api("intro\nWe are still awaiting the owner on Q1 and Q2.\nmore\n")
        _, semantic, limits, failed = self.collect(api)
        self.assertFalse(failed)
        packets = [row for row in semantic if row["id"].startswith("decision:")]
        self.assertEqual(len(packets), 1)
        joined = "\n".join(packets[0]["excerpts"])
        self.assertIn("awaiting the owner on Q1", joined)        # the claim
        self.assertIn('"answer": "D1b"', joined)                   # the applicable answer
        self.assertIn("Decline read-only mode", joined)         # what it superseded
        self.assertIn("decision_record_findings", limits)
        self.assertTrue(packets[0]["private"])
        self.assertEqual(packets[0]["questions"], ["resolution", "deferral"])
        self.assertEqual(evaluate({"source": SOURCE, "semantic": packets})["violations"], [])
        published = json.dumps(evaluate({"source": SOURCE, "semantic": packets})["packets"])
        self.assertNotIn("DO_NOT_PUBLISH", published)
        self.assertNotIn("Q1", packets[0]["id"])                 # keyed, not a guessable tuple
        self.assertNotIn('"path"', packets[0]["excerpts"][-1])   # no private path in the finding

    def test_unanswered_question_raised_as_open_is_not_a_finding(self):
        _, semantic, limits, _ = self.collect(self.api("Q2 is still awaiting the owner.\n"))
        self.assertEqual([row for row in semantic if row["id"].startswith("decision:")], [])
        self.assertNotIn("decision_record_findings", limits)

    def test_data_only_correction_is_reevaluated_at_the_new_revision(self):
        api = self.api("Q1 is pending.\n")
        _, before, _, _ = self.collect(api)
        api.files["records/status-a.md"] = "Q1 decided (D1b).\n"
        api.commits["HEAD"] = "d" * 40
        _, after, limits, _ = self.collect(api)
        self.assertTrue([row for row in before if row["id"].startswith("decision:")])
        self.assertEqual([row for row in after if row["id"].startswith("decision:")], [])
        self.assertTrue(all(row["reconstruction"]["data_ref"] == "d" * 40 for row in after))
        self.assertNotIn("decision_record_findings", limits)

    def test_missing_or_invalid_decision_inputs_are_reported_not_passed(self):
        api = self.api("Q1 is pending.\n")
        api.files["records/ledger.toml"] = "version = 9\n"
        self.assertIn("decision_ledger_invalid", self.collect(api)[2])
        api.files["records/ledger.toml"] = KeyError("gone")
        self.assertIn("decision_ledger_unavailable", self.collect(api)[2])
        api = self.api("x")
        api.files["records/status-a.md"] = KeyError("gone")
        self.assertIn("decision_document_unavailable", self.collect(api)[2])
        self.assertIn("decision_documents_not_configured", self.collect(self.api("x"), self.env(()))[2])
        plain = DataTests().env()
        self.assertIn("decision_data_not_configured", self.collect(self.api("x"), plain)[2])
