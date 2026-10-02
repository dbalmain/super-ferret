# Catalog facts for find: milestone 4a measurement

Measured 2026-10-02 on this 32-CPU machine. One benchmark at a time; no
compilers were running during the paired query series. A host process-name
check saw only the active `ferret-bench`. Load averages are the recorded
1/5/15-minute values. Sizes are exact bytes; MiB means 2²⁰ bytes.

The fixed `dump --stat /home/dave` fixture was replicated under 23 prefixes by
the S1a synthetic driver. Baseline: 10,405,729 names, 10,405,730 inodes and
8,495,924 documents. Both new encodings: 10,448,739 names, with **exactly the
same inode/document counts**. The 43,010 additional names are ignored files or
opaque directory markers, 0.41% of the new name count. The fixture has no
visible special files; their stat/kind behavior is covered by real-crawler
FIFO/socket tests and catalog tests for all seven types.

The original dump omitted type data for stat-free Skip events. For this fixed
fixture, only those 1,870 rows were supplemented by lstat, without reading
content. The updated dump example now writes only d_type's mode bits and zeros
for its other fields for such rows; the synthetic driver discards those fields
and emits `Batch::ignored`. Traverse rows now mint traversed directory tokens,
and the synthetic root receives its raw child count.

## Encoding comparison

A: the specified `u32::MAX - 1` through `-7` tags in the existing blocked child
column, with `-8` reserved for a future tombstone. B: seven tags immediately
following the actual inode count; public readers translate those tags back to
A's target representation. B reduces mixed-block widths but needs a decode rule
based on the generation's count. Both use the same real writer, batches and
fixture. The B patch was applied only for the experiment, then restored.

**Keep A.** B saves 1,176,336 B, 0.20% of the whole snapshot, with similar build
and query times. A's fixed reserved tags also survive adding stable inode ids
in the future incremental format; B's tags would move when the count changes.
Neither a nullable/type side column nor a separate ignored-name-id range was
implemented or measured. A more ignored-heavy workload or a substantial
measured query improvement would reopen the choice.

| build | snapshot B | wall s | peak RSS MiB | fill / commit ms | load at start |
|---|---:|---:|---:|---:|---|
| v2 baseline | 592,577,217 | 10.12 | 1630.4 | 2033 / 7750 | 1.36, 2.11, 3.13 |
| v3 high sentinels | 594,837,226 | 10.71 | 1640.7 | 2179 / 8180 | 7.86, 4.22, 3.22 |
| v3 adjacent tags (trial) | 593,660,890 | 10.69 | 1639.3 | 2189 / 8146 | 9.98, 7.00, 4.56 |

The baseline source is `213cc31` (code unchanged from wt/find); the sentinel
implementation is `974865a` plus the final single-comparison search guard and
documentation corrections. `/usr/bin/time -v` measured whole-process wall/RSS;
the synthetic driver reports fill and commit separately and resets VmHWM
before the commit. The higher later loads prevent treating small wall-time
changes as an isolated causal effect.

## Snapshot sections

These are `ferret-bench sections` outputs. Section inspection loads only the
header, so its load affects no byte count. Recorded section-run loads:
baseline 1.12/1.81/2.88; sentinel 7.76/4.72/3.44; adjacent 9.45/6.98/4.58.

| section | baseline B | high sentinel B | adjacent B |
|---|---:|---:|---:|
| Names | 42,561,884 | 44,348,882 | 43,172,546 |
| NameHeap | 251,635,538 | 252,108,533 | 252,108,533 |
| DirNames | 5,402,849 | 5,402,849 | 5,402,849 |
| Entries | 1,040,735 | 1,040,735 | 1,040,735 |
| Traversed | 225,119 | 225,119 | 225,119 |
| Roots | 8 | 8 | 8 |
| Strings | 238,130 | 238,130 | 238,130 |
| Dev | 2,601,473 | 2,601,473 | 2,601,473 |
| Ino | 19,022,579 | 19,022,579 | 19,022,579 |
| Size | 19,181,044 | 19,181,044 | 19,181,044 |
| Mtime | 15,963,579 | 15,963,579 | 15,963,579 |
| MtimeNs | 28,287,944 | 28,287,944 | 28,287,944 |
| Ctime | 12,993,698 | 12,993,698 | 12,993,698 |
| CtimeNs | 36,199,331 | 36,199,331 | 36,199,331 |
| Mode | 6,503,758 | 6,503,758 | 6,503,758 |
| Owner | 1,300,741 | 1,300,741 | 1,300,741 |
| Nlink | 1,790,376 | 1,790,376 | 1,790,376 |
| Doc | 8,819,406 | 8,819,406 | 8,819,406 |
| States | 2,601,433 | 2,601,433 | 2,601,433 |
| Links | 272,136 | 272,136 | 272,136 |
| WorkTrees | 0 | 0 | 0 |
| Docs | 135,934,792 | 135,934,792 | 135,934,792 |
| header, table and padding | 664 | 680 | 680 |
| **file** | 592,577,217 | 594,837,226 | 593,660,890 |
| Specials | 0 | 0 | 0 |

