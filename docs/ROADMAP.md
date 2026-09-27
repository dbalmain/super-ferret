# Roadmap

The order of work. Each slice ships something usable and ends with a measurement
that can change the next slice. Design detail is in [DESIGN.md](DESIGN.md),
decisions in [DECISIONS.md](DECISIONS.md).

Order follows D3: a usable tool first, so query and usage data accumulate while
the index is built; then the index; then the agent skill. The daemon comes after
the skill; extraction plugins, TUI and GUI much later.

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

## S1 — Find, faster (first usable)

`ferret-policy`, `ferret-crawl`, `ferret-catalog`, and the CLI:

- `ferret index [roots]`: crawl, apply ignore rules (D13), write the catalog; a
  re-run applies only changes. Hashing and doc ids are assigned here (D4) so S2
  starts from a populated catalog.
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
open. The full tables are in the slice 5b done-note.

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

## S5 — Resident daemon (optional)

`ferretd`: inotify with re-crawl backstop, catalog and hot index files resident
(D14), idle-priority indexing, the politeness controller from the research. The
CLI keeps working without it.

## S6 — Opt-in experiments and metrics

Side-by-side mode for any structure S3 left undecided, and opt-in upload of
comparison logs and the query log. Transport and privacy design at that point.

## Later

Extraction: PDFs, image and video metadata out of the box, then a plugin
interface for other formats. TUI (ratatui) and GUI (Tauri, or not — undecided on
purpose). Semantic search as a scoped plugin. Symlinks matched through their
targets' content, then links that pull content in from outside the roots (D18).
