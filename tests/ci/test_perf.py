"""Check the performance comparison's parsing, inventory and verdicts on fixed trial data."""
from contextlib import redirect_stderr
import io
import json
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT / "tools/perf"))
import run_perf as p

SCENARIOS = ["point-read"]


def perf_line(cost=7, info=3, work=100, p50=1000, version=1):
    return "PERF " + json.dumps({
        "scenario": "point-read", "version": version, "work": {"reads": work},
        "cost": {"read_pages_read": cost}, "info": {"tree_height": info},
        "timing": {"open": {"ns": 5000},
                   "read": {"count": work, "p50_ns": p50, "p95_ns": 2 * p50, "max_ns": 9 * p50,
                            "total_ns": work * p50}},
    })


def workload_lines(pages=50, rss=4000, elapsed=600):
    records = {
        "metadata": {"hub_edges": 20000, "sparse_nodes": 5000, "cache_nodes": 256,
                     "space": {"live_pages": 9, "meta_file": 4096},
                     "stats": {"pages_read": pages, "entries_visited": 1, "misses": 1,
                               "evictions": 1, "height": 3, "pages_read_at_open": 5},
                     "peak_rss_kib": rss, "rss_kib": rss, "fds": 7, "elapsed_ms": elapsed},
        "content": {"content_bytes": 1, "segment_bytes": 1, "buffer_peak_staging": 1,
                    "buffer_peak_reading": 1, "peak_rss_kib_staging": rss,
                    "peak_rss_kib_reading": rss, "rss_kib": rss, "fds": 7,
                    "elapsed_ms": elapsed},
        "churn": {"records": 1, "content_bytes_each": 1, "rounds": 2, "live_content_bytes": 1,
                  "stats": {"pages_written": 1, "page_bytes": 1, "content_bytes": 1,
                            "pages_read": 1, "entries_visited": 1, "misses": 1,
                            "evictions": 0},
                  "space_per_round": [{"meta_file": 1, "content_file": 1, "live_pages": 1},
                                      {"meta_file": 2, "content_file": 2, "live_pages": 2}],
                  "peak_rss_kib": rss, "rss_kib": rss, "fds": 7, "elapsed_ms": elapsed},
        "scale": {"n": 1, "n_large": 8,
                  "at_n": {"height": 2, "pages_read_at_open": 4, "point_pages_read": 1,
                           "point_entries": 1, "range_pages_read": 0, "range_entries": 40},
                  "at_8n": {"height": 3, "pages_read_at_open": 5, "point_pages_read": 1,
                            "point_entries": 1, "range_pages_read": 1, "range_entries": 40},
                  "elapsed_ms": elapsed},
    }
    return "\n".join(f"test x ... WORKLOAD {json.dumps(dict(workload=k, **v))}"
                     for k, v in records.items())


def trial(perf=None, workloads=None):
    perf_records, perf_problems = p.parse_perf(perf if perf is not None else perf_line())
    wl_records, wl_problems = p.parse_workloads(
        workloads if workloads is not None else workload_lines())
    assert not perf_problems and not wl_problems
    return {"ok": True, "perf": perf_records, "workloads": wl_records}


def trials(n=3, **kw):
    return [trial(**kw) for _ in range(n)]


class ParsingTests(unittest.TestCase):
    def test_records_are_read_by_name_and_the_last_churn_round_is_kept(self):
        t = trial()
        self.assertEqual(set(t["perf"]), {"point-read"})
        self.assertEqual(set(t["workloads"]), {"metadata", "content", "churn", "scale"})
        self.assertEqual(t["workloads"]["churn"]["space_last"]["meta_file"], 2)
        m = p.trial_metrics(t)
        self.assertEqual(m["perf.point-read.cost.read_pages_read"], ("cost", 7))
        self.assertEqual(m["perf.point-read.timing.read.p50_ns"], ("timing", 1000))
        self.assertEqual(m["workload.metadata.stats.pages_read"], ("cost", 50))
        self.assertEqual(m["workload.metadata.cache_nodes"], ("info", 256))
        self.assertEqual(m["workload.churn.space_last.meta_file"], ("cost", 2))
        self.assertEqual(m["workload.metadata.peak_rss_kib"], ("advisory", 4000))
        self.assertEqual(m["workload.scale.elapsed_ms"], ("timing", 600))

    def test_unreadable_and_repeated_records_are_problems(self):
        _, problems = p.parse_perf("PERF {not json\n" + perf_line() + "\n" + perf_line())
        self.assertEqual(len(problems), 2)
        _, problems = p.parse_workloads("WORKLOAD {\"no\": \"name\"}")
        self.assertEqual(len(problems), 1)


