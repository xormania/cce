"""Public-safe recipes for unfinished diagnosis; evidence remains at its origin.

A recipe pins the original public candidate and, for data-backed packets, the
versioned data snapshot. It contains no excerpts, data paths or data repository
name. Reconstructing an older recipe never makes its evidence current evidence.
"""
from __future__ import annotations

import hashlib
import json
import re

MAX_BACKLOG = 200
MAX_RECONSTRUCTIONS = 8
KINDS = frozenset({'public', 'data', 'execution'})
STATES = frozenset({'deferred', 'reserved', 'unavailable', 'tracked'})
_SHA = re.compile(r'[0-9a-f]{40}\Z')
_HASH = re.compile(r'[0-9a-f]{64}\Z')
_SUBJECT = re.compile(r'[0-9a-f]{8,64}\Z')


def _canonical(value):
    return json.dumps(value, sort_keys=True, separators=(',', ':'),
                      ensure_ascii=False, allow_nan=False).encode()


def _source(source, repository):
    if not isinstance(source, dict) or source.get('repository') != repository:
        raise ValueError('Invalid backlog source')
    if any(not isinstance(source.get(key), str) or not _SHA.fullmatch(source[key])
           for key in ('head_sha', 'base_sha')):
        raise ValueError('Invalid backlog source')
    branch = source.get('branch', '')
    if not isinstance(branch, str) or not branch or len(branch.encode()) > 255 or any(
            ord(char) < 32 for char in branch):
        raise ValueError('Invalid backlog source')
    pulls = source.get('pulls', [])
    if (not isinstance(pulls, list) or len(pulls) > 30 or
            any(type(value) is not int or value <= 0 for value in pulls)):
        raise ValueError('Invalid backlog source')
    result = {key: source[key] for key in ('repository', 'head_sha', 'base_sha')}
    result.update(branch=branch, pulls=list(pulls))
    for key in ('run_id', 'attempt'):
        value = source.get(key, 0)
        if type(value) is not int or value < 0:
            raise ValueError('Invalid backlog source')
        result[key] = value
    return result


def scope_for(source, repository='xormania/cce'):
    """Stable branch scope; the original candidate remains a separate identity."""
    source = _source(source, repository)
    return hashlib.sha256(_canonical([repository, source['branch']])).hexdigest()


def descriptor(packet, fingerprint, kind='public', *, data_ref=None,
               data_key=None, repository='xormania/cce'):
    """Project a packet into an allowlisted reconstruction recipe.

    ``fingerprint`` is computed by the caller's versioned packet implementation.
    ``data_key`` hashes the configured data-source selectors without publishing
    them. A configured collector must match that key before fetching ``data_ref``.
    """
    if (not isinstance(packet, dict) or kind not in KINDS or
            not isinstance(fingerprint, str) or not _HASH.fullmatch(fingerprint) or
            not isinstance(packet.get('subject'), str) or
            not _SUBJECT.fullmatch(packet['subject'])):
        raise ValueError('Invalid backlog recipe')
    source = _source(packet.get('source'), repository)
    result = {'fingerprint': fingerprint, 'subject': packet['subject'],
              'scope': scope_for(source, repository), 'kind': kind,
              'source': source, 'state': 'deferred'}
    if kind == 'data':
        if (not isinstance(data_ref, str) or not _SHA.fullmatch(data_ref) or
                not isinstance(data_key, str) or not _HASH.fullmatch(data_key)):
            raise ValueError('Invalid backlog recipe')
        result.update(data_ref=data_ref, data_key=data_key)
    elif data_ref is not None or data_key is not None:
        raise ValueError('Invalid backlog recipe')
    return result


def safe_descriptor(value, repository='xormania/cce'):
    """Validate cached recipes strictly; never echo arbitrary cached fields."""
    if not isinstance(value, dict) or value.get('state') not in STATES:
        return None
    allowed = {'fingerprint', 'subject', 'scope', 'kind', 'source', 'state'}
    if value.get('kind') == 'data':
        allowed |= {'data_ref', 'data_key'}
    if set(value) != allowed:
        return None
    try:
        clean = descriptor({'subject': value['subject'], 'source': value['source']},
                           value['fingerprint'], value['kind'],
                           data_ref=value.get('data_ref'), data_key=value.get('data_key'),
                           repository=repository)
    except (ValueError, KeyError, TypeError, UnicodeError):
        return None
    if value['scope'] != clean['scope'] or value['source'] != clean['source']:
        return None
    clean['state'] = value['state']
    return clean


