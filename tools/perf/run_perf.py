#!/usr/bin/env python3
"""Compare the performance of a candidate revision against a base revision.

    python3 tools/perf/run_perf.py --output DIR [--base REF] [--candidate REF] [--trials N]
                                   [--seed N] [--timeout SECONDS] [--repo PATH]

Both revisions are checked out as detached worktrees under DIR. The measurement harness (the
`perf` example, the workload tests, their shared support and the storage driver) is copied from
the candidate into the base checkout, so both sides run the same harness against their own
storage code. Both are built in release mode. Each side then runs N independent trials, in an
interleaved order drawn from the recorded seed; a trial is one `perf` process over every scenario
followed by one run of the workload tests, each in a fresh directory.

Engine counters decide the result. They must repeat exactly across one side's trials; a cost
counter (lower is better) that is higher for the candidate is a regression. Timings are reported
with their median and range across trials and never change the result.

DIR/results.json holds the identities, toolchain, environment, seed, every trial with its parsed
output, the comparison and the status; DIR/summary.md is the readable report and DIR/logs/ holds
every command's output. The status is one of passed, regressed, incomplete, invalid,
not-comparable or error, and the exit status is 0 only for passed.

Standard library only.
"""
import argparse
import datetime
import hashlib
import json
import os
import platform
import random
import re
import shutil
import signal
import statistics
import subprocess
import sys
import time
from pathlib import Path

SCHEMA = 1

# Files that make up the measurement harness. They are taken from the candidate for both sides.
HARNESS = [
    "crates/cce-storage/examples/perf.rs",
    "crates/cce-storage/tests/workloads.rs",
    "crates/cce-storage/tests/common",
    "crates/cce-storage/src/bin/storage-driver.rs",
]

PERF_BUILD = ["cargo", "build", "--release", "--locked", "-p", "cce-storage", "--example", "perf"]
WORKLOADS = ["cargo", "test", "--release", "--locked", "-p", "cce-storage", "--test", "workloads"]
WORKLOAD_ARGS = ["--", "--ignored", "--test-threads=1", "--nocapture"]

# The workloads printed by tests/workloads.rs, and how each reported value is treated: `work` values
# describe the inputs and must be equal on both sides, `cost` values are counters where lower is
# better, `advisory` values (memory, descriptors, elapsed time) vary between runs and are reported
# only. Every other numeric value is `info`: compared and reported, with no direction.
WORKLOAD_FIELDS = {
    "metadata": {
        "work": ["hub_edges", "sparse_nodes"],
        "cost": ["stats.pages_read", "stats.entries_visited", "stats.misses", "stats.evictions",
                 "stats.height", "stats.pages_read_at_open", "space.live_pages",
                 "space.meta_file"],
        "advisory": ["peak_rss_kib", "rss_kib", "fds", "elapsed_ms"],
    },
    "content": {
        "work": ["content_bytes", "segment_bytes"],
        "cost": ["buffer_peak_staging", "buffer_peak_reading"],
        "advisory": ["peak_rss_kib_staging", "peak_rss_kib_reading", "rss_kib", "fds",
                     "elapsed_ms"],
    },
    "churn": {
        "work": ["records", "content_bytes_each", "rounds", "live_content_bytes"],
        "cost": ["stats.pages_written", "stats.page_bytes", "stats.content_bytes",
                 "stats.pages_read", "stats.entries_visited", "stats.misses", "stats.evictions",
                 "space_last.meta_file", "space_last.content_file", "space_last.live_pages"],
        "advisory": ["peak_rss_kib", "rss_kib", "fds", "elapsed_ms"],
    },
    "scale": {
        "work": ["n", "n_large"],
        "cost": [f"{s}.{k}" for s in ("at_n", "at_8n")
                 for k in ("height", "pages_read_at_open", "point_pages_read", "point_entries",
                           "range_pages_read", "range_entries")],
        "advisory": ["elapsed_ms"],
    },
}

STATUS_ORDER = ["error", "not-comparable", "invalid", "incomplete", "regressed", "passed"]


class Failure(Exception):
    """A run that cannot produce a comparison, with the status it ends in."""

    def __init__(self, status, reason):
        super().__init__(reason)
        self.status = status
        self.reason = reason


