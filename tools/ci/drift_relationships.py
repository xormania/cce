"""Read pinned contract/proof relationships without executing repository content.

Hunks select their surrounding function or document section. Explicit local links
and named tests supply proof alongside the before/after claim. Bounds are visible
limitations; an excerpt is never described as a complete file or complete proof.
"""
from __future__ import annotations

import ast
from bisect import bisect_right
import hashlib
import json
import posixpath
import re
import time
import urllib.error
import urllib.parse

MAX_FILES = 40
MAX_DOWNLOADS = 32
MAX_HUNKS = 4
MAX_TREE = 5000
MAX_EXCERPTS = 8
MAX_EXCERPT = 1600
MAX_MAPPED_CASES = 8
MAX_CASE_DOWNLOADS = 4
EXTENSIONS = ('.rs', '.py', '.md', '.yml', '.yaml', '.toml', '.json')
_PATH = re.compile(r'[A-Za-z0-9_. /-]{1,299}\Z')
_FUNCTION = re.compile(r'^\s*(?:pub(?:\([^)]*\))?\s+)?(?:async\s+)?(?:fn|def)\s+([A-Za-z_][A-Za-z0-9_]*)\s*\(')
_HUNK = re.compile(r'^@@ -(\d+)(?:,(\d+))? \+(\d+)(?:,(\d+))? @@(.*)$', re.M)
_SYMBOL = re.compile(r'`([A-Za-z_][A-Za-z0-9_]*(?:::[A-Za-z_][A-Za-z0-9_]*)*)`')
_LINK = re.compile(r'\[[^\]\n]*\]\(([^\s)]+)(?:\s+"[^"\n]*")?\)')
_DECISIVE = re.compile(r'assert(?:_eq|_ne)?!?(?:\s|\()|self\.assert|raise |expect\(|unwrap\(|panic!|must |shall |guarantee|owner|defer|supersed|correct|resolv', re.I)
_RAW_STRING = re.compile(r'(?:b|c)?r(#{0,255})"')
_CHAR = re.compile(r"'(?:\\(?:u\{[0-9a-fA-F_]+\}|x[0-9a-fA-F]{2}|[^\n])|[^'\\\n])'")


def safe_path(path):
    return bool(isinstance(path, str) and _PATH.fullmatch(path) and not path.startswith('/')
                and all(part not in {'', '.', '..'} for part in path.split('/')))


def _short_line(line):
    raw = line.encode('utf-8')
    if len(raw) <= 450:
        return line
    return raw[:220].decode('utf-8', 'ignore') + ' [line omitted] ' + raw[-200:].decode('utf-8', 'ignore')


def excerpt(label, path, ref, lines, start=0, end=None, focus=()):
    """Render source identity and sampled decisive lines within one evidence bound."""
    end = len(lines) if end is None else min(end, len(lines))
    start = max(0, min(start, end))
    inventory = list(range(start, end))
    raw = '\n'.join(lines[start:end]).encode('utf-8')
    header = (label + ': ' + path + ' @ ' + ref + '\n'
              + 'lines ' + str(start + 1) + '-' + str(end) + '; sha256=' + hashlib.sha256(raw).hexdigest() + '\n')
    budget = MAX_EXCERPT - len(header.encode('utf-8')) - 70
    rendered = {i: str(i + 1) + ': ' + _short_line(lines[i]) for i in inventory}
    whole = '\n'.join(rendered.values())
    omitted = any(len(lines[i].encode('utf-8')) > 450 for i in inventory)
    if len(whole.encode('utf-8')) <= budget:
        return header + whole + ('\n[long-line content omitted; incomplete context]' if omitted else ''), omitted
    # Retain the affected lines and actual assertions, including assertions at the
    # end of long setup bodies. Equal scoring is resolved toward the final witness.
    priority = list(dict.fromkeys([i for i in focus if i in rendered] +
        [i for i in reversed(inventory) if _DECISIVE.search(lines[i])] +
        inventory[:3] + inventory[-2:] + inventory))
    selected, size = set(), 0
    for i in priority:
        cost = len(rendered[i].encode('utf-8')) + 1
        if size + cost <= budget:
            selected.add(i)
            size += cost
    return header + '\n'.join(rendered[i] for i in sorted(selected)) + '\n[lines omitted; incomplete context]', True


