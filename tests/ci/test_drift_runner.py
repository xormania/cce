"""Offline integration checks for collection, identity and public-output boundaries."""
from __future__ import annotations

import copy
import io
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest
from unittest.mock import patch
import zipfile

ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT / 'tools' / 'ci'))
import drift_runner as runner
from drift_contracts import evaluate

HEAD = 'a' * 40
BASE = 'b' * 40
OTHER = 'c' * 40
TRUSTED = 'd' * 40
REPOSITORY = 'xormania/cce'


class FakeApi:
    def __init__(self, responses=None, files=None, repository=REPOSITORY, logs=None):
        self.repository = repository
        self.responses = responses or {}
        self.files = files or {}
        self.logs = logs or {}
        self.calls = []

    def api(self, path, method='GET', value=None):
        self.calls.append((path, method, copy.deepcopy(value)))
        if method == 'POST':
            row = {'id': 123, 'app': {'slug': 'github-actions'}, **copy.deepcopy(value)}
            self.responses['/check-runs/123'] = row
            return row
        if method == 'PATCH':
            row = dict(self.responses[path], **copy.deepcopy(value))
            self.responses[path] = row
            return row
        if path not in self.responses:
            raise AssertionError('Unexpected API path: ' + path)
        result = self.responses[path]
        if isinstance(result, Exception):
            raise result
        return copy.deepcopy(result)

    def file(self, path, ref):
        self.calls.append(('file:' + path, ref, None))
        return self.files[path]

    def baseline(self, _trusted):
        return {'entries': [], 'pending': [], 'baseline_missing': True}

    def log(self, job_id):
        self.calls.append(('log:' + str(job_id), 'GET', None))
        return self.logs[job_id]


def source(**changes):
    value = {'repository': REPOSITORY, 'head_sha': HEAD, 'base_sha': BASE,
             'run_id': 7, 'attempt': 1, 'branch': 'feature', 'pulls': [9]}
    value.update(changes)
    return value


def packet(n=0, text=None):
    return {'subject': format(n + 1, '024x'), 'source': source(),
            'excerpts': [{'id': 'e1', 'text': text or ('Changed public assertion ' + str(n))}],
            'questions': ['proof']}


def control(**changes):
    value = {'version': runner.VERSION, 'source': source(), 'trusted_sha': TRUSTED,
             'engine_hash': runner.engine_hash(), 'previous_entries': [],
             'violations': [], 'limitations': [], 'data_failed': False, 'waiting': False,
             'selected': 0, 'reused': [], 'pending': [], 'results': [], 'baseline_missing': False}
    value.update(changes)
    return value


def archive(entries, symlink=False):
    out = io.BytesIO()
    with zipfile.ZipFile(out, 'w', compression=zipfile.ZIP_DEFLATED) as handle:
        for name, content in entries:
            info = zipfile.ZipInfo(name)
            if symlink:
                info.create_system = 3
                info.external_attr = 0o120777 << 16
            handle.writestr(info, content)
    return out.getvalue()


