#!/usr/bin/env python3
"""Per-band A vs B aggregates from two `ferret-bench blob-screen run` TSVs.

Usage: compare.py a.tsv b.tsv. Prints Markdown; reads no argument bytes.
"""
import csv
import sys


def load(path):
    with open(path) as f:
        return list(csv.DictReader(f, delimiter="\t"))


def q(values, p):
    # Nearest rank, as ferret-bench's quantile.
    v = sorted(values)
    return v[round((len(v) - 1) * p)] if v else 0


def trio(rows, key, scale=1.0, fmt="{:.0f}"):
    v = [float(r[key]) / scale for r in rows]
    return "/".join(fmt.format(q(v, p)) for p in (0.5, 0.9, 0.99))


a, b = load(sys.argv[1]), load(sys.argv[2])
assert len(a) == len(b) and all(x["band"] == y["band"] and x["len"] == y["len"] for x, y in zip(a, b))
bands = []
for r in a:
    if r["band"] not in bands:
        bands.append(r["band"])

print("| band | arm | n | screen keys p50 | screen empty | refused | own doc missing | candidates p50/p90/p99 | verified MiB p50/p90/p99 | rows p50/p90/p99 | warm ms p50/p90/p99 |")
print("| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |")
for band in bands:
    for arm, rows in (("A", a), ("B", b)):
        rs = [r for r in rows if r["band"] == band]
        print(
            f"| {band} | {arm} | {len(rs)} | {q([int(r['screen_keys']) for r in rs], 0.5)} "
            f"| {sum(r['screen_all'] == '1' and r['routed'] == '1' for r in rs)} "
            f"| {sum(r['refused'] == '1' for r in rs)} "
            f"| {sum(r['own_doc'] == '0' for r in rs)} "
            f"| {trio(rs, 'candidates')} | {trio(rs, 'verified_bytes', 1048576, '{:.0f}')} "
            f"| {trio(rs, 'rows')} | {trio(rs, 'warm_ms', fmt='{:.0f}')} |"
        )

print()
print("| band | p90 candidates B/A | p90 verified bytes B/A | median per-query candidates B/A | queries B rows < A rows | B rows > A rows |")
print("| --- | --- | --- | --- | --- | --- |")
for band in bands:
    pairs = [(x, y) for x, y in zip(a, b) if x["band"] == band]
    if band == "df1":
        continue
    ca = [int(x["candidates"]) for x, _ in pairs]
    cb = [int(y["candidates"]) for _, y in pairs]
    va = [int(x["verified_bytes"]) for x, _ in pairs]
    vb = [int(y["verified_bytes"]) for _, y in pairs]
    ratio = [int(y["candidates"]) / max(1, int(x["candidates"])) for x, y in pairs]
    fewer = sum(int(y["rows"]) < int(x["rows"]) for x, y in pairs)
    more = sum(int(y["rows"]) > int(x["rows"]) for x, y in pairs)
    print(
        f"| {band} | {q(cb, 0.9) / q(ca, 0.9):.2f} | {q(vb, 0.9) / max(1, q(va, 0.9)):.2f} "
        f"| {q(ratio, 0.5):.2f} | {fewer} | {more} |"
    )
