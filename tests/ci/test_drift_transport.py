"""Exercise the actual GitHub adapter paths rather than bypassing them in mocks."""
import io
import json
from pathlib import Path
import sys
import unittest

ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT/'tools/ci'))
from drift_runner import GitHub
from drift_relationships import public_changes


class Opener:
    def __init__(self, response):
        self.response, self.requests = response, []

    def open(self, request, timeout):
        self.requests.append((request, timeout))
        return io.BytesIO(json.dumps(self.response).encode())


class GitHubTransportTests(unittest.TestCase):
    def test_real_adapter_accepts_comparison_separator_and_reaches_collection(self):
        opener = Opener({'files':[]})
        api = GitHub('xormania/cce', opener=opener)
        rows, limits, markdown = public_changes(api, {'head_sha':'a'*40,'base_sha':'b'*40})
        self.assertEqual((rows, limits, markdown), ([], [], False))
        self.assertEqual(opener.requests[0][0].full_url,
                         'https://api.github.com/repos/xormania/cce/compare/'+'b'*40+'...'+'a'*40)
        self.assertEqual(opener.requests[0][1], 15)

    def test_path_traversal_is_rejected_before_a_request(self):
        for suffix in ('/../outside','/%2e%2e/outside','/contents/%2E%2E/outside',
                       '/contents/%5coutside','//other.invalid/path','https://other.invalid/path'):
            with self.subTest(suffix=suffix):
                opener = Opener({})
                with self.assertRaises(ValueError):
                    GitHub('xormania/cce', opener=opener).api(suffix)
                self.assertEqual(opener.requests, [])


if __name__ == '__main__':
    unittest.main()