def _rust_code(text):
    """Mask comments and literals, preserving character and line positions.

    This is a lexical scan, not Rust execution or a general parser. Matching
    braces in the masked text cannot mistake fixture payloads for function ends.
    """
    result = list(text)
    size, i = len(text), 0

    def mask(start, end):
        for index in range(start, end):
            if result[index] != '\n':
                result[index] = ' '

    while i < size:
        if text.startswith('//', i):
            end = text.find('\n', i + 2)
            end = size if end < 0 else end
            mask(i, end)
            i = end
            continue
        if text.startswith('/*', i):
            start, depth, i = i, 1, i + 2
            while i < size and depth:
                if text.startswith('/*', i):
                    depth += 1
                    i += 2
                elif text.startswith('*/', i):
                    depth -= 1
                    i += 2
                else:
                    i += 1
            mask(start, i)
            continue
        raw = _RAW_STRING.match(text, i) if text[i] in 'rbc' and (i == 0 or not (text[i - 1].isalnum() or text[i - 1] == '_')) else None
        if raw:
            closing = '"' + raw[1]
            end = text.find(closing, raw.end())
            end = size if end < 0 else end + len(closing)
            mask(i, end)
            i = end
            continue
        if text[i] == '"':
            start, i = i, i + 1
            while i < size:
                if text[i] == '\\':
                    i = min(size, i + 2)
                elif text[i] == '"':
                    i += 1
                    break
                else:
                    i += 1
            mask(start, i)
            continue
        if text[i] == "'":
            # A lifetime such as 'a has no closing quote after one character.
            char = _CHAR.match(text, i)
            if char:
                end = char.end()
                mask(i, end)
                i = end
                continue
        i += 1
    return ''.join(result)


def _blocks(text, path):
    lines = text.splitlines()
    if path.endswith('.py'):
        try:
            tree = ast.parse(text)
            return [(node.lineno - 1, node.end_lineno, node.name)
                    for node in ast.walk(tree) if isinstance(node, (ast.FunctionDef, ast.AsyncFunctionDef))]
        except (SyntaxError, ValueError, RecursionError):
            pass
    masked = _rust_code(text)
    code_lines = masked.splitlines(keepends=True)
    offsets, position = [], 0
    for line in code_lines:
        offsets.append(position)
        position += len(line)
    blocks = []
    for i, line in enumerate(code_lines):
        match = _FUNCTION.match(line)
        if not match:
            continue
        cursor = offsets[i] + match.end()
        opening = masked.find('{', cursor)
        semicolon = masked.find(';', cursor)
        if opening < 0 or (0 <= semicolon < opening):
            continue
        cursor, depth = opening + 1, 1
        while cursor < len(masked) and depth:
            depth += (masked[cursor] == '{') - (masked[cursor] == '}')
            cursor += 1
        end = bisect_right(offsets, max(0, cursor - 1))
        blocks.append((i, end, match[1]))
    return blocks


def _range(text, path, line=0, symbol=''):
    lines = text.splitlines()
    blocks = _blocks(text, path)
    matches = [block for block in blocks if (block[2] == symbol.split('::')[-1] if symbol else block[0] <= line < block[1])]
    if matches:
        begin, end, name = min(matches, key=lambda block: block[1] - block[0])
        # Include the adjacent doc comment/contract, not preceding unrelated code.
        begin = max(0, begin - 5)
        return lines, begin, end, name
    if path.endswith('.md'):
        headings = [i for i, value in enumerate(lines) if re.match(r'^#{1,6}\s', value)]
        if symbol:
            slug = lambda value: re.sub(r'[^a-z0-9 _-]', '', value.lower()).strip().replace(' ', '-')
            selected = [i for i in headings if slug(re.sub(r'^#+\s*', '', lines[i])) == symbol]
            if selected:
                line = selected[0]
        before = [i for i in headings if i <= line]
        start = before[-1] if before else max(0, line - 5)
        end = next((i for i in headings if i > line), len(lines))
        return lines, start, end, symbol
    return lines, max(0, line - 5), min(len(lines), line + 12), ''