class SourceIdentityTests(unittest.TestCase):
    def setUp(self):
        self.run = {'id': 7, 'run_attempt': 1, 'head_sha': HEAD,
                    'head_repository': {'full_name': REPOSITORY},
                    'path': '.github/workflows/rust.yml', 'status': 'completed',
                    'conclusion': 'success', 'head_branch': 'feature'}
        self.event = {'workflow_run': {'id': 7, 'run_attempt': 1, 'head_sha': HEAD}}

    def api_for(self, run=None):
        return FakeApi({'/actions/runs/7': run or self.run,
                        '/commits/' + HEAD: {'parents': [{'sha': OTHER}]},
                        '/commits/' + HEAD + '/pulls?per_page=30': [
                            {'number': 9, 'state': 'open', 'head': {'sha': HEAD, 'ref': 'feature'},
                             'base': {'sha': BASE}}]})

    def test_source_identity_and_pr_basis_are_attached(self):
        value = runner.source_for(self.api_for(), self.event, 'workflow_run')
        self.assertEqual(value, source())

    def test_forged_or_incomplete_source_is_refused(self):
        mutations = [
            {'head_repository': {'full_name': 'someone/fork'}},
            {'path': '.github/workflows/other.yml'},
            {'head_sha': OTHER}, {'run_attempt': 2}, {'status': 'in_progress'},
        ]
        for changed in mutations:
            with self.subTest(changed=changed):
                run = dict(self.run, **changed)
                with self.assertRaises(ValueError):
                    runner.source_for(self.api_for(run), self.event, 'workflow_run')

    def test_intentional_duplicate_is_not_counted_as_execution(self):
        run = dict(self.run, conclusion='skipped')
        with self.assertRaisesRegex(ValueError, 'intentional_duplicate'):
            runner.source_for(self.api_for(run), self.event, 'workflow_run')

    def test_invalid_manual_candidate_is_refused_before_commit_lookup(self):
        api = FakeApi()
        with self.assertRaises(ValueError):
            runner.source_for(api, {'repository': {'default_branch': 'master'}},
                              'workflow_dispatch', '../other')
        self.assertEqual(api.calls, [])

    def test_missing_pr_uses_committed_parent(self):
        api = self.api_for()
        api.responses['/commits/' + HEAD + '/pulls?per_page=30'] = []
        value = runner.source_for(api, self.event, 'workflow_run')
        self.assertEqual(value['base_sha'], OTHER)
        self.assertEqual(value['pulls'], [])


class DataEvidenceTests(unittest.TestCase):
    def env(self, required=False):
        return {'DRIFT_DATA_REPOSITORY': 'team/records', 'DRIFT_DATA_TOKEN': 'read-only-token',
                'DRIFT_DATA_FILES': json.dumps({'requirements': 'evidence/map.toml',
                                                'summary': 'evidence/status.md'}),
                'DRIFT_DATA_REQUIRED': str(required).lower()}

    def data_api(self, verified=HEAD, case_status='partial'):
        mapping = ('[[cases]]\nid="case-a"\nslice="group-a"\nstatus="green"\n'
                   '[[cases]]\nid="case-b"\nslice="group-a"\nstatus="' + case_status + '"\n')
        summary = ('---\nverified-at: ' + verified + '\n---\n'
                   '| Group | State |\n| group-a | 2/2 green |\n')
        return FakeApi({'/commits/HEAD': {'sha': OTHER}},
                       {'evidence/map.toml': mapping, 'evidence/status.md': summary},
                       repository='team/records')

    def test_data_is_pinned_and_candidate_scoped(self):
        api = self.data_api()
        claims, semantic, limits, failed = runner.data_evidence(source(), self.env(),
                                                               lambda _repo, _token: api)
        self.assertFalse(failed)
        self.assertEqual(limits, ['decision_data_not_configured'])
        self.assertTrue(claims[0]['applicable'])
        result = evaluate({'source': source(), 'claims': claims})
        self.assertIn('claim_case_mismatch', [v['code'] for v in result['violations']])
        self.assertTrue(semantic)
        file_calls = [call for call in api.calls if call[0].startswith('file:')]
        self.assertTrue(file_calls)
        self.assertTrue(all(call[1] == OTHER for call in file_calls))

    def test_same_claim_at_other_candidate_is_uncertain_not_a_violation(self):
        api = self.data_api(verified=OTHER)
        claims, _, limits, _ = runner.data_evidence(source(), self.env(), lambda _r, _t: api)
        self.assertFalse(claims[0]['applicable'])
        self.assertIn('data_candidate_not_proven', limits)
        self.assertEqual(evaluate({'source': source(), 'claims': claims})['violations'], [])

    def test_required_data_without_candidate_identity_cannot_complete_validation(self):
        api = self.data_api(verified=OTHER)
        _, _, limits, failed = runner.data_evidence(source(), self.env(True), lambda _r, _t: api)
        self.assertIn('data_candidate_not_proven', limits)
        self.assertTrue(failed)

    def test_resolved_map_clears_exact_contradiction(self):
        api = self.data_api(case_status='green')
        claims, _, _, _ = runner.data_evidence(source(), self.env(), lambda _r, _t: api)
        self.assertEqual(evaluate({'source': source(), 'claims': claims})['violations'], [])

    def test_data_is_never_imported_or_executed(self):
        api = self.data_api()
        marker = 'raise RuntimeError("private-marker-do-not-run")'
        api.files['evidence/status.md'] += marker
        claims, _, _, failed = runner.data_evidence(source(), self.env(), lambda _r, _t: api)
        self.assertFalse(failed)
        self.assertEqual(len(claims), 1)

    def test_required_data_absence_is_distinct_from_optional_setup(self):
        result = runner.data_evidence(source(), {})
        self.assertEqual(result, ([], [], ['data_not_configured', 'decision_data_not_configured'], False))
        result = runner.data_evidence(source(), {'DRIFT_DATA_REQUIRED': 'true'})
        self.assertTrue(result[-1])
        env = self.env(True)
        env['DRIFT_DATA_TOKEN'] = ''
        self.assertEqual(runner.data_evidence(source(), env)[2:],
                         (['data_credentials_missing'], True))

    def test_unrecognized_data_configuration_is_not_silently_accepted(self):
        env = self.env(True)
        env['DRIFT_DATA_FILES'] = '{"script":"danger.py"}'
        claims, semantic, limits, failed = runner.data_evidence(source(), env)
        self.assertEqual((claims, semantic), ([], []))
        self.assertEqual(limits, ['data_source_unavailable'])
        self.assertTrue(failed)