class VerdictTests(unittest.TestCase):
    def verdict(self, base, candidate):
        return p.analyze(base, candidate, SCENARIOS)

    def test_identical_counters_pass(self):
        status, reasons, rows = self.verdict(trials(), trials())
        self.assertEqual((status, reasons), ("passed", []))
        self.assertTrue(all(r["result"] in ("same", "reported") for r in rows))

    def test_a_higher_cost_counter_is_a_regression(self):
        status, reasons, rows = self.verdict(trials(), trials(perf=perf_line(cost=8)))
        self.assertEqual(status, "regressed")
        self.assertIn("perf.point-read.cost.read_pages_read", reasons[-1])
        status, _, _ = self.verdict(trials(), trials(workloads=workload_lines(pages=51)))
        self.assertEqual(status, "regressed")

    def test_a_lower_cost_counter_is_an_improvement(self):
        status, _, rows = self.verdict(trials(), trials(perf=perf_line(cost=6)))
        self.assertEqual(status, "passed")
        row = next(r for r in rows if r["metric"].endswith("read_pages_read"))
        self.assertEqual(row["result"], "improved")

    def test_a_changed_info_counter_is_reported_without_failing(self):
        status, _, rows = self.verdict(trials(), trials(perf=perf_line(info=4)))
        self.assertEqual(status, "passed")
        row = next(r for r in rows if r["metric"].endswith("tree_height"))
        self.assertEqual(row["result"], "changed")

    def test_timing_never_decides(self):
        slow = trials(perf=perf_line(p50=10_000), workloads=workload_lines(rss=9000,
                                                                           elapsed=6000))
        status, _, rows = self.verdict(trials(), slow)
        self.assertEqual(status, "passed")
        row = next(r for r in rows if r["metric"].endswith("read.p50_ns"))
        self.assertEqual((row["result"], row["ratio"]), ("reported", 10.0))

    def test_timing_varying_between_trials_is_accepted(self):
        base = [trial(perf=perf_line(p50=v)) for v in (900, 1000, 1100)]
        status, _, rows = self.verdict(base, trials())
        self.assertEqual(status, "passed")
        row = next(r for r in rows if r["metric"].endswith("read.p50_ns"))
        self.assertEqual(row["base"], {"median": 1000, "min": 900, "max": 1100, "trials": 3})

    def test_counters_that_do_not_repeat_are_invalid(self):
        noisy = [trial(perf=perf_line(cost=c)) for c in (7, 7, 8)]
        status, reasons, _ = self.verdict(trials(), noisy)
        self.assertEqual(status, "invalid")
        self.assertIn("differs between trials", reasons[0])

    def test_different_work_is_invalid_not_a_comparison(self):
        status, reasons, _ = self.verdict(trials(), trials(perf=perf_line(work=99)))
        self.assertEqual(status, "invalid")
        status, _, _ = self.verdict(trials(), trials(perf=perf_line(version=2)))
        self.assertEqual(status, "invalid")

    def test_invalid_outranks_regressed(self):
        cand = [trial(perf=perf_line(cost=c)) for c in (8, 8, 9)]
        cand[0]["workloads"]["metadata"]["stats"]["pages_read"] = 99
        status, _, _ = self.verdict(trials(), cand)
        self.assertEqual(status, "invalid")

    def test_a_missing_scenario_or_workload_is_incomplete(self):
        status, reasons, _ = self.verdict(trials(), trials(perf=""))
        self.assertEqual(status, "incomplete")
        self.assertTrue(any("perf.point-read.version" in r for r in reasons))
        without_scale = "\n".join(l for l in workload_lines().splitlines() if "scale" not in l)
        status, _, _ = self.verdict(trials(), trials(workloads=without_scale))
        self.assertEqual(status, "incomplete")

    def test_a_metric_missing_from_some_trials_is_incomplete(self):
        cand = trials()
        del cand[1]["workloads"]["content"]["buffer_peak_reading"]
        status, _, _ = self.verdict(trials(), cand)
        self.assertEqual(status, "incomplete")

    def test_a_cost_counter_only_the_candidate_reports_is_shown_without_failing(self):
        line = json.loads(perf_line()[5:])
        line["cost"]["read_new_counter"] = 4
        status, _, rows = self.verdict(trials(), trials(perf="PERF " + json.dumps(line)))
        self.assertEqual(status, "passed")
        row = next(r for r in rows if r["metric"].endswith("read_new_counter"))
        self.assertEqual(row["result"], "candidate only")

    def test_non_integer_or_negative_values_are_invalid(self):
        for bad in (-1, 1.5, "7", None, True):
            line = json.loads(perf_line()[5:])
            line["cost"]["read_pages_read"] = bad
            status, _, _ = self.verdict(trials(), trials(perf="PERF " + json.dumps(line)))
            self.assertIn(status, ("invalid", "incomplete"), bad)

    def test_no_trials_is_incomplete(self):
        self.assertEqual(self.verdict([], trials())[0], "incomplete")