def _corrective_side(text, path, changed, position):
    """Identify unique changed lines within one named function or section.

    These hashes are identity evidence, not a judgment that a change is a fix.
    Repeated lines, unnamed contexts and multi-context edits stay ambiguous.
    """
    if text is None:
        return None
    lines = text.splitlines()
    meaningful = lambda value: bool(value.strip()) and bool(re.search(r'[\w]', value))
    changed = [line for line in changed if 0 <= line < len(lines) and meaningful(lines[line])]
    if len(changed) > 16:
        return None
    positions = changed or [position]
    blocks = _blocks(text, path) if path.endswith(('.rs', '.py')) else []
    if path.endswith('.md'):
        headings = [(i, value.strip()) for i, value in enumerate(lines) if re.match(r'^#{1,6}\s', value)]
        blocks = [(start, headings[index + 1][0] if index + 1 < len(headings) else len(lines), name)
                  for index, (start, name) in enumerate(headings)]
    matches = [block for block in blocks if all(block[0] <= line < block[1] for line in positions)]
    if not matches:
        return None
    start, end, name = min(matches, key=lambda block: block[1] - block[0])
    if sum(block[2] == name for block in blocks) != 1:
        return None
    context = [line.strip() for line in lines[start:end]]
    values = [lines[line].strip() for line in changed]
    if any(context.count(value) != 1 for value in values):
        return None
    anchor = hashlib.sha256(json.dumps([path, name], separators=(',', ':')).encode()).hexdigest()
    return anchor, sorted(hashlib.sha256(value.encode()).hexdigest() for value in values)


def corrective_identity(before, after, before_path, path, old_changed, new_changed, old_line, new_line):
    left = _corrective_side(before, before_path, old_changed, old_line)
    right = _corrective_side(after, path, new_changed, new_line)
    if not left or not right or left[0] != right[0] or not (left[1] or right[1]):
        return None
    return {'anchor': left[0], 'before': left[1], 'after': right[1]}


