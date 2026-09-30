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
- `ferret find`: name substring, glob and regex by scanning the name heap (D14);
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
method as the baseline: `crates/ferret-bench/scripts/findbench.py`, isolated
XDG dirs, one run at a time, 32 threads, NVMe, ext4; the medians are of 3
evicted and 5 warm runs). The baseline is the same harness on the version-1
catalog. Machine: load average 3-5 (another session's agents alive, none
compiling), so read the times as plus or minus 20%; the byte counts are exact.
`$HOME` has drifted: 445,194 names then, 445,442 now (the 10M is built from a
fresh dump, 10,245,189 names against 10,239,255), so compare B/name and
ratios. The "after" `find`, `open` and build figures are from the perf
fixes that followed the first after-measurement (below the tables), re-run on
the same two catalogs at load average 2.1-4.3 with nothing compiling; the
catalogs are byte-identical to what the fixed build writes. Evicted times
swing most (`test` at 10M evicted read 1,216 ms and 788 ms in two runs of one
binary). The `index` rows are the first after-measurement's and were not
re-run. The synthetic now carries the walker's real `nlink` and, for each
directory, the count of its children in the dump (a lower bound on the raw
`getdents` count, since the dump omits what the walker never lists); every
copy shares those values, which is what makes the packed widths a fair worst
case only for inodes and dev, not for sizes and times.

| measure                                     | before `$HOME`      | after `$HOME`       | before 10M        | after 10M         |
| ------------------------------------------- | ------------------- | ------------------- | ----------------- | ----------------- |
| catalog bytes                               | 47.3 MB, 106.2 B/n  | 28.4 MB, 63.7 B/n   | 1,202 MB, 117.4   | 799 MB, 78.0      |
| Names (offsets + lengths)                   | 12.00 B/n           | 7.50                | 12.00             | 9.13              |
| NameHeap                                    | 24.13               | 24.13               | 24.13             | 24.13             |
| DirNames                                    | 0.70                | 0.42                | 0.70              | 0.53              |
| Entries (new: raw directory counts)         | —                   | 0.26                | —                 | 0.26              |
| inode columns (was `Inodes`, fixed 64 B)    | 64.00               | 26.00               | 64.00             | 27.38             |
| States, Traversed, Strings, Links, WorkTrees | 0.48               | 0.48                | 0.32              | 0.32              |
| Docs                                        | 4.91                | 4.91                | 16.29             | 16.29             |
| `ferret-bench open`, name sections, warm    | 5.0 ms (16.5 MB)    | 7.1 ms (14.4 MB)    | 207 ms (378 MB)   | 254 ms (347 MB)   |
| same, evicted                               | 18.2 ms             | 20.0 ms             | 333 ms            | 371 ms            |
| `load_all`, warm / evicted                  | 9.1 / 42.4 ms       | 9.7 / 37.7 ms       | 472 / 840 ms      | 439 / 784 ms      |
| `index`, first run / re-run                 | 55.6 s (cold-ish) / 0.50 s | 43.1 s (cold-ish) / 0.87 s (a) | —  | —          |
| `index` peak RSS, first / re-run            | 78 / 161 MB         | 84 / 100 MB         | —                 | —                 |
| `synthetic` build, time / peak RSS          | —                   | —                   | 9.4 s / 1,748 MB  | 9.7 s / 1,880 MB  |
| same, CPU (user + sys), same sitting        | —                   | —                   | 7.3 s             | 9.5 s             |

