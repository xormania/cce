"""Lifecycle of the trusted contract check on the evaluated commit."""
from __future__ import annotations

import os
import re
from urllib.parse import quote

CHECK = 'CCE drift contracts'
PREFIX = 'cce-drift:'
SHA = re.compile(r'[0-9a-f]{40}\Z')


def _head(source):
    value = source.get('head_sha') if isinstance(source, dict) else None
    if not isinstance(value, str) or not SHA.fullmatch(value):
        raise ValueError('invalid_check_source')
    return value


def _owner():
    run, attempt = os.environ.get('GITHUB_RUN_ID', '0'), os.environ.get('GITHUB_RUN_ATTEMPT', '1')
    if not run.isdigit() or not attempt.isdigit():
        raise ValueError('invalid_check_owner')
    return PREFIX + run + ':' + attempt


def _ours(row, head):
    return (isinstance(row, dict) and row.get('name') == CHECK and row.get('head_sha') == head
            and row.get('app', {}).get('slug') == 'github-actions'
            and isinstance(row.get('external_id'), str) and row['external_id'].startswith(PREFIX)
            and type(row.get('id')) is int)


def begin(api, source):
    """Create or restart the single owned check before collecting required evidence.

    The workflow serializes consumers of the same source SHA. Each owner additionally
    claims the check through external_id, preventing a delayed earlier completion.
    """
    head = _head(source)
    result = api.api('/commits/' + head + '/check-runs?check_name=' + quote(CHECK, safe='') + '&per_page=100')
    candidates = [row for row in result.get('check_runs', []) if _ours(row, head)]
    body = {'name':CHECK, 'head_sha':head, 'external_id':_owner(), 'status':'in_progress',
            'output':{'title':CHECK, 'summary':'Required evidence is being evaluated for this commit.'}}
    if candidates:
        row = max(candidates, key=lambda item:item['id'])
        # head_sha is fixed when the check is created, not a mutable update field.
        body.pop('head_sha')
        result = api.api('/check-runs/' + str(row['id']), 'PATCH', body)
    else:
        result = api.api('/check-runs', 'POST', body)
    if not _ours(result, head) or result.get('external_id') != _owner():
        raise ValueError('invalid_check_identity')
    return result['id']


def current_candidate(api, source):
    head = _head(source)
    pulls = source.get('pulls', [])
    if not isinstance(pulls, list) or any(type(number) is not int or number <= 0 for number in pulls):
        raise ValueError('invalid_check_source')
    if pulls:
        base = source.get('base_sha')
        if not isinstance(base, str) or not SHA.fullmatch(base):
            raise ValueError('invalid_check_source')
        return any(row.get('state') == 'open' and row.get('head', {}).get('sha') == head
                   and row.get('base', {}).get('sha') == base
                   for row in (api.api('/pulls/' + str(number)) for number in pulls))
    branch = source.get('branch')
    if not isinstance(branch, str) or not branch or len(branch) > 255:
        raise ValueError('invalid_check_source')
    return api.api('/branches/' + quote(branch, safe=''))['commit']['sha'] == head


def complete(api, check_id, source, conclusion, summary):
    """Finish only this consumer's check; obsolete candidates are cancelled."""
    head = _head(source)
    if type(check_id) is not int or check_id <= 0 or conclusion not in {'success', 'failure'}:
        raise ValueError('invalid_check_completion')
    if not isinstance(summary, str) or len(summary.encode()) > 60_000:
        raise ValueError('invalid_check_summary')
    row = api.api('/check-runs/' + str(check_id))
    if not _ours(row, head):
        raise ValueError('invalid_check_identity')
    if row.get('external_id') != _owner():
        raise ValueError('superseded_check_owner')
    current = current_candidate(api, source)
    body = {'status':'completed', 'conclusion':conclusion if current else 'cancelled',
            'output':{'title':CHECK, 'summary':summary if current else 'This candidate was superseded.'}}
    result = api.api('/check-runs/' + str(check_id), 'PATCH', body)
    if not _ours(result, head) or result.get('external_id') != _owner():
        raise ValueError('invalid_check_identity')
    return current
