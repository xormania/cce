"""The trusted leak scan: what it reads, what it reports and that an unscanned PR never passes.

Fixtures are synthetic repositories and a stub `gh`; the marker pattern is made up.
"""
from __future__ import annotations

import json
import os
from pathlib import Path
import subprocess
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[2]
CORE = ROOT / "tools/ci/leak_scan.sh"
TRUSTED = ROOT / "tools/ci/leak_scan_trusted.sh"
MARKERS = "zq-hidden-name|xv-other-term"
ENV = {"PATH": os.environ["PATH"], "HOME": os.environ.get("HOME", "/tmp"), "LC_ALL": "C",
       "GIT_CONFIG_NOSYSTEM": "1", "GIT_AUTHOR_NAME": "t", "GIT_AUTHOR_EMAIL": "t@example.invalid",
       "GIT_COMMITTER_NAME": "t", "GIT_COMMITTER_EMAIL": "t@example.invalid"}


def git(repo, *args):
    return subprocess.run(["git", "-C", str(repo), *args], check=True, capture_output=True, text=True,
                          env=ENV).stdout.strip()


class Repo:
    """A repository with a base commit; `change` adds one head commit on top of it."""

    def __init__(self, root):
        self.path = Path(root) / "repo"
        self.path.mkdir()
        git(self.path, "init", "-q", "-b", "main")
        (self.path / "a.txt").write_text("keep\nold zq-hidden-name line\n")
        git(self.path, "add", ".")
        git(self.path, "commit", "-q", "-m", "base")
        self.base = git(self.path, "rev-parse", "HEAD")

    def change(self, files, message="change", remove=()):
        git(self.path, "checkout", "-q", "--detach", self.base)
        for name, content in files.items():
            target = self.path / name
            target.write_bytes(content if isinstance(content, bytes) else content.encode())
        for name in remove:
            (self.path / name).write_text("keep\n")
        git(self.path, "add", "-A")
        git(self.path, "commit", "-q", "-m", message)
        return git(self.path, "rev-parse", "HEAD")


class CoreScanTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.repo = Repo(self.tmp.name)
        self.pr = Path(self.tmp.name) / "pr.txt"
        self.pr.write_text("Title\nA harmless body\n")

    def tearDown(self):
        self.tmp.cleanup()

    def scan(self, head, markers=MARKERS):
        env = dict(ENV, MARKERS=markers)
        done = subprocess.run(["bash", str(CORE), str(self.repo.path / ".git"), self.repo.base, head, str(self.pr)],
                              capture_output=True, text=True, env=env)
        return done.returncode, done.stdout + done.stderr

    def test_clean_change_passes_and_a_removed_marker_line_is_not_a_finding(self):
        rc, out = self.scan(self.repo.change({"b.txt": "plain text\n"}, remove=["a.txt"]))
        self.assertEqual(rc, 0, out)
        self.assertIn("no private markers", out)

    def test_each_input_is_scanned_and_matches_are_never_echoed(self):
        cases = {
            "added": lambda: self.repo.change({"b.txt": "new XV-OTHER-TERM here\n"}),
            "commits": lambda: self.repo.change({"b.txt": "plain\n"}, message="mention zq-hidden-name"),
            "files": lambda: self.repo.change({"zq-hidden-name.txt": "plain\n"}),
            "pr": lambda: (self.pr.write_text("body with xv-other-term\n"), self.repo.change({"b.txt": "x\n"}))[1],
        }
        for where, make in cases.items():
            with self.subTest(where=where):
                self.pr.write_text("Title\nA harmless body\n")
                rc, out = self.scan(make())
                self.assertEqual(rc, 1, out)
                self.assertIn("private marker found in PR " + where, out)
                self.assertNotIn("hidden-name", out.lower().replace("zq-hidden-name.txt", ""))
                self.assertNotIn("other-term", out.lower())

    def test_added_lines_that_look_like_headers_or_binary_content_are_scanned(self):
        for content in ("++ zq-hidden-name\n", b"\x00\x01bin zq-hidden-name\x00\n"):
            with self.subTest(content=repr(content)):
                rc, out = self.scan(self.repo.change({"b.bin": content}))
                self.assertEqual(rc, 1, out)
                self.assertIn("private marker found in PR added", out)

    def test_missing_or_invalid_pattern_never_passes(self):
        head = self.repo.change({"b.txt": "plain\n"})
        rc, out = self.scan(head, markers="")
        self.assertEqual(rc, 1)
        self.assertIn("not available; nothing was scanned", out)
        rc, out = self.scan(head, markers="(")
        self.assertEqual(rc, 1)
        self.assertIn("not a valid extended regex", out)


STUB_GH = r'''#!/usr/bin/env python3
import json, os, sys
d = os.environ["STUB_DIR"]
args = sys.argv[1:]
with open(os.path.join(d, "calls.jsonl"), "a") as log:
    log.write(json.dumps(args) + "\n")
assert args[0] == "api"
if "-X" in args and args[args.index("-X") + 1] == "POST":
    print("77")
elif "-X" in args and args[args.index("-X") + 1] == "PATCH":
    print("{}")
else:
    print(open(os.path.join(d, "pull.json")).read())
'''


