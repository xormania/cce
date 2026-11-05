"""Public artifacts preserve bounded diagnostic evidence without caching failures."""
from __future__ import annotations

import json
import os
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch

from test_drift_runner import BASE, HEAD, OTHER, FakeApi, control, packet, runner, source


def observation(n=0, status='malformed_response', stage='schema', reason='answer_inventory_mismatch'):
    candidate = packet(n)
    return {'fingerprint': runner.fingerprint(candidate), 'record': {
        'subject': candidate['subject'], 'source': source(), 'status': status,
        'requested_model': runner.MODEL, 'observed_model': runner.MODEL,
        'question_version': 1, 'provider_request_attempted': True, 'provider_request_attempts': 1,
        'request_sha256': '1' * 64, 'response_sha256': '2' * 64,
        'latency_seconds': 0.125, 'usage': {'input_tokens': 12, 'output_tokens': 3},
        'full_response_retained': False, 'advisory_only': True,
        'diagnostic': {'version': 1, 'stage': stage, 'reason': reason,
                       'http_status': 401 if stage == 'http' else 200,
                       'response_bytes_read': 123, 'response_truncated': False}}}


class ObservationRetentionTests(unittest.TestCase):
    def publish(self, value, current=HEAD):
        directory = tempfile.TemporaryDirectory()
        self.addCleanup(directory.cleanup)
        root = Path(directory.name)
        runner.save(root / 'control.json', value)
        api = FakeApi({'/pulls/9': {'state': 'open', 'head': {'sha': current}, 'base': {'sha': BASE}},
                       '/commits/' + HEAD + '/check-runs?check_name=CCE%20drift%20contracts&per_page=100':
                       {'check_runs': []}})
        with patch.dict(os.environ, {'GITHUB_STEP_SUMMARY': ''}):
            result = runner.publish(api, root)
        state = runner.load(root / 'public' / 'state.json')
        public = ''.join(path.read_text() for path in (root / 'public').iterdir() if path.is_file())
        posts = [call for call in api.calls if call[1] in {'POST', 'PATCH'}]
        return result, state, public + json.dumps(posts), posts

    def test_http_schema_and_policy_failures_survive_publication(self):
        cases = [('provider_failure', 'http', 'authentication_failed'),
                 ('malformed_response', 'schema', 'answer_inventory_mismatch'),
                 ('malformed_response', 'policy', 'duplicate_support')]
        for status, stage, reason in cases:
            with self.subTest(stage=stage):
                row = observation(status=status, stage=stage, reason=reason)
                result, state, public, posts = self.publish(control(
                    selected=1, results=[row], provider_status='completed', provider_completed=1))
                self.assertEqual(result, 0)
                self.assertEqual(posts[-1][2]['conclusion'], 'success')
                self.assertEqual(state['provider_status'], 'completed')
                self.assertEqual(state['entries'], [])
                self.assertEqual(len(state['observations']), 1)
                retained = state['observations'][0]
                self.assertEqual(retained['fingerprint'], row['fingerprint'])
                self.assertEqual(retained['record']['status'], status)
                self.assertEqual(retained['record']['diagnostic']['stage'], stage)
                self.assertEqual(retained['record']['diagnostic']['reason'], reason)
                self.assertEqual(retained['record']['response_sha256'], '2' * 64)
                self.assertIn(reason, public)

    def test_current_attempts_are_bounded_without_carrying_previous_observations(self):
        attempts = [observation(n) for n in range(runner.MAX_CALLS + 2)]
        old = observation(100)
        _, state, _, _ = self.publish(control(results=attempts, observations=[old],
                                               provider_status='completed'))
        retained = state['observations']
        self.assertEqual(len(retained), runner.MAX_CALLS)
        self.assertTrue({row['fingerprint'] for row in retained}.issubset(
            {row['fingerprint'] for row in attempts}))
        self.assertNotIn(old['fingerprint'], {row['fingerprint'] for row in retained})

    def test_reused_reserved_and_unattempted_results_are_not_new_observations(self):
        current = observation(0)
        reused = observation(1)
        reused['record'].update(reused=True)
        reserved = observation(2, status='previous_request_reserved')
        reserved['record'].update(provider_request_attempted=False, provider_request_attempts=0)
        unattempted = observation(3, status='missing_credentials')
        unattempted['record'].update(provider_request_attempted=False, provider_request_attempts=0)
        truthy = observation(4)
        truthy['record']['provider_request_attempted'] = 'yes'
        _, state, _, _ = self.publish(control(
            results=[reused, reserved, unattempted, truthy, current], provider_status='completed'))
        self.assertEqual([row['fingerprint'] for row in state['observations']], [current['fingerprint']])

    def test_success_cache_still_reuses_success_and_never_treats_failure_as_answer(self):
        cached = {'fingerprint': runner.fingerprint(packet(20)), 'record': {
            'subject': packet(20)['subject'], 'status': 'diagnosed', 'reused': True,
            'provider_request_attempted': False, 'provider_request_attempts': 0,
            'diagnosis': {'answers': {'proof': {'choice': 'supported'}}}}}
        fresh = observation(21, status='diagnosed')
        del fresh['record']['diagnostic']
        fresh['record']['diagnosis'] = {'answers': {'proof': {'choice': 'supported'}},
                                        'supporting_excerpt_ids': ['e1'], 'abstained': False}
        failed = observation(22)
        _, state, _, _ = self.publish(control(previous_entries=[cached], results=[cached, fresh, failed],
                                               provider_status='completed'))
        self.assertEqual({row['fingerprint'] for row in state['entries']},
                         {cached['fingerprint'], fresh['fingerprint']})
        self.assertEqual({row['fingerprint'] for row in state['observations']},
                         {fresh['fingerprint'], failed['fingerprint']})
        selected, reused, deferred = runner.choose([packet(20), packet(21)], state)
        self.assertEqual((selected, deferred), ([], []))
        self.assertEqual({row['fingerprint'] for row in reused},
                         {cached['fingerprint'], fresh['fingerprint']})

    def test_superseded_candidate_keeps_observations_without_publishing_a_new_check(self):
        failed = observation()
        result, state, _, posts = self.publish(control(results=[failed], provider_status='completed'),
                                               current=OTHER)
        self.assertEqual(result, 0)
        self.assertEqual(posts, [])
        self.assertTrue(state['superseded'])
        self.assertEqual(state['provider_status'], 'completed')
        self.assertEqual(state['entries'], [])
        self.assertEqual(state['observations'][0]['record']['diagnostic']['reason'],
                         'answer_inventory_mismatch')

    def test_partial_execution_keeps_only_completed_attempts(self):
        completed = [observation(0), observation(1)]
        _, state, public, _ = self.publish(control(selected=runner.MAX_CALLS, results=completed,
                                                   provider_status='partial', provider_completed=2))
        self.assertEqual(state['provider_status'], 'partial')
        self.assertEqual(len(state['observations']), 2)
        self.assertIn('interrupted or incomplete', public)
        self.assertIn('2 of ' + str(runner.MAX_CALLS), public)

    def test_unknown_fields_never_publish_provider_echoes(self):
        marker = 'synthetic-private-provider-echo'
        row = observation()
        row['record']['untrusted_response'] = marker
        row['record']['diagnostic']['untrusted_detail'] = marker
        row['record']['source']['untrusted_detail'] = marker
        row['untrusted_detail'] = marker
        _, state, public, _ = self.publish(control(results=[row], provider_status='completed'))
        self.assertNotIn(marker, public)
        self.assertEqual(len(state['observations']), 1)

    def test_retained_provenance_is_a_validated_public_identity(self):
        for field, invalid in [('repository', 'private-repository'), ('head_sha', 'not-a-sha'),
                               ('base_sha', None)]:
            with self.subTest(field=field):
                row = observation()
                row['record']['source'][field] = invalid
                _, state, _, _ = self.publish(control(results=[row], provider_status='completed'))
                self.assertNotIn('source', state['observations'][0]['record'])

    def test_unrecognized_diagnostic_values_never_publish_free_text(self):
        marker = 'synthetic-private-provider-value'
        for field in ('stage', 'reason', 'question', 'response_type'):
            with self.subTest(field=field):
                row = observation()
                row['record']['diagnostic'][field] = marker
                _, _, public, _ = self.publish(control(results=[row], provider_status='completed'))
                self.assertNotIn(marker, public)

    def test_unknown_provider_status_is_replaced_by_fixed_unavailable_label(self):
        marker = 'synthetic-private-provider-stage'
        _, state, public, _ = self.publish(control(results=[observation()], provider_status=marker))
        self.assertEqual(state['provider_status'], 'unavailable')
        self.assertNotIn(marker, public)


if __name__ == '__main__':
    unittest.main()