def merge(previous, current, *, finished=(), reserved=(), previous_overflow=0,
          maximum=MAX_BACKLOG, repository='xormania/cce'):
    """Carry unresolved recipes forward; disappearance alone is not resolution.

    Only a validated result or an explicit caller disposition belongs in
    ``finished``. A reservation is retained as unfinished, without retrying it.
    Overflow is a durable coverage count, never a claim of successful analysis.
    """
    if (not isinstance(previous, list) or not isinstance(current, list) or
            type(maximum) is not int or not 0 < maximum <= MAX_BACKLOG or
            type(previous_overflow) is not int or previous_overflow < 0):
        raise ValueError('Invalid backlog inventory')
    finished = {key for key in finished if isinstance(key, str) and _HASH.fullmatch(key)}
    reserved = {key for key in reserved if isinstance(key, str) and _HASH.fullmatch(key)}
    rows, invalid = {}, False
    # Old work stays first so a continuing stream of new changes cannot starve it.
    for row in previous + current:
        clean = safe_descriptor(row, repository)
        if clean is None:
            invalid = True
            continue
        fingerprint_key = clean['fingerprint']
        if fingerprint_key in finished:
            continue
        key = (clean['scope'], fingerprint_key)
        if key not in rows:
            rows[key] = clean
        if fingerprint_key in reserved:
            rows[key]['state'] = 'reserved'
    overflow = previous_overflow + max(0, len(rows) - maximum)
    limitations = []
    if invalid:
        limitations.append('backlog_recipe_invalid')
    if overflow:
        limitations.append('backlog_inventory_truncated')
    return {'items': list(rows.values())[:maximum], 'overflow': overflow,
            'limitations': limitations}


def reconstruct(items, collector, fingerprint, *, scope=None,
                maximum=MAX_RECONSTRUCTIONS, repository='xormania/cce'):
    """Refetch bounded original contexts and return matching original packets.

    ``collector(source, kind, data_ref, data_key)`` reads only pinned source data
    and returns normalized packets. Private fetch configuration stays outside
    this module. No callback-supplied source is relabelled as the latest head.
    ``scope`` optionally drains one branch; other recipes remain retained.
    """
    if (not isinstance(items, list) or len(items) > MAX_BACKLOG or
            type(maximum) is not int or not 0 < maximum <= MAX_RECONSTRUCTIONS or
            (scope is not None and (not isinstance(scope, str) or not _HASH.fullmatch(scope)))):
        raise ValueError('Invalid backlog inventory')
    clean_items, limitations, packets, contexts = [], set(), [], {}
    for row in items:
        clean = safe_descriptor(row, repository)
        if clean is None:
            limitations.add('backlog_recipe_invalid')
            continue
        clean_items.append(clean)
        if clean['state'] == 'reserved' or (scope is not None and clean['scope'] != scope):
            continue
        context_key = _canonical([clean['source'], clean['kind'],
                                  clean.get('data_ref'), clean.get('data_key')])
        if context_key not in contexts:
            if len(contexts) >= maximum:
                limitations.add('backlog_reconstruction_deferred')
                continue
            try:
                observed = collector(dict(clean['source']), clean['kind'],
                                     clean.get('data_ref'), clean.get('data_key'))
                if not isinstance(observed, list) or len(observed) > 256:
                    raise ValueError('Invalid reconstructed inventory')
                candidates = {}
                for packet in observed:
                    if not isinstance(packet, dict) or packet.get('source') != clean['source']:
                        continue
                    candidates[fingerprint(packet)] = packet
                contexts[context_key] = candidates
            except (ValueError, KeyError, TypeError, OSError):
                contexts[context_key] = {}
        packet = contexts[context_key].get(clean['fingerprint'])
        if packet is None or packet.get('subject') != clean['subject']:
            clean['state'] = 'unavailable'
            limitations.add('backlog_source_unavailable')
            continue
        if clean['state'] != 'tracked':
            clean['state'] = 'deferred'
        if not any(fingerprint(existing) == clean['fingerprint'] for existing in packets):
            packets.append(packet)
    return packets, clean_items, sorted(limitations)