High tags add 1,786,998 B to Names and 472,995 B to the name heap; the additional
section-table slot adds 16 B. All inode/stat/document sections keep their exact
baseline sizes. The sparse Specials body is zero here, and costs 8 B per visible
FIFO/socket/device in trees containing them. The new format is v3, with a
680-byte header/table/descriptors, against v2's 664.

## Name search, before and after (D43)

The final series alternates baseline, sentinel and adjacent for each query.
`ferret-bench query` reports warm medians of 7 after warmup and evicted medians
of 3 (fadvise DONTNEED). Times include opening, validating/loading needed
sections, matching and resolving result paths. First is time to the first row;
all is time to finish consuming rows. Every row count is identical across the
three encodings. Loads below are start → finish for each invocation.

| query | encoding | rows | warm first / all ms | evicted first / all ms | MB read | load start → finish |
|---|---|---:|---:|---:|---:|---|
| `flamegraph` | before | 115 | 247.31 / 253.22 | 340.92 / 346.90 | 300.3 | 5.44, 6.51, 5.30 → 5.64, 6.54, 5.32 |
| `flamegraph` | sentinel | 115 | 266.10 / 272.01 | 373.10 / 379.05 | 302.6 | 5.64, 6.54, 5.32 → 5.51, 6.49, 5.31 |
| `flamegraph` | adjacent | 115 | 263.04 / 269.38 | 305.85 / 312.68 | 301.4 | 5.51, 6.49, 5.31 → 5.51, 6.49, 5.31 |
| `test` | before | 162,219 | 245.03 / 307.96 | 354.61 / 414.66 | 300.3 | 5.51, 6.49, 5.31 → 5.47, 6.47, 5.31 |
| `test` | sentinel | 162,219 | 249.96 / 312.40 | 374.80 / 435.92 | 302.6 | 5.47, 6.47, 5.31 → 5.99, 6.56, 5.35 |
| `test` | adjacent | 162,219 | 247.12 / 310.58 | 300.77 / 362.04 | 301.4 | 5.99, 6.56, 5.35 → 5.99, 6.55, 5.35 |
| `ext:jpg` | before | 358,570 | 247.83 / 290.03 | 340.13 / 380.06 | 300.3 | 5.99, 6.55, 5.35 → 5.99, 6.55, 5.35 |
| `ext:jpg` | sentinel | 358,570 | 249.46 / 290.03 | 356.28 / 399.86 | 302.6 | 5.99, 6.55, 5.35 → 5.83, 6.51, 5.34 |
| `ext:jpg` | adjacent | 358,570 | 245.82 / 287.27 | 287.94 / 329.17 | 301.4 | 5.83, 6.51, 5.34 → 5.69, 6.47, 5.33 |
| `*` | before | 10,405,729 | 244.92 / 1339.57 | 331.54 / 1426.15 | 300.3 | 5.69, 6.47, 5.33 → 5.01, 6.28, 5.29 |
| `*` | sentinel | 10,405,729 | 258.21 / 1369.06 | 632.18 / 1739.13 | 302.6 | 5.01, 6.28, 5.29 → 4.41, 6.09, 5.24 |
| `*` | adjacent | 10,405,729 | 248.66 / 1354.00 | 289.79 / 1411.96 | 301.4 | 4.41, 6.09, 5.24 → 3.97, 5.90, 5.20 |

The warm full listing is 2.2% slower with high sentinels (1,339.57 → 1,369.06 ms).
Warm `test` is 1.4% slower; `ext:jpg` is equal. Rare `flamegraph` is 7.4% slower
in total, with its time after the first row unchanged at 5.91 ms; section loading
accounts for that run's difference. Name sections grow from 300.3 to 302.6 MB.
The evicted full-listing run is an outlier (first row 632 ms), so it is recorded
rather than used to argue a storage/scanner effect.

An early implementation obtained every kind from the mode dictionary. It
loaded/validated another 6.5 MB and regressed the rare query by about 40 ms
(262 → 300 ms in the initial series). It was replaced with the sparse Specials
table: plain name search again reads no inode columns. Ignored-row suppression
is one comparison against inode_count before any stat access; a metadata-first
scan checks the same bound before indexing its pass bitset.

## Reproduction and artifacts

All repo work stayed in `/home/dave/w/super-ferret-wt/find-m4a`. The existing S1a
commands are in `crates/ferret-bench/scripts/findbench.py` and the synthetic
example's module header. The exact fixed fixture and saved baseline/trial
binaries are under `/tmp/find-m4a-measure`; the fixture is not committed because
it contains private HOME paths. Fixture SHA-256: `784a7abfb3bda8717250e638793f99de1d73ee19580f910c2c2708ee9894d075`.

```sh
/home/dave/w/super-ferret-wt/find-m4a/target/release/examples/synthetic \
  /tmp/find-m4a-measure/home.tsv /tmp/find-m4a-measure/new-catalog 23
/home/dave/w/super-ferret-wt/find-m4a/target/release/ferret-bench sections \
  /tmp/find-m4a-measure/new-catalog
/home/dave/w/super-ferret-wt/find-m4a/target/release/ferret-bench query \
  /tmp/find-m4a-measure/new-catalog flamegraph test ext:jpg '*'
```

The experiment patch and raw aggregate logs are referenced by the external
find-m4a done-note. No snapshot, HOME dump or protected writer path is committed.