class ExecutionEvidenceTests(unittest.TestCase):
    def api_for(self, test_status='success'):
        runs = [{'id': 11, 'name': 'drift-input', 'path': '.github/workflows/drift-input.yml',
                 'head_sha': HEAD, 'head_repository': {'full_name': REPOSITORY}, 'event': 'push', 'run_number': 1,
                 'run_attempt': 1, 'status': 'completed'},
                {'id': 12, 'name': 'rust', 'path': '.github/workflows/rust.yml',
                 'head_sha': HEAD, 'head_repository': {'full_name': REPOSITORY}, 'event': 'push', 'run_number': 2,
                 'run_attempt': 1, 'status': 'completed'}]
        for row in runs:
            row.update(conclusion='success', head_branch='feature', pull_requests=[])
        return FakeApi({
            '/actions/runs?head_sha=' + HEAD + '&per_page=50': {'workflow_runs': runs},
            '/pulls/9':{'state':'open','head':{'sha':HEAD},'base':{'sha':BASE},'merge_commit_sha':TRUSTED},
            '/actions/runs/11/jobs?per_page=50': {'jobs': [{'id': 111, 'conclusion': 'success', 'steps': [
                {'name': 'Unit and replay tests', 'conclusion': 'success'}]}]},
            '/actions/runs/12/jobs?per_page=50': {'jobs': [{'id': 112, 'conclusion': 'success', 'steps': [
                {'name': 'Format', 'conclusion': 'success'},
                {'name': 'Clippy', 'conclusion': 'success'},
                {'name': 'Test', 'conclusion': test_status}]}]},
            '/commits/' + HEAD + '/check-runs?check_name=scan&per_page=100': {'check_runs': [{
                'id': 501, 'app': {'id': 15368}, 'status': 'completed', 'conclusion': 'success',
                'started_at': '2026-11-04T10:00:00Z',
                'details_url': 'https://github.com/' + REPOSITORY + '/actions/runs/13',
                'output': {'summary': 'no private markers in PR text, commit messages, added lines or file names'}}]},
            '/actions/runs/13': {'id': 13, 'name': 'leak-scan', 'path': '.github/workflows/leak-scan.yml',
                                 'event': 'workflow_run', 'head_sha': TRUSTED,
                                 'head_repository': {'full_name': REPOSITORY}, 'status': 'completed',
                                 'conclusion': 'success', 'run_attempt': 1},
        }, files={path: (ROOT / path).read_text() for path in
                  ['.github/workflows/' + name + '.yml' for name in ('drift-input', 'rust', 'leak-scan')]
                  + ['tools/ci/leak_scan_trusted.sh', 'tools/ci/leak_scan.sh']},
           logs={111: 'Ran 27 tests in 0.3s\nOK\n',
                 112: 'test result: ok. 10 passed; 0 failed; 0 ignored\n'})

    def test_successful_steps_are_identified_by_actual_candidate(self):
        checks, limits, waiting = runner.execution_checks(self.api_for(), source(), False)
        self.assertFalse(waiting)
        self.assertEqual(limits, [])
        self.assertEqual(len(checks), 5)
        self.assertTrue(all(row['executed'] and row['source_sha'] == HEAD for row in checks))
        self.assertEqual(evaluate({'source': source(), 'checks': checks})['violations'], [])

    def test_skipped_required_step_fails_despite_successful_workflow(self):
        checks, _, _ = runner.execution_checks(self.api_for('skipped'), source(), False)
        result = evaluate({'source': source(), 'checks': checks})
        self.assertIn('required_check_not_executed', [v['code'] for v in result['violations']])

    def test_markdown_change_does_not_require_filtered_rust_run(self):
        api = self.api_for()
        checks, _, waiting = runner.execution_checks(api, source(), True)
        self.assertFalse(waiting)
        self.assertEqual([row['id'] for row in checks], ['Unit and replay tests','Trusted leak scan'])
        self.assertFalse(any(call[0].startswith('/actions/runs/12/jobs') for call in api.calls))

    def test_absent_required_execution_does_not_become_success(self):
        api = FakeApi({'/actions/runs?head_sha=' + HEAD + '&per_page=50': {'workflow_runs': []},
                       '/commits/' + HEAD + '/check-runs?check_name=scan&per_page=100': {'check_runs': []}})
        checks, limits, waiting = runner.execution_checks(api, source(), False)
        self.assertTrue(waiting or evaluate({'source':source(),'checks':checks})['violations'])
        self.assertIn('producer_definition_unverified', limits)

    def test_display_name_alone_cannot_satisfy_required_workflow(self):
        for changed in [{'path': '.github/workflows/unrelated.yml'}, {'head_sha': OTHER},
                        {'head_repository': {'full_name': 'someone/fork'}}]:
            with self.subTest(changed=changed):
                api = self.api_for()
                rows = api.responses['/actions/runs?head_sha=' + HEAD + '&per_page=50']['workflow_runs']
                rows[0].update(changed)
                checks, limits, waiting = runner.execution_checks(api, source(), True)
                self.assertFalse(any(row['id'] == 'Unit and replay tests' for row in checks))
                self.assertTrue(waiting)
                self.assertIn('required_execution_unavailable', limits)
                self.assertFalse(any(c[0].startswith('/actions/runs/11/jobs') for c in api.calls))

    def test_success_with_zero_tests_does_not_establish_execution(self):
        api = self.api_for()
        api.logs[111] = 'Ran 0 tests in 0.0s\nOK\n'
        api.logs[112] = 'test result: ok. 0 passed; 0 failed; 0 ignored\n'
        observations = []
        checks, _, _ = runner.execution_checks(api, source(), False, observations)
        result = evaluate({'source': source(), 'checks': checks})
        self.assertEqual(sum(row['code'] == 'required_check_not_executed'
                             for row in result['violations']), 2)
        self.assertEqual(len(observations), 2)

    def test_actual_rust_and_unittest_logs_establish_nonzero_execution(self):
        observations = []
        checks, limits, waiting = runner.execution_checks(self.api_for(), source(), False, observations)
        self.assertEqual(evaluate({'source': source(), 'checks': checks})['violations'], [])
        self.assertEqual(limits, [])
        self.assertFalse(waiting)
        self.assertEqual(len(observations), 2)

    def test_empty_logs_cannot_prove_required_test_execution(self):
        api = self.api_for()
        api.logs = {111: '', 112: ''}
        checks, limits, waiting = runner.execution_checks(api, source(), False, [])
        violations = evaluate({'source': source(), 'checks': checks})['violations']
        self.assertTrue(waiting or limits or violations,
                        'Successful steps with empty logs were treated as proven execution')