def now():
    return datetime.datetime.now(datetime.timezone.utc).isoformat(timespec="seconds")


# --- commands -------------------------------------------------------------------------------------

def group_alive(pgid):
    try:
        os.killpg(pgid, 0)
        return True
    except ProcessLookupError:
        return False
    except PermissionError:
        return True


def stop_group(proc, grace=5):
    """Stops every process in `proc`'s group: SIGTERM, then SIGKILL for whatever is left after
    `grace` seconds. The leader exiting does not end the wait; the group must be empty."""
    try:
        os.killpg(proc.pid, signal.SIGTERM)
    except ProcessLookupError:
        pass
    deadline = time.monotonic() + grace
    while time.monotonic() < deadline:
        proc.poll()
        if proc.returncode is not None and not group_alive(proc.pid):
            return
        time.sleep(0.05)
    try:
        os.killpg(proc.pid, signal.SIGKILL)
    except ProcessLookupError:
        pass
    proc.wait()


def run(command, cwd, log, timeout, env=None):
    """Runs `command` in its own process group, owning it until it and its children are gone."""
    started = time.monotonic()
    log.parent.mkdir(parents=True, exist_ok=True)
    with log.open("w") as stream:
        proc = subprocess.Popen(command, cwd=cwd, env=env, stdout=stream,
                                stderr=subprocess.STDOUT, start_new_session=True)
        timed_out = False
        try:
            proc.wait(timeout=timeout)
        except subprocess.TimeoutExpired:
            timed_out = True
        finally:
            if proc.poll() is None:
                stop_group(proc)
    return {
        "command": list(command),
        "exit": proc.returncode,
        "timed_out": timed_out,
        "seconds": round(time.monotonic() - started, 3),
        "log": str(log),
    }


def git(repo, *args):
    return subprocess.check_output(["git", "-C", str(repo), *args], text=True,
                                   stderr=subprocess.STDOUT).strip()


def resolve(repo, ref):
    try:
        return git(repo, "rev-parse", "--verify", "--end-of-options", ref + "^{commit}")
    except subprocess.CalledProcessError as e:
        raise Failure("error", f"cannot resolve {ref}: {e.output.strip()}") from e


def observe(command):
    try:
        return subprocess.check_output(command, text=True, stderr=subprocess.STDOUT).strip()
    except (OSError, subprocess.CalledProcessError):
        return None


def environment():
    cpu = None
    mem = None
    try:
        for line in Path("/proc/cpuinfo").read_text().splitlines():
            if line.startswith("model name"):
                cpu = line.split(":", 1)[1].strip()
                break
        for line in Path("/proc/meminfo").read_text().splitlines():
            if line.startswith("MemTotal"):
                mem = line.split(":", 1)[1].strip()
                break
    except OSError:
        pass
    ci = {k: os.environ[k] for k in ("GITHUB_RUN_ID", "GITHUB_RUN_ATTEMPT", "GITHUB_REF",
                                     "GITHUB_SHA", "RUNNER_OS", "RUNNER_ARCH", "ImageOS",
                                     "ImageVersion") if k in os.environ}
    return {
        "platform": platform.platform(),
        "machine": platform.machine(),
        "cpus_available": len(os.sched_getaffinity(0)) if hasattr(os, "sched_getaffinity")
        else os.cpu_count(),
        "cpu_model": cpu,
        "memory_total": mem,
        "python": platform.python_version(),
        "ci": ci,
        "cache_state": "uncontrolled: fresh process and fresh store per trial; the OS page cache "
                       "is not dropped",
    }


def tree_digest(root, paths):
    """SHA-256 over the relative path and bytes of every file under `paths`, in sorted order."""
    h = hashlib.sha256()
    files = []
    for p in paths:
        full = root / p
        if full.is_dir():
            files += sorted(f for f in full.rglob("*") if f.is_file())
        elif full.is_file():
            files.append(full)
    for f in sorted(files):
        h.update(str(f.relative_to(root)).encode() + b"\0" + f.read_bytes() + b"\0")
    return h.hexdigest()


