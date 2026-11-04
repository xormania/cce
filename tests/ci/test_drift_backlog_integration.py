"""Exercise retained work through the actual collection and artifact boundaries."""
from __future__ import annotations

import copy
import json
import os
from pathlib import Path
import sys
import tempfile
import unittest
from unittest.mock import patch

ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT / 'tools' / 'ci'))
import drift_backlog as backlog
import drift_runner as runner


def source(head='a' * 40):
    return {'repository': 'xormania/cce', 'head_sha': head, 'base_sha': 'b' * 40,
            'branch': 'feature', 'pulls': [15], 'run_id': 7, 'attempt': 1}


def semantic(identity='relationship', text='Original incomplete assertion'):
    return {'id': identity, 'excerpts': [text], 'questions': ['proof']}


def packet(origin=None, identity='relationship', text='Original incomplete assertion'):
    return runner.packet_inventory(origin or source(), [semantic(identity, text)])[0]


def entry(value, choice='overstated'):
    return {'fingerprint': runner.fingerprint(value), 'record': {
        'subject': value['subject'], 'status': 'diagnosed', 'source': value['source'],
        'diagnosis': {'answers': {'proof': {'choice': choice}}}}}


def recipe(value, state='deferred'):
    row = backlog.descriptor(value, runner.fingerprint(value))
    row['state'] = state
    return row


class ArtifactApi:
    def __init__(self, states):
        self.states = states

    def api(self, path):
        if path == '/actions/workflows/jev.yml/runs?per_page=10':
            return {'workflow_runs': [
                {'id': ident, 'event': 'workflow_run', 'status': 'completed', 'head_branch': 'master'}
                for ident in sorted(self.states, reverse=True)]}
        for ident in self.states:
            if path == '/actions/runs/' + str(ident) + '/artifacts?per_page=20':
                return {'artifacts': [{'id': ident, 'name': 'drift-state', 'expired': False}]}
        raise AssertionError(path)

    def archive(self, artifact_id, _name):
        return {'version': runner.VERSION, 'engine_hash': runner.engine_hash(),
                **copy.deepcopy(self.states[artifact_id])}


class CollectionApi:
    def __init__(self, previous):
        self.previous = previous

    def baseline(self, _trusted):
        return copy.deepcopy(self.previous)


class BacklogIntegrationTests(unittest.TestCase):
    def test_newer_healthy_correction_does_not_resurrect_older_tracked_recipe(self):
        old = packet()
        corrected = packet(source('c' * 40), text='Corrected complete assertion')
        api = ArtifactApi({
            11: {'entries': [entry(old)], 'backlog': [recipe(old, 'tracked')]},
            12: {'entries': [entry(old), entry(corrected, 'supported')], 'backlog': []},
        })
        with patch.dict(os.environ, {'GITHUB_RUN_ID': '99'}):
            state = runner.GitHub.baseline(api, 'd' * 40)
        self.assertEqual(state['backlog'], [],
                         'A retired relationship was resurrected from an older artifact')

    def test_pending_context_is_rebuilt_before_cached_tracked_contexts(self):
        origins = [source(char * 40) for char in 'ace']
        originals = [packet(origin, identity='relation-' + str(i), text='Evidence ' + str(i))
                     for i, origin in enumerate(origins)]
        previous = {'entries': [entry(value) for value in originals[:2]],
                    'pending': [runner.fingerprint(originals[2])],
                    'backlog': [recipe(originals[0], 'tracked'), recipe(originals[1], 'tracked'),
                                recipe(originals[2])], 'backlog_overflow': 0}
        rows_by_head = {origin['head_sha']: [semantic('relation-' + str(i), 'Evidence ' + str(i))]
                        for i, origin in enumerate(origins)}

        def changes(_api, origin):
            return rows_by_head.get(origin['head_sha'], []), [], True

        api = CollectionApi(previous)
        with tempfile.TemporaryDirectory() as directory, \
                patch.object(runner, 'public_changes', changes), \
                patch.object(runner, 'execution_checks', return_value=([], [], False)), \
                patch.object(runner, 'data_evidence', return_value=([], [], [], False)):
            runner.collect(api, {}, 'workflow_dispatch', directory,
                           {'JEV_TRUSTED_SHA': 'f' * 40}, source=source('f' * 40))
            selected = runner.load(Path(directory) / 'private' / 'packets.json')
        self.assertIn(runner.fingerprint(originals[2]),
                      [row['fingerprint'] for row in selected],
                      'Tracked contexts exhausted reconstruction slots before pending work')

    def test_reservation_contains_recipe_before_provider_or_publish(self):
        row = semantic()
        api = CollectionApi({'entries': [], 'backlog': [], 'pending': []})
        with tempfile.TemporaryDirectory() as directory, \
                patch.object(runner, 'public_changes', return_value=([row], [], True)), \
                patch.object(runner, 'execution_checks', return_value=([], [], False)), \
                patch.object(runner, 'data_evidence', return_value=([], [], [], False)):
            runner.collect(api, {}, 'workflow_dispatch', directory,
                           {'JEV_TRUSTED_SHA': 'f' * 40, 'DRIFT_PROVIDER_CONFIGURED': 'true'},
                           source=source())
            reservation = runner.load(Path(directory) / 'public' / 'reservation.json')
        self.assertTrue(reservation.get('backlog'),
                        'Cancellation before publish would lose the reservation reconstruction recipe')
        self.assertNotIn('Original incomplete assertion', json.dumps(reservation))

    def test_missing_credentials_do_not_reserve_an_unattempted_request(self):
        value = packet()
        key = runner.fingerprint(value)
        control = {'version': runner.VERSION, 'source': source(), 'trusted_sha': 'f' * 40,
                   'engine_hash': runner.engine_hash(), 'violations': [], 'data_failed': False,
                   'waiting': False, 'selected': 1, 'pending': [], 'limitations': [],
                   'previous_entries': [], 'backlog': [recipe(value)], 'backlog_overflow': 0,
                   'current_fingerprints': [key], 'results': [{'fingerprint': key, 'record': {
                       'subject': value['subject'], 'status': 'missing_credentials',
                       'provider_request_attempted': False, 'provider_request_attempts': 0}}]}
        with tempfile.TemporaryDirectory() as directory, \
                patch.object(runner, 'current_candidate', return_value=True), \
                patch.object(runner, 'begin_check', return_value=123), \
                patch.object(runner, 'complete_check', return_value=True), \
                patch.dict(os.environ, {'GITHUB_STEP_SUMMARY': ''}):
            runner.save(Path(directory) / 'control.json', control)
            runner.publish(None, directory)
            state = runner.load(Path(directory) / 'public' / 'state.json')
        self.assertEqual(state['backlog'][0]['state'], 'deferred',
                         'An unattempted request must remain eligible after credential setup')


if __name__ == '__main__':
    unittest.main()