class BudgetAndArchiveTests(unittest.TestCase):
    def test_cached_results_reject_unknown_fields_and_choices(self):
        good = {'fingerprint': 'e' * 64, 'record': {'subject': 'f' * 24, 'status': 'diagnosed',
                'diagnosis': {'answers': {'proof': {'choice': 'supported'}}}}}
        for mutation in [
            {'fingerprint': 'e' * 64, 'record': dict(good['record'], secret_echo='private-marker')},
            {'fingerprint': 'e' * 64, 'record': dict(good['record'], diagnosis={
                'answers': {'proof': {'choice': 'private-marker'}}})},
            dict(good, private_field='private-marker'),
        ]:
            with self.subTest(mutation=mutation):
                self.assertFalse(runner.safe_state_entry(mutation))
        good['record']['source'] = {'private_field': 'private-marker'}
        good['record']['diagnosis']['answers']['proof']['untrusted'] = 'private-marker'
        self.assertTrue(runner.safe_state_entry(good))
        self.assertNotIn('private-marker', json.dumps(good))

    def test_baseline_ignores_candidate_push_and_nondefault_dispatch(self):
        safe = {'fingerprint': 'e' * 64, 'record': {'subject': 'f' * 24, 'status': 'abstained'}}
        runs = [{'id': 11, 'event': 'push', 'status': 'completed', 'head_branch': 'feature'},
                {'id': 12, 'event': 'workflow_dispatch', 'status': 'completed', 'head_branch': 'feature'},
                {'id': 13, 'event': 'workflow_run', 'status': 'completed', 'head_branch': 'master'}]
        api = FakeApi({'/actions/workflows/jev.yml/runs?per_page=10': {'workflow_runs': runs},
                       '/': {'default_branch': 'master'},
                       '/actions/runs/13/artifacts?per_page=20': {'artifacts': [
                           {'id': 1, 'name': 'drift-state', 'expired': False}]}})
        api.archive = lambda _id, _name: {'version': runner.VERSION, 'engine_hash': runner.engine_hash(),
                                         'entries': [safe], 'pending': []}
        with patch.dict(os.environ, {'GITHUB_RUN_ID': '99'}):
            result = runner.GitHub.baseline(api, TRUSTED)
        self.assertEqual(len(result['entries']), 1)
        self.assertFalse(result['baseline_missing'])
        self.assertFalse(any('/runs/11/artifacts' in c[0] or '/runs/12/artifacts' in c[0] for c in api.calls))

    def test_reserved_attempt_is_not_automatically_retried(self):
        key = runner.fingerprint(packet())
        previous = {'entries': [], 'reserved': [key]}
        selected, reused, deferred = runner.choose([packet()], previous)
        self.assertEqual((selected, deferred), ([], []))
        self.assertEqual(reused[0]['record']['status'], 'previous_request_reserved')
        selected, reused, _ = runner.choose([packet()], previous, force=True)
        self.assertEqual((len(selected), reused), (1, []))

    def test_identical_packets_share_one_provider_selection(self):
        selected, reused, deferred = runner.choose([packet(), packet()], {'entries': []})
        self.assertEqual(len(selected), 1)
        self.assertEqual((reused, deferred), ([], []))

    def test_unchanged_content_is_reused_across_code_revisions(self):
        first = packet()
        changed_revision = copy.deepcopy(first)
        changed_revision['source']['head_sha'] = OTHER
        self.assertEqual(runner.fingerprint(first), runner.fingerprint(changed_revision))
        entry = {'fingerprint': runner.fingerprint(first), 'record': {'status': 'diagnosed'}}
        selected, reused, pending = runner.choose([changed_revision], {'entries': [entry]})
        self.assertEqual(selected, [])
        self.assertEqual(reused, [entry])
        self.assertEqual(pending, [])

    def test_changed_evidence_reopens_and_budget_carries_excess(self):
        packets = [packet(i) for i in range(runner.MAX_CALLS + 2)]
        selected, _, deferred = runner.choose(packets, {'entries': []})
        self.assertEqual(len(selected), runner.MAX_CALLS)
        self.assertEqual(len(deferred), 2)
        next_selected, _, _ = runner.choose(packets, {'entries': [], 'pending': deferred})
        self.assertEqual({row['fingerprint'] for row in next_selected[:2]}, set(deferred))
        previous = {'entries': [{'fingerprint': runner.fingerprint(packet()), 'record': {}}]}
        selected, reused, _ = runner.choose([packet(text='New assertion')], previous)
        self.assertEqual((len(selected), reused), (1, []))

    def test_archive_is_data_only_with_strict_inventory(self):
        self.assertEqual(runner.read_state_archive(archive([('state.json', '{"version":1}')]))['version'], 1)
        for entries, symlink in [([('../state.json', '{}')], False),
                                 ([('state.json', '{}'), ('extra.py', 'raise RuntimeError()')], False),
                                 ([('state.json', '{}')], True),
                                 ([('state.json', ' ' * (runner.MAX_BYTES + 1))], False)]:
            with self.subTest(entries=[x[0] for x in entries], symlink=symlink):
                with self.assertRaises(ValueError):
                    runner.read_state_archive(archive(entries, symlink))

    def test_paths_cannot_escape_or_name_data_scripts_as_commands(self):
        for path in ['/absolute', '../outside', 'a/../outside', 'a\\b', 'a//b']:
            self.assertFalse(runner.safe_path(path))
        self.assertTrue(runner.safe_path('docs/behavior.md'))


