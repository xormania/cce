"""Collect versioned evidence as data; publish bounded, public-safe CI results."""
from __future__ import annotations

import argparse
import base64
import hashlib
import io
import json
import os
from pathlib import Path
import re
import sys
import time
import tomllib
import urllib.error
import urllib.parse
import urllib.request
import zipfile

sys.path.insert(0, str(Path(__file__).resolve().parent))
from drift_contracts import evaluate
from drift_check import begin as begin_check, complete as complete_check
from jev_client import MODEL, CRITERIA, diagnose

VERSION = 1
CHECK = 'CCE drift contracts'
MAX_BYTES = 1_000_000
MAX_FILES = 40
MAX_CALLS = 4
SHA = re.compile(r'^[0-9a-f]{40}$')
REPO = re.compile(r'^[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+$')
STATUSES = {'green', 'qualified', 'complete', 'partial', 'deferred', 'planned', 'red'}
SOURCE_PATHS = {'.github/workflows/rust.yml', '.github/workflows/drift-input.yml',
                '.github/workflows/leak-guard.yml'}


def canonical(value):
    return json.dumps(value, ensure_ascii=False, sort_keys=True, separators=(',', ':'), allow_nan=False).encode()


def digest(value):
    return hashlib.sha256(canonical(value)).hexdigest()


def load(path):
    path = Path(path)
    if path.is_symlink() or path.stat().st_size > MAX_BYTES:
        raise ValueError('invalid_input')
    return json.loads(path.read_text())


def save(path, value):
    path = Path(path)
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_bytes(canonical(value) + b'\n')


class NoRedirect(urllib.request.HTTPRedirectHandler):
    def redirect_request(self, *args, **kwargs):
        return None