class ReportTests(unittest.TestCase):
    def test_summary_leads_with_status_and_lists_regressions(self):
        status, reasons, rows = p.analyze(trials(), trials(perf=perf_line(cost=8)), SCENARIOS)
        text = p.summary({"status": status, "reasons": reasons, "comparison": rows,
                          "revisions": {"base": "b", "candidate": "c"}, "trials": 3, "seed": 1})
        self.assertTrue(text.startswith("# Performance comparison: regressed"))
        self.assertIn("| `perf.point-read.cost.read_pages_read` | 7 | 8 | regressed |", text)

    def test_summary_without_comparison_says_so(self):
        text = p.summary({"status": "error", "reasons": ["the candidate does not build"]})
        self.assertIn("No comparison was made.", text)


class HarnessTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)

    def test_the_candidate_harness_replaces_the_base_harness(self):
        cand, base = self.root / "c", self.root / "b"
        for side, text in ((cand, "candidate"), (base, "base")):
            for rel in p.HARNESS:
                f = side / rel / "mod.rs" if not rel.endswith(".rs") else side / rel
                f.parent.mkdir(parents=True, exist_ok=True)
                f.write_text(text)
        (base / "crates/cce-storage/tests/common/stale.rs").write_text("base only")
        self.assertNotEqual(p.tree_digest(cand, p.HARNESS), p.tree_digest(base, p.HARNESS))
        p.install_harness(cand, base)
        self.assertEqual(p.tree_digest(cand, p.HARNESS), p.tree_digest(base, p.HARNESS))
        self.assertFalse((base / "crates/cce-storage/tests/common/stale.rs").exists())


class CommandTests(unittest.TestCase):
    def test_a_command_past_its_timeout_is_stopped_with_its_children(self):
        with tempfile.TemporaryDirectory() as d:
            log = Path(d) / "logs" / "slow.log"
            r = p.run(["sh", "-c", "sleep 30 & sleep 30"], d, log, timeout=1)
            self.assertTrue(r["timed_out"])
            self.assertNotEqual(r["exit"], 0)
            self.assertLess(r["seconds"], 15)
            left = subprocess.run(["pgrep", "-f", "sleep 30"], capture_output=True, text=True)
            self.assertEqual(left.stdout.strip(), "")

    def test_two_trials_are_required(self):
        with self.assertRaises(SystemExit), redirect_stderr(io.StringIO()):
            p.main(["--output", "/nonexistent/x", "--trials", "1"])


if __name__ == "__main__":
    unittest.main()
