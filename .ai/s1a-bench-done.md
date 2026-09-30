# S1a bench: done-note

Branch `wt/s1a-bench`, measured at the binary built from commit `9fb11ff` (main
`ccd1dab` plus the dump/synthetic changes; the catalog format is main's).
Nothing pushed.

## What changed

- `crates/ferret-crawl/examples/dump.rs`: `--stat` (anywhere on the line) adds
  the `lstat` fields after each decision, tab-separated, preceded by a
  `<tab>columns<tab>size<tab>mtime_sec...` header line and a
  `<tab>root<tab>...` line with the walk root's stat. Columns are read by name,
  so `nlink` later is one more name at the end of `COLUMNS` in `dump.rs` and one
  `Column` variant in `synthetic.rs` (unknown names are ignored today). Default
  output is unchanged: that is the differential check and mtimes would make it
  noisy.
- `crates/ferret-catalog/examples/synthetic.rs`: reads those columns. A dump
  without them still works (old defaults, sequential inodes). `rerun` and
  `faults=N` are untouched.
- `crates/ferret-bench/src/main.rs`: `ferret-bench sections <dir>`, exact bytes
  and B/name per section.
- `crates/ferret-bench/scripts/findbench.py`: the harness. Its docstring holds
  the exact commands for the before/after table.

## How the synthetic data is realistic now

Verified: the old `synthetic.rs` gave every entry size 100, mtime = ctime =
1_700_000_000 s / 0 ns, uid 1000, gid 100, dev 1, mode 0o644/0o755/0o777, and
sequential inodes (`base + n`). Every column packed to zero bits, so a
frame-of-reference format would have looked far better than it is.

Now each entry carries its real size, mtime/ctime with nanoseconds, mode, uid,
gid, dev and inode number from the walker's `lstat`, directories included (the
root's stat, for each `pN` top, is the walk root's). Inode numbers: copy `c`
uses `real_ino + c * stride`, `stride` = the dump's largest inode number + 1.
Why: the values keep the scatter of a real ext4 (this `$HOME`'s span about
2^27, so ~27+ bits at full width, the worst case for delta packing, as on a
real disk), every copy is disjoint from the others, and `(dev, ino)` stays
unique. The `/synthetic` root takes the stride after the last copy. Copies still
share sizes and times, since they are copies; that is the honest limit (the
values' distribution is real, their per-copy repetition is not).

Not fixed, and you may want to know: every regular file in the dump is
`Index`, and every copy gets fresh hashes, so the synthetic has 8.34M docs for
10.2M names (Docs 16.3 B/name) where `$HOME` has 109k docs for 445k names (4.9
B/name); the dump has no `Binary`/dedupe. That flatters nothing in Inodes but
makes Docs a worst case.

## Baseline

Machine: 32 threads, NVMe ext4, 60 GB. During all timing runs no `cargo` or
`rustc` was running (`pgrep -x`, and `--wait-quiet` in the harness). Not idle:
two agents (grok, codex) were alive, load average 3.3-4.6, from another
session's work; the first `$HOME` harness run saw a one-off 200 ms evicted time
for the first two queries (raw log kept, rerun 22 ms is what is tabled). Treat
evicted numbers as +/- 20%. `$HOME` grew since S1: 445,194 names now, not
441k (77,927 dirs, 365,797 files, 1,471 symlinks); everything else in the
census matches S1's (same default ignore file, seeded by `ferret` into the
isolated config: byte-identical to the compiled-in default).

`$HOME` index, this binary: first run 55.6 s wall, 78 MB peak RSS (page cache
partly cold, 9.8 GiB read, other agents active; S1's 0.83 s was fully warm,
29.1 s cold, so not comparable); re-run 0.50 s wall, 161 MB peak RSS. The 10M
build (`synthetic`, 23 copies): 9.4 s, 1,748 MB peak RSS (S1: 1.66 GB), catalog
1,202,499,606 B.

### Bytes per name

| | names | file B | B/name |
|---|---:|---:|---:|
| `$HOME` | 445,194 | 47,292,435 | 106.23 |
| synthetic 10M | 10,239,255 | 1,202,499,606 | 117.44 |

S1 said 106 and 118: unchanged, as it should be, since every column is fixed
width today. The real values only matter once packed.

`$HOME`:

| section | bytes | B/name |
|---|---:|---:|
| Names | 5342328 | 12.00 |
| NameHeap | 10742948 | 24.13 |
| DirNames | 311708 | 0.70 |
| Traversed | 9741 | 0.02 |
| Roots | 8 | 0.00 |
| Strings | 67755 | 0.15 |
| Inodes | 28492480 | 64.00 |
| States | 111299 | 0.25 |
| Links | 11768 | 0.03 |
| WorkTrees | 14560 | 0.03 |
| Docs | 2187640 | 4.91 |
| header, table and padding | 200 | 0.00 |
| **file** | 47292435 | 106.23 |