class GitHub:
    def __init__(self, repository, token='', opener=None):
        if not REPO.fullmatch(repository):
            raise ValueError('invalid_repository')
        self.repository, self.token = repository, token
        self.opener = opener or urllib.request.build_opener(NoRedirect())

    def api(self, suffix, method='GET', value=None):
        if not suffix.startswith('/') or '..' in suffix or '\\' in suffix:
            raise ValueError('invalid_api_path')
        headers = {'Accept': 'application/vnd.github+json', 'X-GitHub-Api-Version': '2022-11-28',
                   'User-Agent': 'cce-ci'}
        if self.token:
            headers['Authorization'] = 'Bearer ' + self.token
        request = urllib.request.Request('https://api.github.com/repos/' + self.repository + suffix,
            data=canonical(value) if value is not None else None, headers=headers, method=method)
        with self.opener.open(request, timeout=15) as response:
            raw = response.read(MAX_BYTES + 1)
        if len(raw) > MAX_BYTES:
            raise ValueError('response_bound')
        return json.loads(raw)

    def file(self, path, ref):
        if not SHA.fullmatch(ref) or not safe_path(path):
            raise ValueError('invalid_file_reference')
        row = self.api('/contents/' + urllib.parse.quote(path, safe='/') + '?ref=' + ref)
        if row.get('type') != 'file' or row.get('encoding') != 'base64' or row.get('size', MAX_BYTES+1) > MAX_BYTES:
            raise ValueError('invalid_file')
        raw = base64.b64decode(row['content'])
        if len(raw) > MAX_BYTES:
            raise ValueError('file_bound')
        return raw.decode('utf-8')

    def log(self, job_id):
        url = 'https://api.github.com/repos/' + self.repository + '/actions/jobs/' + str(int(job_id)) + '/logs'
        request = urllib.request.Request(url, headers={'Authorization':'Bearer '+self.token, 'User-Agent':'cce-ci'})
        try:
            response = self.opener.open(request, timeout=15)
        except urllib.error.HTTPError as error:
            if error.code != 302:
                raise
            target = urllib.parse.urlparse(error.headers.get('Location', ''))
            if target.scheme != 'https' or target.username or target.password or not (
                (target.hostname or '').endswith('.blob.core.windows.net') or
                (target.hostname or '').endswith('.actions.githubusercontent.com')):
                raise ValueError('invalid_log_redirect') from None
            response = self.opener.open(urllib.request.Request(target.geturl()), timeout=15)
        with response:
            raw = response.read(MAX_BYTES + 1)
        if len(raw) > MAX_BYTES:
            raise ValueError('log_bound')
        return raw.decode('utf-8', errors='replace')

    def baseline(self, trusted_sha):
        # Only our trusted default-branch consumer can supply reusable public state.
        runs = self.api('/actions/workflows/jev.yml/runs?per_page=10')['workflow_runs']
        reserved, entries, pending = set(), {}, []
        found = False
        for run in runs:
            if run['id'] == int(os.environ.get('GITHUB_RUN_ID', '0')) or run.get('status') != 'completed':
                continue
            if run.get('event') not in {'workflow_run','schedule','workflow_dispatch'}:
                continue
            if run.get('event') == 'workflow_dispatch':
                default = self.api('/')['default_branch']
                if run.get('head_branch') != default:
                    continue
            artifacts = self.api('/actions/runs/' + str(run['id']) + '/artifacts?per_page=20')['artifacts']
            for artifact in artifacts:
                if artifact['name'] not in {'drift-state', 'drift-reservation'} or artifact.get('expired'):
                    continue
                state = self.archive(artifact['id'], 'reservation.json' if artifact['name'] == 'drift-reservation' else 'state.json')
                if state.get('version') == VERSION and state.get('engine_hash') == engine_hash():
                    found = True
                    if artifact['name'] == 'drift-reservation':
                        reserved.update(x for x in state.get('fingerprints', []) if isinstance(x,str) and re.fullmatch('[0-9a-f]{64}',x))
                    else:
                        for row in state.get('entries', []):
                            if safe_state_entry(row):
                                entries.setdefault(row['fingerprint'], row)
                        pending.extend(x for x in state.get('pending', []) if isinstance(x,str) and re.fullmatch('[0-9a-f]{64}',x) and x not in pending)
        return {'entries':list(entries.values())[:200], 'pending':pending[:200],
                'reserved':list(reserved)[:200], 'baseline_missing':not found}

    def archive(self, artifact_id, filename='state.json'):
        url = 'https://api.github.com/repos/' + self.repository + '/actions/artifacts/' + str(int(artifact_id)) + '/zip'
        request = urllib.request.Request(url, headers={'Authorization':'Bearer '+self.token, 'User-Agent':'cce-ci'})
        try:
            response = self.opener.open(request, timeout=15)
        except urllib.error.HTTPError as error:
            if error.code != 302:
                raise
            target = urllib.parse.urlparse(error.headers.get('Location', ''))
            if target.scheme != 'https' or target.username or target.password or not (
                (target.hostname or '').endswith('.blob.core.windows.net') or
                (target.hostname or '').endswith('.actions.githubusercontent.com')):
                raise ValueError('invalid_artifact_redirect') from None
            # The signed storage request never receives GitHub credentials.
            response = self.opener.open(urllib.request.Request(target.geturl()), timeout=15)
        with response:
            raw = response.read(MAX_BYTES + 1)
        if len(raw) > MAX_BYTES:
            raise ValueError('artifact_bound')
        return read_state_archive(raw, filename)


def read_state_archive(raw, filename='state.json'):
    with zipfile.ZipFile(io.BytesIO(raw)) as archive:
        rows = archive.infolist()
        if len(rows) != 1 or rows[0].filename != filename or rows[0].file_size > MAX_BYTES or (
            rows[0].external_attr >> 16) & 0o170000 == 0o120000:
            raise ValueError('invalid_artifact')
        return json.loads(archive.read(rows[0]))


def safe_path(path):
    return isinstance(path, str) and re.fullmatch(r'[A-Za-z0-9_. /-]{1,299}', path) and not path.startswith('/') and '\\' not in path and all(
        p not in {'', '.', '..'} for p in path.split('/'))


def engine_hash():
    root = Path(__file__).resolve().parent
    return digest({p.name:hashlib.sha256(p.read_bytes()).hexdigest() for p in sorted(root.glob('*.py'))})