class PublishingTests(unittest.TestCase):
    def publish_control(self, value, current=HEAD):
        directory = tempfile.TemporaryDirectory()
        self.addCleanup(directory.cleanup)
        root = Path(directory.name)
        runner.save(root / 'control.json', value)
        api = FakeApi({'/pulls/9': {'state': 'open', 'head': {'sha': current}},
                       '/commits/' + HEAD + '/check-runs?check_name=CCE%20drift%20contracts&per_page=100':
                       {'check_runs': []}})
        with patch.dict(os.environ, {'GITHUB_STEP_SUMMARY': ''}):
            result = runner.publish(api, root)
        posts = [call for call in api.calls if call[1] in {'POST', 'PATCH'}]
        return result, posts, root

    def test_result_is_attached_to_source_candidate(self):
        result, posts, _ = self.publish_control(control())
        self.assertEqual(result, 0)
        self.assertEqual(posts[0][2]['head_sha'], HEAD)
        self.assertEqual(posts[-1][2]['conclusion'], 'success')

    def test_deterministic_violation_fails_the_applicable_candidate(self):
        value = control(violations=[{'code': 'claim_case_mismatch', 'subject': 'f' * 24}])
        result, posts, _ = self.publish_control(value)
        self.assertEqual(result, 1)
        self.assertEqual(posts[-1][2]['conclusion'], 'failure')

    def test_older_result_cannot_clear_newer_candidate(self):
        result, posts, root = self.publish_control(control(), current=OTHER)
        self.assertEqual(result, 0)
        self.assertEqual(posts, [])
        self.assertTrue(runner.load(root / 'public' / 'state.json')['superseded'])

    def test_waiting_evidence_never_publishes_success(self):
        _, posts, _ = self.publish_control(control(waiting=True))
        self.assertEqual(posts[0][2]['status'], 'in_progress')
        self.assertNotIn('conclusion', posts[0][2])

    def test_private_input_does_not_enter_public_render(self):
        marker = 'private-marker-not-for-public-output'
        normalized = {'source': source(), 'claims': [{'id': marker, 'applicable': True,
                       'status': 'complete', 'case_statuses': ['partial'], 'excerpts': [marker]}]}
        result = evaluate(normalized)
        _, posts, root = self.publish_control(control(violations=result['violations']))
        public = json.dumps(posts) + (root / 'public' / 'summary.md').read_text()
        public += (root / 'public' / 'state.json').read_text()
        self.assertNotIn(marker, public)


