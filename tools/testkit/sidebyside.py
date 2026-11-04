"""Compare two CCE test suites against the same controlled defects."""
from __future__ import annotations

import argparse
from contextlib import contextmanager
from dataclasses import dataclass
import hashlib
import json
import os
from pathlib import Path
import re
import shlex
import signal
import subprocess
import sys
import time

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "ci"))
from drift_relationships import _rust_code


@dataclass
class Run:
    command: list[str]
    code: int
    output: str
    seconds: float
    timed_out: bool = False


def run(command, cwd, log, timeout=600, env=None):
    """Own the command group, including interruption and deadline cleanup."""
    started = time.monotonic()
    with log.open("w") as stream:
        proc = subprocess.Popen(command, cwd=cwd, env=env, stdout=stream,
                                stderr=subprocess.STDOUT, start_new_session=True)
        timed_out = False
        try:
            proc.wait(timeout=timeout)
        except (subprocess.TimeoutExpired, KeyboardInterrupt):
            timed_out = True
            stop_group(proc)
            if sys.exc_info()[0] is KeyboardInterrupt:
                raise
        finally:
            if proc.poll() is None:
                stop_group(proc)
    return Run(list(command), proc.returncode, log.read_text(errors="replace"),
               time.monotonic() - started, timed_out)


def stop_group(proc):
    for sig in (signal.SIGTERM, signal.SIGKILL):
        try:
            os.killpg(proc.pid, sig)
        except ProcessLookupError:
            pass
        try:
            proc.wait(timeout=5)
        except subprocess.TimeoutExpired:
            continue
    proc.wait()


def git(repo, *args):
    return subprocess.check_output(["git", "-C", str(repo), *args],
                                   text=True, stderr=subprocess.STDOUT).strip()


def resolve(repo, ref):
    return git(repo, "rev-parse", "--verify", "--end-of-options", ref + "^{commit}")


@contextmanager
def checkout(repo, ref, path):
    git(repo, "worktree", "add", "--detach", str(path), ref)
    try:
        yield path
    finally:
        git(repo, "worktree", "remove", "--force", str(path))


def production_inputs(root):
    """Fingerprint the current workspace's production files and build pins.

    Test module declarations and io.rs's trailing unit module are the only
    normalization rules. New ordinary source files remain part of the digest.
    """
    result = {}
    for name in git(root, "ls-files").splitlines():
        source = name.startswith("crates/") and "/src/" in name and name.endswith(".rs")
        pin = name in {"Cargo.toml", "Cargo.lock", "rust-toolchain.toml"} or name.endswith("/Cargo.toml")
        if not source and not pin:
            continue
        if name.endswith("_tests.rs") or name.endswith("/sim.rs"):
            continue
        if "/src/test_support/" in name or name.endswith("/src/test_support.rs"):
            continue
        text = (root / name).read_text()
        if name.endswith("/src/lib.rs"):
            text = re.sub(r"#\[cfg\(test\)\]\s*\nmod [a-z_]+;\n", "", text)
        if name.endswith("/src/io.rs"):
            text = without_io_tests(text)
        result[name] = hashlib.sha256(text.encode()).hexdigest()
    return result


def without_io_tests(text):
    code = _rust_code(text)
    match = re.search(r"#\[cfg\(test\)\]\s*mod tests\s*\{", code)
    if not match:
        return text
    depth = 1
    for end in range(match.end(), len(code)):
        if code[end] == "{":
            depth += 1
        elif code[end] == "}":
            depth -= 1
            if depth == 0:
                end += 1
                if text[end:end+1] == "\n":
                    end += 1
                return text[:match.start()] + text[end:]
    raise ValueError("unterminated I/O test module")


def digest(value):
    return hashlib.sha256(json.dumps(value, sort_keys=True).encode()).hexdigest()


def apply_fault(root, fault):
    path = Path(fault["path"])
    witness = Path(fault["witness"])
    if any(p.is_absolute() or ".." in p.parts for p in (path, witness)):
        raise ValueError("fault path must be relative")
    target = root / path
    before = target.read_text()
    old, new = fault["old"], fault["new"]
    count = fault.get("count", 1)
    if not old or old == new or type(count) is not int or count < 1 or before.count(old) != count:
        raise ValueError("fault anchor count mismatch: " + fault["id"])
    expectation = fault.get("success_expectation")
    if expectation:
        lines = [line.strip() for line in (root / witness).read_text().splitlines()]
        if not expectation.get("contract") or lines.count(expectation.get("line")) != 1:
            raise ValueError("successful-operation expectation is ambiguous")
    after = before.replace(old, new)
    target.write_text(after)
    return target, before, {"before": hashlib.sha256(before.encode()).hexdigest(),
                           "after": hashlib.sha256(after.encode()).hexdigest(),
                           "replacements": count}