def safe_state_entry(row):
    if not isinstance(row, dict) or set(row) - {'fingerprint','record'} or not re.fullmatch('[0-9a-f]{64}', row.get('fingerprint', '')):
        return False
    record = row.get('record')
    if not isinstance(record, dict) or record.get('status') not in {'diagnosed','abstained'} or not re.fullmatch(
        '[0-9a-f]{8,64}', record.get('subject', '')):
        return False
    allowed = {'subject','source','status','requested_model','observed_model','question_version',
        'provider_request_attempted','provider_request_attempts','request_sha256','response_sha256',
        'latency_seconds','usage','full_response_retained','advisory_only','diagnosis','reused'}
    if set(record) - allowed:
        return False
    diagnosis = record.get('diagnosis', {})
    if not isinstance(diagnosis, dict) or set(diagnosis) - {'answers','supporting_excerpt_ids','abstained'} or not isinstance(diagnosis.get('answers', {}), dict):
        return False
    for kind, answer in diagnosis.get('answers', {}).items():
        if kind not in CRITERIA or not isinstance(answer, dict) or answer.get('choice') not in CRITERIA[kind]:
            return False
    # Rebuild a projection, rather than trusting provider echoes or unknown stored metadata.
    row['record'] = {'subject':record['subject'], 'status':record['status'], 'reused':True,
        'provider_request_attempted':False, 'provider_request_attempts':0,
        'diagnosis':{'answers':{kind:{'choice':answer['choice']} for kind,answer in diagnosis.get('answers',{}).items()}}}
    return True