def install_harness(candidate, base):
    """Replaces the base checkout's harness files with the candidate's."""
    for p in HARNESS:
        src, dst = candidate / p, base / p
        if dst.is_dir():
            shutil.rmtree(dst)
        elif dst.exists():
            dst.unlink()
        if src.is_dir():
            shutil.copytree(src, dst)
        elif src.exists():
            dst.parent.mkdir(parents=True, exist_ok=True)
            shutil.copy2(src, dst)


# --- parsing --------------------------------------------------------------------------------------

def flatten(value, prefix=""):
    out = {}
    for k, v in value.items():
        key = f"{prefix}{k}"
        if isinstance(v, dict):
            out.update(flatten(v, key + "."))
        else:
            out[key] = v
    return out


def is_count(v):
    return isinstance(v, int) and not isinstance(v, bool) and v >= 0


def parse_perf(text):
    """The `PERF` records of one harness run, by scenario, or a list of problems."""
    records, problems = {}, []
    for line in text.splitlines():
        if not line.startswith("PERF "):
            continue
        try:
            r = json.loads(line[5:])
        except json.JSONDecodeError as e:
            problems.append(f"unreadable PERF line: {e}")
            continue
        name = r.get("scenario")
        if not isinstance(name, str) or name in records:
            problems.append(f"missing or repeated scenario name: {name!r}")
            continue
        records[name] = r
    return records, problems


def parse_workloads(text):
    records, problems = {}, []
    for line in text.splitlines():
        m = re.search(r"WORKLOAD (\{.*\})\s*$", line)
        if not m:
            continue
        try:
            r = json.loads(m.group(1))
        except json.JSONDecodeError as e:
            problems.append(f"unreadable WORKLOAD line: {e}")
            continue
        name = r.get("workload")
        if not isinstance(name, str) or name in records:
            problems.append(f"missing or repeated workload name: {name!r}")
            continue
        rounds = r.pop("space_per_round", None)
        if isinstance(rounds, list) and rounds:
            r["space_last"] = rounds[-1]
            r["rounds_reported"] = len(rounds)
        records[name] = r
    return records, problems


def perf_metrics(record):
    """Classified metrics of one scenario record: {name: (class, value)}."""
    s = record["scenario"]
    out = {f"perf.{s}.version": ("work", record.get("version"))}
    for cls in ("work", "cost", "info"):
        for k, v in flatten(record.get(cls, {})).items():
            out[f"perf.{s}.{cls}.{k}"] = (cls, v)
    for k, v in record.get("timing", {}).items():
        if isinstance(v, dict) and "p50_ns" in v:
            out[f"perf.{s}.timing.{k}.p50_ns"] = ("timing", v["p50_ns"])
            out[f"perf.{s}.timing.{k}.p95_ns"] = ("timing", v.get("p95_ns"))
            out[f"perf.{s}.timing.{k}.count"] = ("work", v.get("count"))
        elif isinstance(v, dict):
            out[f"perf.{s}.timing.{k}.ns"] = ("timing", v.get("ns"))
    return out


def workload_metrics(record):
    name = record["workload"]
    fields = WORKLOAD_FIELDS.get(name, {})
    out = {}
    for k, v in flatten({k: v for k, v in record.items() if k != "workload"}).items():
        if k in fields.get("work", []) or k == "rounds_reported":
            cls = "work"
        elif k in fields.get("cost", []):
            cls = "cost"
        elif k == "elapsed_ms":
            cls = "timing"
        elif k in fields.get("advisory", []):
            cls = "advisory"
        else:
            cls = "info"
        out[f"workload.{name}.{k}"] = (cls, v)
    return out


def trial_metrics(trial):
    out = {}
    for r in trial.get("perf", {}).values():
        out.update(perf_metrics(r))
    for r in trial.get("workloads", {}).values():
        out.update(workload_metrics(r))
    return out


def expected_inventory(scenarios):
    """Metric names every valid trial must report: every scenario's version and every declared
    workload cost and work field."""
    names = {f"perf.{s}.version" for s in scenarios}
    for w, fields in WORKLOAD_FIELDS.items():
        names |= {f"workload.{w}.{k}" for k in fields["work"] + fields["cost"]}
    return names


# --- analysis -------------------------------------------------------------------------------------

