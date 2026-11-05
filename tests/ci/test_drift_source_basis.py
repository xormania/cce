"""A collected PR basis cannot borrow freshness from another PR on the same head."""
import unittest
import test_drift_runner as fixtures
from test_drift_runner import runner, BASE, HEAD, OTHER


class SourceBasisTests(unittest.TestCase):
    def test_only_pull_requests_with_the_collected_base_are_retained(self):
        fixture = fixtures.SourceIdentityTests()
        fixture.setUp()
        api = fixture.api_for()
        path = '/commits/' + HEAD + '/pulls?per_page=30'
        api.responses[path].append({'number': 10, 'state': 'open',
                                   'head': {'sha': HEAD, 'ref': 'feature'},
                                   'base': {'sha': OTHER}})
        candidate = runner.source_for(api, fixture.event, 'workflow_run')
        self.assertEqual(candidate['base_sha'], BASE)
        self.assertEqual(candidate['pulls'], [9])


if __name__ == '__main__':
    unittest.main()