def source_for(api, event, event_name, manual_sha=''):
    if event_name == 'workflow_run':
        announced = event['workflow_run']
        run = api.api('/actions/runs/' + str(int(announced['id'])))
        if run.get('head_repository', {}).get('full_name') != api.repository or (
            run.get('path') not in SOURCE_PATHS or run.get('head_sha') != announced.get('head_sha') or
            run.get('run_attempt') != announced.get('run_attempt') or run.get('status') != 'completed'):
            raise ValueError('untrusted_source')
        if run.get('conclusion') == 'skipped':
            raise ValueError('intentional_duplicate')
        head = run['head_sha']
    else:
        if event_name == 'schedule':
            pulls = api.api('/pulls?state=open&per_page=20')
            heads = [p['head']['sha'] for p in pulls if p['head']['repo']['full_name'] == api.repository]
            if heads:
                manual_sha = heads[int(time.time() // 1800) % len(heads)]
        head = manual_sha or api.api('/commits/' + urllib.parse.quote(event['repository']['default_branch'], safe=''))['sha']
        run = {'id':0, 'run_attempt':0, 'head_branch':event['repository']['default_branch']}
    if not SHA.fullmatch(head):
        raise ValueError('invalid_candidate')
    commit = api.api('/commits/' + head)
    prs = api.api('/commits/' + head + '/pulls?per_page=30')
    active = [p for p in prs if p.get('state') == 'open' and p.get('head', {}).get('sha') == head]
    base = active[0]['base']['sha'] if active else commit['parents'][0]['sha'] if commit.get('parents') else head
    if not SHA.fullmatch(base):
        raise ValueError('invalid_base')
    return {'repository':api.repository, 'head_sha':head, 'base_sha':base,
            'run_id':run['id'], 'attempt':run['run_attempt'], 'branch':active[0]['head']['ref'] if active else run['head_branch'],
            'pulls':[p['number'] for p in active]}


def bounded_excerpt(text, limit=6000):
    # Single gigantic assertion lines (e.g. byte arrays) cannot crowd out decisive lines.
    lines = [line[:500] + (' [line omitted]' if len(line) > 500 else '') for line in text.splitlines()]
    value = '\n'.join(lines)
    limit = min(limit, 1500)
    raw = value.encode('utf-8')
    return raw[:limit].decode('utf-8', errors='ignore') + ('\n[excerpt truncated]' if len(raw) > limit else '')


def public_changes(api, source):
    data = api.api('/compare/' + source['base_sha'] + '...' + source['head_sha'])
    files = data.get('files', [])
    limitations = ['file_inventory_truncated'] if len(files) >= 300 or len(files) > MAX_FILES else []
    rows = []
    # Include code/test bodies as well as a diff, so a summary does not become the oracle.
    for row in files[:MAX_FILES]:
        path = row['filename']
        if not safe_path(path) or not path.endswith(('.rs', '.py', '.md', '.yml', '.yaml', '.toml', '.json')):
            continue
        excerpts = ['Public file: ' + path, 'Changed lines:\n' + bounded_excerpt(row.get('patch', '[diff unavailable]'), 4000)]
        if row.get('status') != 'removed':
            try:
                excerpts.append('Current file:\n' + bounded_excerpt(api.file(path, source['head_sha']), 6000))
            except (ValueError, KeyError, UnicodeError, urllib.error.URLError):
                limitations.append('source_excerpt_unavailable')
        rows.append({'id':path, 'excerpts':excerpts, 'questions':['proof', 'guarantee']})
    return rows, limitations, bool(files) and all(r['filename'].endswith('.md') for r in files)


def execution_checks(api, source, markdown_only, observations=None):
    checks, limitations, waiting = [], [], False
    runs = api.api('/actions/runs?head_sha=' + source['head_sha'] + '&per_page=50')['workflow_runs']
    expected = {'drift-input': ['Unit and replay tests']}
    if not markdown_only:
        expected['rust'] = ['Format', 'Clippy', 'Test']
    for name, steps in expected.items():
        expected_path = '.github/workflows/' + name + '.yml'
        candidates = [r for r in runs if r['name'] == name and r.get('path') == expected_path and
            r.get('head_sha') == source['head_sha'] and r.get('head_repository',{}).get('full_name') == api.repository and
            r.get('event') in {'push', 'pull_request'} and r.get('conclusion') != 'skipped']
        if not candidates:
            # Older/manual candidates may predate this producer. It is incomplete, never a pass.
            limitations.append('required_execution_unavailable')
            waiting = True
            continue
        run = max(candidates, key=lambda r:(r['run_number'], r.get('run_attempt', 1)))
        if run['status'] != 'completed':
            waiting = True
            continue
        jobs = api.api('/actions/runs/' + str(run['id']) + '/jobs?per_page=50')['jobs']
        logs = ''
        if observations is not None:
            try:
                logs = '\n'.join(api.log(job['id']) for job in jobs if job.get('conclusion') != 'skipped')
                if not logs.strip():
                    limitations.append('execution_log_unavailable')
                    waiting = True
                decisive = '\n'.join(line for line in logs.splitlines() if any(word in line for word in (
                    'test result:', 'FAILED', 'panicked at', 'SIGABRT', 'memory allocation of',
                    'Ran ', 'OK', 'FAILED (', 'skipping', 'nothing was scanned')))
                if decisive:
                    observations.append({'id':'execution-'+name, 'excerpts':[bounded_excerpt(decisive)],
                                         'questions':['witness', 'proof']})
            except (ValueError, KeyError, OSError, urllib.error.URLError):
                limitations.append('execution_log_unavailable')
                waiting = True
        for name in steps:
            actual = [s for j in jobs for s in j.get('steps', []) if s['name'] == name]
            status = actual[0].get('conclusion') if len(actual) == 1 else None
            ran = status in {'success','failure'}
            if observations is not None and logs and status == 'success':
                if name == 'Test':
                    ran = sum(int(n) for n in re.findall(r'test result: ok\. (\d+) passed', logs)) > 0
                elif name == 'Unit and replay tests':
                    ran = any(int(n) > 0 for n in re.findall(r'Ran (\d+) tests?', logs))
            checks.append({'id':name, 'required':True, 'executed':status in {'success','failure'},
                'status':'pass' if status == 'success' else 'fail' if status == 'failure' else 'skipped',
                'source_sha':source['head_sha']})
            checks[-1]['executed'] = ran
    return checks, limitations, waiting


def data_evidence(source, env, api_factory=GitHub, normalized_mutations=None):
    """Optional authenticated data input; never imports or executes data-repository code."""
    repository, token, paths = (env.get('DRIFT_DATA_REPOSITORY', ''), env.get('DRIFT_DATA_TOKEN', ''),
                                env.get('DRIFT_DATA_FILES', ''))
    required = env.get('DRIFT_DATA_REQUIRED', 'false').lower() == 'true'
    if not repository and not paths:
        return [], [], ['data_not_configured'], required
    if not token:
        return [], [], ['data_credentials_missing'], required
    try:
        files = json.loads(paths)
        if not isinstance(files, dict) or not {'requirements', 'summary'} <= set(files) or set(files) - {'requirements', 'summary', 'records'}:
            raise ValueError('invalid_data_config')
        api = api_factory(repository, token)
        ref = api.api('/commits/HEAD')['sha']
        mapping = tomllib.loads(api.file(files['requirements'], ref))
        summary = api.file(files['summary'], ref)
        verified = re.search(r'^verified-at:\s*([a-f0-9]{7,40})\s*$', summary, re.M)
        applicable = bool(verified and source['head_sha'].startswith(verified[1]))
        cases = mapping.get('cases', [])
        if not isinstance(cases, list) or len(cases) > 1000:
            raise ValueError('invalid_data')
        grouped = {}
        for case in cases:
            if not isinstance(case, dict) or case.get('status') not in STATUSES:
                raise ValueError('invalid_case')
            grouped.setdefault(case.get('slice', ''), []).append(case['status'])
        claims, semantic = [], []
        for line in summary.splitlines():
            if not line.startswith('|'):
                continue
            columns = line.split('|')[1:-1]
            if len(columns) < 2:
                continue
            subject = columns[0].strip(' *`')
            if subject not in grouped:
                continue
            statement = columns[1].strip()
            # Only explicit all-green claims are deterministic; natural-language inference is Jev's job.
            complete = bool(re.search(r'(?<!\d)(\d+)\s*/\s*\1\s+green\b', statement))
            claims.append({'id':subject, 'applicable':applicable, 'status':'complete' if complete else 'partial',
                           'evidence_sha':source['head_sha'] if applicable else ref,
                           'case_statuses':grouped[subject], 'excerpts':[]})
            semantic.append({'id':subject, 'excerpts':[bounded_excerpt(line), bounded_excerpt(json.dumps([c for c in cases if c.get('slice') == subject]))],
                             'questions':['proof', 'deferral', 'resolution']})
        records = files.get('records', [])
        if not isinstance(records, list) or len(records) > 8:
            raise ValueError('invalid_record_inventory')
        for path in records:
            semantic.append({'id':path, 'excerpts':[bounded_excerpt(api.file(path, ref))],
                             'questions':['witness','proof','resolution']})
        current = [m for m in mapping.get('mutations', []) if isinstance(m, dict) and
                   isinstance(m.get('source_head'), str) and len(m['source_head']) >= 7 and
                   source['head_sha'].startswith(m['source_head'])]
        if len(current) > 12:
            raise ValueError('mutation_inventory_bound')
        for mutation in current:
            path = mutation.get('evidence', '') + '/result.json'
            result = json.loads(api.file(path, ref))
            if result.get('source_dirty') or result.get('source_head') != source['head_sha']:
                continue  # A dirty or different candidate is not a demonstrated identity match.
            disposition = mutation.get('disposition')
            if normalized_mutations is not None:
                normalized_mutations.append({'id':mutation['id'], 'applicable':True,
                    'disposition':disposition, 'claimed_semantic':disposition == 'caught',
                    'runs':result.get('runs', []), 'excerpts':[]})
            semantic.append({'id':mutation['id'], 'excerpts':[bounded_excerpt(json.dumps(mutation)),
                bounded_excerpt(json.dumps(result))], 'questions':['witness','proof','resolution']})
        return claims, semantic, ([] if applicable else ['data_candidate_not_proven']), required and not applicable
    except (ValueError, KeyError, TypeError, UnicodeError, urllib.error.URLError):
        return [], [], ['data_source_unavailable'], required


def fingerprint(packet):
    # A code revision is provenance, not semantic change. Excerpts/config define reuse.
    return digest({'subject':packet['subject'], 'excerpts':packet['excerpts'],
                   'questions':packet['questions'], 'model':MODEL, 'version':VERSION})


def choose(packets, previous, force=False):
    entries = {r['fingerprint']:r for r in previous.get('entries', []) if isinstance(r, dict) and
               re.fullmatch('[0-9a-f]{64}', r.get('fingerprint', ''))}
    pending = previous.get('pending', [])
    rows = sorted(packets, key=lambda p:(fingerprint(p) not in pending, pending.index(fingerprint(p))
                  if fingerprint(p) in pending else 0, fingerprint(p)))
    selected, reused, deferred = [], [], []
    seen = set()
    for packet in rows:
        key = fingerprint(packet)
        if key in seen:
            continue
        seen.add(key)
        if not force and key in entries:
            reused.append(entries[key])
        elif not force and key in previous.get('reserved', []):
            reused.append({'fingerprint':key, 'record':{'subject':packet['subject'],
                'status':'previous_request_reserved', 'provider_request_attempts':0}})
        elif len(selected) < MAX_CALLS:
            selected.append({'fingerprint':key, 'packet':packet})
        else:
            deferred.append(key)
    return selected, reused, deferred


def collect(api, event, event_name, out, env, source=None, check_id=None):
    source = source or source_for(api, event, event_name, env.get('DRIFT_HEAD_SHA', ''))
    semantic, limitations, markdown_only = public_changes(api, source)
    log_semantic = []
    checks, more, waiting = execution_checks(api, source, markdown_only, log_semantic)
    mutations = []
    claims, private_semantic, data_limits, data_failed = data_evidence(source, env, normalized_mutations=mutations)
    bundle = {'source':source, 'checks':checks, 'claims':claims, 'mutations':mutations,
              'semantic':semantic + log_semantic + private_semantic}
    result = evaluate(bundle)
    for packet in result['packets']:
        packet['source'] = source
    limitations += more + data_limits + result['limitations']
    trusted_sha = env.get('JEV_TRUSTED_SHA', '')
    try:
        previous = api.baseline(trusted_sha)
    except (ValueError, KeyError, zipfile.BadZipFile, urllib.error.URLError):
        previous = {'entries':[], 'pending':[], 'baseline_missing':True}
    selected, reused, deferred = choose(result['packets'], previous, env.get('DRIFT_FORCE') == 'true')
    locations = {hashlib.sha256(('semantic\0'+row['id']).encode()).hexdigest()[:24]:row['id'] for row in semantic}
    control = {'version':VERSION, 'source':source, 'trusted_sha':trusted_sha, 'engine_hash':engine_hash(),
        'violations':result['violations'], 'limitations':sorted(set(limitations)),
        'data_failed':data_failed, 'waiting':waiting, 'selected':len(selected), 'reused':reused,
        'pending':deferred, 'baseline_missing':previous.get('baseline_missing', False), 'locations':locations,
        'previous_entries':previous.get('entries', []), 'check_id':check_id, 'provider_status':'not_executed'}
    # Private packets never enter the upload directory.
    save(Path(out)/'private'/'packets.json', selected)
    save(Path(out)/'control.json', control)
    if env.get('DRIFT_PROVIDER_CONFIGURED') == 'true' and selected:
        save(Path(out)/'public'/'reservation.json', {'version':VERSION, 'engine_hash':engine_hash(),
             'fingerprints':[row['fingerprint'] for row in selected]})
    return control


def run_provider(out, key):
    root = Path(out)
    control = load(root/'control.json')
    rows = load(root/'private'/'packets.json')
    results = list(control['reused'])
    for row in rows:
        record = diagnose(row['packet'], key=key)
        results.append({'fingerprint':row['fingerprint'], 'record':record})
    control['results'] = results
    control['provider_status'] = 'completed'
    save(root/'control.json', control)
    return control


def summary(control):
    lines = ['### CCE drift analysis', '', 'Candidate: `' + control['source']['head_sha'] + '`.',
             'Deterministic violations: ' + str(len(control['violations'])) + '.',
             'Semantic packets selected: ' + str(control['selected']) +
             '; deferred: ' + str(len(control['pending'])) + '.']
    lines.append('Provider stage: `' + control.get('provider_status','not_executed') + '`.')
    if control.get('baseline_missing'):
        lines.append('No reusable baseline was available in the bounded history window.')
    for violation in control['violations']:
        lines.append('- `' + violation['code'] + '`; reference `' + violation['subject'] + '`.')
    for row in control.get('results', []):
        record = row['record']
        lines.append('- Jev: `' + record['status'] + '`' + (' (reused; no request)' if record.get('reused') else '') + '; reference `' + record['subject'] + '`.')
        location = control.get('locations', {}).get(record['subject'])
        if location:
            url = 'https://github.com/' + control['source']['repository'] + '/blob/' + control['source']['head_sha'] + '/' + urllib.parse.quote(location, safe='/')
            lines.append('  Public source: [' + location.replace('[','').replace(']','') + '](' + url + ').')
        answers = record.get('diagnosis', {}).get('answers', {})
        if answers:
            lines.append('  Classifications: ' + ', '.join('`'+name+': '+answer['choice']+'`' for name,answer in answers.items()) + '.')
    if control['limitations']:
        lines += ['', 'Coverage limitations: ' + ', '.join('`'+x+'`' for x in control['limitations']) + '.']
    lines += ['', 'Private packets and full provider responses are not retained. Public fingerprints identify this analysis.',
              'Semantic judgments are advisory; deterministic contract checks have their own verdict.']
    return '\n'.join(lines)+'\n'


def current_candidate(api, source):
    if source['pulls']:
        return any(api.api('/pulls/'+str(int(n)))['head']['sha'] == source['head_sha'] for n in source['pulls'])
    branch = urllib.parse.quote(source['branch'], safe='')
    return api.api('/branches/'+branch)['commit']['sha'] == source['head_sha']


def publish(api, out):
    root = Path(out)
    control = load(root/'control.json')
    public = root/'public'
    if not current_candidate(api, control['source']):
        if control.get('check_id'):
            complete_check(api, control['check_id'], control['source'], 'success', 'Superseded candidate.')
        save(public/'state.json', {'version':VERSION, 'trusted_sha':control['trusted_sha'],
            'entries':[], 'pending':control['pending'], 'superseded':True})
        return 0
    failed = bool(control['violations'] or control['data_failed'])
    status = 'completed' if failed or not control['waiting'] else 'in_progress'
    check_id = control.get('check_id') or begin_check(api, control['source'])
    if status == 'completed':
        complete_check(api, check_id, control['source'], 'failure' if failed else 'success', summary(control))
    rows = control.get('results', [])
    entries = {r['fingerprint']:r for r in control.get('previous_entries', [])}
    entries.update({r['fingerprint']:r for r in rows if r['record']['status'] in {'diagnosed','abstained'}})
    save(public/'state.json', {'version':VERSION, 'trusted_sha':control['trusted_sha'],
          'engine_hash':control['engine_hash'], 'entries':list(entries.values())[-200:], 'pending':control['pending']})
    (public/'summary.md').write_text(summary(control))
    if os.environ.get('GITHUB_STEP_SUMMARY'):
        with Path(os.environ['GITHUB_STEP_SUMMARY']).open('a') as f:
            f.write(summary(control))
    return int(failed)


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument('phase', choices=['collect','diagnose','publish'])
    parser.add_argument('--out', required=True)
    args = parser.parse_args()
    root = Path(args.out)
    api = source = check_id = None
    try:
        if args.phase == 'diagnose':
            run_provider(root, os.environ.get('TYPESAFE_API_KEY',''))
            return 0
        api = GitHub(os.environ['GITHUB_REPOSITORY'], os.environ.get('GH_TOKEN',''))
        if args.phase == 'publish':
            control = load(root/'control.json')
            source, check_id = control['source'], control.get('check_id')
            return publish(api, root)
        event = load(os.environ['GITHUB_EVENT_PATH'])
        source = source_for(api, event, os.environ['GITHUB_EVENT_NAME'], os.environ.get('DRIFT_HEAD_SHA', ''))
        check_id = begin_check(api, source)
        collect(api, event, os.environ['GITHUB_EVENT_NAME'], root, os.environ, source, check_id)
        return 0
    except (ValueError, KeyError, TypeError, OSError, zipfile.BadZipFile, urllib.error.URLError):
        # Exception strings can contain private API paths, payloads or credentials.
        root.mkdir(parents=True, exist_ok=True)
        (root/'failure.txt').write_text('Analysis incomplete: collection, configuration or execution failed.\n')
        if api and source and check_id:
            try:
                complete_check(api, check_id, source, 'failure', 'Required drift validation could not complete. No private detail is published.')
            except (ValueError, KeyError, TypeError, OSError, urllib.error.URLError):
                pass  # The candidate remains in progress; never restore a previous success.
        print('::error::Analysis incomplete; no successful contract verdict was published.')
        return 1


if __name__ == '__main__':
    raise SystemExit(main())