class CollectionIntegrationTests(unittest.TestCase):
    def test_green_markdown_source_collects_and_reaches_provider_cli_without_credentials(self):
        run = {'id': 7, 'run_attempt': 1, 'head_sha': HEAD,
               'head_repository': {'full_name': REPOSITORY},
               'path': '.github/workflows/drift-input.yml', 'status': 'completed',
               'conclusion': 'success', 'head_branch': 'feature'}
        api = FakeApi({
            '/actions/runs/7': run,
            '/commits/' + HEAD: {'parents': [{'sha': BASE}]},
            '/commits/' + HEAD + '/pulls?per_page=30': [],
            '/compare/' + BASE + '...' + HEAD: {'files': [
                {'filename': 'README.md', 'status': 'modified',
                 'patch': '-The reader reports damage.\n+The reader preserves the expected bytes.'}]},
            '/actions/runs?head_sha=' + HEAD + '&per_page=50': {'workflow_runs': [
                {'id': 7, 'name': 'drift-input', 'path': '.github/workflows/drift-input.yml',
                 'head_sha': HEAD, 'head_repository': {'full_name': REPOSITORY}, 'event': 'push', 'run_number': 1,
                 'run_attempt': 1, 'status': 'completed'}]},
            '/actions/runs/7/jobs?per_page=50': {'jobs': [{'id': 71, 'conclusion': 'success', 'steps': [
                {'name': 'Unit and replay tests', 'conclusion': 'success'}]}]},
        }, {'README.md': 'The reader preserves the expected bytes.',
            **{'.github/workflows/'+name+'.yml':(ROOT/'.github/workflows'/(name+'.yml')).read_text()
               for name in ('drift-input','rust','leak-guard')}}, logs={71: 'Ran 27 tests in 0.3s\nOK\n'})
        event = {'workflow_run': {'id': 7, 'run_attempt': 1, 'head_sha': HEAD}}
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            value = runner.collect(api, event, 'workflow_run', root, {'JEV_TRUSTED_SHA': TRUSTED})
            self.assertFalse(value['violations'])
            self.assertGreater(value['selected'], 0)
            packets = runner.load(root / 'private' / 'packets.json')
            self.assertTrue(all(row['packet']['source']['head_sha'] == HEAD for row in packets))
            after = runner.run_provider(root, '')
            self.assertTrue(all(row['record']['status'] == 'missing_credentials'
                                for row in after['results']))
            self.assertFalse(any(row['record']['provider_request_attempted']
                                 for row in after['results']))


