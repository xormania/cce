#!/usr/bin/env python3
"""Run the qualification tiers on one candidate and record the results.

    python3 tools/tiers/run_tiers.py --output DIR [--tier NAME ...] [--fresh-clone]
                                     [--timeout SECONDS] [--repo PATH]

Each tier is a named check of the candidate (see docs/testing.md, "Qualification tiers"). The
runner records, in DIR/results.json (rewritten after every tier), the candidate commit and whether
its working tree was dirty, the toolchain, and for every tier its exact command, environment
additions, timing, exit status, outcome, test counts, every executed test with its result, and the
path and SHA-256 of its log. It exits 0 only if every selected tier passed.

A Cargo tier passes only if Cargo exited 0, at least one test ran, no test failed, and every
ignored test is one this file declares as ignored on purpose (with the reason and the tier that
runs it). `--fresh-clone` runs the tiers in a new clone of the candidate commit with a new target
directory, so no build output or untracked file of the working copy takes part.

Standard library only. CARGO_BUILD_JOBS and RUST_TEST_THREADS are passed through to Cargo and
recorded; set them on a shared machine.
"""
import argparse
import datetime
import hashlib
import json
import os
import re
import subprocess
import sys
import tempfile
import time
from pathlib import Path

SCHEMA = 1

# Tests ignored in an ordinary run on purpose: the tier that runs them, or why none does.
DECLARED_IGNORED = {
    "cce-storage:workloads:metadata_beyond_the_cache_with_a_high_degree_node_pages_correctly":
        "bounded-resources",
    "cce-storage:workloads:large_content_streams_through_one_segment_buffer": "bounded-resources",
    "cce-storage:workloads:a_fixed_live_set_churned_reuses_space_and_only_history_grows":
        "bounded-resources",
    "cce-storage:workloads:opening_and_point_and_range_reads_do_not_grow_with_the_store":
        "bounded-resources",
    "cce-storage:golden_fixtures:regenerate_the_format_5_fixture":
        "maintenance action: rewrites the committed format fixture",
    "cce-storage:lib:fuzz_replay_tests::write_seeds":
        "maintenance action: rewrites the committed fuzz seeds",
}

# The campaign selection: staged batches, each one test run with its own budget and seed, each
# printing one report line. Every batch must run its test and meet its own checks; the totals must
# meet CAMPAIGN_MINIMUMS.
MODEL_SEEDS = ["0x5eed0001cce00001", "0x5eed0001cce00002", "0x5eed0001cce00003",
               "0x5eed0001cce00004"]
CRASH_SEEDS = ["0x5eed0002cce00002", "0x5eed0002cce00003"]
MODEL = "model_tests::"


def _batches():
    b = []
    for seed in MODEL_SEEDS:
        b.append({"test": MODEL + "generated_operation_sequences_match_the_logical_model",
                  "report": "model", "cases": 250, "total": "generated_sequences",
                  "env": {"CCE_MODEL_CASES": "250", "CCE_MODEL_SEED": seed}})
        b.append({"test": MODEL + "generated_tree_reshaping_sequences_match_the_logical_model",
                  "report": "model", "cases": 250,
                  "env": {"CCE_MODEL_SHAPE_CASES": "250", "CCE_MODEL_SEED": seed}})
    for seed in MODEL_SEEDS[:2]:
        b.append({"test": MODEL + "generated_operation_sequences_on_real_files_match_the_"
                  "logical_model",
                  "report": "model", "cases": 32,
                  "env": {"CCE_MODEL_FS_CASES": "32", "CCE_MODEL_SEED": seed}})
    for seed in CRASH_SEEDS:
        b.append({"test": MODEL + "every_cut_of_short_generated_sequences_recovers_a_model_state",
                  "report": "crash", "cuts": "every",
                  "env": {"CCE_CRASH_CASES": "64", "CCE_CRASH_SEED": seed}})
        b.append({"test": MODEL + "sampled_cuts_of_long_generated_sequences_recover_a_model_"
                  "state", "report": "crash", "cuts": 48,
                  "env": {"CCE_CRASH_LONG_CASES": "16", "CCE_CRASH_SAMPLED_CUTS": "48",
                          "CCE_CRASH_SEED": seed}})
        b.append({"test": MODEL + "an_io_failure_at_every_cut_of_short_generated_sequences_is_"
                  "reported_and_recovered", "report": "crash", "cuts": "every",
                  "env": {"CCE_FAILURE_CASES": "64", "CCE_CRASH_SEED": seed}})
        b.append({"test": MODEL + "an_io_failure_at_sampled_cuts_of_long_generated_sequences_is_"
                  "reported_and_recovered", "report": "crash", "cuts": 48,
                  "env": {"CCE_FAILURE_LONG_CASES": "16", "CCE_FAILURE_SAMPLED_CUTS": "48",
                          "CCE_CRASH_SEED": seed}})
    b.append({"test": "fuzz_replay_tests::seeded_mutations_of_the_seeds_run_through_every_decoder",
              "report": "decoder", "total": "decoder_cases",
              "env": {"CCE_DECODER_CASES": "10000"}})
    return b