class Collector:
    def __init__(self, api, source):
        self.api, self.source = api, source
        self.cache = {}
        self.downloads = 0
        self.limitations = set()
        self.inventory = None
        self.case_paths = None

    def file(self, path, ref, required=True):
        if not safe_path(path):
            self.limitations.add('unsafe_reference_ignored')
            return None
        key = path, ref
        # Charge each relationship's distinct lookup inventory even on cache
        # hits. Its packet must not depend on which other cases ran before it.
        if self.case_paths is not None and key not in self.case_paths:
            if len(self.case_paths) >= MAX_CASE_DOWNLOADS:
                self.limitations.add('relationship_case_download_bound')
                return None
            self.case_paths.add(key)
        if key in self.cache:
            if self.cache[key] is None and required:
                self.limitations.add('source_excerpt_unavailable')
            return self.cache[key]
        if self.downloads >= MAX_DOWNLOADS:
            self.limitations.add('relationship_download_bound')
            return None
        self.downloads += 1
        try:
            text = self.api.file(path, ref)
            if not isinstance(text, str):
                raise ValueError('invalid_source')
        except (ValueError, KeyError, OSError, UnicodeError, urllib.error.URLError):
            if required:
                self.limitations.add('source_excerpt_unavailable')
            text = None
        self.cache[key] = text
        return text

    def paths(self):
        if self.inventory is None:
            try:
                tree = self.api.api('/git/trees/' + self.source['head_sha'] + '?recursive=1')
                rows = tree.get('tree', [])
                if tree.get('truncated') or len(rows) > MAX_TREE:
                    self.limitations.add('relationship_inventory_truncated')
                self.inventory = [row['path'] for row in rows[:MAX_TREE] if row.get('type') == 'blob'
                                  and safe_path(row.get('path')) and row['path'].endswith(('.rs', '.py'))]
            except (ValueError, KeyError, OSError, urllib.error.URLError):
                self.inventory = []
                self.limitations.add('relationship_inventory_unavailable')
        return self.inventory

    def render(self, label, path, ref, text, line=0, symbol=''):
        anchor = re.fullmatch(r'L(\d+)(?:-L?(\d+))?', symbol)
        if anchor:
            lines = text.splitlines()
            begin = max(0, int(anchor[1]) - 1)
            end = min(len(lines), int(anchor[2] or anchor[1]))
            if begin >= end:
                self.limitations.add('named_proof_unavailable')
            line = begin
        elif label == 'Linked proof' and not symbol:
            lines, begin, end = text.splitlines(), 0, len(text.splitlines())
        else:
            lines, begin, end, _ = _range(text, path, line, symbol)
        value, omitted = excerpt(label, path, ref, lines, begin, end, (line,))
        if omitted:
            self.limitations.add('relationship_context_incomplete')
        return value

    def symbol(self, symbol, ref, preferred=()):
        name = symbol.split('::')[-1]
        preferred = list(dict.fromkeys(path for path in preferred if safe_path(path)))
        # An explicit mapped crate/target identifies the proof's namespace. Avoid
        # downloading unrelated tests merely to re-establish that identity.
        matches = []
        for path in preferred:
            text = self.file(path, ref, required=False)
            if text is not None and any(block[2] == name for block in _blocks(text, path)):
                matches.append((path, text))
        if len(matches) == 1:
            path, text = matches[0]
            return [self.render('Named proof ' + symbol, path, ref, text, symbol=symbol)]
        if len(matches) > 1:
            self.limitations.add('named_proof_ambiguous')
            return []
        paths = [path for path in self.paths() if path not in preferred]
        found = []
        for path in paths:
            text = self.file(path, ref, required=False)
            if text is not None and any(block[2] == name for block in _blocks(text, path)):
                found.append((path, text))
                if len(found) == 2:
                    break
        if len(found) != 1:
            self.limitations.add('named_proof_ambiguous' if found else 'named_proof_unavailable')
            return []
        path, text = found[0]
        return [self.render('Named proof ' + symbol, path, ref, text, symbol=symbol)]

    def linked(self, path, context, ref, preferred=()):
        excerpts, covered = [], set()
        for target in _LINK.findall(context):
            parsed = urllib.parse.urlsplit(target)
            if parsed.scheme or parsed.netloc or parsed.query or not parsed.path:
                continue
            decoded = urllib.parse.unquote(parsed.path)
            resolved = posixpath.normpath(posixpath.join(posixpath.dirname(path), decoded))
            if not safe_path(resolved):
                self.limitations.add('unsafe_reference_ignored')
                continue
            if not resolved.endswith(EXTENSIONS):
                continue
            text = self.file(resolved, ref)
            if text is None:
                continue
            fragment = urllib.parse.unquote(parsed.fragment)
            if fragment and resolved.endswith(('.rs', '.py')) and not re.fullmatch(r'L\d+(?:-L?\d+)?', fragment):
                if not any(block[2] == fragment.split('::')[-1] for block in _blocks(text, resolved)):
                    self.limitations.add('named_proof_unavailable')
                    continue
            excerpts.append(self.render('Linked proof', resolved, ref, text, symbol=fragment))
            if fragment:
                covered.add(fragment.split('::')[-1])
            if len(excerpts) >= 3:
                self.limitations.add('relationship_reference_bound')
                break
        for symbol in dict.fromkeys(_SYMBOL.findall(context)):
            if symbol.split('::')[-1] in covered or len(symbol) < 5 or symbol in {'complete', 'partial', 'green', 'planned'}:
                continue
            # Plain prose identifiers are not automatically symbols. Resolve named
            # tests, qualified symbols, or names explicitly introduced as a test.
            if '::' not in symbol and not re.search(r'(?:test|assertion|proof)\s+`' + re.escape(symbol) + '`', context, re.I):
                continue
            excerpts += self.symbol(symbol, ref, preferred)
            if len(excerpts) >= 3:
                self.limitations.add('relationship_reference_bound')
                break
        return excerpts[:3]