class TrustedRunnerTests(unittest.TestCase):
    """The workflow_run glue: fetches as data, publishes the check, fails closed."""

    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        root = Path(self.tmp.name)
        self.repo = Repo(root)
        self.remote = root / "remote.git"
        git(root, "init", "-q", "--bare", str(self.remote))
        git(self.remote, "config", "uploadpack.allowAnySHA1InWant", "true")
        self.stub = root / "stub"
        self.stub.mkdir()
        (self.stub / "gh").write_text(STUB_GH)
        (self.stub / "gh").chmod(0o755)

    def tearDown(self):
        self.tmp.cleanup()

    def publish(self, head, pull_head=None, state="open", fork=False, body="A harmless body"):
        git(self.repo.path, "push", "-q", "-f", str(self.remote), self.repo.base + ":refs/heads/main",
            head + ":refs/pull/5/head")
        pull = {"state": state, "title": "Title", "body": body,
                "head": {"sha": pull_head or head, "repo": {"full_name": "fork/cce" if fork else "own/cce"}},
                "base": {"sha": self.repo.base}}
        (self.stub / "pull.json").write_text(json.dumps(pull))

    def run_trusted(self, pulls='[{"number": 5}]', markers=MARKERS):
        (self.stub / "calls.jsonl").write_text("")
        env = dict(ENV, PATH=str(self.stub) + os.pathsep + ENV["PATH"], STUB_DIR=str(self.stub), GH_TOKEN="t",
                   REPO="own/cce", PULLS=pulls, MARKERS=markers, SERVER="https://example.invalid",
                   RUN_URL="https://example.invalid/run", LEAK_SCAN_REMOTE=str(self.remote))
        done = subprocess.run(["bash", str(TRUSTED)], capture_output=True, text=True, env=env)
        calls = [json.loads(line) for line in (self.stub / "calls.jsonl").read_text().splitlines()]
        return done.returncode, done.stdout + done.stderr, calls

    @staticmethod
    def field(call, key):
        return [a.split("=", 1)[1] for a in call if a.startswith(key + "=")][0]

    def verdict(self, calls):
        posts = [c for c in calls if "POST" in c]
        patches = [c for c in calls if "PATCH" in c]
        self.assertEqual(len(posts), 1)
        self.assertEqual(self.field(posts[0], "name"), "scan")
        self.assertEqual(self.field(posts[0], "status"), "in_progress")
        self.assertEqual(len(patches), 1)
        return self.field(posts[0], "head_sha"), self.field(patches[0], "conclusion"), self.field(patches[0], "output[summary]")

    def test_clean_pull_request_gets_a_passing_scan_on_its_head(self):
        head = self.repo.change({"b.txt": "plain\n"})
        self.publish(head)
        rc, out, calls = self.run_trusted()
        self.assertEqual(rc, 0, out)
        self.assertEqual(self.verdict(calls)[:2], (head, "success"))

    def test_marker_fails_the_scan_without_revealing_it(self):
        head = self.repo.change({"b.txt": "zq-hidden-name\n"})
        self.publish(head, body="and xv-other-term")
        rc, out, calls = self.run_trusted()
        self.assertEqual(rc, 1)
        sha, conclusion, summary = self.verdict(calls)
        self.assertEqual(conclusion, "failure")
        self.assertIn("PR added", summary)
        self.assertIn("PR pr", summary)
        self.assertNotIn("hidden-name", summary + out)
        self.assertNotIn("other-term", summary + out)

    def test_scan_that_cannot_run_is_published_as_failure(self):
        head = self.repo.change({"b.txt": "plain\n"})
        self.publish(head)
        rc, _, calls = self.run_trusted(markers="")
        self.assertEqual(rc, 1)
        self.assertEqual(self.verdict(calls)[1], "failure")
        # The pull request head moved after the API read: this run must not certify either head.
        self.publish(head, pull_head="0" * 40)
        rc, out, calls = self.run_trusted()
        self.assertEqual(rc, 1)
        _, conclusion, summary = self.verdict(calls)
        self.assertEqual(conclusion, "failure")
        self.assertIn("head moved", summary)

    def test_forks_closed_and_missing_pull_requests_publish_nothing(self):
        head = self.repo.change({"b.txt": "plain\n"})
        for kwargs, pulls in (({"fork": True}, '[{"number": 5}]'), ({"state": "closed"}, '[{"number": 5}]'),
                              ({}, "[]")):
            with self.subTest(kwargs=kwargs, pulls=pulls):
                self.publish(head, **kwargs)
                rc, out, calls = self.run_trusted(pulls=pulls)
                self.assertEqual(rc, 0, out)
                self.assertFalse([c for c in calls if "POST" in c or "PATCH" in c])


class WorkflowTests(unittest.TestCase):
    def test_trusted_workflow_runs_default_branch_code_and_confines_the_secret(self):
        text = (ROOT / ".github/workflows/leak-scan.yml").read_text()
        self.assertIn("workflow_run:", text)
        self.assertIn("workflows: [leak-guard]", text)
        self.assertNotIn("pull_request_target", text)
        self.assertIn("ref: ${{ github.event.repository.default_branch }}", text)
        self.assertIn("persist-credentials: false", text)
        self.assertIn("head_repository.full_name == github.repository", text)
        self.assertIn("bash trusted/tools/ci/leak_scan_trusted.sh", text)
        self.assertEqual(text.count("secrets."), 1)
        self.assertIn("MARKERS: ${{ secrets.PRIVATE_MARKERS }}", text)
        self.assertNotIn("cancel-in-progress: true", text)


if __name__ == "__main__":
    unittest.main()