Synthetic 10M:

| section | bytes | B/name |
|---|---:|---:|
| Names | 122871060 | 12.00 |
| NameHeap | 247083700 | 24.13 |
| DirNames | 7168828 | 0.70 |
| Traversed | 224026 | 0.02 |
| Roots | 8 | 0.00 |
| Strings | 236842 | 0.02 |
| Inodes | 655312384 | 64.00 |
| States | 2559814 | 0.25 |
| Links | 270664 | 0.03 |
| WorkTrees | 0 | 0.00 |
| Docs | 166772080 | 16.29 |
| header, table and padding | 200 | 0.00 |
| **file** | 1202499606 | 117.44 |

### Open and load (`ferret-bench open`; "every section" calls `load_all`)

`$HOME`:

| open | cache | ms | bytes read |
|---|---|---:|---:|
| header and table | warm | 0.00 | 200 |
| header and table | evicted | 0.24 | 200 |
| name sections (a name query's load) | warm | 5.04 | 16486456 |
| name sections (a name query's load) | evicted | 18.23 | 16486456 |
| every section | warm | 9.07 | 47292435 |
| every section | evicted | 42.42 | 47292435 |
| 16486456 B into a fresh buffer | warm | 0.79 | 16486456 |
| 16486456 B into a resident buffer | warm | 0.72 | 16486456 |

Synthetic 10M:

| open | cache | ms | bytes read |
|---|---|---:|---:|
| header and table | warm | 0.00 | 200 |
| header and table | evicted | 0.16 | 200 |
| name sections (a name query's load) | warm | 207.20 | 377855328 |
| name sections (a name query's load) | evicted | 333.04 | 377855328 |
| every section | warm | 471.86 | 1202499606 |
| every section | evicted | 840.19 | 1202499606 |
| 377855328 B into a fresh buffer | warm | 128.21 | 377855328 |
| 377855328 B into a resident buffer | warm | 28.19 | 377855328 |

### `find`, `$HOME`

| query | strategy | rows | evicted first / wall | fresh wall | in-proc first / total | max RSS | bytes read | single reads |
| --- | --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| `case:Flamegraph` | HeapScan | 4 | 19.8 / 22.2 | 13.8 | 11.3 / 12.2 | 19 MB | 16.6 MB | 4 |
| `flamegraph` | HeapScan | 5 | 19.5 / 22.6 | 12.3 | 10.0 / 11.1 | 19 MB | 16.6 MB | 5 |
| `test` | HeapScan | 7003 | 20.3 / 110.0 | 30.5 | 9.9 / 28.8 | 46 MB | 45.5 MB | 6956 |
| `ext:jpg` | HeapScan | 15590 | 21.3 / 52.0 | 27.5 | 9.8 / 26.1 | 46 MB | 45.5 MB | 6956 |
| `src/**/*.rs` | HeapScan | 20472 | 22.2 / 59.9 | 33.4 | 10.6 / 32.1 | 47 MB | 45.5 MB | 6956 |
| `re:^test_.*\.py$` | HeapScan | 901 | 20.9 / 26.5 | 14.2 | 10.8 / 13.0 | 19 MB | 16.7 MB | 901 |
| `re:^[0-9a-f]{8}$` | AllNames | 1395 | 23.0 / 38.4 | 25.8 | 11.9 / 24.7 | 19 MB | 16.7 MB | 1395 |
| `mtime:<1d` | InodeScan | 5947 | 45.1 / 50.6 | 31.0 | 25.0 / 29.3 | 46 MB | 45.1 MB | 0 |
| `size:>100M` | InodeScan | 870 | 44.9 / 50.4 | 29.4 | 24.9 / 27.8 | 46 MB | 45.1 MB | 0 |
| `ext:rs size:>10k` | HeapScan | 5849 | 21.2 / 56.2 | 29.4 | 10.5 / 28.0 | 46 MB | 45.5 MB | 6956 |
| `*` | AllNames | 445194 | 21.7 / 131.0 | 110.0 | 10.6 / 108.1 | 46 MB | 45.5 MB | 6956 |

### `find`, synthetic 10M

| query | strategy | rows | evicted first / wall | fresh wall | in-proc first / total | max RSS | bytes read | single reads |
| --- | --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| `case:Flamegraph` | HeapScan | 92 | 343.9 / 373.5 | 242.6 | 216.6 / 240.8 | 365 MB | 380.4 MB | 92 |
| `flamegraph` | HeapScan | 115 | 337.9 / 379.5 | 242.9 | 213.6 / 241.4 | 365 MB | 380.4 MB | 115 |
| `test` | HeapScan | 161069 | 339.5 / 2324.2 | 643.4 | 204.4 / 642.1 | 990 MB | 1046.0 MB | 159988 |
| `ext:jpg` | HeapScan | 358570 | 335.5 / 930.9 | 580.2 | 206.3 / 578.0 | 990 MB | 1046.0 MB | 159988 |
| `src/**/*.rs` | HeapScan | 470856 | 342.3 / 1447.7 | 713.6 | 207.8 / 711.6 | 991 MB | 1046.0 MB | 159988 |
| `re:^test_.*\.py$` | HeapScan | 20723 | 335.6 / 484.9 | 274.1 | 209.2 / 272.3 | 366 MB | 381.7 MB | 20723 |
| `re:^[0-9a-f]{8}$` | AllNames | 32085 | 359.1 / 675.8 | 515.2 | 233.4 / 513.6 | 366 MB | 382.5 MB | 32085 |
| `mtime:<1d` | InodeScan | 136436 | 836.4 / 938.8 | 627.1 | 525.2 / 625.6 | 992 MB | 1035.7 MB | 0 |
| `size:>100M` | InodeScan | 20010 | 834.5 / 923.8 | 612.0 | 528.2 / 610.8 | 991 MB | 1035.7 MB | 0 |
| `ext:rs size:>10k` | HeapScan | 134504 | 332.7 / 1367.7 | 607.0 | 218.1 / 605.0 | 990 MB | 1046.0 MB | 159988 |
| `*` | AllNames | 10239255 | 342.5 / 2825.2 | 2487.8 | 216.6 / 2486.5 | 991 MB | 1046.0 MB | 159988 |

Max RSS is `ru_maxrss` from `wait4`; a bare `ferret --version` on the same path
reads 15 MB, so the floor is Python's image, not ferret's. D48's 1 GB line:
the 10M `find` peaks at 990 MB (name-heap queries), 365 MB for the
`HeapScan`-on-a-rare-needle queries, 991 MB for `InodeScan` and `*`.

## Where things are

Scratch: `/tmp/claude-1000/-home-dave-w-super-ferret/a6349507-0055-4177-afda-ffdda5ef455e/scratchpad/bench/`

- dump: `home-dump-stat.tsv` (made with `dump --stat $HOME` at about 12:05 on
  2026-09-30; 447k lines)
- `cat-home/` ($HOME catalog), `cat-10m/` (23 copies), `cat-1/` (one copy)
- `xdg/` (isolated XDG dirs, including the query log)
- `raw/`: `find-home-baseline.{md,log}` (run 1, the 200 ms outlier),
  `find-home-baseline-2.{md,log}` (tabled), `find-10m-baseline.{md,log}`,
  `home-index-first.txt`, `home-index-rerun.txt`, `home-stats.txt`,
  `10m-build.txt`, `10m-stats.txt`

## Commands

Exactly the ones in the docstring of `findbench.py`; the ones I ran:

```sh
env -u FERRET_INDEX XDG_CONFIG_HOME=$S/xdg/config XDG_DATA_HOME=$S/xdg/data \
  XDG_STATE_HOME=$S/xdg/state XDG_CACHE_HOME=$S/xdg/cache \
  target/release/ferret --index $S/cat-home index $HOME
target/release/examples/dump --stat $HOME > $S/home-dump-stat.tsv
target/release/examples/synthetic $S/home-dump-stat.tsv $S/cat-10m 23
crates/ferret-bench/scripts/findbench.py --bin target/release/ferret \
  --bench target/release/ferret-bench --index $S/cat-{home,10m} --xdg $S/xdg \
  --label ... --out ... --wait-quiet
```

## Divergences from the prompt

- All checkable claims held: fixed size 100, one mtime (also ctime, uid, gid,
  dev, mode, sequential ino). `ferret-bench open` already calls `load_all`
  (its third case, "every section"), so no new mode was needed.
- The prompt's 441k names: now 445,194 (the tree grew, plus my own scratch
  files under `$HOME`? No: scratch is under /tmp). Not a policy difference.
- The 10M is 23 copies (S1's figure; 22 copies would be 9.8M).
- `ru_maxrss` has a floor from the launching interpreter (15 MB), see above.
- If the sibling's `nlink` lands in `ferret_catalog::Stat`, the `Stat { .. }`
  literal in `plain_stat` needs the field; `moved` uses `..*stat` and is fine.

## Decisions I made

- `--stat` is opt-in on `dump`, to keep the differential output stable.
- Kept the doc-per-file worst case rather than modelling dedupe (see above).
- Did not measure a cold (evicted) `$HOME` index: it reads 10 GB and other
  agents share the disk; the first run above was partly cold.