class IsolatedCliTests(unittest.TestCase):
    def test_real_diagnose_cli_uses_trusted_imports_and_never_calls_without_key(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            hostile = root / 'unrelated'
            hostile.mkdir()
            for name in ('drift_contracts.py', 'jev_client.py', 'drift_runner.py'):
                (hostile / name).write_text('raise AssertionError("wrong import")\n')
            out = root / 'out'
            runner.save(out / 'control.json', control(selected=1))
            row = {'fingerprint': runner.fingerprint(packet()), 'packet': packet()}
            runner.save(out / 'private' / 'packets.json', [row])
            env = dict(os.environ, TYPESAFE_API_KEY='', PYTHONPATH=str(hostile))
            process = subprocess.run([sys.executable, '-I', str(ROOT / 'tools/ci/drift_runner.py'),
                                      'diagnose', '--out', str(out)], cwd=hostile, env=env,
                                     text=True, capture_output=True, timeout=10)
            self.assertEqual(process.returncode, 0, process.stdout + process.stderr)
            result = runner.load(out / 'control.json')['results'][0]['record']
            self.assertEqual(result['status'], 'missing_credentials')
            self.assertFalse(result['provider_request_attempted'])
            self.assertEqual(result['provider_request_attempts'], 0)


if __name__ == '__main__':
    unittest.main()