def side_values(trials, problems, side):
    """{metric: (class, [value per trial])} over a side's trials; problems are appended."""
    metrics = [trial_metrics(t) for t in trials]
    names = set().union(*metrics) if metrics else set()
    out = {}
    for name in sorted(names):
        classes = {m[name][0] for m in metrics if name in m}
        values = [m[name][1] if name in m else None for m in metrics]
        cls = classes.pop() if len(classes) == 1 else "info"
        if any(v is None for v in values):
            problems.append(("incomplete", f"{side}: {name} missing from some trials"))
            continue
        if not all(is_count(v) for v in values):
            problems.append(("invalid", f"{side}: {name} is not a non-negative integer: {values}"))
            continue
        if cls in ("work", "cost", "info") and len(set(values)) > 1:
            problems.append(("invalid", f"{side}: {name} differs between trials: {values}"))
            continue
        out[name] = (cls, values)
    return out


def timing_summary(values):
    return {"median": statistics.median(values), "min": min(values), "max": max(values),
            "trials": len(values)}


def analyze(base_trials, candidate_trials, scenarios):
    """Compares the two sides' trials. Returns (status, reasons, comparison rows)."""
    problems = []
    if not base_trials or not candidate_trials:
        return "incomplete", ["no trials on one side"], []
    base = side_values(base_trials, problems, "base")
    cand = side_values(candidate_trials, problems, "candidate")
    expected = expected_inventory(scenarios)
    for name in sorted(expected - set(cand)):
        problems.append(("incomplete", f"candidate: expected {name} was not reported"))
    for name in sorted(expected - set(base)):
        problems.append(("incomplete", f"base: expected {name} was not reported"))
    rows = []
    regressed = False
    for name in sorted(set(base) | set(cand)):
        b, c = base.get(name), cand.get(name)
        cls = (c or b)[0]
        row = {"metric": name, "class": cls}
        if b is None or c is None:
            row["result"] = "candidate only" if b is None else "base only"
            row["base" if c is None else "candidate"] = (b or c)[1][0] \
                if cls != "timing" and cls != "advisory" else timing_summary((b or c)[1])
            rows.append(row)
            continue
        if cls in ("timing", "advisory"):
            bs, cs = timing_summary(b[1]), timing_summary(c[1])
            row.update(base=bs, candidate=cs, result="reported")
            if bs["median"] > 0:
                row["ratio"] = round(cs["median"] / bs["median"], 4)
            rows.append(row)
            continue
        bv, cv = b[1][0], c[1][0]
        row.update(base=bv, candidate=cv)
        if cls == "cost":
            row["result"] = "same" if cv == bv else ("regressed" if cv > bv else "improved")
            regressed |= cv > bv
        elif cls == "work":
            row["result"] = "same" if cv == bv else "differs"
            if cv != bv:
                problems.append(("invalid", f"{name}: work differs ({bv} vs {cv})"))
        else:
            row["result"] = "same" if cv == bv else "changed"
        rows.append(row)
    statuses = [s for s, _ in problems] + (["regressed"] if regressed else [])
    status = min(statuses, key=STATUS_ORDER.index) if statuses else "passed"
    reasons = [r for _, r in problems]
    if regressed:
        reasons.append("cost counters regressed: " + ", ".join(
            r["metric"] for r in rows if r.get("result") == "regressed"))
    return status, reasons, rows


# --- report ---------------------------------------------------------------------------------------

def fmt(v):
    if isinstance(v, dict):
        return f"{v['median']:.0f} ({v['min']:.0f}–{v['max']:.0f})"
    return "–" if v is None else str(v)


