"""Corrective changes must address the retained obligation, not a hunk number."""
from __future__ import annotations

import difflib
import os
from pathlib import Path
import sys
import tempfile
import unittest
from unittest.mock import patch

ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT / 'tools' / 'ci'))
import drift_backlog as backlog
import drift_relationships as relationships
import drift_runner as runner
from test_drift_relationships import Api


class CorrectiveRetirementTests(unittest.TestCase):
    path = 'tests/test_values.py'

    def packets(self, before, after, head, base):
        source = {'repository': 'xormania/cce', 'head_sha': head, 'base_sha': base,
                  'branch': 'master', 'pulls': [], 'run_id': 1, 'attempt': 1}
        delta = ''.join(difflib.unified_diff(before.splitlines(True), after.splitlines(True), n=1))
        api = Api({(self.path, base): before, (self.path, head): after}, [
            {'filename': self.path, 'status': 'modified', 'patch': delta}])
        rows, _, _ = relationships.public_changes(api, source)
        return runner.packet_inventory(source, rows)

    def publish_transition(self, before, weakened, corrected, legacy=False, reconstruct=False):
        old = self.packets(before, weakened, 'a' * 40, 'b' * 40)[0]
        new = self.packets(weakened, corrected, 'c' * 40, 'a' * 40)[0]
        old_key, new_key = runner.fingerprint(old), runner.fingerprint(new)
        old_recipe = backlog.descriptor(old, old_key)
        old_recipe['state'] = 'tracked'
        if legacy:
            old_recipe.pop('correction', None)
        if reconstruct:
            _, rebuilt, _ = backlog.reconstruct([old_recipe], lambda *_: [old], runner.fingerprint)
            old_recipe = rebuilt[0]
        new_recipe = backlog.descriptor(new, new_key)
        healthy = {'fingerprint': new_key, 'record': {'subject': new['subject'],
                   'status': 'diagnosed', 'source': new['source'], 'diagnosis': {'answers': {
                       'proof': {'choice': 'supported'}, 'guarantee': {'choice': 'strengthened'}}}}}
        control = {'version': runner.VERSION, 'source': new['source'], 'trusted_sha': 'd' * 40,
                   'engine_hash': runner.engine_hash(), 'previous_entries': [], 'violations': [],
                   'limitations': [], 'data_failed': False, 'waiting': False, 'selected': 1,
                   'pending': [], 'results': [healthy], 'baseline_missing': False,
                   'current_fingerprints': [new_key], 'backlog': [old_recipe, new_recipe],
                   'backlog_overflow': 0}
        with tempfile.TemporaryDirectory() as directory, \
                patch.object(runner, 'current_candidate', return_value=True), \
                patch.object(runner, 'begin_check', return_value=123), \
                patch.object(runner, 'complete_check', return_value=True), \
                patch.dict(os.environ, {'GITHUB_STEP_SUMMARY': ''}):
            runner.save(Path(directory) / 'control.json', control)
            runner.publish(None, directory)
            state = runner.load(Path(directory) / 'public' / 'state.json')
        return old_recipe, new_recipe, state['backlog']

    def test_different_function_does_not_retire_first_hunk(self):
        before = ('def test_total():\n    assert total() > 0\n\n\n' + '\n' * 12 +
                  'def test_reader():\n    assert held.read() == original\n')
        weakened = before.replace('held.read() == original', 'held.read() is not None')
        corrected = weakened.replace('total() > 0', 'total() == 42')
        old, _, retained = self.publish_transition(before, weakened, corrected)
        self.assertIn(old, retained)

    def test_other_assertion_in_same_function_does_not_resolve_old_one(self):
        before = 'def test_values():\n    assert held.read() == original\n    assert total() > 0\n'
        weakened = before.replace('held.read() == original', 'held.read() is not None')
        corrected = weakened.replace('total() > 0', 'total() == 42')
        old, _, retained = self.publish_transition(before, weakened, corrected)
        self.assertIn(old, retained)

    def test_repair_of_weakened_assertion_retires_its_record(self):
        before = 'def test_values():\n    assert held.read() == original\n    assert total() > 0\n'
        weakened = before.replace('held.read() == original', 'held.read() is not None')
        _, _, retained = self.publish_transition(before, weakened, before)
        self.assertEqual(retained, [])

    def test_restoring_deleted_assertion_retires_its_record(self):
        before = 'def test_values():\n    assert held.read() == original\n    assert total() > 0\n'
        weakened = before.replace('    assert held.read() == original\n', '')
        _, _, retained = self.publish_transition(before, weakened, before)
        self.assertEqual(retained, [])

    def test_legacy_ordinal_recipe_never_claims_corrective_identity(self):
        before = 'def test_values():\n    assert held.read() == original\n    assert total() > 0\n'
        weakened = before.replace('held.read() == original', 'held.read() is not None')
        old, _, retained = self.publish_transition(before, weakened, before, legacy=True)
        self.assertIn(old, retained)

    def test_ambiguous_repeated_line_keeps_the_old_record(self):
        before = 'def test_values():\n    assert value() == expected\n    assert value() is not None\n'
        weakened = before.replace('value() == expected', 'value() is not None')
        old, _, retained = self.publish_transition(before, weakened, before)
        self.assertIn(old, retained)

    def test_original_reconstruction_recovers_legacy_correction(self):
        before = 'def test_values():\n    assert held.read() == original\n    assert total() > 0\n'
        weakened = before.replace('held.read() == original', 'held.read() is not None')
        old, _, retained = self.publish_transition(before, weakened, before, legacy=True, reconstruct=True)
        self.assertIn('correction', old)
        self.assertEqual(retained, [])


if __name__ == '__main__':
    unittest.main()