def public_changes(api, source):
    """Return normalized semantic rows, limitations and producer Markdown policy."""
    data = api.api('/compare/' + source['base_sha'] + '...' + source['head_sha'])
    files = data.get('files', [])
    collector = Collector(api, source)
    if len(files) >= 300 or len(files) > MAX_FILES:
        collector.limitations.add('file_inventory_truncated')
    preferred = [row.get('filename', '') for row in files if row.get('filename', '').endswith(('.rs', '.py'))]
    rows = []
    for item in files[:MAX_FILES]:
        path = item.get('filename', '')
        if not safe_path(path) or not path.endswith(EXTENSIONS):
            continue
        before_path = item.get('previous_filename', path)
        before = None if item.get('status') == 'added' else collector.file(before_path, source['base_sha'])
        after = None if item.get('status') == 'removed' else collector.file(path, source['head_sha'])
        patch = item.get('patch', '')
        hunks = list(_HUNK.finditer(patch))
        if len(hunks) > MAX_HUNKS:
            collector.limitations.add('relationship_hunk_bound')
        if not hunks:
            collector.limitations.add('changed_hunk_unavailable')
        for index, hunk in enumerate(hunks[:MAX_HUNKS] or [None]):
            old_line = max(0, int(hunk[1]) - 1) if hunk else 0
            new_line = max(0, int(hunk[3]) - 1) if hunk else 0
            hunk_text = ''
            old_changed, new_changed = [], []
            if hunk:
                end = hunks[index + 1].start() if index + 1 < len(hunks) else len(patch)
                hunk_text = patch[hunk.start():end]
                old_cursor, new_cursor = old_line, new_line
                for diff_line in hunk_text.splitlines()[1:]:
                    if diff_line.startswith('-'):
                        old_changed.append(old_cursor)
                        old_cursor += 1
                    elif diff_line.startswith('+'):
                        new_changed.append(new_cursor)
                        new_cursor += 1
                    elif diff_line.startswith(' '):
                        old_cursor += 1
                        new_cursor += 1
                old_line = old_changed[0] if old_changed else old_line
                new_line = new_changed[0] if new_changed else new_line
            excerpts = []
            contexts = []
            for label, text, candidate, line, file_path in (
                ('Before claim/assertion', before, source['base_sha'], old_line, before_path),
                ('After claim/assertion', after, source['head_sha'], new_line, path)):
                if text is not None:
                    excerpts.append(collector.render(label, file_path, candidate, text, line))
                    lines, start, end, _ = _range(text, file_path, line)
                    contexts.append('\n'.join(lines[start:end]))
            if hunk:
                contexts.append(hunk_text)
            if not excerpts:
                value, omitted = excerpt('Changed hunk; source unavailable', path, source['head_sha'], patch.splitlines())
                excerpts.append(value)
                if omitted:
                    collector.limitations.add('relationship_context_incomplete')
            excerpts += collector.linked(path, '\n'.join(contexts), source['head_sha'], preferred)
            row = {'id': path + '#relationship-' + str(index + 1), 'public_path':path,
                         'public_line':new_line + 1, 'excerpts':excerpts[:MAX_EXCERPTS],
                         'questions':['proof', 'guarantee']}
            correction = corrective_identity(before, after, before_path, path,
                                             old_changed, new_changed, old_line, new_line)
            if correction:
                row['correction'] = correction
            rows.append(row)
    return rows, sorted(collector.limitations), bool(files) and all(row['filename'].endswith('.md') for row in files)