def summary(results):
    lines = [f"# Performance comparison: {results['status']}", ""]
    ids = results.get("revisions", {})
    lines += [f"- Base: `{ids.get('base', '?')}`", f"- Candidate: `{ids.get('candidate', '?')}`",
              f"- Trials per side: {results.get('trials')}, order seed {results.get('seed')}",
              f"- Harness digest: `{results.get('harness', {}).get('digest', '?')}`", ""]
    for r in results.get("reasons", []):
        lines.append(f"- {r}")
    rows = results.get("comparison", [])
    if not rows:
        lines += ["", "No comparison was made."]
        return "\n".join(lines) + "\n"
    gate = [r for r in rows if r["class"] == "cost" and r["result"] != "same"]
    changed = [r for r in rows if r["class"] == "info" and r["result"] != "same"]
    timing = [r for r in rows if r["class"] in ("timing", "advisory")]
    lines += ["", "## Cost counters (gate; lower is better)", ""]
    if gate:
        lines += ["| Metric | Base | Candidate | Result |", "|---|---:|---:|---|"]
        lines += [f"| `{r['metric']}` | {fmt(r.get('base'))} | {fmt(r.get('candidate'))} | "
                  f"{r['result']} |" for r in gate]
    else:
        n = sum(1 for r in rows if r["class"] == "cost")
        lines.append(f"All {n} cost counters are identical.")
    if changed:
        lines += ["", "## Changed info counters", "", "| Metric | Base | Candidate |",
                  "|---|---:|---:|"]
        lines += [f"| `{r['metric']}` | {r.get('base')} | {r.get('candidate')} |"
                  for r in changed]
    if timing:
        lines += ["", "## Timing and resources (advisory; median and range over trials)", "",
                  "| Metric | Base | Candidate | Candidate / base |", "|---|---:|---:|---:|"]
        lines += [f"| `{r['metric']}` | {fmt(r.get('base'))} | {fmt(r.get('candidate'))} | "
                  f"{r.get('ratio', '–')} |" for r in timing]
    return "\n".join(lines) + "\n"


def write(out, results):
    (out / "results.json").write_text(json.dumps(results, indent=2, sort_keys=True) + "\n")
    (out / "summary.md").write_text(summary(results))


# --- orchestration --------------------------------------------------------------------------------

def side_env(root):
    """The environment for one side's commands. Its target directory is set explicitly, so an
    inherited CARGO_TARGET_DIR can neither move the build away from where the binaries are run
    nor make the two sides share one."""
    return dict(os.environ, CARGO_TERM_COLOR="never", CARGO_TARGET_DIR=str(root / "target"))


def build(side, root, out, timeout, env):
    steps = [run(PERF_BUILD, root, out / "logs" / f"build-{side}-perf.log", timeout, env),
             run(WORKLOADS + ["--no-run"], root, out / "logs" / f"build-{side}-workloads.log",
                 timeout, env)]
    return steps, all(s["exit"] == 0 and not s["timed_out"] for s in steps)


def trial(side, n, root, out, timeout, env):
    work = out / "work" / f"{side[0]}{n:02d}"
    work.mkdir(parents=True)
    result = {"side": side, "trial": n, "started": now(), "ok": False, "problems": []}
    perf = root / "target" / "release" / "examples" / "perf"
    p = run([str(perf), "--dir", str(work / "perf")], root,
            out / "logs" / f"trial-{side}-{n:02d}-perf.log", timeout, env)
    (work / "tmp").mkdir()
    w = run(WORKLOADS + WORKLOAD_ARGS, root, out / "logs" / f"trial-{side}-{n:02d}-workloads.log",
            timeout, dict(env, TMPDIR=str(work / "tmp")))
    result["commands"] = [p, w]
    result["perf"], perf_problems = parse_perf(Path(p["log"]).read_text(errors="replace"))
    result["workloads"], wl_problems = parse_workloads(
        Path(w["log"]).read_text(errors="replace"))
    result["problems"] = perf_problems + wl_problems
    for c in (p, w):
        if c["exit"] != 0 or c["timed_out"]:
            result["problems"].append(
                f"{' '.join(c['command'][:2])} exited {c['exit']}"
                + (" after the timeout" if c["timed_out"] else ""))
    result["ok"] = not result["problems"]
    result["finished"] = now()
    if result["ok"]:
        shutil.rmtree(work, ignore_errors=True)
    return result


