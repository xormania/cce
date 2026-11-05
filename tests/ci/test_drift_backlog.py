"""Unfinished diagnosis survives a changing diff without publishing its inputs."""
from __future__ import annotations

import copy
import hashlib
import json
from pathlib import Path
import sys
import unittest

ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT / 'tools' / 'ci'))
import drift_backlog as backlog

HEAD, BASE, NEW = 'a' * 40, 'b' * 40, 'c' * 40


def source(head=HEAD, branch='feature'):
    return {'repository': 'xormania/cce', 'head_sha': head, 'base_sha': BASE,
            'branch': branch, 'pulls': [15], 'run_id': 7, 'attempt': 1}


def packet(n=0, head=HEAD, branch='feature'):
    return {'subject': format(n + 1, '024x'), 'source': source(head, branch),
            'excerpts': [{'id': 'e1', 'text': 'Original assertion ' + str(n)}],
            'questions': ['proof']}


def fingerprint(value):
    return hashlib.sha256(json.dumps({key: value[key] for key in (
        'subject', 'excerpts', 'questions')}, sort_keys=True).encode()).hexdigest()


def recipe(value, **kwargs):
    return backlog.descriptor(value, fingerprint(value), **kwargs)


class BacklogTests(unittest.TestCase):
    def test_repeated_omission_does_not_invent_new_lost_relationships(self):
        rows = [recipe(packet(i)) for i in range(3)]
        previous = {'items': [], 'overflow': 0}
        counts = []
        for _ in range(3):
            previous = backlog.merge(previous['items'], rows, maximum=2,
                                     previous_overflow=previous['overflow'])
            counts.append(previous['overflow'])
        self.assertEqual(counts, [1, 1, 1])
        self.assertIn('backlog_inventory_truncated', previous['limitations'])

    def test_correction_metadata_is_bounded_and_contains_only_hashes(self):
        original = packet()
        valid = {'anchor': 'a' * 64, 'before': ['b' * 64], 'after': ['c' * 64]}
        row = recipe(dict(original, correction=valid))
        self.assertEqual(backlog.safe_descriptor(row), row)
        for changed in [dict(valid, anchor='private-marker'), dict(valid, extra='private-marker'),
                        dict(valid, before=['b' * 64, 'b' * 64]),
                        dict(valid, after=[format(i, '064x') for i in range(17)])]:
            self.assertIsNone(backlog.safe_descriptor(dict(row, correction=changed)))

    def test_budget_overflow_reconstructs_after_unrelated_followup(self):
        original = [packet(i) for i in range(6)]
        pending = backlog.merge([], [recipe(value) for value in original[4:]])
        # The follow-up no longer contains either original relationship.
        current = [packet(20, head=NEW)]
        calls = []

        def collector(origin, kind, ref, key):
            calls.append((origin, kind, ref, key))
            return copy.deepcopy(original)

        packets, retained, limits = backlog.reconstruct(
            pending['items'], collector, fingerprint,
            scope=backlog.scope_for(source(NEW)))
        self.assertEqual(limits, [])
        self.assertEqual([value['subject'] for value in packets],
                         [value['subject'] for value in original[4:]])
        self.assertEqual(len(calls), 1)
        self.assertTrue(all(value['source']['head_sha'] == HEAD for value in packets))
        next_queue = backlog.merge(retained, [recipe(value) for value in current])
        self.assertEqual(len(next_queue['items']), 3)
        # Only actual finished results remove the old work.
        finished = backlog.merge(next_queue['items'], [],
                                 finished=[fingerprint(value) for value in packets])
        self.assertEqual([row['subject'] for row in finished['items']], [current[0]['subject']])

    def test_private_marker_never_enters_public_recipe(self):
        value = packet()
        marker = 'private-do-not-publish/requirements-and-path'
        value.update(private_repository=marker, private_path=marker)
        value['excerpts'][0]['text'] = marker
        row = recipe(value, kind='data', data_ref='d' * 40,
                     data_key=hashlib.sha256(marker.encode()).hexdigest())
        queue = backlog.merge([], [row])
        self.assertNotIn(marker, json.dumps(queue))
        self.assertNotIn('excerpts', json.dumps(queue))
        self.assertNotIn('private_repository', json.dumps(queue))
        self.assertEqual(row['data_ref'], 'd' * 40)

    def test_merged_branch_work_can_be_drained_without_relabelling_default_head(self):
        original = packet(5)
        queue = backlog.merge([], [recipe(original)])
        current = packet(20, head=NEW, branch='master')
        queue = backlog.merge(queue['items'], [recipe(current)])
        observed = {HEAD: original, NEW: current}
        packets, retained, limits = backlog.reconstruct(
            queue['items'], lambda origin, *_args: [observed[origin['head_sha']]],
            fingerprint)
        self.assertEqual(limits, [])
        self.assertEqual(len(retained), 2)
        self.assertEqual(packets[0]['source']['head_sha'], HEAD)
        self.assertEqual(packets[0]['source']['branch'], 'feature')
        self.assertEqual(packets[1]['source']['head_sha'], NEW)

    def test_missing_original_is_unavailable_not_resolved(self):
        value = packet()
        packets, retained, limits = backlog.reconstruct(
            [recipe(value)], lambda *_args: [], fingerprint)
        self.assertEqual(packets, [])
        self.assertEqual(retained[0]['state'], 'unavailable')
        self.assertEqual(retained[0]['fingerprint'], fingerprint(value))
        self.assertIn('backlog_source_unavailable', limits)
        self.assertEqual(len(backlog.merge(retained, [])['items']), 1)

    def test_changed_context_cannot_impersonate_original(self):
        value = packet()
        changed = copy.deepcopy(value)
        changed['excerpts'][0]['text'] = 'Weaker replacement assertion'
        packets, retained, limits = backlog.reconstruct(
            [recipe(value)], lambda *_args: [changed], fingerprint)
        self.assertEqual(packets, [])
        self.assertEqual(retained[0]['state'], 'unavailable')
        self.assertIn('backlog_source_unavailable', limits)

    def test_reconstructed_packet_retains_actual_source_candidate(self):
        value = packet()
        relabelled = packet(head=NEW)
        # Fingerprints intentionally ignore revision; provenance still must match.
        self.assertEqual(fingerprint(value), fingerprint(relabelled))
        packets, retained, limits = backlog.reconstruct(
            [recipe(value)], lambda *_args: [relabelled], fingerprint)
        self.assertEqual(packets, [])
        self.assertEqual(retained[0]['source']['head_sha'], HEAD)
        self.assertIn('backlog_source_unavailable', limits)

    def test_branch_scope_prevents_cross_pr_collection(self):
        own, other = packet(), packet(1, branch='other-pr')
        calls = []

        def collector(origin, *_args):
            calls.append(origin['branch'])
            return [own, other]

        packets, retained, limits = backlog.reconstruct(
            [recipe(own), recipe(other)], collector, fingerprint,
            scope=backlog.scope_for(own['source']))
        self.assertEqual(calls, ['feature'])
        self.assertEqual(packets, [own])
        self.assertEqual(len(retained), 2)
        self.assertEqual(limits, [])

    def test_reserved_work_remains_visible_without_repeat_fetch(self):
        value = packet()
        queued = backlog.merge([], [recipe(value)], reserved=[fingerprint(value)])
        calls = []
        packets, retained, limits = backlog.reconstruct(
            queued['items'], lambda *_args: calls.append('unexpected'), fingerprint)
        self.assertEqual(calls, [])
        self.assertEqual(packets, [])
        self.assertEqual(retained[0]['state'], 'reserved')
        self.assertEqual(limits, [])
        self.assertEqual(len(backlog.merge(retained, [])['items']), 1)

    def test_same_semantic_fingerprint_preserves_separate_branch_recipes(self):
        own, other = packet(), packet(branch='other-pr')
        self.assertEqual(fingerprint(own), fingerprint(other))
        queue = backlog.merge([recipe(own)], [recipe(other)])
        self.assertEqual(len(queue['items']), 2)
        self.assertEqual({row['source']['branch'] for row in queue['items']},
                         {'feature', 'other-pr'})

    def test_diagnosed_open_or_abstained_relationship_stays_tracked_after_diff_changes(self):
        for status in ('diagnosed-open', 'abstained'):
            with self.subTest(status=status):
                original = packet()
                row = recipe(original)
                row['state'] = 'tracked'
                current = packet(4, head=NEW)
                queue = backlog.merge([row], [recipe(current)])
                by_head = {HEAD: original, NEW: current}
                packets, retained, limits = backlog.reconstruct(
                    queue['items'], lambda origin, *_args: [by_head[origin['head_sha']]],
                    fingerprint)
                self.assertEqual(limits, [])
                self.assertEqual(retained[0]['state'], 'tracked')
                self.assertEqual(packets[0], original)
                self.assertEqual(backlog.merge(retained, [])['items'][0]['state'], 'tracked')

    def test_allowlist_rejects_forged_private_fields_and_repository_urls(self):
        good = recipe(packet())
        for change in [dict(good, path='private-marker'),
                       dict(good, kind='execute'),
                       dict(good, scope='e' * 64),
                       dict(good, source=dict(good['source'], repository='https://evil.invalid/repo')),
                       dict(good, source=dict(good['source'], extra='private-marker')),
                       dict(good, state='resolved')]:
            with self.subTest(change=tuple(change)):
                self.assertIsNone(backlog.safe_descriptor(change))
                merged = backlog.merge([], [change])
                self.assertEqual(merged['items'], [])
                self.assertIn('backlog_recipe_invalid', merged['limitations'])
                self.assertNotIn('private-marker', json.dumps(merged))

    def test_reconstruction_failure_is_retained_with_static_limitation(self):
        def unavailable(*_args):
            raise OSError('private-path-and-provider-detail')
        packets, retained, limits = backlog.reconstruct([recipe(packet())], unavailable, fingerprint)
        self.assertEqual(packets, [])
        self.assertEqual(retained[0]['state'], 'unavailable')
        self.assertEqual(limits, ['backlog_source_unavailable'])
        self.assertNotIn('private-path', json.dumps(limits))

    def test_bounds_preserve_overflow_as_coverage_loss(self):
        rows = [recipe(packet(i)) for i in range(4)]
        bounded = backlog.merge([], rows, maximum=2)
        self.assertEqual(len(bounded['items']), 2)
        self.assertEqual(bounded['overflow'], 2)
        self.assertIn('backlog_inventory_truncated', bounded['limitations'])
        later = backlog.merge(bounded['items'], [], previous_overflow=bounded['overflow'])
        self.assertEqual(later['overflow'], 2)
        self.assertIn('backlog_inventory_truncated', later['limitations'])

    def test_reconstruction_budget_keeps_unvisited_recipes(self):
        originals = [packet(i, head=char * 40) for i, char in enumerate('ade')]
        rows = [recipe(value) for value in originals]
        by_head = {value['source']['head_sha']: value for value in originals}
        calls = []

        def collector(origin, *_args):
            calls.append(origin['head_sha'])
            return [by_head[origin['head_sha']]]

        packets, retained, limits = backlog.reconstruct(rows, collector, fingerprint, maximum=1)
        self.assertEqual(len(calls), 1)
        self.assertEqual(len(packets), 1)
        self.assertEqual(len(retained), 3)
        self.assertEqual(limits, ['backlog_reconstruction_deferred'])
        self.assertTrue(all(row['state'] == 'deferred' for row in retained))

    def test_data_recipe_requires_both_pin_and_matching_config_identity(self):
        for changes in [{'kind': 'data'}, {'kind': 'data', 'data_ref': HEAD},
                        {'data_ref': HEAD}, {'kind': 'data', 'data_ref': '../HEAD', 'data_key': 'f' * 64}]:
            with self.subTest(changes=changes):
                with self.assertRaisesRegex(ValueError, '^Invalid backlog recipe$'):
                    recipe(packet(), **changes)


if __name__ == '__main__':
    unittest.main()