def inventory(output):
    return {m[1] for m in re.finditer(r"^([^\s]+): test$", output, re.M)}


def healthy(result):
    summaries = re.findall(r"test result: ok\. (\d+) passed; (\d+) failed;", result.output)
    return (result.code == 0 and not result.timed_out and
            sum(int(n) for n, _ in summaries) > 0 and all(int(f) == 0 for _, f in summaries))


def classify(result, fault, root):
    if result.timed_out or result.code < 0 or result.code in {134, 137}:
        return "timeout/abort", None
    if "signal: " in result.output or "process abort signal" in result.output:
        return "timeout/abort", None
    test = fault["test"]
    if re.search(r"^test " + re.escape(test) + r" \.\.\. ok$", result.output, re.M) and healthy(result):
        return "survived", None
    failed = re.search(r"^test " + re.escape(test) + r" \.\.\. FAILED$", result.output, re.M)
    if failed and result.code != 0:
        for m in re.finditer(r"panicked at ([^\n]+?):(\d+):\d+", result.output):
            name, line = m[1], int(m[2])
            if not name.endswith(fault["witness"]):
                continue
            source = (root / fault["witness"]).read_text().splitlines()
            context = "\n".join(source[max(0, line-1):line+4])
            code_lines = _rust_code("\n".join(source)).splitlines()
            observed_line = code_lines[line-1] if 0 < line <= len(code_lines) else ""
            # Ordinary unwrap failures are setup errors. Explicit error expectations
            # and an intentional refusal panic can be behavioral witnesses.
            assertion = re.search(r"\bassert(?:_eq|_ne)?!|\.unwrap_err\(|\.expect_err\(", observed_line)
            refusal = fault.get("refusal") and fault["refusal"] in observed_line and "panic!" in observed_line
            expectation = fault.get("success_expectation", {})
            success = (bool(expectation.get("contract")) and bool(observed_line.strip()) and
                       observed_line.strip() == _rust_code(expectation.get("line", "")).strip())
            if assertion or refusal or success:
                return "detected", {"path": fault["witness"], "line": line, "context": context,
                    "kind": "declared successful operation" if success else "behavioral assertion"}
    if "running 0 tests" in result.output:
        return "not exercised", None
    return "build/setup error", None


def cargo_command(cargo, fault=None):
    if fault is None:
        return [cargo, "test", "--workspace", "--locked"]
    args = [cargo, "test", "--locked", "-p", "cce-storage"]
    args += ["--test", fault["target"]] if fault.get("target") else ["--lib"]
    return args + [fault["test"], "--", "--exact", "--test-threads=1"]


def publish(out, report):
    (out / "results.json").write_text(json.dumps(report, indent=2) + "\n")
    lines = ["# Test suite comparison", "", "Status: " + report["status"], "",
             "| Fault | Original | Candidate |", "|---|---|---|"]
    for row in report["faults"]:
        lines.append("| " + row["id"] + " | " + row.get("original", {}).get("result", "incomplete")
                     + " | " + row.get("candidate", {}).get("result", "incomplete") + " |")
    if report.get("error"):
        lines += ["", "Incomplete: " + report["error"]]
    lines += ["", "Reproduce:", "", "    " + shlex.join(report["reproduce"]), ""]
    (out / "comparison.md").write_text("\n".join(lines))