(a) Measured before the build fix; not re-measured on the final code, as the machine was
not quiet (load 5, another session's find-compat harness running).

Per name the inode row went from 64 B to 26 B on `$HOME` and 27.4 B on the
synthetic (nlink and the entry counts are new and included). Widths in the
10M catalog: ino 30 bits, size 35, mtime 31, mtime nanoseconds 30, ctime 24,
ctime nanoseconds 30, nlink 10, doc 23, dev 0 (one value), mode 5 (20 values),
owner 1 (two values). The nanosecond columns are 7.6 B of the 27.4 and cannot
pack: they are entropy. `nlink` costs 10 bits on every name for one outlier
directory (an entry near 1,000); a frame-of-reference or exception scheme would
take it to about 1 bit. Ino at 30 bits is the synthetic's copy stride
(`$HOME` alone is 25 bits, 3.1 B).

| `find` (fresh process, ms)         | before `$HOME` | after `$HOME` | before 10M | after 10M |
| ---------------------------------- | -------------: | ------------: | ---------: | --------: |
| `flamegraph` fresh                 | 12.3           | 14.2          | 243        | 295       |
| `flamegraph` evicted               | 22.6           | 23.9          | 380        | 406       |
| `test` fresh                       | 30.5           | 15.6          | 643        | 362       |
| `test` evicted                     | 110.0          | 26.1          | 2,324      | 492       |
| `size:>100M` fresh                 | 29.4           | 21.2          | 612        | 453       |
| `size:>100M` evicted               | 50.4           | 34.0          | 924        | 613       |
| `*` fresh                          | 110.0          | 97.2          | 2,488      | 2,315     |
| `*` evicted                        | 131.0          | 105.1         | 2,825      | 2,417     |
| peak RSS, name queries             | 19-46 MB       | 17 MB         | 365-991 MB | 333-334 MB |
| bytes read, name queries           | 16.6-45.5 MB   | 14.4 MB       | 380-1,046 MB | 347 MB  |

Peak `find` RSS at 10M is now 333 MB for a name query and 372-376 MB with a
metadata atom, a third of D48's 1 GB line (it was 990 MB). The 15 MB Python
floor is in every RSS figure on both sides. `find --json` reads the three
columns it prints: `flamegraph` 0.33 s, `test` 0.49 s, 452 MB at 10M.

What changed the numbers, separated as far as the data allows (`perf` on the
10M catalog, user space only).

- **Bytes read.** A row carries no decoded inode now; a caller that prints
  metadata loads those columns and reads them by the row's inode id. So a name
  query reads only the name sections (347 MB at 10M, against v1's 380 MB for a
  rare needle and 1,046 MB for a common one, which paid single 64 B row reads).
  The first after-measurement loaded all twelve inode columns (283 MB) for the
  first reported row: 630 MB for every query, the larger half of the
  rare-needle regression (0.26 s of system time against 0.14 s now).
- **Decode.** The rest is unpacking. Validating the name sections decodes
  every packed offset, parent and child where v1 read aligned `u32`s: a
  `flamegraph` query spends 0.14 s of user time against v1's 0.09 s (0.18 s
  before the fixes, which decoded runs of 64 values in one pass instead of
  one value at a time in two). That, not bytes, is why `flamegraph` is still
  50 ms behind v1 while every query that reports many rows is ahead.
- **`*`** went from 2.49 to 3.70 s and is now 2.32 s: about 1.1 s was
  decoding every inode field for rows that print only a path, and 0.2 s was
  finding each name's end by a NUL search, which the next name's offset now
  gives.
- **Build.** The 19.4 s first measured was mostly load (56% CPU at load
  average 8) and `fsync` of the 800 MB file, which alone varies from 0.5 to
  8 s between runs; v1 in the same sitting ranged 7.5-10.4 s. On CPU, S1a
  took 10.9 s against v1's 7.3 s: each stat column is its own pass over the
  inode rows, and each pass found each row's batch by binary search again.
  Resolving each row's stat once (8 B per inode, allocated below the build's
  peak) took 1.2 s off, a cheaper packer 0.2 s. Of the 2.2 s that remain,
  0.4 s is the synthetic's own filling (it now counts entries and reads
  nlink); the rest is ten scattered passes where v1 made one, plus the pass
  that sizes columns and collects dictionaries. Closing it means holding
  encoded columns in memory, which D40 rules out.

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
matching GNU `find` except that ignored paths do not exist (D47). The S1 atom
grammar moves to `ferret search`. Tested with our own cases, written from what
the private differential corpus (`~/w/find-compat`) teaches, against GNU find,
bfs and fd.

**Measure:** the 10M catalog's resident size with full `find` support. Under 1
GB, with scan latency acceptable, means no name index (D48); otherwise a name
index experiment (suffix array, terms, trigrams) comes before S2.

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