def mapped_relationships(api, source, cases, summary='', context=None, *, data_ref=None, subjects=None, offset=None):
    """Join existing mapped case/test names to public source; return private rows.

    ``context`` is existing requirement/decision text indexed by its existing ID.
    The caller owns private input provenance and must keep these row IDs private.
    No new file or annotation is required from an agent or project contributor.
    """
    collector = Collector(api, source)
    rows = []
    candidates = [case for case in cases if isinstance(case, dict) and isinstance(case.get('tests'), list) and case['tests']]
    if subjects is not None:
        wanted = set(subjects)
        candidates = [case for case in candidates if hashlib.sha256(
            ('semantic\0mapped-relationship:' + str(case.get('id', ''))).encode()).hexdigest()[:24] in wanted]
    elif candidates:
        # Rotate whole batches so every mapped relationship, including the tail,
        # is eventually collected. Targeted reconstruction is independent of time.
        slot = int(time.time() // 1800) if offset is None else offset
        if type(slot) is not int or slot < 0:
            raise ValueError('invalid_relationship_offset')
        start = (slot % ((len(candidates) + MAX_MAPPED_CASES - 1) // MAX_MAPPED_CASES)) * MAX_MAPPED_CASES
        candidates = candidates[start:] + candidates[:start]
    selected = candidates[:MAX_MAPPED_CASES]
    if len(candidates) > MAX_MAPPED_CASES:
        collector.limitations.add('relationship_case_bound')
    ref = data_ref or source['head_sha']
    if not isinstance(ref, str) or not re.fullmatch(r'[a-f0-9]{40}', ref):
        raise ValueError('invalid_data_revision')
    for case in selected:
        collector.case_paths = set()
        tests = case['tests']
        related = [line for line in summary.splitlines() if case.get('slice', '') in line]
        clause = '\n'.join(related[:2])
        requirements = case.get('requirements', [])
        if context:
            # Existing maps and documents can be supplied unchanged by the data
            # adapter. Search the case/requirement neighborhood, never a prefix.
            needles = [str(case.get('id', ''))] + [str(item.get('id', '')) if isinstance(item, dict) else str(item)
                                                    for item in requirements]
            clauses = []
            for _path, text in list(context.items())[:12]:
                if not isinstance(text, str):
                    continue
                lines = text.splitlines()
                for i, line in enumerate(lines):
                    if any(needle and needle in line for needle in needles):
                        clauses.extend(lines[max(0, i - 2):min(len(lines), i + 6)])
                        break
            clause += '\n' + '\n'.join(clauses)
        initial = 'Mapped claim: ' + str(case.get('id', '')) + '\n' + clause + '\nStatus: ' + str(case.get('status', 'unknown'))
        value, omitted = excerpt('Existing recorded claim', 'mapped-claim', ref, initial.splitlines())
        if omitted:
            collector.limitations.add('relationship_context_incomplete')
        excerpts = [value]
        for test in tests[:3]:
            if not isinstance(test, str):
                continue
            parts = test.split(':', 2)
            preferred = []
            symbol = parts[-1]
            if len(parts) == 3 and re.fullmatch(r'[A-Za-z0-9_-]+', parts[0]) and re.fullmatch(r'[A-Za-z0-9_-]+', parts[1]):
                crate, target, symbol = parts
                preferred = ['crates/' + crate + '/tests/' + target + '.rs',
                             'crates/' + crate + '/src/' + symbol.split('::')[0] + '.rs']
            excerpts += collector.symbol(symbol, source['head_sha'], preferred)
        if len(tests) > 3:
            collector.limitations.add('relationship_reference_bound')
        rows.append({'id':'mapped-relationship:' + str(case.get('id', '')), 'excerpts':excerpts[:MAX_EXCERPTS],
                     'questions':['proof', 'guarantee', 'resolution']})
    return rows, sorted(collector.limitations)
