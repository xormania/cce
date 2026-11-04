"""Semantic reuse keeps immutable provenance without repeating unrelated work."""
from pathlib import Path
import sys
import unittest

ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT/'tools/ci'))
from drift_runner import fingerprint


class SemanticFingerprintTests(unittest.TestCase):
    def packet(self, revision, assertion='assert_eq!(held.read(), original);'):
        return {'subject':'e'*24, 'questions':['proof','guarantee'], 'excerpts':[
            {'id':'e1', 'text':'Named proof: tests/read.rs @ '+revision+'\nlines 4-8; sha256='+'c'*64+'\n'+assertion}]}

    def test_revision_annotation_alone_does_not_repeat_a_request(self):
        self.assertEqual(fingerprint(self.packet('a'*40)), fingerprint(self.packet('b'*40)))

    def test_changed_assertion_reopens_the_relationship(self):
        self.assertNotEqual(fingerprint(self.packet('a'*40)),
                            fingerprint(self.packet('b'*40, 'assert!(held.read().is_err());')))


if __name__ == '__main__':
    unittest.main()
