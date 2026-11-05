"""Candidate-scoped lifecycle tests with an in-memory GitHub transport."""
import copy
import os
from pathlib import Path
import sys
import unittest
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).resolve().parents[2] / 'tools' / 'ci'))
import drift_check as dc

HEAD, OTHER = 'a' * 40, 'b' * 40
SOURCE = {'head_sha':HEAD, 'base_sha':OTHER, 'branch':'topic', 'pulls':[7]}


def row(identity=10, head=HEAD, owner='cce-drift:8:1', app='github-actions'):
    return {'id':identity, 'name':dc.CHECK, 'head_sha':head, 'external_id':owner,
            'app':{'slug':app}, 'status':'completed', 'conclusion':'success'}


class API:
    def __init__(self, rows=None, current=HEAD, state='open', base=OTHER):
        self.rows = {r['id']:copy.deepcopy(r) for r in (rows or [])}
        self.current, self.state, self.base = current, state, base
        self.calls = []

    def api(self, path, method='GET', value=None):
        self.calls.append((path, method, copy.deepcopy(value)))
        if path.startswith('/commits/'):
            return {'check_runs':list(copy.deepcopy(self.rows).values())}
        if path.startswith('/pulls/'):
            return {'state':self.state, 'head':{'sha':self.current}, 'base':{'sha':self.base}}
        if path.startswith('/branches/'):
            return {'commit':{'sha':self.current}}
        if path == '/check-runs':
            identity = max(self.rows, default=0) + 1
            self.rows[identity] = dict(value, id=identity, app={'slug':'github-actions'})
            return copy.deepcopy(self.rows[identity])
        if path.startswith('/check-runs/'):
            identity = int(path.rsplit('/', 1)[1])
            if method == 'PATCH':
                self.rows[identity].update(value)
            return copy.deepcopy(self.rows[identity])
        raise AssertionError(path)


class LifecycleTests(unittest.TestCase):
    def setUp(self):
        self.environment = patch.dict(os.environ, {'GITHUB_RUN_ID':'11', 'GITHUB_RUN_ATTEMPT':'2'})
        self.environment.start()
        self.addCleanup(self.environment.stop)

    def test_new_check_is_on_source_head_and_in_progress(self):
        api = API()
        identity = dc.begin(api, SOURCE)
        self.assertEqual(api.rows[identity]['head_sha'], HEAD)
        self.assertEqual(api.rows[identity]['status'], 'in_progress')
        self.assertEqual(api.rows[identity]['external_id'], 'cce-drift:11:2')

    def test_own_existing_check_is_restarted_not_duplicated(self):
        api = API([row()])
        self.assertEqual(dc.begin(api, SOURCE), 10)
        self.assertEqual(len(api.rows), 1)
        self.assertEqual(api.rows[10]['status'], 'in_progress')

    def test_unowned_check_cannot_be_overwritten(self):
        api = API([row(owner='foreign'), row(11, app='other')])
        self.assertEqual(dc.begin(api, SOURCE), 12)
        self.assertEqual(api.rows[10]['external_id'], 'foreign')
        self.assertEqual(api.rows[11]['app']['slug'], 'other')

    def test_wrong_head_is_not_reused(self):
        api = API([row(head=OTHER)])
        self.assertEqual(dc.begin(api, SOURCE), 11)

    def test_success_completes_same_check(self):
        api = API()
        identity = dc.begin(api, SOURCE)
        self.assertTrue(dc.complete(api, identity, SOURCE, 'success', 'Validated.'))
        self.assertEqual(api.rows[identity]['conclusion'], 'success')
        self.assertEqual(len(api.rows), 1)

    def test_collection_failure_replaces_previous_success(self):
        api = API([row()])
        identity = dc.begin(api, SOURCE)
        dc.complete(api, identity, SOURCE, 'failure', 'Required collection failed.')
        self.assertEqual(api.rows[identity]['status'], 'completed')
        self.assertEqual(api.rows[identity]['conclusion'], 'failure')

    def test_newer_head_cannot_receive_old_success(self):
        api = API(current=OTHER)
        identity = dc.begin(api, SOURCE)
        self.assertFalse(dc.complete(api, identity, SOURCE, 'success', 'Old success.'))
        self.assertEqual(api.rows[identity]['head_sha'], HEAD)
        self.assertEqual(api.rows[identity]['conclusion'], 'cancelled')
        self.assertNotIn('Old success', api.rows[identity]['output']['summary'])

    def test_changed_base_cannot_receive_old_success(self):
        api = API()
        identity = dc.begin(api, SOURCE)
        api.base = 'c' * 40
        self.assertFalse(dc.complete(api, identity, SOURCE, 'success', 'Old merge success.'))
        self.assertEqual(api.rows[identity]['conclusion'], 'cancelled')

    def test_missing_base_is_not_a_current_pr_candidate(self):
        api = API(base=None)
        self.assertFalse(dc.current_candidate(api, SOURCE))

    def test_another_pull_with_only_matching_head_cannot_supply_freshness(self):
        api = API(base='c' * 40)
        self.assertFalse(dc.current_candidate(api, dict(SOURCE, pulls=[7, 8])))

    def test_closed_pull_is_not_current(self):
        api = API(state='closed')
        identity = dc.begin(api, SOURCE)
        self.assertFalse(dc.complete(api, identity, SOURCE, 'success', 'Old success.'))

    def test_superseded_owner_cannot_cancel_or_complete_newer_work(self):
        api = API()
        identity = dc.begin(api, SOURCE)
        api.rows[identity]['external_id'] = 'cce-drift:12:1'
        before = copy.deepcopy(api.rows[identity])
        with self.assertRaisesRegex(ValueError, '^superseded_check_owner$'):
            dc.complete(api, identity, SOURCE, 'success', 'Old success.')
        self.assertEqual(api.rows[identity], before)

    def test_forged_check_identity_is_rejected_before_update(self):
        api = API([row(head=OTHER, owner='cce-drift:11:2')])
        with self.assertRaisesRegex(ValueError, '^invalid_check_identity$'):
            dc.complete(api, 10, SOURCE, 'failure', 'No.')
        self.assertFalse(any(method == 'PATCH' for _, method, _ in api.calls))

    def test_branch_without_pull_is_checked(self):
        api = API(current=OTHER)
        source = dict(SOURCE, pulls=[], branch='topic/next')
        self.assertFalse(dc.current_candidate(api, source))
        self.assertEqual(api.calls[-1][0], '/branches/topic%2Fnext')

    def test_invalid_inputs_are_content_free(self):
        for source in [{'head_sha':'private-marker'}, {'head_sha':HEAD, 'pulls':['7']}]:
            with self.assertRaises(ValueError) as error:
                if source['head_sha'] == HEAD:
                    dc.current_candidate(API(), source)
                else:
                    dc.begin(API(), source)
            self.assertNotIn('private-marker', str(error.exception))


if __name__ == '__main__':
    unittest.main()
