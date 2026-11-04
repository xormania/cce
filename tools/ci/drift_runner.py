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
from drift_check import begin as begin_check, complete as complete_check, current_candidate
from drift_execution import execution_checks
from drift_relationships import public_changes
from drift_data import data_evidence as collect_data, config_key as data_config_key
import drift_backlog as backlog
from jev_client import MODEL, CRITERIA, diagnose

VERSION = 2
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
        parsed = urllib.parse.urlsplit(suffix)
        decoded = urllib.parse.unquote(parsed.path)
        if (not suffix.startswith('/') or suffix.startswith('//') or parsed.scheme or parsed.netloc or parsed.fragment
                or '\\' in decoded or any(part in {'.','..'} for part in decoded.split('/'))):
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
        reserved, entries, pending, recipes = set(), {}, [], []
        overflow = 0
        found = False
        snapshot_found = False
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
            run_state, run_reservation = None, None
            for artifact in artifacts:
                if artifact['name'] not in {'drift-state', 'drift-reservation'} or artifact.get('expired'):
                    continue
                state = self.archive(artifact['id'], 'reservation.json' if artifact['name'] == 'drift-reservation' else 'state.json')
                if state.get('version') == VERSION and state.get('engine_hash') == engine_hash():
                    found = True
                    if artifact['name'] == 'drift-reservation':
                        reserved.update(x for x in state.get('fingerprints', []) if isinstance(x,str) and re.fullmatch('[0-9a-f]{64}',x))
                        run_reservation = state
                    else:
                        run_state = state
                        for row in state.get('entries', []):
                            if safe_state_entry(row):
                                entries.setdefault(row['fingerprint'], row)
                        pending.extend(x for x in state.get('pending', []) if isinstance(x,str) and re.fullmatch('[0-9a-f]{64}',x) and x not in pending)
            # Each published snapshot contains the entire retained queue. An
            # older snapshot cannot resurrect work retired by a newer one.
            # Newer reservations without publication survive cancellation.
            if not snapshot_found:
                snapshot = run_state if run_state is not None else run_reservation
                if snapshot is not None:
                    recipes.extend(row for row in snapshot.get('backlog', []) if backlog.safe_descriptor(row))
                    count = snapshot.get('backlog_overflow', 0)
                    if type(count) is int and count >= 0:
                        overflow = max(overflow, count)
                snapshot_found = run_state is not None
        carried = backlog.merge(recipes, [], finished=[key for key,row in entries.items() if settled(row['record'])],
                                reserved=reserved-set(entries), previous_overflow=overflow)
        return {'entries':list(entries.values())[:200], 'pending':pending[:200],
                'reserved':list(reserved-set(entries))[:200], 'baseline_missing':not found,
                'backlog':carried['items'], 'backlog_overflow':carried['overflow'],
                'backlog_limitations':carried['limitations']}

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
    provenance = record.get('source', {})
    if isinstance(provenance, dict) and provenance.get('repository') == 'xormania/cce' and all(
            isinstance(provenance.get(key), str) and SHA.fullmatch(provenance[key]) for key in ('head_sha','base_sha')):
        row['record']['source'] = {key:provenance[key] for key in ('repository','head_sha','base_sha')}
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


def data_evidence(source, env, api_factory=GitHub, normalized_mutations=None, **options):
    return collect_data(source, env, api_factory, normalized_mutations, **options)


