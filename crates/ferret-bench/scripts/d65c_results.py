#!/usr/bin/env python3
"""Export counts and timings without source paths, terms or contents."""

import json
from pathlib import Path
import re

import argparse
import csv

parser = argparse.ArgumentParser(description="Collect D65C numeric benchmark reports")
parser.add_argument("--run-dir", type=Path, required=True)
parser.add_argument("--output-dir", type=Path, required=True)
args = parser.parse_args()
BASE = args.run_dir
args.output_dir.mkdir(parents=True, exist_ok=True)
ARMS = ['control', 'h0', 'c1', 'c2', 'c3']


def value(v):
    try:
        return float(v) if '.' in v else int(v)
    except ValueError:
        return v


def fields(path):
    result = {}
    if not path.exists():
        return result
    for line in path.read_text().splitlines():
        found = re.fullmatch(r'([a-z0-9_]+): ([^ ]+(?: kB)?)', line)
        if found:
            result[found[1]] = value(found[2])
    return result


result = {}
for arm in ARMS:
    data = {'build': fields(BASE / f'{arm}-build.log'),
            'churn': fields(BASE / f'{arm}-churn.log'), 'churn_queries': [], 'queries': {}}
    path = BASE / f'{arm}-churn.log'
    if path.exists():
        text = path.read_text()
        for line in text.splitlines():
            if line.startswith('churn_records '):
                data['churn_queries'].append(dict((k, value(v)) for k, v in
                                                  (part.split('=', 1) for part in line.split()[1:])))
        for key in ['followed_records', 'demoted_records', 'purged_records']:
            data['churn'][key] = sum(map(int, re.findall(rf'{key}: (\d+)', text)))
    for state in ['first', 'steady', 'full']:
        path = BASE / f'{arm}-{state}-query.log'
        if not path.exists():
            continue
        query = {'classes': {}, 'sizes': []}
        keys = ['n', 'candidates_median', 'warm_median', 'warm_p90', 'warm_p95',
                'cold_median', 'cold_p90', 'cold_p95', 'rows_median', 'content_driven']
        for line in path.read_text().splitlines():
            if line.startswith('d65_size '):
                query['sizes'].append(dict((k, value(v)) for k, v in
                                           (part.split('=', 1) for part in line.split()[1:])))
            elif line.startswith('| '):
                cells = [c.strip() for c in line.split('|')[1:-1]]
                if len(cells) == 11 and cells[0] in ['df1', 'df2-100', 'df1-natural',
                                                   'df2-100-natural', 'absent', 'mid', 'common']:
                    query['classes'][cells[0]] = dict(zip(keys, map(value, cells[1:])))
            elif '_records_per_term: ' in line:
                cls, rest = line.split('_records_per_term: ', 1)
                numbers = list(map(value, re.findall(r'\b\d+(?:\.\d+)?\b', rest)))
                query['classes'].setdefault(cls, {}).update(dict(zip(
                    ['records_median', 'records_p90', 'records_max', 'attempts_median',
                     'attempts_p90', 'read_bytes_median', 'read_bytes_p90'], numbers)))
        data['queries'][state] = query
    result[arm] = data
(args.output_dir / 'results.json').write_text(json.dumps(result, indent=2) + '\n')
for arm, data in result.items():
    print(arm, 'states', {s: data['build'].get(p + '_total_bytes') for s, p in
                         [('first', 'built'), ('steady', 'steady'), ('full', 'full')]},
          'churn', data['churn'].get('final_index_bytes'))


def write_csv(name, rows):
    columns = list(dict.fromkeys(key for row in rows for key in row))
    with (args.output_dir / name).open('w', newline='') as output:
        writer = csv.DictWriter(output, fieldnames=columns)
        writer.writeheader()
        writer.writerows(rows)


write_csv('lifecycle.csv', [dict(arm=arm, phase=phase, **data[phase])
                          for arm, data in result.items()
                          for phase in ('build', 'churn') if data[phase]])
write_csv('queries.csv', [dict(arm=arm, state=state, query_class=cls, **row)
                         for arm, data in result.items()
                         for state, query in data['queries'].items()
                         for cls, row in query['classes'].items()])
write_csv('latency-sizes.csv', [dict(arm=arm, state=state, **row)
                              for arm, data in result.items()
                              for state, query in data['queries'].items()
                              for row in query['sizes']])

write_csv('churn-records.csv', [dict(arm=arm, **row)
                               for arm, data in result.items()
                               for row in data['churn_queries']])