CAMPAIGN_BATCHES = _batches()
CAMPAIGN_MINIMUMS = {
    "generated_sequences": 1000,
    "operations_per_sequence": 64,
    "decoder_cases": 10000,
}

TIERS = {
    "format": {
        "kind": "command",
        "argv": ["cargo", "fmt", "--all", "--", "--check"],
        "default": True,
    },
    "lint": {
        "kind": "command",
        "argv": ["cargo", "clippy", "--locked", "--workspace", "--all-targets", "--",
                 "--deny", "warnings"],
        "default": True,
    },
    "routine": {
        "kind": "cargo-test",
        "args": ["--workspace", "--locked", "--no-fail-fast"],
        "test_args": [],
        "default": True,
    },
    "release-story": {
        "kind": "cargo-test",
        "args": ["--release", "--locked", "-p", "cce-storage", "--test", "story_release"],
        "test_args": [],
        "default": True,
    },
    "bounded-resources": {
        "kind": "cargo-test",
        "args": ["--release", "--locked", "-p", "cce-storage", "--test", "workloads"],
        "test_args": ["--ignored", "--test-threads=1", "--nocapture"],
        "default": True,
    },
    "tooling": {
        "kind": "command",
        "argv": [sys.executable, "-m", "unittest", "discover", "-s", "tests/ci"],
        "default": True,
    },
    "campaigns": {
        "kind": "campaigns",
        "args": ["--release", "--locked", "-p", "cce-storage", "--lib"],
        "default": False,
    },
    "planted-defects": {"kind": "planted-defects", "default": False},
}

RUNNING = re.compile(r"^\s+Running (?:unittests )?(\S+) \((.+)\)\s*$")
DOCTESTS = re.compile(r"^\s+Doc-tests (\S+)\s*$")
TEST_LINE = re.compile(r"^test (.+?) \.\.\. (ok|FAILED|ignored)\b")
RESULT_LINE = re.compile(
    r"^test result: (?:ok|FAILED)\. (\d+) passed; (\d+) failed; (\d+) ignored; "
    r"(\d+) measured; (\d+) filtered out"
)
REPORT_LINE = re.compile(r"^(model|crash|decoder) report: (\{.*\})\s*$")


