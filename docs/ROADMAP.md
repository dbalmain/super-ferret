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

## S1a — Catalog compaction (done 2026-09-30)

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

After S1a, ahead of the daemon (D40): a re-run writes what changed rather than
the whole snapshot (D26 B's change log over A's snapshot), so a refresh costs
the change and not the catalog. The daemon's small inotify bursts need it.

**Measure:** bytes written and time for a one-file change at 10M entries.

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
