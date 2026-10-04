# Roadmap

The order of work. Each slice ships something usable and ends with a measurement
that can change the next slice. Design detail is in [DESIGN.md](DESIGN.md),
decisions in [DECISIONS.md](DECISIONS.md).

Order follows D3: a usable tool first, so query and usage data accumulate while
the index is built; then the index; then the agent skill. Since D46 the daemon
comes with S1b, before the content index; extraction plugins, TUI and GUI much
later.

## Done before this roadmap

- **Research** (2026-09-04/05): landscape, index structures, architecture, and
  the M1 baseline on `~/w` — [docs/research/](research/).
- **intpack** (2026-09): the posting-list codec crate, at or ahead of Lucene's
  numbers on the corpora —
  [github.com/dbalmain/intpack](https://github.com/dbalmain/intpack), bench
  harness
  [github.com/dbalmain/intpack-bench](https://github.com/dbalmain/intpack-bench),
  results and decisions in [docs/intpack/](intpack/).

## S0 — Skeleton (done 2026-09-23)

Workspace, licence (`MIT OR Apache-2.0`), lints, gates (fmt, clippy
`-D warnings`, test), crate stubs with their boundaries documented, and the
toolchain-ledger pattern from intpack ready for the first compiler-steering
item.

**Done when:** the gates run green on an empty workspace. Landed with a test
that holds every crate's dependencies to the graph in DESIGN.md, and a working
guide for agents in [CLAUDE.md](../CLAUDE.md). The toolchain ledger is a rule in
that guide, created by the first item that needs it rather than empty now.

## S1 — Find, faster (first usable) (done 2026-09-28)

`ferret-policy`, `ferret-crawl`, `ferret-catalog`, and the CLI:

- `ferret index [roots]`: crawl, apply ignore rules (D13), write the catalog.
  Every run rewrites the whole snapshot, kept roots included (D26 A′); a file
  whose `(dev, ino, size, mtime, ctime)` is unchanged keeps its hash and doc id
  without being re-read. Hashing and doc ids are assigned here (D4) so S2 starts
  from a populated catalog.
- `ferret search`: name substring, glob and regex by scanning the name heap (D14);
  metadata predicates (`ext:`, `size:`, `mtime:`, `type:`, `path:`). Output per
  path; JSON lines behind a flag.
- `ferret stats`: file and byte census by extension, size histogram, directory
  count, duplicate content — M1 rerun over `$HOME` for free.
- Local query and timing log.

**Measure:** catalog bytes and RSS per file; crawl time cold and warm; name
query latency cold and warm; the `$HOME` census. **Decides:** the memory budget
default (D5), and whether the document tier is big enough to move extraction
earlier.

**Measured** (slice 5b, 2026-09-28, the release `ferret` binary, one run at a
time; 32 threads, NVMe, ext4). "Cold" is the page cache emptied per file with
`posix_fadvise(DONTNEED)`, checked with `fincore`. Without root, the dentry and
inode caches stay warm, so cold crawls understate a reboot. Every `find` time is
measured against the **D38 B reader** (positional section reads); D43 (mmap) is
open.

The `$HOME` census:

- 2.11M entries. The default global rules prune 1.28M of them (`target/` 30%,
  `.cache/` 18%, `node_modules/` 7%, `.git/` 3%). Work-tree `.gitignore` rules
  prune another 0.39M.
- 441k are catalogued: 77k directories, 362k files and 1.5k symlinks, at a
  median depth of 8 and a median name length of 20 B.
- No indexed file has a second indexed name. `$HOME` holds 330k files with a
  link count above 1, so their other names lie outside the index. One was
  checked: its other name is under an ignored `node_modules/`. The rest were not
  traced and may sit in other ignored trees (`.cache/`, package stores).
- 109k documents, 19.5k of them held by more than one inode.
- The document tier is small: 1,222 PDFs, 134 `.docx`, 74 `.xlsx`, 10 `.pptx`
  and 4 `.epub`.

| measure                                 | `$HOME` (441k names)                              | synthetic 10M                      |
| --------------------------------------- | ------------------------------------------------- | ---------------------------------- |
| catalog bytes                           | 46.9 MB, 106 B/name                               | 1.18 GB, 118 B/name                |
| `index` peak RSS                        | 78 MB first run, 160 MB re-run (180 / 360 B/name) | 1.66 GB (the `synthetic` build)    |
| `index`, first run                      | 0.83 s warm, 29.1 s cold (10.3 GB read)           | —                                  |
| `index`, re-run                         | 0.45 s warm, 0.74 s cold                          | —                                  |
| content-fault pass                      | 2 ms                                              | 52 ms with none, 289 ms with 8,142 |
| `find flamegraph`: cold / fresh process | 15 / 12 ms                                        | 390 / 233 ms                       |
| `find test`, all rows: cold / fresh     | 62 / 29 ms                                        | 2,307 / 634 ms                     |
| `find size:>100M`: cold / fresh         | 36 / 27 ms                                        | 592 / 367 ms                       |
| `find` peak RSS                         | 19–46 MB                                          | 358–970 MB                         |

## Re-plan after S1 (2026-09-30)

D46–D49 reorder what follows S1: the daemon is the mode of operation (D46), with
one engine hosted by `ferretd` or by an in-process batch run; `ferret find`
takes find(1) syntax (D47); compaction comes first (D48). The order below runs
through complete `find` support before S2.

## S1a — Catalog compaction

Bit-packed fixed-width columns sized from the catalog, per-catalog dictionaries
for dev, mode and (uid, gid), then names (D43, D48). The row gains nlink and
each directory's raw entry count, which `find`'s `-links` and `-empty` need
(D47).

**Measure:** bytes per name, open time and query time on `$HOME` and synthetic
10M, against S1's table.

**Measured** (2026-09-30, the release `ferret` at the S1a branch, the same
method as the baseline: `crates/ferret-bench/scripts/findbench.py`, isolated XDG
dirs, one run at a time, 32 threads, NVMe, ext4; the medians are of 3 evicted
and 5 warm runs). The baseline is the same harness on the version-1 catalog.
Machine: load average 3-5 (another session's agents alive, none compiling), so
read the times as plus or minus 20%; the byte counts are exact. `$HOME` has
drifted: 445,194 names then, 445,442 now (the 10M is built from a fresh dump,
10,245,189 names against 10,239,255), so compare B/name and ratios. The "after"
`find`, `open` and build figures are from the perf fixes that followed the first
after-measurement (below the tables), re-run on the same two catalogs at load
average 2.1-4.3 with nothing compiling; the catalogs are byte-identical to what
the fixed build writes. Evicted times swing most (`test` at 10M evicted read
1,216 ms and 788 ms in two runs of one binary). The `index` rows are the first
after-measurement's and were not re-run. The synthetic now carries the walker's
real `nlink` and, for each directory, the count of its children in the dump (a
lower bound on the raw `getdents` count, since the dump omits what the walker
never lists); every copy shares those values, which is what makes the packed
widths a fair worst case only for inodes and dev, not for sizes and times.

Units: RSS is in MiB throughout this section. `findbench.py` reports
`ru_maxrss` / 1024, so its figures were MiB all along and only the label
changed; the `synthetic` and `time` figures are KiB, first written as kB / 1000
"MB" and now converted from the recorded KiB. The `index` rows' raw KiB were not
kept and are converted from the MB as written (plus or minus 1 MiB). Catalog
and section bytes, and bytes read, stay decimal MB unless marked MiB.

| measure                                      | before `$HOME`             | after `$HOME`                  | before 10M        | after 10M         |
| -------------------------------------------- | -------------------------- | ------------------------------ | ----------------- | ----------------- |
| catalog bytes                                | 47.3 MB, 106.2 B/n         | 28.4 MB, 63.7 B/n              | 1,202 MB, 117.4   | 799 MB, 78.0      |
| Names (offsets + lengths)                    | 12.00 B/n                  | 7.50                           | 12.00             | 9.13              |
| NameHeap                                     | 24.13                      | 24.13                          | 24.13             | 24.13             |
| DirNames                                     | 0.70                       | 0.42                           | 0.70              | 0.53              |
| Entries (new: raw directory counts)          | —                          | 0.26                           | —                 | 0.26              |
| inode columns (was `Inodes`, fixed 64 B)     | 64.00                      | 26.00                          | 64.00             | 27.38             |
| States, Traversed, Strings, Links, WorkTrees | 0.48                       | 0.48                           | 0.32              | 0.32              |
| Docs                                         | 4.91                       | 4.91                           | 16.29             | 16.29             |
| `ferret-bench open`, name sections, warm     | 5.0 ms (16.5 MB)           | 7.1 ms (14.4 MB)               | 207 ms (378 MB)   | 254 ms (347 MB)   |
| same, evicted                                | 18.2 ms                    | 20.0 ms                        | 333 ms            | 371 ms            |
| `load_all`, warm / evicted                   | 9.1 / 42.4 ms              | 9.7 / 37.7 ms                  | 472 / 840 ms      | 439 / 784 ms      |
| `index`, first run / re-run                  | 55.6 s (cold-ish) / 0.50 s | 43.1 s (cold-ish) / 0.87 s (a) | —                 | —                 |
| `index` peak RSS, first / re-run             | 76 / 157 MiB               | 82 / 98 MiB                    | —                 | —                 |
| `synthetic` build, time / peak RSS           | —                          | —                              | 9.4 s / 1,707 MiB | 9.7 s / 1,836 MiB |
| same, CPU (user + sys), same sitting         | —                          | —                              | 7.3 s             | 9.5 s             |

(a) Measured before the build fix; not re-measured on the final code, as the
machine was not quiet (load 5, another session's find-compat harness running).

Per name the inode row went from 64 B to 26 B on `$HOME` and 27.4 B on the
synthetic (nlink and the entry counts are new and included). Widths in the 10M
catalog: ino 30 bits, size 35, mtime 31, mtime nanoseconds 30, ctime 24, ctime
nanoseconds 30, nlink 10, doc 23, dev 0 (one value), mode 5 (20 values), owner 1
(two values). The nanosecond columns are 7.6 B of the 27.4 and cannot pack: they
are entropy. `nlink` costs 10 bits on every name for one outlier directory (an
entry near 1,000); a frame-of-reference or exception scheme would take it to
about 1 bit. Ino at 30 bits is the synthetic's copy stride (`$HOME` alone is 25
bits, 3.1 B).

| `find` (fresh process, ms) | before `$HOME` | after `$HOME` |   before 10M |   after 10M |
| -------------------------- | -------------: | ------------: | -----------: | ----------: |
| `flamegraph` fresh         |           12.3 |          14.2 |          243 |         295 |
| `flamegraph` evicted       |           22.6 |          23.9 |          380 |         406 |
| `test` fresh               |           30.5 |          15.6 |          643 |         362 |
| `test` evicted             |          110.0 |          26.1 |        2,324 |         492 |
| `size:>100M` fresh         |           29.4 |          21.2 |          612 |         453 |
| `size:>100M` evicted       |           50.4 |          34.0 |          924 |         613 |
| `*` fresh                  |          110.0 |          97.2 |        2,488 |       2,315 |
| `*` evicted                |          131.0 |         105.1 |        2,825 |       2,417 |
| peak RSS, name queries     |      19-46 MiB |        17 MiB |  365-991 MiB | 333-334 MiB |
| bytes read, name queries   |   16.6-45.5 MB |       14.4 MB | 380-1,046 MB |      347 MB |

Peak `find` RSS at 10M is now 333 MiB for a name query and 372-376 MiB with a
metadata atom, a third of D48's 1 GB line (it was 990 MiB). The 15 MiB Python
floor is in every RSS figure on both sides. `search --json` reads the three
columns it prints: `flamegraph` 0.33 s, `test` 0.49 s at 10M.

What changed the numbers, separated as far as the data allows (`perf` on the 10M
catalog, user space only).

- **Bytes read.** A row carries no decoded inode now; a caller that prints
  metadata loads those columns and reads them by the row's inode id. So a name
  query reads only the name sections (347 MB at 10M, against v1's 380 MB for a
  rare needle and 1,046 MB for a common one, which paid single 64 B row reads).
  The first after-measurement loaded all twelve inode columns (283 MB) for the
  first reported row: 630 MB for every query, the larger half of the rare-needle
  regression (0.26 s of system time against 0.14 s now).
- **Decode.** The rest is unpacking. Validating the name sections decodes every
  packed offset, parent and child where v1 read aligned `u32`s: a `flamegraph`
  query spends 0.14 s of user time against v1's 0.09 s (0.18 s before the fixes,
  which decoded runs of 64 values in one pass instead of one value at a time in
  two). That, not bytes, is why `flamegraph` is still 50 ms behind v1 while
  every query that reports many rows is ahead.
- **`*`** went from 2.49 to 3.70 s and is now 2.32 s: about 1.1 s was decoding
  every inode field for rows that print only a path, and 0.2 s was finding each
  name's end by a NUL search, which the next name's offset now gives.
- **Build.** The 19.4 s first measured was mostly load (56% CPU at load
  average 8) and `fsync` of the 800 MB file, which alone varies from 0.5 to 8 s
  between runs; v1 in the same sitting ranged 7.5-10.4 s. On CPU, S1a took 10.9
  s against v1's 7.3 s: each stat column is its own pass over the inode rows,
  and each pass found each row's batch by binary search again. Resolving each
  row's stat once (8 B per inode, allocated below the build's peak) took 1.2 s
  off, a cheaper packer 0.2 s. Of the 2.2 s that remain, 0.4 s is the
  synthetic's own filling (it now counts entries and reads nlink); the rest is
  ten scattered passes where v1 made one, plus the pass that sizes columns and
  collects dictionaries. (Closing it did not need encoded columns held in
  memory, as this note first said: see below.)

**After the first review round** (2026-09-30, same harness, base = the commit
above rebuilt from a detached worktree, A/B interleaved at load average 5-11
from another session's harness; bytes exact, times plus or minus 10%). Name
offsets and `nlink` became blocked columns (a frame of reference per 128 rows),
document ids a sequence column that is width 0 without holes, and the build
writes every column positionally in one pass over the inode rows.

| measure                                  | before `$HOME` | after `$HOME` | before 10M           | after 10M            |
| ---------------------------------------- | -------------- | ------------- | -------------------- | -------------------- |
| catalog bytes                            | 63.73 B/n      | 60.25 B/n     | 799 MB, 78.02        | 735 MB, 71.78        |
| Names (offsets blocked)                  |                |               | 9.13 B/n             | 7.21                 |
| Nlink                                    |                |               | 12.81 MB             | 1.77 MB              |
| Docs (ids implicit)                      |                |               | 16.29 B/n            | 13.03                |
| `find` peak RSS, name / metadata queries |                |               | 333 / 372-377 MiB    | 315 / 354-358 MiB    |
| `open`, name sections, warm / evicted    | 7.1 / 12.7 ms  | 6.9 / 11.5 ms | 246-252 / 346-356 ms | 243-251 / 344-348 ms |
| `find flamegraph`, fresh                 |                |               | 292 ms               | 281 ms               |
| `find re:^[0-9a-f]{8}$`, fresh           |                |               | 535 ms               | 571 ms               |
| `find '*'`, fresh                        |                |               | 2,290 ms             | 2,469 ms             |
| `synthetic` build, wall (two runs)       |                |               | 11.0 / 12.8 s        | 9.5 / 9.5 s          |
| same, user / sys                         |                |               | 8.4 / 1.3-1.5 s      | 8.1 / 1.3 s          |
| same, peak RSS                           |                |               | 1,837 MiB            | 1,814 MiB            |

A scan of every name is 7-8% slower (user 2.12 to 2.28 s at 10M): each offset
read now goes through its block's table entry. It was 11% until blocked columns
were read through their own type; a view that branched on the coding stopped
`get` inlining into the name loop. Reading the names in decoded runs (the
review's finding 7) is the planned recovery; it followed, below.

**After the second review round** (2026-09-30, same harness; before = the round
above, `075f428`; load average 4-9 from another session's harness, so the close
comparisons below were also taken with `perf stat -r 10`, each variant's binary
on its own catalog, interleaved). Scans read the name columns a block at a time,
carrying each name's end from the next one's start, and test metadata by a pass
over each column; a symlink check gallops from the last one; a directory's path
is built from its parent's when the directory before it was a sibling. Then the
parent, child, size, mtime and ctime columns became blocked too (the review's
finding 2 asked for a child-range table in place of the parent column; blocked
parents are smaller, 6.1 MB against 7.2, and keep a name's parent one read
away).

| measure                                  | before `$HOME` | after `$HOME` | before 10M           | after 10M         |
| ---------------------------------------- | -------------- | ------------- | -------------------- | ----------------- |
| catalog bytes                            | 60.25 B/n      | 51.37 B/n     | 735 MB, 71.78        | 635 MB, 62.03     |
| Names (all three blocked)                |                |               | 7.21 B/n             | 4.10              |
| Size + Mtime + Ctime                     |                |               | 115.2 MB             | 47.3 MB           |
| `find` peak RSS, name / metadata queries |                |               | 315 / 354-358 MiB    | 284 / 301-304 MiB |
| `open`, name sections, warm / evicted    |                |               | 243-251 / 344-348 ms | 247 / 354 ms      |
| `find '*'`, fresh                        |                |               | 2,469 ms             | 1,520 ms          |
| `find re:^[0-9a-f]{8}$`, fresh           |                |               | 571 ms               | 475 ms            |
| `find flamegraph`, fresh                 |                |               | 281 ms               | 290 ms            |
| `find test`, fresh                       |                |               | 363 ms               | 338 ms            |
| `find mtime:<1d`, fresh                  |                |               | 476 ms               | 366 ms            |
| `find size:>100M`, fresh                 |                |               | 443 ms               | 351 ms            |
| `synthetic` build, user (two runs)       |                |               | 8.13 / 8.21 s        | 8.37 / 8.41 s     |
| same, peak RSS                           |                |               | 1,814 MiB            | 1,813 MiB         |

A scan of every name is now 35% faster than before S1a's first review round (`*`
2,290 ms at `5d2bf7d`), not 7% slower. By step, in user time for `*`: 2.31 s,
1.76 s with decoded runs and the symlink cursor, 1.33 s with paths built from
the parent's. Blocking the child column cost a full scan 0.1-0.9% of task-clock
and saved 11.2 MB; blocking parents cost nothing measurable in a query, and a
path read in isolation got 13% faster once it stopped decoding each level's
child (before that change, 5% slower). About 30% of what remains in `*` is the
regex that `*` compiles to, run on every name. Flamegraph at 281 against 290 ms
is load; `perf stat` put the two within 2%, with blocked parents ahead. The
build's 0.2 s more user time is sizing the extra blocked columns.

**After the third review round** (2026-09-30; before = the round above,
`897d670`'s code, on its catalogs; load average 3-11 from another session's
harness, so the close comparisons are `perf stat -r 10`, each variant's binary
on its own catalog, interleaved, and the builds three interleaved rounds). The
remaining plain inode columns became blocked: ino and both nanosecond columns
plainly, `DocId` and entry counts in a nullable blocked coding (each block
framed by its real values, a flag beside the width when it holds a none, width
0 when it holds nothing else). A metadata pass loads its column as it begins,
skips runs of 64 inodes an earlier test cleared, and a pass that clears every
bit ends the query. The build frees the batches' content hashes once document
ids are decided, and writes every column through a bounded 64 KiB buffer.

| measure                                  | before `$HOME`      | after `$HOME`   | before 10M            | after 10M             |
| ---------------------------------------- | ------------------- | --------------- | --------------------- | --------------------- |
| catalog bytes                            | 22.91 MB, 51.36 B/n | 21.31 MB, 47.76 | 635.5 MB, 62.03       | 579.8 MB, 56.59       |
| Ino                                      |                     |                 | 36.6 MiB              | 17.9 MiB              |
| MtimeNs + CtimeNs                        |                     |                 | 73.2 MiB              | 60.4 MiB              |
| Doc (nullable blocked)                   |                     |                 | 28.1 MiB              | 8.3 MiB               |
| Entries (nullable blocked)               |                     |                 | 2.6 MiB               | 1.0 MiB               |
| `find` peak RSS, name / metadata queries |                     |                 | 284-285 / 301-304 MiB | 284-285 / 301-304 MiB |
| `find '*'`, task-clock                   |                     |                 | 1,478 ms              | 1,484-1,491 ms        |
| `find size:>1T mtime:<1d`, task-clock    |                     |                 | 107 ms                | 31 ms                 |
| `find size:>100M mtime:<1d`, task-clock  |                     |                 | 412 ms                | 339 ms                |
| `find mtime:<1d` / `size:>100M`          |                     |                 | 371 / 357 ms          | 348 / 334 ms          |
| `synthetic` build, peak RSS              |                     |                 | 1,813 MiB             | 1,604 MiB             |
| same, fill peak / commit peak            |                     |                 | — / —                 | 1,401 / 1,604 MiB     |

The build's peak fell 209 MiB in two steps. The `synthetic` driver held its
81.8 MB dump and parsed rows through the commit: dropping them took the
process's peak from 1,813 to 1,688 MiB, and it now reports a fill peak and a
commit peak apart, the latter after resetting `VmHWM` (the commit peak read
1,688-1,700 MiB across runs of that binary). The builder kept every batch's
content hashes alive through the document sort: freeing them took the commit
peak from 1,700 to 1,604 MiB, run for run. The column codings
changed the peak by less than 1 MiB. The bounded buffer changes no figure here
(the synthetic's link targets are short); before it, a section written in one
call, the strings, was copied whole into the buffer.

A second generation, timed on the same code with the old codings (a detached
worktree reverting only the column codings) against the new, three rounds
each: a re-run that carries every file begins 12% faster (5.98 to 5.25 s: it
loads the previous catalog whole and sorts its inodes by `(dev, ino)`), then
carries 8% slower (6.26 to 6.75 s: 8.3M binary searches over blocked inode
numbers), for user time 18.89 against 18.82 s and a peak of 2,069 against
2,013 MiB. A run that keeps the root whole instead (`synthetic ... keep`)
begins 5.26 s against 6.00, keeps in 2.69 s against 2.74, and uses 14.10 s of
user time against 14.87. The commit's wall time ranges 7.7-14.4 s on both, all
of it `fsync`.

Every changed column paid except directory names: blocked, they were 5.1 to
2.1 MiB, and a scan of every name took 1.5-2% longer (1,517 / 1,527 against
1,495 / 1,491 ms, the same code with only that column reverted), since it reads
one per directory it enters. They stay one nullable frame. Ctime nanoseconds
save the least (2.6 MiB) and cost nothing measurable: a keep, which reads every
inode's whole row once, is no slower. The impossible-then-costly query
(`size:>1T mtime:<1d`) now leaves the mtime column on disk, which a test through
the real run path checks.

## S1+ — Incremental catalog

Design and build slices: [S1PLUS.md](S1PLUS.md) (M0, 2026-10-03).

After S1a, ahead of the daemon (D40): a re-run writes what changed rather than
the whole snapshot (D26 B's change log over A's snapshot), so publication costs
the change. A full recrawl still walks and compares the tree. The daemon's small
inotify bursts need this shared producer and resident session.

**Measure:** bytes written and time for a one-file change at 10M entries.

**M1 measured** (2026-10-04 local time, production code `13051e5`, release
build via `nix develop --command cargo build --release -p ferret -p ferret-bench`).
Machine: AMD Ryzen 9 9955HX, 32 logical CPUs, ext4.
The original 10M synthetic v3 fixture is actually **10,448,739 names**,
10,405,730 inodes, 1,800,947 directories and 8,495,924 documents:
`/tmp/find-m4a-measure/sentinel-final/catalog`. It was copied into an isolated
index and migrated with the real `ferret import-v3`; the packed v3 sections are
byte-preserved. No log or overlay exists. The final follow-up changes tests
and documentation only, so `13051e5` identifies the measured production code.

All manual runs used:

```sh
export XDG_CONFIG_HOME=/tmp/s1plus-m1-measure/config
export XDG_DATA_HOME=/tmp/s1plus-m1-measure/data
export XDG_STATE_HOME=/tmp/s1plus-m1-measure/state
export XDG_CACHE_HOME=/tmp/s1plus-m1-measure/cache
export FERRET_INDEX=/tmp/s1plus-m1-measure/index
```

Source commands below abbreviate
`B=/home/dave/w/super-ferret-wt/s1plus/target/release/ferret-bench` and
`I=/tmp/s1plus-m1-measure/index`. The runner
`/home/dave/w/super-ferret/.ai/s1plus-m1-measurements/run.py` checks `uptime` and
`pgrep -af 'harness.run|ferret_timing|ignore_timing|synthetic|ferret-bench'`
before **every** invocation; no competing timing process was present. The
runner excludes itself and its ancestors from pgrep's matches. Raw commands,
loads and outputs are preserved beside that runner in `sections.json` and
`timings.json`. One benchmark ran at a time, with no concurrent compilation.

Section bytes are exact. **Every snapshot row** below comes from
`$B sections "$I"`, at `13051e5`, load averages **1.84 / 2.09 / 1.81**.
The 128 B `current` manifest is separate from the snapshot total, measured
with `stat --format=%s "$I/current"` at `13051e5`, load **2.64 / 2.46 / 2.09**.

| Section | Bytes | B/name |
| --- | ---: | ---: |
| Names | 44,348,882 | 4.2444 |
| NameHeap | 252,108,533 | 24.1281 |
| DirNames | 5,402,849 | 0.5171 |
| Entries | 1,040,735 | 0.0996 |
| Traversed | 225,119 | 0.0215 |
| Roots | 8 | 0.0000 |
| Strings | 238,130 | 0.0228 |
| Dev | 2,601,473 | 0.2490 |
| Ino | 19,022,579 | 1.8206 |
| Size | 19,181,044 | 1.8357 |
| Mtime | 15,963,579 | 1.5278 |
| MtimeNs | 28,287,944 | 2.7073 |
| Ctime | 12,993,698 | 1.2436 |
| CtimeNs | 36,199,331 | 3.4645 |
| Mode | 6,503,758 | 0.6224 |
| Owner | 1,300,741 | 0.1245 |
| Nlink | 1,790,376 | 0.1713 |
| Doc | 8,819,406 | 0.8441 |
| States | 2,601,433 | 0.2490 |
| Links | 272,136 | 0.0260 |
| Specials | 0 | 0.0000 |
| WorkTrees | 0 | 0.0000 |
| Docs | 135,934,792 | 13.0097 |
| DocRefs | 33,983,696 | 3.2524 |
| RetainedAt | 225,128 | 0.0215 |
| Policy | 16 | 0.0000 |
| **Snapshot total** | **629,046,618** | **60.2031** |
| `current` manifest | 128 | <0.0001 |

Normalizing the snapshot to exactly 10M names gives **602.031 MB**, against
M0's **602.0 MB estimate**. The unchanged v3 packed payload contributes
594,836,546 B; DocRefs adds 33,983,696 B, all-none RetainedAt 225,128 B,
Policy 16 B and the checked head 1,232 B. This is a measured artifact-size
comparison; the 10M normalization remains a projection, not another fixture.

Warm timings use the S1a/S1c median method: one unreported warm-up then seven
fresh-process runs, with the fixture resident in the page cache. Open times
include manifest/head I/O, section reads, checksum verification and structural
validation. Snapshot bytes read exclude the manifest's additional 128 B.
RSS is Linux `VmHWM` in KiB converted to MiB, per fresh benchmark process;
there is no Python parent-process RSS floor in this figure.

| Measurement | Warm median | Snapshot bytes read | Peak RSS, median (range), MiB | Source command | Commit | Load averages, ranges (1 / 5 / 15 min) |
| --- | ---: | ---: | ---: | --- | --- | --- |
| Name open | 307.54 ms | 302,596,889 | 291.70 (291.61–291.73) | `$B open-once "$I" names` | `13051e5` | 1.69 / 2.06 / 1.80 |
| Name + inode metadata open | 421.10 ms | 457,862,251 | 439.80 (439.71–439.85) | `$B open-once "$I" metadata` | `13051e5` | 1.64 / 2.04 / 1.79 |
| Full open | 638.93 ms | 629,046,618 | 634.41 (633.91–634.96) | `$B open-once "$I" full` | `13051e5` | 1.64–1.91 / 2.04–2.09 / 1.79–1.81 |
| All-section checksum throughput | 5.273 GB/s; 119.29 ms | 629,045,386 bytes hashed | — | `$B checksum "$I"` | `13051e5` | 1.91–2.00 / 2.09–2.10 / 1.81–1.82 |

The name set is Names/NameHeap/DirNames/Roots/Strings/Traversed/Links/Specials;
metadata adds the twelve inode sections. Entries is absent from this name
load, while M0's conservative name-set estimate included it. Full open also
loads Entries, Docs, DocRefs, RetainedAt and Policy; its peak includes the
32.4 MiB temporary reference-count array used for DocRefs validation, which
is freed after checking. The checksum measurement calls the same BLAKE3-128
primitive as the decoder over each persisted section, with file reads and
warming outside the timed interval; it measures resident hashing rather than
I/O or validation throughput. The seven measured checksum runs span
118.35–121.95 ms (5.158–5.315 GB/s).

M1 keeps the existing full-checkpoint writer: an unchanged recrawl still
publishes a new epoch. The zero-write unchanged pass, tiny log commits and
incremental timings remain M2–M7 work.

### S1+ M2 — Durable log transactions (2026-10-04)

M2 adds checked `changes.<checkpoint>` headers, complete transaction envelopes,
three-barrier append publication, locked prefix recovery, and pinned lazy family
loads. Checkpoint publication now syncs the snapshot/log pair before publishing
`current`. Recovery refuses damaged published payloads, discards every unpublished
suffix, and syncs the selected manifest's directory entry before retiring obsolete
pairs after an `Undurable` result. Ordinary queries preserve their no-log behavior;
until M3 supplies the overlay they explicitly refuse a nonempty log. Tests drive
actual writers/readers at every sync/rename boundary, including every ancestor on
first publication, partial writes, signed malformed records, every log truncation
and single-bit flip, lock/open races, and lazy loads after unlink.

Measurements use production commit **`8baafd4`**, release builds with the same
compiler/environment on the Ryzen 9 9955HX (32 logical CPUs), Linux 6.18.43,
ext4/NVMe. Later edits add reserved-option/reference bounds and tests; the measured
valid record paths and checkpoint loads otherwise remain the same. No dependencies
or manifests changed. The 10,448,739-name fixture is M1's v3 import; the entire old
packed payload was verified byte-for-byte identical across v3/v4 (594,836,546 B).
Fixtures and all four XDG directories are isolated under
`/tmp/s1plus-m2-measure`; `FERRET_INDEX` is set for each invocation. Before **every**
run the host runner checks `uptime` and
`pgrep -af 'harness.run|ferret_timing|ignore_timing|synthetic|ferret-bench'`;
no competing timing was found. Runs are serial.

**Baseline correction:** `main` at `9d04f1f` is format **v2**, and refused the v3
fixture. The actual v3 comparison uses pre-M1 **`db80b2f`**, rather than presenting
v2 as v3. Only identical fresh-process `open-once` timing/RSS instrumentation was
added to that isolated baseline's benchmark driver; its catalog code is unchanged.
The patch, fixture SHA256s, complete commands, load checks, raw samples and traces
are preserved in `/home/dave/w/super-ferret/.ai/s1plus-m2-measurements/`.

Warm opens have one unreported warm-up and **13 samples per version/set**. Each
v3/v4 pair alternates its ordering (AB/BA) to balance drift; the matched-section
v4 full load follows each full pair. RSS is fresh-process Linux `VmHWM` in MiB.
`B3=/tmp/s1plus-m2-v3/target/release/ferret-bench`,
`B4=/home/dave/w/super-ferret-wt/s1plus/target/release/ferret-bench`, and
`I=/tmp/s1plus-m2-measure` in the commands below.

| Open | Warm median (range), ms | Snapshot bytes read | Peak RSS median, MiB | Source command | Commit | Load ranges (1 / 5 / 15 min) |
| --- | ---: | ---: | ---: | --- | --- | --- |
| v3 names | 269.11 (258.90–294.48) | 302,596,337 | 291.59 | `$B3 open-once "$I/v3" names` | `db80b2f` + driver patch | 1.89–2.30 / 1.98–2.08 / 3.02–3.06 |
| v4 names | 317.05 (301.63–334.92) | 302,596,889 | 291.80 | `$B4 open-once "$I/v4" names` | `8baafd4` | same paired load range |
| v3 full | 466.23 (459.15–514.84) | 594,837,226 | 570.33 | `$B3 open-once "$I/v3" full` | `db80b2f` + driver patch | same paired load range |
| v4 matched v3 sections | 552.98 (537.58–580.11) | 594,837,778 | 570.54 | `$B4 open-once "$I/v4" legacy-full` | `8baafd4` | same paired load range |
| v4 full | 643.27 (624.57–684.35) | 629,046,618 | 634.46 | `$B4 open-once "$I/v4" full` | `8baafd4` | same paired load range |

Name open adds **47.94 ms / 17.8%**, exceeding the approximate 15% question.
Matched-section full open adds **86.75 ms / 18.6%**. These isolate the checked
format's first-load overhead on identical payloads and nearly identical validation;
they are an A/B of formats, not a hash-only profiler. Actual full open adds
177.04 ms / 38.0%; that also pays for the new sections and DocRefs validation,
including its temporary refcount array. Snapshot byte counts omit v4's additional
128 B manifest and 64 B empty log header. The 552 B head difference is included.

**Per-block verification assessment (not implemented):** Simply moving the
checksum into block access would not recover this measured name open: today's
`check_names` validates every row and basename, so it would touch/check the entire
set anyway. Recovering most of the 48 ms at open would require deferring structural
validation too, with fallible block loads and integrity state checked before each
access. Sparse path/metadata queries could then avoid untouched blocks. A complete
name search still scans the heap and must pay its verification cost somewhere;
this shifts work and can improve first-row latency, not remove full-scan hashing.
At illustrative 64 KiB byte blocks, digests alone add about 0.154 MB per snapshot
(0.074 MB for this name set), plus small validity bitsets. The maintenance cost is
larger: a new wire table and writer sealing/import paths, checked block caches,
strings/packed rows crossing byte blocks, validation across adjacent blocks and
references, fallible query access, concurrent first-use handling, and a revised
corruption matrix. Keep section checks for now; revisit only with sparse-query or
first-row measurements that justify that complexity.

Durable append times exclude writer-open recovery and preparation of replacement
rows. Each replacement preserves the real inode's content state and DocId; only
permissions change. Thirteen samples follow one warm-up per case. Sync calls batch
all records in the transaction. Load was **1.96–1.97 / 2.05 / 3.27–3.29** for every
append/trace row below; source commit is **`8baafd4`** throughout.

| Write / barrier | Bytes | Median (range), ms | Source command |
| --- | ---: | ---: | --- |
| Empty change set | 0 | 0.001 (0.001–0.001) | `$B4 log-append-once "$I/tiny" 0` |
| One inode observation | 232 log + 128 manifest = **360** | 8.998 (4.278–18.811) | `$B4 log-append-once "$I/tiny" 1` |
| 1,000 inode observations | 88,144 log + 128 manifest = **88,272** | 11.801 (5.278–16.174) | `$B4 log-append-once "$I/batched" 1000` |
| Log fsync, traced | — | 3.802 (1.130–21.460) | `strace -T -yy -e trace=fsync,rename,renameat,renameat2,pwrite64 -o trace.txt $B4 log-append-once "$I/tiny" 1` |
| Manifest fsync, traced | — | 2.190 (0.808–14.159) | same command, 11 trace samples |
| Directory fsync, traced | — | 1.533 (0.464–8.171) | same command, 11 trace samples |

Every trace shows `pwrite64(log) → fsync(log) → fsync(current.tmp) →
rename(current.tmp,current) → fsync(directory)`. Traced barrier distributions are
reported separately from untraced commit latency; their medians are not additive.
The empty API result reports microsecond timer resolution; tests verify literally
zero writes and syncs. Fsync variation dominates a tiny commit on this device.

Header-only opens below use the same 10M checkpoint, one Inodes block per
transaction, warm-up plus 13 samples. `N` is total log records. Setup uses the real
writer (`$B4 log-fill "$I/header-<records-per-tx>" <added-transactions>
<records-per-tx>`). Every timing row's command is
`$B4 log-open-once "$I/header-<records-per-tx>"`, commit **`8baafd4`**, load ranges
**1.31–1.49 / 1.86–1.88 / 2.90–2.91**.

| Records/transaction | T | N | Median (range), ms | Log bytes read | Peak RSS median, MiB |
| ---: | ---: | ---: | ---: | ---: | ---: |
| 1 | 0 | 0 | 0.037 (0.029–0.072) | 64 | 3.09 |
| 1 | 1 | 1 | 0.043 (0.037–0.065) | 208 | 3.13 |
| 1 | 100 | 100 | 0.161 (0.151–0.320) | 14,464 | 3.16 |
| 1 | 1,000 | 1,000 | 1.219 (1.194–1.359) | 144,064 | 3.44 |
| 1,000 | 0 | 0 | 0.031 (0.026–0.066) | 64 | 3.09 |
| 1,000 | 1 | 1,000 | 0.041 (0.029–0.046) | 208 | 3.14 |
| 1,000 | 100 | 100,000 | 0.200 (0.159–0.291) | 14,464 | 3.14 |
| 1,000 | 1,000 | 1,000,000 | 1.412 (1.285–1.863) | 144,064 | 3.47 |

Every row also reads the 1,232 B checkpoint head and 128 B manifest, and **zero
section or log payload bytes**. Log disk size at T=1,000 is 232,064 B for one-row
transactions versus 88,144,064 B for 1,000-row transactions. Read volume follows
`64 + 144*T` for this one-family shape, independent of N; the runtime/RSS likewise
track transaction framing rather than replay. Recovery deliberately loads all
published payloads before allowing a writer to append; that is not header-only
query opening. Overlay construction/replay and crawl production remain M3/M4.

### S1+ M3 — Scoped section verification experiment (2026-10-04)

The separable M3 startup experiment was **reverted**: scoped concurrent section
reads/BLAKE3 checks recovered 23.03 ms (48.0% of M2's 47.94 ms name-open cost),
rather than most of it, and the prototype added 88 lines. Structural checks
remained serial in dependency order after digests passed; only requested sections
and their dependencies were fetched. Section-level integrity stays as accepted.
The larger full-open gain below is recorded without changing the requested keep
criterion.

The actual merged serial baseline is `a979412`; the parallel arm is that source
plus the preserved prototype patch, with **no catalog format or dependency
change**. Release builds used the same compiler on the Ryzen 9 9955HX, Linux
6.18.43/ext4/NVMe. The no-log 10,448,739-name v4 fixture is the same imported
checkpoint as M1/M2. One warm-up then 13 fresh-process samples per arm/set,
paired **AB/BA**. Before every invocation the host runner checked uptime and the
required pgrep; no competing timing was found. All four XDG paths and FERRET_INDEX
were isolated under `/tmp/s1plus-m3-measure`.

| Open | Serial median, ms | Scoped median, ms | Saved | Source command | Source | Load ranges (1 / 5 / 15 min) |
| --- | ---: | ---: | ---: | --- | --- | --- |
| Names | 308.68 | 285.65 | 23.03 ms / 7.5% | `{serial,parallel}-bench open-once "$I" names` | `a979412`, parallel + archived patch | 1.94–3.34 / 2.32–2.61 / 2.18–2.28 |
| Full | 635.54 | 477.80 | 157.74 ms / 24.8% | `{serial,parallel}-bench open-once "$I" full` | same | same |

Binaries were `/tmp/s1plus-m3-{serial,parallel}-bench`,
`I=/tmp/s1plus-m3-measure/index`; complete commands, individual load checks,
raw samples/RSS, compiler and binary/patch SHA256s are preserved in
`/home/dave/w/super-ferret/.ai/s1plus-m3-measurements/`. The saved prototype can
be applied to the recorded baseline to reproduce the parallel arm. A dominant
NameHeap section limits concurrency between sections; recovering the remaining
cost needs work beyond this requested small experiment.


### S1+ M3 — Effective reader and resident overlays (2026-10-04)

Measured source **`2e12d7b`**, release `ferret-bench`, same Ryzen 9 9955HX,
Linux 6.18.43/ext4/NVMe and imported v4 10,448,739-name checkpoint as M1/M2.
No competing timing was found: before **every invocation**, the host runner
checked `uptime` and `pgrep -af 'harness.run|ferret_timing|ignore_timing|synthetic|ferret-bench'`.
All four XDG directories and `FERRET_INDEX` were isolated under
`/tmp/s1plus-m3-overlays`. Full command/env/load records, binary SHA256, raw samples,
summary and runner are in `/home/dave/w/super-ferret/.ai/s1plus-m3-measurements/`.
The interrupted 8e95513 preparation and superseded ebeda64 runs are archived;
none of their figures enter the tables below.

Here `B=/home/dave/w/super-ferret-wt/s1plus/target/release/ferret-bench` and
`I=/tmp/s1plus-m3-overlays/p{0,1,2}` denotes a **separate invocation per fixture**.
The 1/2% labels mean 100k/200k distinct names **and** 100k/200k distinct inode
field replacements per nominal 10M entries. One transaction appends `.m3` to
the first BFS name rows (including directory basenames) and changes size/mode
on the first non-directory inode rows, preserving identity and document bindings.
Moving those directory spellings affects descendant paths; this is a mixed
namespace/metadata stress case. Exact payload sizes depend on actual basenames.
Preparation command: `$B overlay-fill "$I" {100000,200000}` (one count per fixture),
source `2e12d7b`, loads 2.55–2.60 / 2.71–2.72 / 2.76–2.77.
The 0/1/2% log files are 64 / 14,353,544 / 28,331,456 B; the checkpoint is shared unchanged.

Warm opens have one warm-up per case/fixture, then seven fresh-process samples,
rotating 0/1/2 and reversing each round. RSS here is **VmHWM**, not current RSS.

| Overlay | Names median (range), ms | Full median (range), ms | Names / full peak MiB | Source commands | Commit | Load ranges (1 / 5 / 15 min) |
| --- | ---: | ---: | ---: | --- | --- | --- |
| 0% | 330.46 (318.25–347.78) | 709.28 (688.43–752.65) | 291.96 / 634.57 | `$B open-once "$I" names`; `$B open-once "$I" full` | `2e12d7b` | 2.36–2.65 / 2.65–2.71 / 2.74–2.76 |
| 1% | 557.88 (548.17–573.75) | 1011.98 (995.73–1020.87) | 379.06 / 729.37 | `$B open-once "$I" names`; `$B open-once "$I" full` | `2e12d7b` | 2.36–2.65 / 2.65–2.71 / 2.74–2.76 |
| 2% | 816.45 (799.78–823.48) | 1351.19 (1329.92–1366.45) | 461.57 / 823.96 | `$B open-once "$I" names`; `$B open-once "$I" full` | `2e12d7b` | 2.36–2.65 / 2.65–2.71 / 2.74–2.76 |

Resident runs load all sections before timing, parse once, discard one warm-up,
then execute 13 queries through the real query reader, touching each emitted
path/id. Each query/fixture is one process; current **VmRSS** and peak **VmHWM**
are read after the samples. The broad query emits 10,405,729 rows; rare name 92,
common name 162,219, size 20,010. Extension+size emits 133,538 / 133,426 / 133,286
because the rename suffix changes extensions. The differing result count is
included rather than treating the three final states as identical.

| Query | Median ms, 0 / 1 / 2% | Current RSS MiB, 0 / 1 / 2% | Peak MiB, 0 / 1 / 2% | Source command | Commit | Load ranges (1 / 5 / 15 min) |
| --- | ---: | ---: | ---: | --- | --- | --- |
| `""` (all names) | 526.60 / 765.02 / 801.03 | 603.48 / 699.64 / 796.30 | 634.27 / 729.99 / 823.63 | `$B resident-once "$I" ""` | `2e12d7b` | 2.40–2.76 / 2.64–2.72 / 2.73–2.76 |
| `case:Flamegraph` | 8.63 / 9.43 / 9.21 | 603.34 / 699.64 / 796.29 | 635.04 / 729.04 / 823.24 | `$B resident-once "$I" "case:Flamegraph"` | `2e12d7b` | 2.40–2.76 / 2.64–2.72 / 2.73–2.76 |
| `test` | 67.42 / 106.70 / 117.69 | 603.41 / 699.63 / 796.34 | 634.54 / 729.78 / 823.76 | `$B resident-once "$I" "test"` | `2e12d7b` | 2.40–2.76 / 2.64–2.72 / 2.73–2.76 |
| `size:>100M` | 191.81 / 213.70 / 227.75 | 605.96 / 700.70 / 796.28 | 634.77 / 729.51 / 823.79 | `$B resident-once "$I" "size:>100M"` | `2e12d7b` | 2.40–2.76 / 2.64–2.72 / 2.73–2.76 |
| `ext:rs size:>10k` | 81.88 / 134.58 / 140.26 | 603.48 / 699.57 / 796.34 | 634.84 / 728.71 / 823.66 | `$B resident-once "$I" "ext:rs size:>10k"` | `2e12d7b` | 2.40–2.76 / 2.64–2.72 / 2.73–2.76 |

Geometric carry measurements use real writer transactions and `Catalog::advance`;
only the resident advance is timed, excluding record construction, writer setup
and durable publication. Each candidate is also committed through the writer.
The small sequence publishes 1,024 distinct one-inode updates. For the large
boundary, bursts seed 131,070 distinct fields in geometric runs, then three
one-inode updates straddle the 131,072-row carry. One warm-up process and 13
fresh-process samples, resetting the log each time. Times are printed to 0.01 ms;
`0.00` is below that display resolution, not zero work.

| Update / carry | Runs before → after | Median (range), ms | RSS / peak MiB | Source command | Commit | Load ranges (1 / 5 / 15 min) |
| --- | --- | ---: | ---: | --- | --- | --- |
| 1,024-update sequence | at most 10; final 10 → 1 | 0.01 (0.00–0.06); final carry 0.06 | 643.36 / 643.36 | `$B overlay-carry /tmp/s1plus-m3-overlays/carry 1024` | `2e12d7b` | 2.70 / 2.69 / 2.75 |
| 131,072 boundary, before | 16 → 17 | 0.01 (0.01–0.02) | 670.41 / 670.41 | `$B overlay-carry-boundary /tmp/s1plus-m3-overlays/boundary 131072` | `2e12d7b` | 2.70–2.82 / 2.69–2.72 / 2.75 |
| 131,072 boundary, carry | 17 → 1 | 5.10 (4.96–6.20) | 670.41 / 670.41 | `$B overlay-carry-boundary /tmp/s1plus-m3-overlays/boundary 131072` | `2e12d7b` | 2.70–2.82 / 2.69–2.72 / 2.75 |
| 131,072 boundary, after | 1 → 2 | 0.02 (0.01–0.03) | 670.41 / 670.41 | `$B overlay-carry-boundary /tmp/s1plus-m3-overlays/boundary 131072` | `2e12d7b` | 2.70–2.82 / 2.69–2.72 / 2.75 |

The mixed 2% overlay adds **192.82 MiB (202.19 MB)** current RSS for the broad
query and **189.39 MiB** peak on full open. This exceeds the design's 20–80 MB
inode/doc-only estimate, although the measured workload also replaces names
and directory paths. It stays below D48's 1 GB resident line, but broad-query
latency rises **52.1%** and full-open time **90.5%**. Start with a **1% dirty-row
checkpoint target**, lowered from 2% for headroom; M7 implements and tunes that
policy. Do not add a second persistent tree to hide this representation cost.
The 5.10 ms large carry is occasional CPU merge latency, not a durability or
end-to-end scoped-update measurement. Full recrawl diff and durable update
costs belong to M4; repeated churn/compaction and retained-reader budgets to M7.


The namespace generation has a separate cost: immutable **record runs** merge
geometrically, but a changed namespace rematerialises its sparse latest-name heap
and base-id suppression stream. Metadata-only generations share those buffers.
To avoid hiding this cost under the metadata carry result, one real NamePut at the
first live edge was advanced and then published at 0/1/2% existing overlays.
Source **`4f87e00`** adds only this bench command and documentation to the same
reader implementation as `2e12d7b`. Writer open, inverse reference preparation,
record creation and durable I/O are outside the timer. One warm-up and seven
samples per fixture, reversing fixture order each round, fresh process/log reset.
All usual uptime/pgrep and XDG/index isolation checks apply.

| Existing overlay | Namespace advance median (range), ms | Runs before → after | RSS / peak MiB | Source command | Commit | Load ranges (1 / 5 / 15 min) |
| --- | ---: | --- | ---: | --- | --- | --- |
| 0% | 0.05 (0.05–0.06) | 0 → 1 | 643.09 / 643.09 | `$B overlay-rename-once /tmp/s1plus-m3-overlays/rename` | `4f87e00` | 2.34–2.50 / 2.56–2.60 / 2.53–2.54 |
| 1% | 15.41 (15.03–15.94) | 2 → 3 | 752.31 / 752.31 | `$B overlay-rename-once /tmp/s1plus-m3-overlays/rename` | `4f87e00` | 2.34–2.50 / 2.56–2.60 / 2.53–2.54 |
| 2% | 31.47 (30.45–32.36) | 2 → 3 | 858.09 / 858.09 | `$B overlay-rename-once /tmp/s1plus-m3-overlays/rename` | `4f87e00` | 2.34–2.50 / 2.56–2.60 / 2.53–2.54 |

These updates do **not** carry the large record run. The 1/2% cost is derived
namespace materialisation, O(dirty names), not the 1–10 ms metadata-only model.
M4 must batch name changes; M6 must account for this in namespace burst latency.
A heap per immutable run would avoid this generation-wide sparse copy but adds
multiple-heap span/ownership handling and query merging. It is a possible later
optimisation, not part of this measured implementation. No base heap is copied.

### S1+ M4 — Covered batch recrawl producer (2026-10-04)

M4 is implemented: a resident writer session caches lookups and the checked
view; the batch CLI appends a deterministic final change set instead of replacing
the checkpoint. Unchanged recrawls publish nothing. Full coverage is required;
incomplete EACCES keeps amended A′ through owned checkpoint fallback or a blocked
resident request. Broad protected-scope recovery remains M5. Seventeen new
real-crawl tests, including 8 seeds × 32 mutation prefixes, reuse M3's fresh
materialised-checkpoint oracle. Workspace gates: **504 passed / 4 ignored**, all
fmt/clippy/test gates green with zero warnings, real user log size/mtime unchanged.

Measured production code **`6517fc7`**, release producer/driver built with
`nix develop --command cargo build --release -p ferret-crawl --example recrawl`
and `nix develop --command cargo build --release -p ferret-bench`. Same machine
and existing v4 fixture as M1–M3: **10,448,739 names**, 10,405,730 inodes,
1,800,947 directories, 8,495,924 documents, from
`/tmp/s1plus-m3-measure/index`. The fixture has no filesystem tree. The producer
replays its rows into real crawl batches and calls production reconciliation
and the durable writer: these are **synthetic observation replay** measurements,
not filesystem enumeration or content-hashing throughput. Changed cases alter
N distinct indexed regular-file inodes, with consistent observations for every
alias, fresh times and deterministic new content hashes. 100,000 is nominal 1%
of 10M, rather than exactly 1% of the actual name count.

One warm-up per case, then **three samples per case**, reversing case order in
the second round; a fresh process/private manifest and log for every invocation.
The immutable checkpoint is hardlinked into each private index. Its generation,
size and mtime are checked unchanged, and the no-change producer asserts zero
records, zero writes and no new generation. No compilation or other benchmark
ran during timing. Before every invocation, the host-visible guard checks
`uptime` and
`pgrep -af 'harness.run|ferret_timing|ignore_timing|synthetic|ferret-bench'`;
only runner ancestors and pgrep itself are excluded. No competing benchmark was
present. Commands, per-invocation loads, binary SHA-256s, outputs and environments
are archived under `/home/dave/w/super-ferret/.ai/s1plus-m4-measurements/`, in
`recrawl-samples.json`; `recrawl-run.py` is the runner.

Commands abbreviate:

```sh
B=/home/dave/w/super-ferret-wt/s1plus/target/release/ferret-bench
P=/home/dave/w/super-ferret-wt/s1plus/target/release/examples/recrawl
export XDG_CONFIG_HOME=/tmp/s1plus-m4-measure/config
export XDG_DATA_HOME=/tmp/s1plus-m4-measure/data
export XDG_STATE_HOME=/tmp/s1plus-m4-measure/state
export XDG_CACHE_HOME=/tmp/s1plus-m4-measure/cache
# Set N to 0, 1 or 100000 for the corresponding command below.
export FERRET_INDEX=/tmp/s1plus-m4-measure/c${N}
```

Logical writes are appended log bytes plus the 128 B manifest replacement;
filesystem block amplification and fixture reset are excluded. Post-setup total
includes replay, diff, dropping batches and durable publication. RSS is measured
in the producer; final RSS is after batches are dropped, peak spans the complete
process. Phase medians are reported independently.

| Changed file inodes | Log + manifest bytes | Replay / diff / commit median, ms | Post-setup total median (range), s | Final / peak RSS, MiB | Source command | Commit | Load ranges (1 / 5 / 15 min) |
| --- | ---: | ---: | ---: | ---: | --- | --- | --- |
| No change | 0 + 0 | 4165.49 / 40702.46 / 0.00 | 45.04 (44.79–45.05) | 811.41 / 2484.27 | `$B recrawl-once /tmp/s1plus-m4-measure/c0 0 "$P"` | `6517fc7` | 2.66–2.94 / 2.64–2.76 / 2.31–2.46 |
| One file | 328 + 128 | 4252.96 / 40716.41 / 5.28 | 45.07 (44.88–45.39) | 811.52 / 2484.99 | `$B recrawl-once /tmp/s1plus-m4-measure/c1 1 "$P"` | `6517fc7` | 2.61–3.18 / 2.61–2.83 / 2.32–2.49 |
| 100,000 (nominal 1%) | 13,600,192 + 128 | 4534.47 / 41439.60 / 249.11 | 46.32 (46.08–46.78) | 836.95 / 2508.18 | `$B recrawl-once /tmp/s1plus-m4-measure/c100000 100000 "$P"` | `6517fc7` | 2.80–3.05 / 2.63–2.86 / 2.34–2.52 |

Session setup is separate: opening/validating the resident view, caching base
reference counts and sorting identity/hash/directory lookups. A resident host
pays this once, rather than per burst; it is outside the post-setup totals above.

| Case | Session setup median (range), ms | Setup current / peak RSS, MiB | Source command | Commit | Load ranges (1 / 5 / 15 min) |
| --- | ---: | ---: | --- | --- | --- |
| No change | 10051.78 (10034.53–10116.76) | 721.55 / 721.55 | `$B recrawl-once /tmp/s1plus-m4-measure/c0 0 "$P"` | `6517fc7` | 2.66–2.94 / 2.64–2.76 / 2.31–2.46 |
| One file | 10085.82 (10032.46–10168.30) | 721.65 / 721.65 | `$B recrawl-once /tmp/s1plus-m4-measure/c1 1 "$P"` | `6517fc7` | 2.61–3.18 / 2.61–2.83 / 2.32–2.49 |
| 100,000 (nominal 1%) | 10154.17 (10034.52–10167.92) | 721.65 / 721.65 | `$B recrawl-once /tmp/s1plus-m4-measure/c100000 100000 "$P"` | `6517fc7` | 2.80–3.05 / 2.63–2.86 / 2.34–2.52 |

The unchanged pass writes **0 B**; one changed content binding writes **456 B**
(three records); 100,000 writes **13,600,320 B** (300,000 records), without a new
checkpoint. M4 publishes even when a diff exceeds the proposed 1% target; M7
owns compaction. Metadata-only changes are a different, smaller record workload.

Publication scales with the changed set, but a whole recrawl is still expensive:
about 4–4.6 s replay plus 40–42 s diff, before setup, and about 2.43–2.45 GiB
peak RSS. After replay, resident batches add about **1.28 GiB** beyond the
session. M4 sorts an 8 B locator per file observation to group hard links; its
diff comparison work is O(files log files). These results do not establish the
earlier warm full-recrawl estimate. M6 must stream observations and avoid this
whole-file sort for scoped refreshes; small durable commits do not imply small
full-recrawl CPU or transient memory. M4 also widens a kept alias root when the
last refreshed hard-link name is deleted, to get trustworthy shared metadata;
M6 can narrow that to checked alias scopes.

### S1+ M4b — Directory-local recrawl reduction (2026-10-04)

M4b replaces whole-file sorting with directory-local basename merging. Resident
batches retain fully equal single-name files as compact old-name references;
only changed/unmatched observations and possible aliases enter the global
identity-sorted residue. Equality includes stat, kind, content state/hash and
symlink target. Indexed alias counts matter as well as filesystem nlink. A later
unmatched alias expands compact observations of that inode back into the group,
preserving conflict handling and canonical ordering. Directory tokens carry
checked old-directory hints and resolve through dense per-batch tables. The
directory graph borrows stats; local listings reuse a name-byte buffer.
Checkpoint child iteration uses its existing basename order without copying or
sorting. Effective overlays keep their sparse child merging. The final-set
edge/refcount logic, root widening and amended A′ fault rule remain M4's.

This moves local observation reduction ahead of M6: eliminating the sort alone
would leave whole file batches above the RSS target. It still retains directory
observations and compact name references for the run; fully streamed directory
scopes remain M6. M5 can build coverage scopes on this shape. Eighteen real-crawl
recrawl tests pass, including M4's generated full-index oracle sequences and a
new compact-row/new-alias conflict regression. The EACCES test now checks that
fallback expands compact siblings. All workspace gates: **505 passed / 4 ignored**,
zero warnings, real user log size/mtime unchanged.

Measured production code **`8ffa06d`**, with the same release commands and
**10,448,739-name** v4 fixture as M4. **One warm-up and three recorded samples
per case**, second round reversed; fresh process/private manifest/log each time,
immutable snapshot hardlinked from `/tmp/s1plus-m3-measure/index`. Before every
invocation, the host-visible runner checks `uptime` and
`pgrep -af 'harness.run|ferret_timing|ignore_timing|synthetic|ferret-bench'`,
excluding only itself/ancestors and pgrep. No competing benchmark or compilation
ran during timing. Evidence, commands, binary SHA-256s, environments and all loads
are in `/home/dave/w/super-ferret/.ai/s1plus-m4b-measurements/recrawl-samples.json`;
`recrawl-run.py` is the guard/runner and `report.py` generates these tables.

```sh
B=/home/dave/w/super-ferret-wt/s1plus/target/release/ferret-bench
P=/home/dave/w/super-ferret-wt/s1plus/target/release/examples/recrawl
export XDG_CONFIG_HOME=/tmp/s1plus-m4b-measure/config
export XDG_DATA_HOME=/tmp/s1plus-m4b-measure/data
export XDG_STATE_HOME=/tmp/s1plus-m4b-measure/state
export XDG_CACHE_HOME=/tmp/s1plus-m4b-measure/cache
# Set N to 0, 1 or 100000 for the corresponding command below.
export FERRET_INDEX=/tmp/s1plus-m4b-measure/c${N}
```

These remain **synthetic observation replay** measurements, without filesystem
walking or reading/hashing file contents. Replay now includes local equal-row
comparison/reduction; moving that work out of diff is included in post-setup
total. Logical writes are appended log bytes plus the manifest replacement,
excluding fixture resets and filesystem block amplification. Final RSS follows
batch release; peak covers the entire producer, including setup. Each phase's
median is calculated independently. 100,000 means distinct indexed regular-file
inodes (all aliases agree), nominal 1% of 10M, as in M4.

| Changed file inodes | Log + manifest bytes | Replay / diff / commit median, ms | Post-setup total median (range), s | Final / peak RSS, MiB | Source command | Commit | Load ranges (1 / 5 / 15 min) |
| --- | ---: | ---: | ---: | ---: | --- | --- | --- |
| No change | 0 + 0 | 6686.98 / 2565.07 / 0.00 | 9.28 (9.24–9.40) | 764.52 / 1386.72 | `$B recrawl-once /tmp/s1plus-m4b-measure/c0 0 "$P"` | `8ffa06d` | 1.66–1.78 / 1.70–1.73 / 1.76–1.76 |
| One file | 328 + 128 | 6680.29 / 2568.50 / 5.44 | 9.29 (9.24–9.35) | 764.57 / 1386.52 | `$B recrawl-once /tmp/s1plus-m4b-measure/c1 1 "$P"` | `8ffa06d` | 1.58–1.80 / 1.69–1.73 / 1.75–1.77 |
| 100,000 (nominal 1%) | 13,600,192 + 128 | 7027.61 / 3236.60 / 260.38 | 10.57 (10.48–10.59) | 799.92 / 1435.52 | `$B recrawl-once /tmp/s1plus-m4b-measure/c100000 100000 "$P"` | `8ffa06d` | 1.49–1.76 / 1.66–1.72 / 1.74–1.76 |

Session setup remains separate and retains `4ae23c0`'s cached-key sorts; M4b does
not claim that earlier setup speedup. Setup opens/validates the view and builds
resident identity/hash/directory lookups once per session.

| Case | Session setup median (range), ms | Setup current / peak RSS, MiB | Source command | Commit | Load ranges (1 / 5 / 15 min) |
| --- | ---: | ---: | --- | --- | --- |
| No change | 3072.75 (3056.20–3096.50) | 721.59 / 871.19 | `$B recrawl-once /tmp/s1plus-m4b-measure/c0 0 "$P"` | `8ffa06d` | 1.66–1.78 / 1.70–1.73 / 1.76–1.76 |
| One file | 3081.04 (3068.30–3084.30) | 721.52 / 870.91 | `$B recrawl-once /tmp/s1plus-m4b-measure/c1 1 "$P"` | `8ffa06d` | 1.58–1.80 / 1.69–1.73 / 1.75–1.77 |
| 100,000 (nominal 1%) | 3063.01 (3052.55–3074.10) | 721.65 / 871.47 | `$B recrawl-once /tmp/s1plus-m4b-measure/c100000 100000 "$P"` | `8ffa06d` | 1.49–1.76 / 1.66–1.72 / 1.74–1.76 |

**Both no-change targets are met:** median post-setup **9.28 s** (all three samples
9.24–9.40 s, below 9.5 s), median whole-process peak **1.35 GiB** (below 1.6 GiB).
Against M4's original table, post-setup falls from 45.04 to 9.28 s and peak from
2.43 to 1.35 GiB. Setup is about **3.07 s**, giving roughly **12.35 s** for setup
plus replay/reconcile/publication in a fresh writer; a resident session amortises
setup. This satisfies the specified post-setup comparison with the 9.5 s full
build, rather than establishing a sub-9.5 s fresh-writer total.

No-change writes **0 B** and publishes no generation; one file writes **456 B**;
100,000 writes **13,600,320 B**, preserving the checkpoint. Remaining no-change
cost is about **6.69 s replay/local comparison** plus **2.57 s graph, seen/sweep
and final reconciliation**. Whole directory tables and compact name references
still dominate transient memory. The local buffer is bounded by the largest
listing per worker; prior dirty overlays and retained readers are outside these
clean-checkpoint timings. The filesystem observer's conservative content carry
still uses its resident identity lookup; these synthetic measurements do not
measure that filesystem observation cost. M6 owns narrower scopes, fully streamed
directories and smaller alias/reference storage.

### S1+ M5 — Typed coverage reconciliation (2026-10-05)

M5 resolves owned `IoOp`/context/error events after all walkers have joined.
Checked old directory/edge scopes discard every worker's untrusted observations
before alias grouping, and stop the old-name sweep at their boundaries. No
protected subtree is copied. Overlapping scopes collapse; new unreadable child
directories can be opaque, while replacements require an unchanged ancestor.
A relocated old directory widens protection to its checked owner root when its
old incoming path is no longer anchored. A protected file edge makes its parent
count unknown while trustworthy siblings update. Fresh hard-link observations
outside a scope can still update the shared inode. Successful recovery clears
coverage and retained-at in the same transaction; identical faults publish
nothing. Global policy/sniffer transitions under protection and uncertain new
root boundaries abort. Disjoint root removals remain valid.

The existing M1/M3 coverage flags and `RetainedAt` wire format were sufficient;
reader semantic validation remains unchanged (D53 recommendation A, as directed).
Tests use the real walker, crawl API, writer and disk reader, with M3's full-index
oracle. Retained expectations take actual rows from the pre-fault checkpoint.
The matrix covers namespace operations/errors/contexts, content I/O, vanished
children, invalid patterns, partial listings across four workers, new/replaced
and moved directories, overlapping scopes, stale counts, repeat/recovery,
policy/sniffer changes and protected hard links. The CLI test verifies retained
search results and find's live listing/metadata fallback. Workspace gates:
**521 passed / 4 ignored**, zero warnings; real log size/mtime unchanged.

Measured production **`3f7fc27`**, one warm-up and three recorded samples per
case, second round reversed. The no-change row uses the original **10,448,739-name**
M4b fixture at `/tmp/s1plus-m3-measure/index`, without modifications. Fault rows
start from the same separately prepared, writer-validated view: promote one
one-file leaf to a nested configured root and seed the default policy tag from
a real empty crawl. This changes one root boundary, preserving paths, inode and
document rows; the two scopes contain **1** and **10,448,737** names. Preparation
and private manifest/log resets are outside timing and write totals. The snapshot
is hardlinked and never rewritten.

Before every invocation, including preparation and warm-ups, the host-visible
runner checks `uptime` and
`pgrep -af 'harness.run|ferret_timing|ignore_timing|synthetic|ferret-bench'`,
excluding only runner ancestors and pgrep. No competing benchmark or compilation
ran during timing. Commands, all loads, environments, binary SHA-256s and samples:
`/home/dave/w/super-ferret/.ai/s1plus-m5-measurements/recrawl-samples.json`;
`recrawl-run.py` guards/runs it, and `report.py` produces the medians.

```sh
B=/home/dave/w/super-ferret-wt/s1plus/target/release/ferret-bench
P=/home/dave/w/super-ferret-wt/s1plus/target/release/examples/recrawl
export XDG_CONFIG_HOME=/tmp/s1plus-m5-measure/config
export XDG_DATA_HOME=/tmp/s1plus-m5-measure/data
export XDG_STATE_HOME=/tmp/s1plus-m5-measure/state
export XDG_CACHE_HOME=/tmp/s1plus-m5-measure/cache
# N is 0, fault-small or fault-large; each private index is reset first.
export FERRET_INDEX=/tmp/s1plus-m5-measure/${N}
$B recrawl-once "$FERRET_INDEX" "$N" "$P"
# One-time private fault preparation, before copying it to the two fault cases:
FERRET_INDEX=/tmp/s1plus-m5-measure/fault-base \
  $P /tmp/s1plus-m5-measure/fault-base fault-prepare
```

| Case | Log + manifest bytes | Post-setup median (range), ms | Final / peak RSS, MiB | Source command | Commit | Load ranges (1 / 5 / 15 min) |
| --- | ---: | ---: | ---: | --- | --- | --- |
| No change, original 10M fixture | 0 + 0 | 9198.74 (9197.28–9253.01) | 764.47 / 1386.92 | `$B recrawl-once /tmp/s1plus-m5-measure/0 0 "$P"` | `3f7fc27` | 1.27–2.32 / 1.22–1.49 / 1.08–1.18 |
| Protect 1 name | 176 + 128 | 5.49 (2.84–6.51) | 724.02 / 1049.66 | `$B recrawl-once /tmp/s1plus-m5-measure/fault-small fault-small "$P"` | `3f7fc27` | 1.47–2.17 / 1.26–1.50 / 1.09–1.18 |
| Protect 10,448,737 names | 176 + 128 | 6.07 (5.97–6.09) | 723.96 / 1049.78 | `$B recrawl-once /tmp/s1plus-m5-measure/fault-large fault-large "$P"` | `3f7fc27` | 1.59–2.08 / 1.29–1.49 / 1.10–1.18 |

| Case (same command, commit and load as above) | Session setup median (range), ms | Setup current / peak RSS, MiB |
| --- | ---: | ---: |
| No change | 3052.91 (3042.30–3082.83) | 721.84 / 871.19 |
| Protect 1 name | 3886.28 (3864.83–3909.61) | 721.95 / 871.06 |
| Protect 10,448,737 names | 3870.43 (3855.85–3895.21) | 721.95 / 871.64 |

No-change remains synthetic observation replay, including local equal-row
reduction: median **6622.18 ms replay / 2554.04 ms reconciliation / 0 ms commit**.
At **9.20 s** and **1.35 GiB** peak, there is no observed regression from M4b's
9.28 s / 1.35 GiB; all three runs remain below 9.5 s / 1.6 GiB. Different loads
and three samples do not establish a speedup.

Fault cases call the **real resident recrawl API** with a selected-root refresh
and get an actual kernel `OpenDir`/NotFound error on the nonexistent synthetic
root. The other root is kept, isolating retention from unrelated enumeration.
Median walk/reconciliation-publication is **0.07/5.31 ms** for the small scope,
**0.08/5.87 ms** for the large scope. Both append one boundary `DirPut` and a
manifest: **304 B** total. Protecting over ten million times as many names adds
about **0.58 ms**, within the small-scope observed range, and about **0.12 MiB**
peak: retention work depends on boundaries, not old descendant count. This does
not measure walking healthy siblings or a faulted listing's already-read prefix.
Fault setup includes the same prepared root-boundary overlay in both cases;
compare its cost within that pair, not directly with the clean no-change setup.

Peak RSS covers the whole producer, including setup, disk-reader coverage
verification and an identical-fault retry that must append zero bytes and
publish no generation. Those verification/retry steps are outside the first
recrawl's post-setup timer. Logical writes exclude filesystem block amplification.
All fault runs preserve inode/name/directory/document counts and checkpoint
size/mtime; cold disk replay confirms the retained-at marker.

## S1b — The engine, batch mode and the daemon

One engine: open the catalog resident (names and inodes read in full, indexes
mapped) and answer from memory (D46). Hosts: `ferret batch` (many queries in one
run: CI and the test suites) and `ferretd` (inotify with a re-crawl backstop,
directory entry counts kept current, idle-priority indexing, the politeness
controller from the research). A one-shot query starts the daemon, or builds the
engine in process when it cannot (D49).

**Measure:** open time and resident bytes per name at 10M, against D48's 1 GB
line.

## S1c — `ferret find` in find(1) syntax

POSIX.1-2024 `find` over the index, plus GNU extensions ranked by real use,
matching GNU `find` in `-I` mode and respecting ignore rules by default (D47).
The S1 atom grammar moves to `ferret search`. Tested with our own cases, written from what
the private differential corpus (`~/w/find-compat`) teaches, against GNU find,
bfs and fd.

**Measure:** the 10M catalog's resident size with full `find` support. Under 1
GB, with scan latency acceptable, means no name index (D48); otherwise a name
index experiment (suffix array, terms, trigrams) comes before S2.

**Built** (2026-10-02 to 2026-10-03, branch `wt/find`, milestones M1–M5c). The
contract is [FIND.md](FIND.md); the decisions are D47 and D50.

- **M1** (2026-10-02): the find parser (GNU leading options, every corpus
  primary and operator), C-locale byte globs, the evaluator and a sequential
  live walk. S1's atom grammar moved to `ferret search`. GNU differential: 36
  expressions.
- **M2a** (2026-10-02): actions and output: `-exec`, `-execdir`, `-ok`,
  `-okdir`, `-delete`, `-printf`/`-fprintf`, `-fprint*`, `-ls`/`-fls`, `-regex`
  with its dialects, `-H`/`-L`, `-xtype`, `-lname`. Differential: 297
  expressions.
- **M2b** (2026-10-02): the stat tests (`-perm`, `-size`, the time tests,
  `-newer*` including GNU's date forms, ownership, `-links`, `-inum`, `-empty`,
  access, `-fstype`). Differential: 108 expressions.
- **M3a** (2026-10-02): the GNU regex dialects in `ferret-verify`, with
  backreferences on a bounded search. 1,352 new regex expressions; the evaluator
  differential reached 1,718.
- **M3b** (2026-10-02): single-threaded live walk speed, mostly allocation. `-I`
  beat GNU on all twelve timing rows, and was within 2% of `bfs -j1` or faster
  on all but `-maxdepth 2`.
- **M4a** (2026-10-02 to 03): catalog format v3, with name-only rows for ignored
  names and a sparse Specials section for visible FIFOs, sockets and devices
  (D47's 4a brief).
- **M4b** (2026-10-03): default mode over the catalog, the `find_no_ignore` key,
  and refusal without a covering index. The full corpus after M4: `-I` 67,394
  agree, 20 differ, 6 harness failures; default 52,909 agree, 28 differ, 3
  harness failures. Every one of those rows was a harness artefact (F1, F2, F9),
  handled in find-compat's H1 slice.
- **M5a** (2026-10-03): default mode became a pure index query: catalog order and
  stored metadata (F7, F8, F10). Its full corpus found 399 new differ rows: 314
  order-only and 85 real action rows; 77 were fixed, and the other 8 led to F12.
- **M5b** (2026-10-03): the parallel walk with concurrent actions (F10 B, F11 A),
  on up to min(16, CPUs) workers. Full corpus: zero real differences, zero
  errors, 190 raw differ rows (61 default, 129 `-I`), each explained by hand.
- **M5c** (2026-10-03): shared `-exec +` batches, whole-entry output, the `-quit`
  latch, and sequenced starts for effectful expressions. Raw differ rows fell to
  46 (18 default, 28 `-I`).

**Measured** (300k-entry timing tree in find-compat, 32 CPUs, warm medians in
ms, fd 10.4.2). M5c's final table, 15 samples per cell, start load
3.11/2.39/2.66:

| Query                        | GNU     | bfs     | fd     | ferret -I | ferret |
| ---------------------------- | ------: | ------: | -----: | --------: | -----: |
| `-name *.c`                  | 204.061 | 80.203  | 27.234 | 31.701    | 10.925 |
| `-type f`                    | 163.122 | 53.899  | 28.556 | 32.650    | 21.073 |
| `-maxdepth 2 -mindepth 1`    | 14.320  | 3.815   | 12.398 | 3.357     | 7.355  |
| `-type f -size +1024c`       | 426.948 | 142.608 | 53.655 | 46.979    | 22.940 |
| `-print0`                    | 150.435 | 43.480  | 29.976 | 32.417    | 22.594 |
| `-mtime 0`                   | 438.028 | 163.259 | 49.482 | 51.110    | 23.347 |
| `-name *.c -o -name *.h`     | 244.356 | 116.896 | —      | 35.230    | 12.448 |
| `-path */d1*/* -name *.rs`   | 205.189 | 90.709  | —      | 29.920    | 22.556 |
| `-type d`                    | 147.352 | 36.051  | 23.764 | 31.470    | 9.629  |
| `-empty`                     | 497.313 | 248.014 | 47.467 | 55.022    | 22.085 |
| `-name *.py -newer ./README` | 243.451 | 193.020 | —      | 35.944    | 11.805 |
| `-regex .*\.\(c\|h\)`        | 310.941 | 88.624  | —      | 35.910    | 28.905 |

fd has no equivalent for the rows marked —. bfs's `-regex` row matches nothing,
so it does different work. Default mode was within 1.84% faster and 2.29% slower
than M5b on every row, which Dave accepted as noise. Per-cell loads are in
`find-compat/.scratch/ferret-impl/m5c/revised-timing.json`.

For comparison, on the same rows other than `-maxdepth`, M3b's single-threaded
`-I` took 117–471 ms, and M4b's default mode, which listed each directory live
to keep GNU's order, took 161–453 ms. Those figures drove F3, F7 and F10.

M5c's targeted probes, 15 samples, start load 2.71/1.93/2.53:

| Probe                                    | M5b default | M5c default | M5b `-I` | M5c `-I` |
| ---------------------------------------- | ----------: | ----------: | -------: | -------: |
| 368 read-only starts                     | 17.843      | 17.832      | 27.251   | 27.766   |
| 368 effectful starts, unreachable exec   | 31.915      | 39.398      | 28.152   | 133.455  |
| `-exec +` on name-selected files         | 24.121      | 21.998      | 46.694   | 36.760   |
| `-exec +` on every file                  | 74.055      | 76.600      | 47.793   | 54.168   |

Sequencing effectful starts costs real time on many small starts; a cheap guard
against donating narrow roots made it worse (148.730 against 135.290 ms `-I`)
and was removed. Before staging, `-exec +` on every file took 144.411/188.290 ms
(default/`-I`) against M5b's 75.811/49.490, which is what justified staging.

Catalog v3 (M4a) at 10M, against v2: 594,837,226 B against 592,577,217 B, all of
the 2,260,009 B difference in Names (+1,786,998 B), NameHeap (+472,995 B) and the
header (+16 B); 43,010 more names (the ignored ones, 0.41%); identical inode,
document and stat sections. Build 10.71 s against 10.12 s, peak RSS 1,640.7
against 1,630.4 MiB, at higher load (7.86 against 1.36). The final search guard
cost 1.2% on a warm full listing (1,348.46 to 1,365.20 ms) and 0.4–4.9% on the
other name queries. An adjacent-tag encoding measured 1,176,336 B (0.20%)
smaller and was rejected (D47).

**Where it stands** (2026-10-03). The last full corpus ran on bfe0d37's binary:
135,693 rows, zero errors, 47 differ (16 default, 31 `-I`), plus 9 rows where
both sides timed out, which sit outside the gate (F2).

- 45 rows are output from an order the contract allows, which the harness cannot
  yet prove. The find-compat harness slice H2 is teaching it to.
- 1 row is a `-L` alias race between concurrent `rm` commands, accepted under
  F11 A. (H2 names a second such command; it did not differ in this run.)
- 1 row, `77e2ba5a5ff3`, was a real gap: a read-only later start reported a
  missing path before an earlier start's `-quit`. 98d4b4c fixed it by sequencing
  starts under `-quit`; the corpus has not been re-run since.

Review fixes from the main-thread review (`REVIEW.md`) are in flight as R1. An
Astra review of the whole stretch follows, before `wt/find` merges to main.

## S2 — Content index

`ferret-text`, `ferret-index`, `ferret-verify`, `ferret-query`:

- The `CandidateSource` trait, reviewed before any structure is built on it.
- Tokenizer with identifier splitting (D9).
- Term postings on intpack; segments, manifest commit, merge; liveness from the
  catalog.
- `ferret search`: terms, boolean, phrase (postings + verify), composed with
  every S1 predicate.

**Measure:** index bytes per content byte; build CPU; query latency by term
class; phrase candidate counts (the positions question, D6). **Decides:** term
dictionary structure; whether positions become an experiment row.

## S3 — Regex

- Cox regex → trigram query derivation (the
  [cox-trigrams course](research/grok/cox-trigrams/index.html) covers it).
- Per-doc trigram filters and trigram postings, both as `CandidateSource`s.
- `ferret-bench` with the first experiment: filters versus postings, and per-doc
  term filters versus postings (D6, D8).

**Measure:** bytes, build CPU, cold and warm regex latency on `$HOME`.
**Decides:** which structure ships as default, and which ship behind the opt-in
comparison.

## S4 — Agent skill

A Claude Code skill that routes AI agents' file and content search through
`ferret` (JSON lines, stable exit codes, byte offsets). Its usage log is the
second source of real queries.

## S5 — Daemon for the content index

S1b's daemon extended to the content index: postings kept current from its
change events, hot index files resident (D14).

## S6 — Opt-in experiments and metrics

Side-by-side mode for any structure S3 left undecided, and opt-in upload of
comparison logs and the query log. Transport and privacy design at that point.

## Later

Extraction: PDFs, image and video metadata out of the box, then a plugin
interface for other formats. TUI (ratatui) and GUI (Tauri, or not — undecided on
purpose). Semantic search as a scoped plugin. Symlinks matched through their
targets' content, then links that pull content in from outside the roots (D18).