def now():
    return datetime.datetime.now(datetime.timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")


def out(argv, cwd):
    r = subprocess.run(argv, cwd=cwd, capture_output=True, text=True)
    return r.stdout.strip() if r.returncode == 0 else None


def package_name(package_id):
    """The package name in a Cargo package id (`path+file:///…/name#1.0` or `…#name@1.0`)."""
    base, _, frag = package_id.partition("#")
    if "@" in frag:
        return frag.split("@", 1)[0]
    return base.rstrip("/").rsplit("/", 1)[-1]


def executables(json_lines):
    """Test executable file name -> (package, target) from `cargo test --no-run` JSON output.
    Library unit tests are target `lib`; doc tests are attributed by crate name separately."""
    found, crates = {}, {}
    for line in json_lines:
        try:
            d = json.loads(line)
        except ValueError:
            continue
        if d.get("reason") != "compiler-artifact":
            continue
        pkg = package_name(d["package_id"])
        kinds = d["target"]["kind"]
        if "lib" in kinds:
            crates[d["target"]["name"]] = pkg
        if d.get("executable") and d["profile"].get("test"):
            target = "lib" if "lib" in kinds else d["target"]["name"]
            found[os.path.basename(d["executable"])] = (pkg, target)
    return found, crates


def parse_test_output(text, exes, crates):
    """Attributes every `test … ok|FAILED|ignored` line to the binary Cargo said it was running.
    Returns (tests, binaries): tests maps `package:target:name` to its result; binaries lists
    per-binary counts from its `test result:` line."""
    tests, binaries, current = {}, [], None
    for line in text.splitlines():
        m = RUNNING.match(line)
        if m:
            pkg, target = exes.get(os.path.basename(m.group(2)), ("?", m.group(1)))
            current = f"{pkg}:{target}"
            continue
        m = DOCTESTS.match(line)
        if m:
            current = f"{crates.get(m.group(1), m.group(1))}:doc"
            continue
        m = TEST_LINE.match(line)
        if m and current:
            tests[f"{current}:{m.group(1)}"] = m.group(2).lower()
            continue
        m = RESULT_LINE.match(line)
        if m and current:
            p, f, i, _, filt = (int(x) for x in m.groups())
            binaries.append(
                {"binary": current, "passed": p, "failed": f, "ignored": i, "filtered": filt}
            )
    return tests, binaries


def qualify(exit_code, tests, binaries, declared=DECLARED_IGNORED):
    """Outcome of a Cargo tier and the problems behind a failed one."""
    problems = []
    executed = sum(b["passed"] + b["failed"] for b in binaries)
    failed = sorted(t for t, r in tests.items() if r == "failed")
    unexpected = sorted(t for t, r in tests.items() if r == "ignored" and t not in declared)
    if failed:
        problems.append({"failed": failed})
    if unexpected:
        problems.append({"unexpected_ignored": unexpected})
    if executed == 0:
        problems.append({"no_tests": "no test executed"})
    if exit_code != 0 and not failed:
        problems.append({"exit": exit_code})
    if failed or exit_code != 0:
        outcome = "failed"
    elif unexpected:
        outcome = "unexpected-ignored"
    elif executed == 0:
        outcome = "no-tests"
    else:
        outcome = "passed"
    return outcome, problems


def reports(text):
    """The `<kind> report: {json}` lines of a run, by kind."""
    found = {"model": [], "crash": [], "decoder": []}
    for line in text.splitlines():
        m = REPORT_LINE.match(line)
        if m:
            try:
                found[m.group(1)].append(json.loads(m.group(2)))
            except ValueError:
                pass
    return found


def check_batch(batch, found, minimums=CAMPAIGN_MINIMUMS):
    """Checks one batch's report against the batch. Returns (the report or None, gaps)."""
    lines = found[batch["report"]]
    if len(lines) != 1:
        return None, [f"{len(lines)} {batch['report']} reports, expected 1"]
    r, gaps = lines[0], []
    if not r.get("passed"):
        gaps.append("the report says the campaign failed")
    if batch["report"] == "model":
        if r.get("cases") != batch["cases"]:
            gaps.append(f"cases {r.get('cases')} != {batch['cases']}")
        if r.get("sequences_run", 0) < batch["cases"]:
            gaps.append(f"sequences run {r.get('sequences_run', 0)} < {batch['cases']}")
        shortest = (r.get("operations_per_sequence") or [0])[0]
        if shortest < minimums["operations_per_sequence"]:
            gaps.append(f"sequences of {shortest} operations < "
                        f"{minimums['operations_per_sequence']}")
        if "rejected_cases" not in r:
            gaps.append("the report does not count rejected generated cases")
        elif r["rejected_cases"] != 0:
            gaps.append(f"{r['rejected_cases']} generated cases rejected")
    elif batch["report"] == "crash":
        if batch["cuts"] == "every":
            if r.get("cuts_executed", 0) < r.get("cut_points_available", 1):
                gaps.append(f"cuts executed {r.get('cuts_executed', 0)} < cut points "
                            f"{r.get('cut_points_available')}")
        else:
            want = r.get("cases", 0) * batch["cuts"]
            if r.get("cuts_executed") != want:
                gaps.append(f"cuts executed {r.get('cuts_executed')} != {want}")
        for k in ("recovered_with_step", "recovered_without_step"):
            if not r.get(k):
                gaps.append(f"no case {k.replace('_', ' ')}")
    return r, gaps


def campaign_totals(batches, minimums=CAMPAIGN_MINIMUMS):
    """Totals over checked batches (`report` set by `check_batch`) and the minimums they miss."""
    totals = {"generated_sequences": 0, "decoder_cases": 0, "attempted_operations": 0}
    for b in batches:
        r = b.get("report_line")
        if not r or not r.get("passed"):
            continue
        totals["attempted_operations"] += r.get("attempted_operations", 0)
        if b["batch"].get("total") == "generated_sequences":
            totals["generated_sequences"] += r.get("cases", 0)
        if b["batch"].get("total") == "decoder_cases":
            totals["decoder_cases"] += r.get("cases", 0)
    gaps = [f"{k.replace('_', ' ')} {totals[k]} < {minimums[k]}"
            for k in ("generated_sequences", "decoder_cases") if totals[k] < minimums[k]]
    return totals, gaps


def sha256_file(path):
    h = hashlib.sha256()
    with open(path, "rb") as f:
        for chunk in iter(lambda: f.read(1 << 20), b""):
            h.update(chunk)
    return h.hexdigest()


def run_logged(argv, cwd, env, log, timeout):
    """Runs `argv`, writing combined output to `log`. Returns (exit code or None, text)."""
    with open(log, "w") as f:
        try:
            r = subprocess.run(argv, cwd=cwd, env=env, stdout=f, stderr=subprocess.STDOUT,
                               timeout=timeout)
            code = r.returncode
        except subprocess.TimeoutExpired:
            code = None
    return code, Path(log).read_text(errors="replace")


class Runner:
    def __init__(self, workspace, output, timeout, fresh):
        self.ws = workspace
        self.output = output
        self.timeout = timeout
        self.logs = output / "logs"
        self.logs.mkdir(parents=True)
        self.env = dict(os.environ)
        if fresh:
            self.env["CARGO_TARGET_DIR"] = str(output / "target")
        self.results = {
            "schema": SCHEMA,
            "runner_sha256": sha256_file(__file__),
            "started": now(),
            "fresh_clone": fresh,
            "candidate": self.candidate(),
            "toolchain": {
                "rustc": out(["rustc", "-vV"], workspace),
                "cargo": out(["cargo", "-V"], workspace),
                "python": sys.version.split()[0],
            },
            "environment": {
                k: v for k, v in sorted(os.environ.items())
                if k in ("CARGO_BUILD_JOBS", "RUST_TEST_THREADS") or k.startswith("CCE_")
            },
            "tiers": [],
        }

    def candidate(self):
        commit = out(["git", "rev-parse", "HEAD"], self.ws)
        status = out(["git", "status", "--porcelain", "--untracked-files=all"], self.ws)
        c = {"commit": commit, "dirty": bool(status)}
        if status:
            files = out(["git", "ls-files", "-co", "--exclude-standard"], self.ws) or ""
            c["files_sha256"] = {
                f: sha256_file(self.ws / f)
                for f in sorted(files.splitlines()) if (self.ws / f).is_file()
            }
        return c

    def save(self):
        tmp = self.output / "results.json.tmp"
        tmp.write_text(json.dumps(self.results, indent=1) + "\n")
        tmp.replace(self.output / "results.json")

    def run(self, names):
        ok = True
        for name in names:
            tier = TIERS[name]
            record = {"name": name, "started": now()}
            t0 = time.monotonic()
            try:
                getattr(self, "tier_" + tier["kind"].replace("-", "_"))(name, tier, record)
            except Exception as e:  # recorded, never hidden
                record.update(outcome="error", problems=[{"error": repr(e)}])
            record["seconds"] = round(time.monotonic() - t0, 1)
            record["finished"] = now()
            self.results["tiers"].append(record)
            self.save()
            ok = ok and record["outcome"] == "passed"
            print(f"{name}: {record['outcome']}", flush=True)
        self.results["finished"] = now()
        self.results["passed"] = ok
        self.save()
        return ok

    def logged(self, name, argv, record, env_add=None, suffix=""):
        env = dict(self.env, **(env_add or {}))
        log = self.logs / f"{name}{suffix}.log"
        code, text = run_logged(argv, self.ws, env, log, self.timeout)
        record.setdefault("commands", []).append(
            {"argv": argv, "env": env_add or {}, "exit": code,
             "log": str(log.relative_to(self.output)), "log_sha256": sha256_file(log)}
        )
        return code, text

    def tier_command(self, name, tier, record):
        code, _ = self.logged(name, tier["argv"], record)
        record["outcome"] = "passed" if code == 0 else ("timed-out" if code is None else "failed")

    def cargo_tests(self, name, tier, record):
        env_add = tier.get("env")
        _, listing = self.logged(
            name, ["cargo", "test", *tier["args"], "--no-run", "--message-format=json"],
            record, env_add, "-build")
        exes, crates = executables(listing.splitlines())
        argv = ["cargo", "test", *tier["args"], "--", *tier["test_args"]]
        code, text = self.logged(name, argv, record, env_add)
        tests, binaries = parse_test_output(text, exes, crates)
        record["counts"] = {
            "binaries": binaries,
            "passed": sum(b["passed"] for b in binaries),
            "failed": sum(b["failed"] for b in binaries),
            "ignored": sum(b["ignored"] for b in binaries),
            "filtered": sum(b["filtered"] for b in binaries),
        }
        record["tests"] = tests
        return code, text, tests, binaries

    def tier_cargo_test(self, name, tier, record):
        code, _, tests, binaries = self.cargo_tests(name, tier, record)
        if code is None:
            record.update(outcome="timed-out", problems=[{"timeout": self.timeout}])
            return
        record["outcome"], record["problems"] = qualify(code, tests, binaries)

    def tier_campaigns(self, name, tier, record):
        """Runs each batch as its own test run; a batch whose test is absent, fails, or whose
        report falls short is a gap, and so is a total below its minimum."""
        _, listing = self.logged(
            name, ["cargo", "test", *tier["args"], "--no-run", "--message-format=json"],
            record, None, "-build")
        exes, crates = executables(listing.splitlines())
        batches, gaps, failed = [], [], False
        for i, batch in enumerate(CAMPAIGN_BATCHES):
            argv = ["cargo", "test", *tier["args"], "--", "--exact", batch["test"],
                    "--nocapture"]
            code, text = self.logged(name, argv, record, batch["env"], f"-{i:02}")
            tests, binaries = parse_test_output(text, exes, crates)
            outcome, problems = qualify(code if code is not None else -1, tests, binaries)
            line, batch_gaps = check_batch(batch, reports(text))
            if outcome == "no-tests":
                batch_gaps = ["the test does not exist in this candidate"]
            failed = failed or outcome == "failed" or code is None
            batches.append({"batch": batch, "outcome": outcome, "problems": problems,
                            "report_line": line, "gaps": batch_gaps, "tests": tests})
            gaps += [f"{batch['test']} {batch['env']}: {g}" for g in batch_gaps]
        totals, total_gaps = campaign_totals(batches)
        record["minimums"] = CAMPAIGN_MINIMUMS
        record["batches"] = batches
        record["totals"] = totals
        record["gaps"] = gaps + total_gaps
        record["outcome"] = ("failed" if failed else
                             "incomplete" if record["gaps"] else "passed")

    def tier_planted_defects(self, name, tier, record):
        commit = self.results["candidate"]["commit"]
        if self.results["candidate"]["dirty"]:
            record.update(outcome="failed",
                          problems=[{"dirty": "planted defects run on a committed candidate"}])
            return
        report = self.output / "planted-defects"
        argv = [sys.executable, "tools/testkit/sidebyside.py", "--repo", str(self.ws),
                "--original", commit, "--candidate", commit, "--production", commit,
                "--output", str(report)]
        code, _ = self.logged(name, argv, record)
        summary = report / "results.json"
        if summary.is_file():
            record["report"] = str(summary.relative_to(self.output))
            record["report_sha256"] = sha256_file(summary)
        record["outcome"] = "passed" if code == 0 else ("timed-out" if code is None else "failed")


def fresh_clone(repo, output):
    """A new clone of `repo`'s HEAD commit under `output`, detached at that commit."""
    commit = out(["git", "rev-parse", "HEAD"], repo)
    dest = output / "checkout"
    subprocess.run(["git", "clone", "--quiet", "--no-local", "--no-checkout", str(repo),
                    str(dest)], check=True)
    subprocess.run(["git", "-C", str(dest), "checkout", "--quiet", "--detach", commit],
                   check=True)
    return dest


def main(argv=None):
    p = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    p.add_argument("--output", required=True, type=Path,
                   help="new directory for results.json and the logs")
    p.add_argument("--tier", action="append", choices=sorted(TIERS),
                   help="tier to run (repeatable); default: every default tier")
    p.add_argument("--repo", type=Path, default=Path(__file__).resolve().parents[2])
    p.add_argument("--fresh-clone", action="store_true",
                   help="run in a new clone of the candidate commit with a new target directory")
    p.add_argument("--timeout", type=float, default=None, help="seconds per command")
    a = p.parse_args(argv)
    if a.output.exists():
        p.error(f"{a.output} exists; the output directory must be new")
    a.output.mkdir(parents=True)
    output = a.output.resolve()
    ws = fresh_clone(a.repo.resolve(), output) if a.fresh_clone else a.repo.resolve()
    names = a.tier or [n for n, t in TIERS.items() if t["default"]]
    ok = Runner(ws, output, a.timeout, a.fresh_clone).run(names)
    print(f"results: {output / 'results.json'}")
    return 0 if ok else 1


if __name__ == "__main__":
    sys.exit(main())