def compare(args):
    repo = Path(args.repo).resolve()
    out = Path(args.output).resolve()
    out.mkdir(parents=True, exist_ok=False)
    seed = args.seed if args.seed is not None else int.from_bytes(os.urandom(4), "big")
    results = {"schema": SCHEMA, "started": now(), "status": "error", "reasons": [],
               "trials": args.trials, "seed": seed, "environment": environment(),
               "toolchain": {}, "comparison": [], "runs": []}
    checkouts = []
    try:
        ids = {"base": resolve(repo, args.base), "candidate": resolve(repo, args.candidate)}
        results["revisions"] = ids
        results["refs"] = {"base": args.base, "candidate": args.candidate}
        roots = {}
        for side in ("candidate", "base"):
            root = out / "checkouts" / side
            git(repo, "worktree", "add", "--detach", str(root), ids[side])
            checkouts.append(root)
            roots[side] = root
        own = tree_digest(roots["base"], HARNESS)
        install_harness(roots["candidate"], roots["base"])
        results["harness"] = {"paths": HARNESS, "source": "candidate",
                              "digest": tree_digest(roots["candidate"], HARNESS),
                              "base_own_digest": own,
                              "installed_digest": tree_digest(roots["base"], HARNESS)}
        envs = {side: side_env(roots[side]) for side in roots}
        results["toolchain"] = {"rustc": observe(["rustc", "-vV"]),
                                "cargo": observe(["cargo", "-V"])}
        for side in ("candidate", "base"):
            steps, ok = build(side, roots[side], out, args.timeout, envs[side])
            results["runs"] += [dict(s, side=side, phase="build") for s in steps]
            write(out, results)
            if not ok:
                raise Failure("error" if side == "candidate" else "not-comparable",
                              f"the {side} does not build with the candidate's harness")
        listing = observe([str(roots["candidate"] / "target/release/examples/perf"), "--list"])
        scenarios = [line.split()[0] for line in (listing or "").splitlines() if line.strip()]
        if not scenarios:
            raise Failure("error", "the harness lists no scenarios")
        results["scenarios"] = scenarios
        rng = random.Random(seed)
        order = []
        for n in range(args.trials):
            pair = ["base", "candidate"]
            rng.shuffle(pair)
            order += [(side, n) for side in pair]
        results["order"] = [f"{s}-{n}" for s, n in order]
        trials = {"base": [], "candidate": []}
        for side, n in order:
            t = trial(side, n, roots[side], out, args.timeout, envs[side])
            results["runs"].append({k: v for k, v in t.items() if k not in ("perf", "workloads")})
            results.setdefault("trial_records", []).append(t)
            trials[side].append(t)
            write(out, results)
        failed = {s: [t for t in ts if not t["ok"]] for s, ts in trials.items()}
        if failed["candidate"]:
            raise Failure("error", "candidate trials failed: " + "; ".join(
                p for t in failed["candidate"] for p in t["problems"]))
        if failed["base"]:
            raise Failure("invalid", "base trials failed: " + "; ".join(
                p for t in failed["base"] for p in t["problems"]))
        status, reasons, rows = analyze(trials["base"], trials["candidate"], scenarios)
        results.update(status=status, reasons=reasons, comparison=rows)
    except Failure as f:
        results.update(status=f.status, reasons=results["reasons"] + [f.reason])
    except Exception as e:  # recorded, never reported as a comparison
        results.update(status="error", reasons=results["reasons"] + [f"{type(e).__name__}: {e}"])
    finally:
        for root in checkouts:
            try:
                git(repo, "worktree", "remove", "--force", str(root))
            except subprocess.CalledProcessError as e:
                results["reasons"].append(f"could not remove worktree {root}: {e.output.strip()}")
        results["finished"] = now()
        write(out, results)
    print(summary(results))
    return 0 if results["status"] == "passed" else 1


def main(argv=None):
    p = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    p.add_argument("--output", required=True, help="new directory for the evidence bundle")
    p.add_argument("--base", default="origin/master")
    p.add_argument("--candidate", default="HEAD")
    p.add_argument("--trials", type=int, default=5)
    p.add_argument("--seed", type=int, help="order seed (random and recorded if omitted)")
    p.add_argument("--timeout", type=int, default=900, help="seconds for each command")
    p.add_argument("--repo", default=".")
    args = p.parse_args(argv)
    if args.trials < 2:
        p.error("--trials must be at least 2, so repeatability can be checked")
    return compare(args)


if __name__ == "__main__":
    sys.exit(main())