def fingerprint(packet):
    # A code revision is provenance, not semantic change. Excerpts/config define reuse.
    excerpts = [{**row, 'text':re.sub(r' @ [0-9a-f]{40}(?=\n)', ' @ [source revision]', row['text'])}
                for row in packet['excerpts']]
    return digest({'subject':packet['subject'], 'excerpts':excerpts,
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


def settled(record):
    """Only explicit healthy diagnoses retire a watched relationship."""
    healthy = {'proof':{'supported'}, 'guarantee':{'preserved','strengthened'},
               'witness':{'semantic_behavior'}, 'deferral':{'retained_with_owner'},
               'resolution':{'addressed'}}
    answers = record.get('diagnosis', {}).get('answers', {})
    return record.get('status') == 'diagnosed' and bool(answers) and all(
        kind in healthy and answer.get('choice') in healthy[kind] for kind,answer in answers.items())


def packet_inventory(source, rows):
    packets = evaluate({'source':source, 'semantic':rows})['packets']
    for packet in packets:
        packet['source'] = source
    return packets


def collect(api, event, event_name, out, env, source=None, check_id=None):
    source = source or source_for(api, event, event_name, env.get('DRIFT_HEAD_SHA', ''))
    semantic, limitations, markdown_only = public_changes(api, source)
    log_semantic = []
    checks, more, waiting = execution_checks(api, source, markdown_only, log_semantic)
    mutations, data_meta = [], {}
    claims, private_semantic, data_limits, data_failed = data_evidence(
        source, env, normalized_mutations=mutations, public_api=api, metadata=data_meta)
    all_rows = semantic + log_semantic + private_semantic
    bundle = {'source':source, 'checks':checks, 'claims':claims, 'mutations':mutations, 'semantic':all_rows}
    result = evaluate(bundle)
    limitations += more + data_limits + result['limitations']
    trusted_sha = env.get('JEV_TRUSTED_SHA', '')
    try:
        previous = api.baseline(trusted_sha)
    except (ValueError, KeyError, zipfile.BadZipFile, urllib.error.URLError):
        previous = {'entries':[], 'pending':[], 'baseline_missing':True}
    limitations += previous.get('backlog_limitations', [])
    recipes, locations = [], {}
    origins = {hashlib.sha256(('semantic\0'+row['id']).encode()).hexdigest()[:24]:row for row in all_rows}
    for packet in result['packets']:
        packet['source'] = source
        row = origins.get(packet['subject'], {})
        if row.get('private'):
            identity = row.get('reconstruction', {})
            ref, key = identity.get('data_ref', data_meta.get('data_ref')), identity.get('config_key', data_meta.get('config_key'))
            if not ref or not key:
                limitations.append('backlog_source_unavailable')
                continue
            recipe = backlog.descriptor(packet, fingerprint(packet), 'data', data_ref=ref, data_key=key)
        else:
            kind = 'execution' if row.get('id','').startswith('execution-') else 'public'
            recipe = backlog.descriptor(packet, fingerprint(packet), kind)
            path = row.get('public_path')
            if path and safe_path(path):
                locations[recipe['fingerprint']] = {'path':path, 'head_sha':source['head_sha'],
                                                     'line':row.get('public_line', 1)}
        packet['source'] = recipe['source']
        recipes.append(recipe)
    old_recipes = previous.get('backlog', [])
    if env.get('DRIFT_FORCE') == 'true':
        old_recipes = [{**row, 'state':'deferred'} for row in old_recipes]
    current_keys = {row['fingerprint'] for row in recipes}
    # Already present current packets need no historical fetch. Unavailable old
    # contexts stay visible; disappearance from a diff never resolves them.
    to_rebuild = [row for row in old_recipes if row['fingerprint'] not in current_keys]
    # Pending work has priority. Rotate immutable contexts within each state so
    # one permanently unavailable source cannot starve every later context.
    def fair_group(rows):
        groups = {}
        for row in rows:
            groups.setdefault(digest([row['source'],row['kind'],row.get('data_ref'),row.get('data_key')]), []).append(row)
        keys = list(groups)
        if keys:
            offset = int(time.time() // 1800) % len(keys)
            keys = keys[offset:]+keys[:offset]
        return [row for key in keys for row in groups[key]]
    to_rebuild = fair_group([row for row in to_rebuild if row['state'] not in {'tracked','reserved'}]) + fair_group(
        [row for row in to_rebuild if row['state'] == 'tracked']) + [row for row in to_rebuild if row['state'] == 'reserved']
    def original_packets(original, kind, data_ref, data_key):
        if kind == 'data':
            if data_config_key(env) != data_key:
                raise ValueError('backlog_configuration_changed')
            subjects = [row['subject'] for row in to_rebuild if row['kind'] == 'data'
                        and row['source'] == original and row.get('data_ref') == data_ref][:8]
            _, rows, _, _ = data_evidence(original, env, public_api=api, data_ref=data_ref, subjects=subjects)
        elif kind == 'execution':
            rows = []
            _, _, md_only = public_changes(api, original)
            execution_checks(api, original, md_only, rows)
        else:
            rows, _, _ = public_changes(api, original)
        return packet_inventory(original, rows)
    rebuilt, retained, rebuild_limits = backlog.reconstruct(to_rebuild, original_packets, fingerprint, maximum=2)
    limitations += rebuild_limits
    combined = {fingerprint(packet):packet for packet in result['packets']}
    for packet in rebuilt:
        combined.setdefault(fingerprint(packet), packet)
    pending_order = [row['fingerprint'] for row in old_recipes if row['state'] != 'tracked']
    choosing = dict(previous, pending=list(dict.fromkeys(pending_order+previous.get('pending', []))))
    selected, reused, deferred = choose(list(combined.values()), choosing, env.get('DRIFT_FORCE') == 'true')
    # Keep only allowlisted reconstruction recipes in public artifacts. Full
    # packets, private source locations and private configuration remain local.
    carried = backlog.merge(retained+[row for row in old_recipes if row['fingerprint'] in current_keys],
                            recipes, previous_overflow=previous.get('backlog_overflow', 0))
    limitations += carried['limitations']
    control = {'version':VERSION, 'source':source, 'trusted_sha':trusted_sha, 'engine_hash':engine_hash(),
        'violations':result['violations'], 'limitations':sorted(set(limitations)),
        'data_failed':data_failed, 'waiting':waiting, 'selected':len(selected), 'reused':reused,
        'pending':deferred, 'baseline_missing':previous.get('baseline_missing', False), 'locations':locations,
        'previous_entries':previous.get('entries', []), 'check_id':check_id, 'provider_status':'not_executed',
        'backlog':carried['items'], 'backlog_overflow':carried['overflow'], 'current_fingerprints':list(current_keys)}
    save(Path(out)/'private'/'packets.json', selected)
    save(Path(out)/'control.json', control)
    if env.get('DRIFT_PROVIDER_CONFIGURED') == 'true' and selected:
        reserved_keys = {row['fingerprint'] for row in selected}
        save(Path(out)/'public'/'reservation.json', {'version':VERSION, 'engine_hash':engine_hash(),
             'fingerprints':[row['fingerprint'] for row in selected],
             'backlog':[{**row, 'state':'reserved'} if row['fingerprint'] in reserved_keys else row for row in carried['items']],
             'backlog_overflow':carried['overflow']})
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
    states = [row['state'] for row in control.get('backlog', [])]
    lines.append('Retained relationships: ' + str(states.count('tracked')) + ' tracked; ' +
                 str(sum(state != 'tracked' for state in states)) + ' unfinished; ' +
                 str(control.get('backlog_overflow', 0)) + ' beyond the retained inventory.')
    if control.get('baseline_missing'):
        lines.append('No reusable baseline was available in the bounded history window.')
    for violation in control['violations']:
        lines.append('- `' + violation['code'] + '`; reference `' + violation['subject'] + '`.')
    for row in control.get('results', []):
        record = row['record']
        lines.append('- Jev: `' + record['status'] + '`' + (' (reused; no request)' if record.get('reused') else '') + '; reference `' + record['subject'] + '`.')
        location = control.get('locations', {}).get(row['fingerprint'])
        if isinstance(location, dict) and safe_path(location.get('path')) and SHA.fullmatch(location.get('head_sha','')):
            path = location['path']
            line = location.get('line', 1)
            anchor = '#L'+str(line) if type(line) is int and line > 0 else ''
            url = 'https://github.com/' + control['source']['repository'] + '/blob/' + location['head_sha'] + '/' + urllib.parse.quote(path, safe='/') + anchor
            lines.append('  Public source: [' + path + '](' + url + ').')
        provenance = record.get('source', {})
        if provenance.get('head_sha') and provenance['head_sha'] != control['source']['head_sha']:
            lines.append('  Historical observation at `' + provenance['head_sha'] + '`; not a current-candidate verdict.')
        answers = record.get('diagnosis', {}).get('answers', {})
        if answers:
            lines.append('  Classifications: ' + ', '.join('`'+name+': '+answer['choice']+'`' for name,answer in answers.items()) + '.')
    if 'decision_data_not_configured' in control['limitations']:
        lines += ['', 'Private owner-decision consistency was NOT checked: no decision data is configured. '
                  'This analysis covers public code, tests and execution evidence only.']
    if control['limitations']:
        lines += ['', 'Coverage limitations: ' + ', '.join('`'+x+'`' for x in control['limitations']) + '.']
    lines += ['', 'Private packets and full provider responses are not retained. Public fingerprints identify this analysis.',
              'Semantic judgments are advisory; deterministic contract checks have their own verdict.']
    return '\n'.join(lines)+'\n'


def publish(api, out):
    root = Path(out)
    control = load(root/'control.json')
    public = root/'public'
    rows = control.get('results', [])
    finished = {row['fingerprint'] for row in rows if settled(row['record'])}
    current = set(control.get('current_fingerprints', []))
    healthy_subjects = {row['record']['subject'] for row in rows if row['fingerprint'] in current and settled(row['record'])}
    current_scope = backlog.scope_for(control['source'])
    finished.update(row['fingerprint'] for row in control.get('backlog', [])
                    if row['scope'] == current_scope and row['subject'] in healthy_subjects)
    reserved = {row['fingerprint'] for row in rows if row['record']['status'] not in {'diagnosed','abstained'}
                and (row['record'].get('provider_request_attempted') or row['record'].get('provider_request_attempts',0)
                     or row['record']['status'] == 'previous_request_reserved')}
    carried = backlog.merge(control.get('backlog', []), [], finished=finished, reserved=reserved,
                            previous_overflow=control.get('backlog_overflow', 0))
    analyzed = {row['fingerprint'] for row in rows if row['record']['status'] in {'diagnosed','abstained'}}
    for recipe in carried['items']:
        if recipe['fingerprint'] in analyzed:
            recipe['state'] = 'tracked'
    control.update(backlog=carried['items'], backlog_overflow=carried['overflow'])
    control['limitations'] = sorted(set(control['limitations'] + carried['limitations']))
    if not current_candidate(api, control['source']):
        if control.get('check_id'):
            complete_check(api, control['check_id'], control['source'], 'success', 'Superseded candidate.')
        save(public/'state.json', {'version':VERSION, 'trusted_sha':control['trusted_sha'],
            'engine_hash':control['engine_hash'], 'entries':[], 'pending':control['pending'], 'superseded':True,
            'backlog':control.get('backlog', []), 'backlog_overflow':control.get('backlog_overflow', 0)})
        return 0
    failed = bool(control['violations'] or control['data_failed'])
    status = 'completed' if failed or not control['waiting'] else 'in_progress'
    check_id = control.get('check_id') or begin_check(api, control['source'])
    if status == 'completed':
        complete_check(api, check_id, control['source'], 'failure' if failed else 'success', summary(control))
    entries = {r['fingerprint']:r for r in control.get('previous_entries', [])}
    entries.update({r['fingerprint']:r for r in rows if r['record']['status'] in {'diagnosed','abstained'}})
    save(public/'state.json', {'version':VERSION, 'trusted_sha':control['trusted_sha'],
          'engine_hash':control['engine_hash'], 'entries':list(entries.values())[-200:], 'pending':control['pending'],
          'backlog':control.get('backlog', []), 'backlog_overflow':control.get('backlog_overflow', 0)})
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
