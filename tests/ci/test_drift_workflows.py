"""Regression checks for the workflow trust and output boundary."""
from pathlib import Path
import re
import unittest


ROOT = Path(__file__).resolve().parents[2]


class WorkflowBoundaryTests(unittest.TestCase):
    def test_candidate_job_is_secretless_and_includes_documentation(self):
        text = (ROOT/'.github/workflows/drift-input.yml').read_text()
        self.assertNotIn('secrets.', text)
        self.assertNotIn('paths-ignore', text)
        self.assertIn('python3 -m unittest discover -s tests/ci', text)

    def test_consumer_uses_default_code_and_never_uploads_private_packets(self):
        text = (ROOT/'.github/workflows/jev.yml').read_text()
        self.assertIn('ref: ${{ github.event.repository.default_branch }}', text)
        self.assertNotIn('pull_request_target:', text)
        self.assertIn('checks: write', text)
        self.assertIn('workflow_run:', text)
        paths = re.findall(r'^\s+path:\s+(.+)$', text, re.M)
        self.assertTrue(paths)
        self.assertTrue(all('private' not in p and 'packets' not in p for p in paths))
        self.assertNotIn('${{ runner.temp }}/drift/', paths)

    def test_reservation_precedes_request_and_failure_is_not_globally_ignored(self):
        text = (ROOT/'.github/workflows/jev.yml').read_text()
        self.assertLess(text.index('name: Reserve bounded'), text.index('name: Ask Jev'))
        provider = text[text.index('- name: Ask Jev'):text.index('- name: Publish candidate')]
        publisher = text[text.index('- name: Publish candidate'):text.index('- name: Preserve public')]
        self.assertIn('continue-on-error: true', provider)
        self.assertNotIn('continue-on-error', publisher)
        self.assertIn('always()', publisher)

    def test_data_and_model_credentials_are_confined_to_separate_steps(self):
        text = (ROOT/'.github/workflows/jev.yml').read_text()
        collect = text[text.index('- name: Collect candidate'):text.index('- name: Reserve bounded')]
        provider = text[text.index('- name: Ask Jev'):text.index('- name: Publish candidate')]
        self.assertNotIn('TYPESAFE_API_KEY: ${{ secrets.TYPESAFE_API_KEY }}', collect)
        self.assertIn('TYPESAFE_API_KEY: ${{ secrets.TYPESAFE_API_KEY }}', provider)
        self.assertNotIn('DRIFT_DATA_TOKEN', provider)
        self.assertIn('python3 -I trusted/tools/ci/drift_runner.py', text)


if __name__ == '__main__':
    unittest.main()
