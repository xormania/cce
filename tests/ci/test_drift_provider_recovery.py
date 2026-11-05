"""Completed observations survive interrupted batches without exposing packets.

Provider replies and repository responses are fixtures. The termination case
uses an actual subprocess signal; no network or real provider credentials.
"""
from __future__ import annotations

import json
import os
from pathlib import Path
import signal
import subprocess
import sys
import tempfile
import unittest
from unittest.mock import patch

from test_drift_runner import FakeApi, HEAD, control, packet, source
import drift_runner as runner


ROOT = Path(__file__).resolve().parents[2]
PRIVATE_TEXT = 'private-fixture-obligation-not-for-public-output'
PRIVATE_KEY = 'private-fixture-provider-key'


def completed(value):
    return {'subject': value['subject'], 'source': value['source'],
            'status': 'diagnosed', 'provider_request_attempted': True,
            'provider_request_attempts': 1,
            'diagnosis': {'answers': {'proof': {'choice': 'supported'}}}}


class ProviderRecoveryTests(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        self.out = Path(temporary.name)
        self.rows = []
        for index in range(3):
            value = packet(index, text=PRIVATE_TEXT + str(index))
            value['source'] = source(pulls=[])
            self.rows.append({'fingerprint': runner.fingerprint(value), 'packet': value})
        recipes = [runner.backlog.descriptor(row['packet'], row['fingerprint'])
                   for row in self.rows]
        value = control(source=source(pulls=[]), selected=len(self.rows),
                        provider_status='not_executed', backlog=recipes,
                        backlog_overflow=0,
                        current_fingerprints=[row['fingerprint'] for row in self.rows])
        runner.save(self.out / 'control.json', value)
        runner.save(self.out / 'private/packets.json', self.rows)

    def assert_first_answer_retained(self):
        saved = runner.load(self.out / 'control.json')
        self.assertEqual([row['fingerprint'] for row in saved.get('results', [])],
                         [self.rows[0]['fingerprint']])
        self.assertEqual(saved['results'][0]['record']['status'], 'diagnosed')
        self.assertNotIn(saved.get('provider_status'), {'not_executed', 'completed'})
        return saved

    def terminate_during_second_request(self):
        script = '''
import os, signal, sys
sys.path.insert(0, sys.argv[1])
import drift_runner as runner
calls = 0
def diagnose(value, key):
    global calls
    calls += 1
    if calls == 2:
        os.kill(os.getpid(), signal.SIGTERM)
    print('first response returned', flush=True)
    return {'subject': value['subject'], 'source': value['source'],
            'status': 'diagnosed', 'provider_request_attempted': True,
            'provider_request_attempts': 1,
            'diagnosis': {'answers': {'proof': {'choice': 'supported'}}}}
runner.diagnose = diagnose
runner.run_provider(sys.argv[2], sys.argv[3])
'''
        child = subprocess.run([sys.executable, '-c', script, str(ROOT / 'tools/ci'),
                                str(self.out), PRIVATE_KEY], capture_output=True,
                               text=True, timeout=10)
        self.assertEqual(child.returncode, -signal.SIGTERM, child.stderr)
        self.assertIn('first response returned', child.stdout)

    def test_later_exception_does_not_discard_completed_answer(self):
        with patch.object(runner, 'diagnose', side_effect=[
                completed(self.rows[0]['packet']), ValueError(PRIVATE_TEXT)]) as provider:
            with self.assertRaises(ValueError):
                runner.run_provider(self.out, PRIVATE_KEY)
        self.assertEqual(provider.call_count, 2)
        self.assert_first_answer_retained()

    @unittest.skipUnless(os.name == 'posix', 'process signal behavior requires POSIX')
    def test_sigterm_during_next_request_keeps_completed_answer(self):
        self.terminate_during_second_request()
        self.assert_first_answer_retained()

    def test_partial_publication_preserves_answer_and_unfinished_work_without_retry(self):
        with patch.object(runner, 'diagnose', side_effect=[
                completed(self.rows[0]['packet']), ValueError(PRIVATE_TEXT)]):
            with self.assertRaises(ValueError):
                runner.run_provider(self.out, PRIVATE_KEY)
        api = FakeApi({
            '/branches/feature': {'commit': {'sha': HEAD}},
            '/commits/' + HEAD + '/check-runs?check_name=CCE%20drift%20contracts&per_page=100':
                {'check_runs': []},
        })
        with patch.object(runner, 'diagnose', side_effect=AssertionError('unexpected replay')), \
                patch.dict(os.environ, {'GITHUB_STEP_SUMMARY': ''}):
            result = runner.publish(api, self.out)
        self.assertEqual(result, 0, 'An interrupted advisory provider must not invent a deterministic violation')
        state = runner.load(self.out / 'public/state.json')
        self.assertEqual([row['fingerprint'] for row in state['entries']],
                         [self.rows[0]['fingerprint']])
        remaining = {row['fingerprint'] for row in state['backlog']}
        self.assertTrue({row['fingerprint'] for row in self.rows[1:]} <= remaining)
        summary = (self.out / 'public/summary.md').read_text()
        self.assertIn('diagnosed', summary)
        self.assertNotIn('Provider stage: `not_executed`', summary)
        public = json.dumps(state) + summary + json.dumps(api.calls)
        self.assertNotIn(PRIVATE_TEXT, public)
        self.assertNotIn(PRIVATE_KEY, public)

    def test_complete_batch_keeps_reused_and_new_answers(self):
        reused_packet = packet(99)
        reused = {'fingerprint': runner.fingerprint(reused_packet),
                  'record': completed(reused_packet)}
        saved = runner.load(self.out / 'control.json')
        saved['reused'] = [reused]
        runner.save(self.out / 'control.json', saved)
        with patch.object(runner, 'diagnose', side_effect=lambda value, key: completed(value)) as provider:
            runner.run_provider(self.out, PRIVATE_KEY)
        saved = runner.load(self.out / 'control.json')
        self.assertEqual(provider.call_count, len(self.rows))
        self.assertEqual([row['fingerprint'] for row in saved['results']],
                         [reused['fingerprint']] + [row['fingerprint'] for row in self.rows])
        self.assertEqual(saved['provider_status'], 'completed')

    def test_failed_checkpoint_keeps_previous_complete_json(self):
        replace = os.replace

        def fail_second_answer_checkpoint(temporary, destination):
            checkpoint = json.loads(Path(temporary).read_text())
            if len(checkpoint.get('results', [])) == 2:
                raise OSError('fixture checkpoint write failure')
            return replace(temporary, destination)

        with patch.object(runner.os, 'replace', side_effect=fail_second_answer_checkpoint), \
                patch.object(runner, 'diagnose', side_effect=lambda value, key: completed(value)) as provider:
            with self.assertRaises(OSError):
                runner.run_provider(self.out, PRIVATE_KEY)
        self.assertEqual(provider.call_count, 2)
        self.assert_first_answer_retained()
        self.assertEqual(list(self.out.glob('.control.json.*')), [])

    @unittest.skipUnless(os.name == 'posix', 'process signal behavior requires POSIX')
    def test_sigkill_before_checkpoint_replace_keeps_previous_complete_json(self):
        script = '''
import json, os, signal, sys
from pathlib import Path
sys.path.insert(0, sys.argv[1])
import drift_runner as runner
replace = os.replace
def interrupted_replace(temporary, destination):
    checkpoint = json.loads(Path(temporary).read_text())
    if len(checkpoint.get('results', [])) == 2:
        print('second answer ready for checkpoint', flush=True)
        os.kill(os.getpid(), signal.SIGKILL)
    return replace(temporary, destination)
def diagnose(value, key):
    return {'subject': value['subject'], 'source': value['source'],
            'status': 'diagnosed', 'provider_request_attempted': True,
            'provider_request_attempts': 1,
            'diagnosis': {'answers': {'proof': {'choice': 'supported'}}}}
runner.os.replace = interrupted_replace
runner.diagnose = diagnose
runner.run_provider(sys.argv[2], sys.argv[3])
'''
        child = subprocess.run([sys.executable, '-c', script, str(ROOT / 'tools/ci'),
                                str(self.out), PRIVATE_KEY], capture_output=True,
                               text=True, timeout=10)
        self.assertEqual(child.returncode, -signal.SIGKILL, child.stderr)
        self.assertIn('second answer ready for checkpoint', child.stdout)
        self.assert_first_answer_retained()

    def test_interrupted_phase_cannot_automatically_resend_requests(self):
        with patch.object(runner, 'diagnose', side_effect=[
                completed(self.rows[0]['packet']), ValueError(PRIVATE_TEXT)]):
            with self.assertRaises(ValueError):
                runner.run_provider(self.out, PRIVATE_KEY)
        before = (self.out / 'control.json').read_bytes()
        with patch.object(runner, 'diagnose') as provider:
            with self.assertRaises(ValueError):
                runner.run_provider(self.out, PRIVATE_KEY)
        provider.assert_not_called()
        self.assertEqual((self.out / 'control.json').read_bytes(), before)

    def test_completed_phase_is_idempotent_without_packet_file_or_provider(self):
        with patch.object(runner, 'diagnose', side_effect=lambda value, key: completed(value)):
            original = runner.run_provider(self.out, PRIVATE_KEY)
        before = (self.out / 'control.json').read_bytes()
        (self.out / 'private/packets.json').unlink()
        with patch.object(runner, 'diagnose') as provider:
            repeated = runner.run_provider(self.out, PRIVATE_KEY)
        provider.assert_not_called()
        self.assertEqual(repeated, original)
        self.assertEqual((self.out / 'control.json').read_bytes(), before)


if __name__ == '__main__':
    unittest.main()
