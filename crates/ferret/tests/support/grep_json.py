"""Validate rg event groups and remove only documented physical fields.

Used by owned CLI fixtures. Whole file groups may reorder; events and
submatches within each group must remain ordered and byte-exact.
"""
import base64
import json
import sys


def byte_field(value):
    assert isinstance(value, dict) and len(value) == 1, value
    if 'text' in value:
        assert isinstance(value['text'], str), value
        return value['text'].encode('utf-8')
    assert set(value) == {'bytes'}, value
    data = base64.b64decode(value['bytes'], validate=True)
    assert base64.b64encode(data).decode() == value['bytes'], value
    try:
        data.decode('utf-8')
    except UnicodeDecodeError:
        return data
    raise AssertionError('valid UTF-8 must use text')


def stats(value):
    assert set(value) == {'elapsed', 'searches', 'searches_with_match',
                          'bytes_searched', 'bytes_printed', 'matched_lines', 'matches'}, value
    duration(value['elapsed'])
    for key in value.keys() - {'elapsed'}:
        assert type(value[key]) is int and value[key] >= 0, value
    return {key: value[key] for key in ('searches_with_match', 'matched_lines', 'matches')}


def duration(value):
    assert set(value) == {'secs', 'nanos', 'human'}, value
    assert type(value['secs']) is int and value['secs'] >= 0, value
    assert type(value['nanos']) is int and 0 <= value['nanos'] < 1000000000, value
    assert isinstance(value['human'], str), value


def normalize(raw, quiet=False):
    groups = []
    current = None
    summary = None
    totals = dict.fromkeys(('searches_with_match', 'matched_lines', 'matches'), 0)
    counts = None
    for raw_line in raw.splitlines():
        event = json.loads(raw_line)
        assert set(event) == {'type', 'data'}, event
        kind, data = event['type'], event['data']
        assert summary is None, 'event after summary'
        if kind == 'begin':
            assert current is None and set(data) == {'path'}, event
            path = byte_field(data['path'])
            current = [event]
            counts = {'matched_lines': 0, 'matches': 0}
        elif kind in ('match', 'context'):
            assert current is not None, event
            assert set(data) == {'path', 'lines', 'line_number', 'absolute_offset', 'submatches'}, event
            assert byte_field(data['path']) == path, event
            source = byte_field(data['lines'])
            assert data['line_number'] is None or (type(data['line_number']) is int and data['line_number'] > 0), event
            assert type(data['absolute_offset']) is int and data['absolute_offset'] >= 0, event
            previous = 0
            for span in data['submatches']:
                assert set(span) in ({'match', 'start', 'end'}, {'match', 'replacement', 'start', 'end'}), span
                if 'replacement' in span:
                    byte_field(span['replacement'])
                start, end = span['start'], span['end']
                assert type(start) is int and type(end) is int and previous <= start <= end <= len(source), span
                assert byte_field(span['match']) == source[start:end], span
                previous = end
            if kind == 'match':
                counts['matched_lines'] += source.count(b'\n') + int(not source.endswith(b'\n'))
                counts['matches'] += len(data['submatches'])
            current.append(event)
        elif kind == 'end':
            assert current is not None, event
            assert set(data) == {'path', 'binary_offset', 'stats'}, event
            assert byte_field(data['path']) == path, event
            binary = data['binary_offset']
            assert binary is None or (type(binary) is int and binary >= 0), event
            data['stats'] = stats(data['stats'])
            assert all(data['stats'][key] == value for key, value in counts.items()), event
            for key in totals:
                totals[key] += data['stats'][key]
            current.append(event)
            groups.append(current)
            current = None
        elif kind == 'summary':
            assert current is None and set(data) == {'elapsed_total', 'stats'}, event
            duration(data.pop('elapsed_total'))
            data['stats'] = stats(data['stats'])
            if not quiet:
                assert data['stats'] == totals, (data['stats'], totals)
            summary = event
        else:
            raise AssertionError(kind)
    assert current is None, 'unterminated file group'
    if raw:
        assert summary is not None, 'missing summary'
    return json.dumps([sorted(groups, key=lambda group: json.dumps(group, sort_keys=True)), summary],
                      sort_keys=True, ensure_ascii=True, separators=(',', ':')).encode()


def projection(raw):
    """Readable event signatures for independent, literal stream expectations."""
    result = []
    for line in raw.splitlines():
        event = json.loads(line)
        kind, data = event['type'], event['data']
        if kind == 'begin':
            result.append([kind, byte_field(data['path']).decode('utf-8')])
        elif kind in ('match', 'context'):
            spans = []
            for span in data['submatches']:
                item = [span['start'], span['end'], byte_field(span['match']).decode('utf-8')]
                if 'replacement' in span:
                    item.append(byte_field(span['replacement']).decode('utf-8'))
                spans.append(item)
            result.append([kind, byte_field(data['lines']).decode('utf-8'),
                           data['line_number'], data['absolute_offset'], spans])
        elif kind in ('end', 'summary'):
            counts = [data['stats'][key] for key in ('matched_lines', 'matches', 'searches_with_match')]
            result.append([kind, data['binary_offset'], counts] if kind == 'end' else [kind, counts])
        else:
            raise AssertionError(kind)
    return result


if __name__ == '__main__':
    raw = sys.stdin.buffer.read()
    partial = '--expect-partial' in sys.argv
    normalized = b'' if partial else normalize(raw, quiet='--quiet' in sys.argv)
    flag = '--expect-partial' if partial else '--expect'
    if flag in sys.argv:
        expected = json.loads(sys.argv[sys.argv.index(flag) + 1])
        actual = projection(raw)
        assert actual == expected, (actual, expected)
    else:
        sys.stdout.buffer.write(normalized)