def compare(args):
    repo = Path(args.repo).resolve()
    out = Path(args.output).resolve()
    out.mkdir(parents=True, exist_ok=False)
    (out / "checkouts").mkdir()
    catalog_path = Path(args.catalog).resolve()
    faults = json.loads(catalog_path.read_text())
    if args.fault:
        wanted = set(args.fault)
        faults = [f for f in faults if f["id"] in wanted]
        if wanted != {f["id"] for f in faults}:
            raise ValueError("unknown fault selection")
    if not faults or len({f["id"] for f in faults}) != len(faults):
        raise ValueError("fault inventory is empty or has duplicate identities")
    report = {"status": "incomplete", "faults": [],
              "original": resolve(repo, args.original), "candidate": resolve(repo, args.candidate),
              "production": resolve(repo, args.production or args.original),
              "catalog_sha256": hashlib.sha256(catalog_path.read_bytes()).hexdigest(),
              "tool_sha256": hashlib.sha256(Path(__file__).read_bytes()).hexdigest(),
              "python": sys.version, "reproduce": [sys.executable, str(Path(__file__).resolve()),
                  "--repo", str(repo), "--original", args.original, "--candidate", args.candidate,
                  "--production", args.production or args.original, "--catalog", str(catalog_path),
                  "--cargo", args.cargo, "--timeout", str(args.timeout),
                  "--output", str(out) + "-repeat"] +
                  [v for f in args.fault for v in ("--fault", f)]}
    publish(out, report)
    for key in ("original", "candidate", "production"):
        index = report["reproduce"].index("--" + key)
        report["reproduce"][index + 1] = report[key]
    try:
        with checkout(repo, report["production"], out / "checkouts/production") as base:
            expected = production_inputs(base)
        report["production_inputs"] = expected
        report["healthy"] = {}
        report["environment"] = {}
        for role in ("original", "candidate"):
            report["environment"][role] = {}
            with checkout(repo, report[role], out / ("checkouts/" + role)) as root:
                actual = production_inputs(root)
                if actual != expected:
                    raise ValueError(role + " production inputs differ from the pinned baseline")
                report["environment"][role]["production_sha256"] = digest(actual)
                env = dict(os.environ, CARGO_TERM_COLOR="never", CARGO_INCREMENTAL="0",
                           CARGO_TARGET_DIR=str(out / ("build/" + role)),
                           TMPDIR=str(out / ("temporary/" + role)))
                Path(env["TMPDIR"]).mkdir(parents=True)
                for command in ([args.cargo, "-V"], ["rustc", "-vV"]):
                    label = command[0].split("/")[-1]
                    result = run(command, root, out / (role + "-" + label + "-version.log"), args.timeout, env)
                    if result.code:
                        raise ValueError("toolchain observation failed")
                    report["environment"][role][label] = result.output.strip()
                result = run(cargo_command(args.cargo) + ["--", "--list"], root,
                             out / (role + "-inventory.log"), args.timeout, env)
                cases = inventory(result.output)
                if result.code or not cases:
                    raise ValueError(role + " test discovery failed")
                missing = {f["test"] for f in faults} - cases
                if missing:
                    raise ValueError(role + " missing selected cases: " + ", ".join(sorted(missing)))
                report["environment"][role]["tests"] = sorted(cases)
                result = run(cargo_command(args.cargo), root, out / (role + "-healthy.log"), args.timeout, env)
                report["healthy"][role] = {"passed": healthy(result), "seconds": result.seconds,
                                            "command": result.command, "exit": result.code}
                if not healthy(result):
                    raise ValueError(role + " healthy suite did not pass")
                print(role + ": healthy suite passed; " + str(len(cases)) + " discovered cases", flush=True)
                for fault in faults:
                    row = next((r for r in report["faults"] if r["id"] == fault["id"]), None)
                    if row is None:
                        row = {"id": fault["id"], "definition": fault}
                        report["faults"].append(row)
                    path, before, change = apply_fault(root, fault)
                    try:
                        result = run(cargo_command(args.cargo, fault), root,
                                     out / (role + "-" + fault["id"] + ".log"), args.timeout, env)
                        verdict, witness = classify(result, fault, root)
                        row[role] = {"result": verdict, "witness": witness, "source": change,
                                     "seconds": result.seconds, "command": result.command,
                                     "exit": result.code, "timed_out": result.timed_out}
                    finally:
                        path.write_text(before)
                    print(role + ": " + fault["id"] + " -> " + row[role]["result"], flush=True)
                    publish(out, report)
        report["status"] = "passed" if all(
            row.get(role, {}).get("result") == "detected"
            for row in report["faults"] for role in ("original", "candidate")) else "failed"
    except (Exception, KeyboardInterrupt) as exc:
        report["error"] = type(exc).__name__ + ": " + str(exc)
    finally:
        publish(out, report)
    print((out / "comparison.md").read_text())
    return 0 if report["status"] == "passed" else 1


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--repo", default=str(Path(__file__).resolve().parents[2]))
    parser.add_argument("--original", required=True)
    parser.add_argument("--candidate", required=True)
    parser.add_argument("--production")
    parser.add_argument("--catalog", default=str(Path(__file__).with_name("faults.json")))
    parser.add_argument("--output", required=True)
    parser.add_argument("--fault", action="append", default=[])
    parser.add_argument("--timeout", type=float, default=600)
    parser.add_argument("--cargo", default="cargo")
    args = parser.parse_args()
    if args.timeout <= 0:
        parser.error("timeout must be positive")
    signal.signal(signal.SIGTERM, lambda *_: (_ for _ in ()).throw(KeyboardInterrupt()))
    return compare(args)


if __name__ == "__main__":
    sys.exit(main())
