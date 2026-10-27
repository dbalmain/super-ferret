# Decisions

The living decision record for Super Ferret. Every open question is written in
one shape — question, named options, the tradeoff per option, a recommendation
and the one fact that would change it — and answered questions stay here with
their answer, so the record survives the conversation that produced it.

Predecessors, carried forward where still open:

- [`research/claude/architecture.html`](research/claude/architecture.html) §
  Open questions (Q1–Q7, 2026-09-05)
- [`research/grok/decisions.html`](research/grok/decisions.html) (survey
  scoping, 2026-09-04)

## Status

| Id  | Question                                                               | Status         | Answer                                                                                                                          |
| --- | ---------------------------------------------------------------------- | -------------- | ------------------------------------------------------------------------------------------------------------------------------- |
| D1  | Repository shape                                                       | answered       | A: one repo, workspace under `crates/`; crate boundaries get the most design thought                                            |
| D2  | Format of the living documents                                         | answered       | A: Markdown living docs; research HTML under `docs/research/`; intpack pages copied to `docs/intpack/`                          |
| D3  | Build order: index first, or the no-index tool first                   | answered       | B: usable tool first, to start collecting data                                                                                  |
| D4  | What identifies a document                                             | answered       | ordinal doc ids in add order; doc → hash → inodes → names (restatement confirmed)                                               |
| D5  | Where the mutable state (paths, inodes) lives                          | answered       | A: own catalog; memory budget configurable, set by experiment                                                                   |
| D6  | What the index holds: postings, filters, positions                     | answered       | every structure is a candidate filter, verified by scan; trade-offs by experiment                                               |
| D7  | Positions                                                              | merged into D6 |                                                                                                                                 |
| D8  | Regex at first ship                                                    | answered       | not a bare scan: trigram filters (B) or postings (C), by experiment                                                             |
| D9  | What a term is                                                         | answered       | B: identifier splitting, filenames especially                                                                                   |
| D10 | Which roots                                                            | answered       | A: configured roots; `.gitignore` respected, `.ferretignore` and global overrides                                               |
| D11 | `unsafe` posture and the intpack dependency                            | answered       | flexible: no blanket `forbid`; SIMD where it pays; intpack may be vendored                                                      |
| D12 | Licence                                                                | answered       | A: `MIT OR Apache-2.0`                                                                                                          |
| D13 | Ignore rules: precedence, and whose matcher                            | answered       | A: `ignore` crate behind `DirRules::decide`; `!` un-ignores over an ancestor `.ferretignore` or a `.gitignore`                  |
| D14 | Filename search: scan the names, or index them                         | answered       | C, scan first; an optional resident daemon keeps names warm                                                                     |
| D15 | Result unit: per path or per document                                  | answered       | per path; a view may group (e.g. image search, once per content)                                                                |
| D16 | Replace `ignore` with our own gitignore matcher                        | adopted        | A, adopted: own matcher in `ferret-policy`, no dependencies; at or below `ignore` on every measured rule set                    |
| D17 | Whose regex engine, and when                                           | reopened       | S3: trigger fired (warm rare-literal verify 97% of wall, engine 57%); rec keep A until stronger screening is measured           |
| D18 | Symlinks: catalogue as links, and what they match                      | deferred       | links catalogued as links now (target text stored); reverse map and content matches later; no pull-in                           |
| D19 | Ignore matching: whole paths, or per-directory rule sets               | answered       | B-flat-indexed: one shared, indexed rule list per directory; last match wins                                                    |
| D20 | Walk across mount points, or stay on the root's device                 | answered       | A: cross mount points below a root, as now                                                                                      |
| D21 | Walk by path, or by directory handle                                   | answered       | B: `rustix` handles for every operation below the root, now                                                                     |
| D22 | A root inside a git work tree                                          | answered       | A (revised 2026-09-27): nothing above a root applies; a root inside a work tree is treated as outside one                       |
| D23 | Recording work trees, so duplicate results can be hidden               | answered       | A: a `worktrees` table; collapse identical copies by `DocId` by default                                                         |
| D24 | How many walk workers by default                                       | answered       | B: `min(16, cores)`, fastest cold (2.50 s vs 3.08 s at 8); `default_workers()`                                                  |
| D25 | A configured root that git ignores                                     | answered       | B: a configured root is always walked; rules apply below it                                                                     |
| D26 | A re-run: rebuild the snapshot, or mutate through a log                | answered       | A′: rebuild each run; any walker fault blocks publication until faults are typed                                                |
| D27 | `InoId` and `NameId`: stable, or renumbered                            | answered       | A: renumber each snapshot; C once indexing is incremental                                                                       |
| D28 | Name layout: raw sorted by parent, or front-coded                      | answered       | A: raw NUL-terminated heap by (parent, name); suffix array a later experiment                                                   |
| D29 | How a parallel walk tells the catalog each parent                      | answered       | A: the walker carries a per-directory value                                                                                     |
| D30 | What `ferret find` builds when it opens the catalog                    | answered       | C: lazy, plus a persisted directory `InoId → NameId`; cold find measured                                                        |
| D31 | One inode, several names                                               | answered       | C: one row per file inode, per directory name; shared per-run hash cache                                                        |
| D32 | A reader while `ferret index` runs                                     | answered       | A: generations plus a single-writer lock                                                                                        |
| D33 | What the walker must also hand the catalog                             | answered       | A: walker hands over the directory handle and work-tree kind                                                                    |
| D34 | Which roots a re-run replaces                                          | answered       | B: roots stored with the index; explicit removal; innermost root owns its subtree                                               |
| D35 | Work-tree context for a root inside a repository                       | answered       | moot: with D22 A, no work-tree context above a root                                                                             |
| D36 | Dead documents before there is an index to merge                       | answered       | B for S1: drop dead docs, keep the id counter; reactivation is S2's call                                                        |
| D37 | Remembering why a file has no document                                 | answered       | B: 2-bit content state plus sniffer version                                                                                     |
| D38 | Reading the snapshot: whole file, or by section                        | answered       | B: section-wise positional reads, in slice 5 (via D40)                                                                          |
| D39 | A checksum over the snapshot                                           | answered       | A: none in S1; revisit with incremental indexing                                                                                |
| D40 | What entry count S1 is built for                                       | answered       | B: about 10M catalogued entries, measured at about 40M                                                                          |
| D41 | The name scanner: `memchr::memmem`, or our own                         | answered       | B: own case-folding filter, AVX2 + SWAR, vs memmem control                                                                      |
| D42 | A re-run's memory: the previous generation held                        | replied        | A stands; reply explains carry-over and kept roots                                                                              |
| D43 | A 10M name query costs its section loads, not its scan                 | answered       | B later: compaction, then a name index, then mmap                                                                               |
| D44 | Single inode reads against an evicted catalog                          | answered       | superseded by D46                                                                                                               |
| D45 | Query text in the local log                                            | answered       | A: keep query text; switch with the config file                                                                                 |
| D46 | Is the daemon the only mode of operation?                              | answered       | C, plus a batch mode for CI and tests                                                                                           |
| D47 | `ferret find` as a drop-in for find(1)                                 | answered       | C: find semantics except ignore rules                                                                                           |
| D48 | The next move after S1                                                 | answered       | A: compaction; name index only if 10M misses 1 GB                                                                               |
| D49 | A one-shot query with no daemon running                                | answered       | A: spawn on first use, in-process fallback                                                                                      |
| D50 | `ferret find` stretch calls F1–F13                                     | answered       | F10 B free order, F11 A concurrent actions, F8 B stored stat, F12 D, F13 A                                                      |
| D51 | Compaction while the watcher is busy                                   | answered       | A: idle-boundary compaction under the writer lock; pause measured and reported in M7                                            |
| D52 | D27 C: ids across compaction                                           | answered       | B: epoch-scoped InoId/NameId; DocId stays stable                                                                                |
| D53 | Cold-open overlay validation                                           | answered       | A for M5/M6; evaluate C with M7 if cold-open budget warrants the durable index                                                  |
| D54 | In-memory names: interning, postings, row order                        | answered       | B after S1+ merges; prototype passed the scoped check (worst 7.4 ms, bar 10 ms)                                                 |
| D55 | Storing mtime as an order rather than a value                          | answered       | D + F: seconds-only ordered dictionaries for mtime and ctime, exact per-block max, racy rule; no ordered structure              |
| D56 | Where an indexed find with actions executes                            | answered       | A: action queries run in the client through the shared engine; routing exception to D49                                         |
| D57 | Socket codec: shared JSON lines or binary framing                      | answered       | A: one tagged JSONL codec for batch and the socket; measure before considering binary                                           |
| D58 | Batch input: serde_json or a hand-written reader                       | answered       | B: hand-written request reader with a fuzz target and round-trip property test                                                  |
| D59 | How intpack enters the workspace                                       | answered       | A: git dependency pinned by rev; vendor only for project-specific changes; crates.io later                                      |
| D60 | Reactivate a dead DocId when its content returns                       | answered       | B now: never reactivate; A′ (reactivate by content hash, no git) is the named upgrade; git-index idea dropped (saves ~3%)       |
| D61 | Term frequencies in S2's postings                                      | answered       | B: doc ids only for now; frequencies arrive with ranking, by reindex                                                            |
| D62 | `ferret search` syntax for content and booleans                        | answered       | A: `text:` atoms; find-style `OR`, `NOT`, `(` `)`; side survey of query languages done; `ferret grep` follows ripgrep (D72–D75) |
| D63 | Tokenize in a follow pass, or in the crawl's hashing read              | answered       | A: the index follows the catalog in DocId order; D60's experiment found git does not remove the re-hash in a way that matters   |
| D64 | Segment bytes: positional reads or `mmap`                              | answered       | A: positional reads into owned buffers; no `unsafe` or `mmap` until measurements justify it                                     |
| D65 | The term dictionary is 87% of the index                                | answered       | blocks + block-prefix FST; slice C: codec 1 stays; tokens: T1, blob `text:` via trigrams, no date index                         |
| D66 | Phrase queries read too much: positions, or not                        | answered       | D: per-document Bloom 8 over adjacent pairs, pairs only, streaming fold; agent transcripts out of the content index             |
| D67 | How `ferret search` spells a content regex, and when it refuses        | answered       | (a) A2: `grep:REGEX`, case-sensitive, `(?i)` inline, line anchors; (b) i: refuse past live/4 Maybe, `--scan`                    |
| D68 | Per-document filters: a screening stage, terms and trigrams separately | answered       | B: neither filter built (M1b: trigram filters 1.20× postings; term filters 0.31× bytes but 5–6,343× slower)                     |
| D69 | Saturated large documents in trigram postings                          | answered       | A: index every document's trigrams (M1b: no threshold is both large and fast)                                                   |
| D70 | How large the trigram follow buffer may grow                           | answered       | B: one shared 64 MiB budget (follow RSS 458→248 MiB, +1.1% steady term index); budget to become configurable                    |
| D71 | Agent habits `ferret search` silently gets wrong                       | answered       | A: operators in any case; `content:` aliases `text:`; operator-in-argument error only when another prefix follows               |
| D72 | `ferret grep`: scan broad queries automatically?                       | answered       | A: grep scans when the index can't narrow; works without trigrams, documented as no faster than rg                              |
| D73 | `ferret grep`: what freshness does indexed grep promise?               | answered       | A+D: snapshot by default, plus `--live` keeping ferret's ignore rules                                                           |
| D74 | `ferret grep`: may `-g`/`--hidden` recover excluded paths?             | answered       | A: intersect with D47 visibility; `.ignore`/`.rgignore` a possible later feature                                                |
| D75 | `ferret grep`: BOMs and UTF-16, or raw bytes only?                     | answered       | D: strip a leading UTF-8 BOM for every input; UTF-16 a known difference from rg                                                 |

What the research already measured, and this record assumes (M1, 2026-09-04, on
`~/w`): 578,200 files / 153 GB, of which 96% of bytes are build output; after
exclusion 73,565 files / 5.73 GB, and 55 CSV/DB files hold 72% of that. ripgrep
answers a hard regex over the surviving tree in 50 ms warm. The content index
therefore earns its bytes on cold cache, on extracted documents, on ranking and
on corpora that are not build output — not on warm regex latency. The stage-0
measurements the architecture asked for (cold scan of the text tier; the same
census over `$HOME`) have **not** been run.

---

## D1 — Repository shape

**Question:** Does the code live in this repository alongside the documentation,
and how do extractable components graduate?

| Option                                                                                                                                      | Costs                                                                                          | Buys                                                                                                                                 |
| ------------------------------------------------------------------------------------------------------------------------------------------- | ---------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------ |
| A. One repo: cargo workspace under `crates/`, docs under `docs/`; a crate moves to its own repo when it is reusable (the intpack precedent) | A graduation is a history split. Workspace-wide gates run over everything.                     | One clone tracks the project from any machine. Design and code change in one commit. Extraction stays a real option, not a pretence. |
| B. Separate repo per crate from the start, this repo holds docs only                                                                        | N repos to keep in step; path/git deps everywhere; a design change touches two or three repos. | Each crate is publishable on day one.                                                                                                |
| C. Code elsewhere, this repo is documentation                                                                                               | The roadmap and the code drift; decision history references a tree it cannot see.              | Nothing A does not.                                                                                                                  |

**Recommendation:** A. intpack already shows what "extractable" means in
practice: it was built as its own crate with its own bench, and nothing here
depends on where it lives. The fact that would change it: if a second person is
expected to work on one crate without the rest — not the case.

> Dave: Let's go with A. Splitting into crates will be important though. See
> /review-craft for what I care about in terms of structure. The orthogonality I
> try to achieve will be challenging here because the query planner for example
> will need to know about how the index is structured. That is where I think
> we'll need to put the most thought.

**Answer (2026-09-23): A.** The planner/index coupling is the hardest boundary;
the proposed seam is in D6's answer — the planner sees the index only as
candidate sources with an exactness flag and a cost estimate, never as a format.

## D2 — Format of the living documents

**Question:** Are the roadmap, design and this record Markdown or HTML?

| Option                                                                                                                                            | Costs                                                                                                                  | Buys                                                                                                   |
| ------------------------------------------------------------------------------------------------------------------------------------------------- | ---------------------------------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------ |
| A. Markdown for living docs; the HTML research set stays as it is under `docs/research/`; intpack's results and decisions pages copied in as HTML | Two formats in one tree. The HTML pages need GitHub Pages (or a local browser) to read from another machine.           | Renders on GitHub, diffs in review, editable by any agent without a design system. Matches `GOALS.md`. |
| B. Everything HTML in the research set's design system                                                                                            | Every edit is a page rebuild; diffs are unreadable; no rendering on GitHub without Pages.                              | One consistent document set.                                                                           |
| C. Markdown everywhere, research converted                                                                                                        | The research pages are ~600 KB of styled tables and provenance chips; conversion loses the chips, which are the point. | One format.                                                                                            |

**Recommendation:** A. Copy the intpack results and decisions pages into
`docs/intpack/` rather than linking claude.ai, so the record does not depend on
a hosted artifact. Enable GitHub Pages over `docs/` only if it turns out to be
wanted. The fact that would change it: if the design itself needs the density
bars and provenance chips — it will need tables and numbers, and Markdown
carries those.

**Answer (2026-09-23): A.** Research moved to `docs/research/{claude,grok}/`;
intpack's results and decisions pages copied to `docs/intpack/`.

## D3 — Build order

**Question:** Does the index come first (your ordering), or does the
architecture's stage 1 — filename/metadata search plus a scanner, no content
index, used for a month to produce a query log — come first?

| Option                                                                                                                                                                | Costs                                                                                                                                                                         | Buys                                                                                                                                                               |
| --------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------ |
| A. Index first: catalog (D4/D5) + content postings + a one-shot batch indexer (`ferret index`) + CLI query. No daemon, no watcher. Reconcile by re-running the crawl. | No query log before the index exists, so D6/D7/D9 are decided from the research and the bench, not from use. The politeness work (the thing that killed the prior art) waits. | The learning payload first. Every experiment in D6 needs this anyway. The catalog is required for the identity chain, so filename search arrives with it for free. |
| B. Architecture order: exclusion engine + crawler + catalog + scanner, then daemon, then index                                                                        | 4–6 weeks before a posting list exists. Two stages of "not the interesting part" first.                                                                                       | A shippable tool at week 3 that answers `find` and `rg` queries; a month of real queries before choosing tokenizer and structure.                                  |
| C. A, but spend the first day on the stage-0 measurements (cold scan of the text tier; `$HOME` census) before writing code                                            | One day.                                                                                                                                                                      | The two numbers that gate D8 and D10; the same script that produced M1.                                                                                            |

**Recommendation:** C. The catalog is the part of the architecture's stage 1
that the identity chain needs anyway, so A already contains most of B's first
stage; what it drops is the month of usage, and the fact that would change the
answer is whether you intend to use the tool daily while it is being built. If
yes, B's ordering pays for itself; if this is a build-then-use project, C.

> Dave: Let's go with B - I want to use it ASAP so we can start collecting data.
> On this topic, we should respect .gitignore files but have .ferretignore files
> which can override .gitignore by force including or force ignoring.

**Answer (2026-09-23): B.** First usable slice: exclusion rules, crawler,
catalog, filename and metadata search, and a local query log — used daily while
the index is built behind it. Ignore rules moved to D13. Because D8 rules out a
bare scan over `$HOME`, content search arrives with the first index slice rather
than as a stop-gap scanner.

## D4 — What identifies a document

**Question:** In `document → hash → inode(s) → name → dir inode → … → root`,
what is the primary key of a document, and what follows from it?

| Option                                                                                                                                                                        | Costs                                                                                                                                                                                                                                                                                                                                                                   | Buys                                                                                                                                                                                                                                                                                                                              |
| ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| A. Content-addressed: doc id ← (content hash, extractor version, tokenizer version). `(dev, ino) → doc`. `(parent ino, name) → child ino`. Paths resolved by walking parents. | Every indexed file is hashed in full (it is being read in full to tokenize, so the marginal cost is the hash). A result is a (doc, path) pair, not a path: a doc with three names is three results. Metadata predicates (`ext:`, `path:`, `mtime:`) live on the inode/name, not the doc, so composing them with term postings needs a doc↔inode mapping at query time. | Rename or move of a file or a whole directory is one row update; a cross-filesystem move (copy + delete) reuses the doc. Duplicate content is indexed once. A changed tokenizer is a reindex keyed by version, not a migration. Inode reuse (a new file landing on a recycled number) is detected by hash mismatch, not by trust. |
| B. Inode-addressed: doc id ← `(dev, ino)`; hash kept only to skip re-tokenizing unchanged content                                                                             | A duplicate file is indexed twice. Cross-filesystem moves reindex. Inode reuse after delete is a correctness hazard: `(dev, ino)` alone is not an identity on Linux — it needs `ctime` or a generation number alongside.                                                                                                                                                | Simpler results: one doc, one path. Simpler query-time composition.                                                                                                                                                                                                                                                               |
| C. Path-addressed                                                                                                                                                             | Every rename is a reindex — the thing the chain exists to avoid.                                                                                                                                                                                                                                                                                                        | Nothing.                                                                                                                                                                                                                                                                                                                          |

**Recommendation:** A, with two things stated now because they shape the index:
(1) the query engine's unit of result is `(doc, name)`, and metadata filters are
evaluated per name and then mapped to docs (a doc passes if any of its names
passes); (2) content that changes in place produces a new doc id and tombstones
the old one unless another inode still holds the old hash — so postings churn on
edit is the same as any index, and the chain's saving is on rename, move and
duplication only. Hash function: BLAKE3 (one dependency, SIMD, cryptographic so
dedup cannot collide by accident) over vendoring an xxh3; the fact that would
change it is a decision to take zero dependencies in the core, in which case
xxh3-128 is a few hundred lines.

**Expand on:** whether you expect results per path or per document, and whether
hard links and bind mounts on your machines make the many-names case common or a
corner.

> Dave: the documents are indexed from 0 as they're added. This will help
> optimise postings lists. I think we might need to discuss this one as I
> thought that was clear from my explanation. When I add
> /home/dave/sandbox/file.txt, and it is the 3rd document I've added, it gets a
> document id of 2. 2 maps to the hash of the file which maps to all inodes that
> match that hash. If I later add another file with the same hash, it gets added
> here. If I update the file, the hash will change so index 0 will no longer
> point at that hash (which may need to be grave-posted) and the new hash will
> be updated to point at the new inode (or if the hash already exists, 2 will
> point at the existing hash and the inode will be added to that hash).

**Restated (2026-09-23), confirmed by Dave the same day.** Option A's wording,
"doc id ← hash", read as the hash _being_ the id; it meant keyed by, and your
model is the right statement of it:

- **Doc ids are ordinals**, assigned densely in the order content is first
  added, which is what the postings codecs want. A first crawl runs in directory
  order, so the first build is close to path-sorted for free.
- **`doc → hash` is 1:1 and never changes**, because a doc's postings describe
  one content. `hash → {inode}` and `(parent inode, name) → inode` are the only
  edges that move.
- **Rename or move** (file or directory) edits one name edge. No postings touch.
- **Duplicate content** adds an inode to an existing hash. No postings touch.
- **Edit in place**: the inode leaves hash H and joins H′. If H′ is new it gets
  the next doc id and is indexed; if H′ already exists the inode just joins its
  set. If H is left with no inodes, its doc id is tombstoned (your
  "grave-posted") and reclaimed at merge. (Your text says "2 will point at the
  existing hash" — I read that as _the inode_ pointing at it, since doc 2's
  postings describe the old content.)
- **Tokenizer or extractor change**: a reindex assigning new ordinals; versions
  are recorded per segment, not folded into the hash.

Consequence: two id spaces. Content postings are over doc ids; filename terms
and metadata (`ext:`, `mtime:`, `path:`) are over names/inodes in the catalog. A
query mixing them joins across `doc → hash → inodes → names`, so the catalog
needs a fast doc → inodes lookup, and D15 asks which side a result lives on.
Hash: BLAKE3 unless you want zero dependencies there.

## D5 — Where the mutable state lives

**Question:** The names tree and the inode table change on every rename;
postings are immutable segments. What holds the mutable part?

| Option                                                                                                                                                                                       | Costs                                                                                                                                                                                                                                                 | Buys                                                                                                                                                                        |
| -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| A. Own catalog: an mmap-able flat snapshot (fixed-width inode and name records, front-coded names) plus an append log; a reader loads the snapshot and replays the log; the indexer compacts | It is a small database and you own its recovery. Snapshot rewrite on compaction. The daemon, when it exists, holds the log tail in memory; the snapshot is page cache. ~1M files × ~48–64 B ≈ 50–64 MB on disk; RAM is whatever the kernel keeps hot. | No dependency. The shape FSearch and Everything use. The CLI reads it with the daemon stopped. The path column is never inside a segment, so rename never touches postings. |
| B. Embedded KV (redb)                                                                                                                                                                        | A dependency in the core, and its file format is its own. Ordered keys, transactions, crash safety for free.                                                                                                                                          | Weeks of not writing a database.                                                                                                                                            |
| C. Path column inside the immutable segments; rename = rewrite the path doc value for the affected docs                                                                                      | A directory move of 50k files is 50k doc-value rewrites, and a new segment generation for each batch.                                                                                                                                                 | One storage model.                                                                                                                                                          |

**Recommendation:** A. The architecture's Q7 recommended redb for the work queue
on the grounds that the queue is infrastructure; the catalog is not — it is the
identity chain, and it is where filename search (`find` replacement, `GOALS.md`)
is answered, which argues for owning it. The fact that would change it: if the
daemon's resident-memory budget is tight enough that even a log tail plus
page-cache-hot snapshot is too much, which is a number you should name (see
below).

**Expand on:** the daemon RSS you will tolerate at idle and during a crawl.
"Nice to CPU" is in the goals; the memory number is not.

> Dave: Agree strongly with A here. We need to really own this storage
> mechanism. We need to experiment to know what memory numbers are tolerable and
> make it configurable.

**Answer (2026-09-23): A.** The memory budget is a config value; its default
comes from an experiment row (catalog RSS and query latency against budget).

## D6 — What the index holds: postings, filters, positions

**Question:** For the term → candidate-documents structure over tier-1 text,
what is compared, where, and when?

The candidates: (i) term → doc-id postings (intpack `pfor128skip` or Elias-Fano,
both in intpack); (ii) a per-document filter over its terms (bloom or binary
fuse, ~9 bits/key) with a scan of the survivors; (iii) per-block filters over
the concatenated compressed text, VictoriaLogs-style. The research's
`research/grok/decisions.html` last question and
`research/claude/architecture.html` § What was rejected both argued (i) with a
filter gate in front — Splunk's layering — but argued it from vendor figures;
you want it measured.

| Option                                                                                                                                                                                                   | Costs                                                                                                                              | Buys                                                                      |
| -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ---------------------------------------------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------- |
| A. Decide offline first: a `ferret-bench` in the intpack-bench mould, run on msa2 over your corpus, comparing bytes/doc, build CPU, and query latency for rare, mid and common terms; ship one structure | A harness before the product. The comparison is on one corpus.                                                                     | An answer with numbers, in the repo, before the on-disk format is frozen. |
| B. Ship both behind one trait with an opt-in experiment mode that builds both and logs one against the other                                                                                             | Two implementations to keep correct; the experiment doubles the index for the part under test; the telemetry channel has to exist. | The experiment you described, on corpora that are not yours.              |
| C. Ship both always, pick per query                                                                                                                                                                      | Twice the bytes, permanently — against the density goal.                                                                           | Never wrong.                                                              |

**Recommendation:** A, then B for whatever A leaves within a factor of two on
any axis. What must be decided now regardless: the trait boundary —
`add(doc, terms)` on the write side, `candidates(term) → iterator over doc ids`
with an exact-or-superset flag on the read side — and that the index format
records which implementation wrote each segment. The fact that would change it:
if A's result is decisive (one structure wins on bytes and latency on every term
class), B is not built at all.

**Expand on:** "postings only" — no positions (D7), no stored fields, no doc
values in the segment? Or only "no positions"?

> Dave: positions are usually used for phrase search and other types of queries.
> They take up a lot of space though. My hypothesis is that the index should
> filter the candidates and then we do a ripgrep type search over the
> candidates. I suppose it would be nice to decide you want to sacrifice space
> for faster search. We should experiment to find out the actual trade-offs.
> I'll value having all of the tools available for other search projects in the
> future, like a rust-based embeddable VictoriaLogs alternative.

**Answer (2026-09-23).** The index is a **candidate filter**; a verifier (a
ripgrep-style scan of the candidate's bytes) makes every answer exact. Postings,
per-document filters, block filters and positions are then one family — each
trades bytes for a narrower candidate set — measured, not assumed. Structures
that are not clearly better ship with the opt-in side-by-side experiment. Two
design consequences:

1. **The planner/index seam.** A structure exposes
   `candidates(atom) → doc-id iterator`, an `exact` flag and a cost estimate —
   nothing about its format. The planner composes candidate sources and decides
   whether to verify; it never knows whether it holds postings or a filter. This
   is the boundary D1 flagged.
2. **Structure crates know nothing about files.** They index doc ids and byte
   strings, so they are reusable for the embeddable VictoriaLogs-style store.
   Everything filesystem-shaped stays in the catalog and crawler crates.

## D7 — Positions

**Question:** Store positional postings, or resolve phrase, proximity and
highlighting by re-reading the candidate file?

| Option                                                                                                                                                      | Costs                                                                                                                                   | Buys                                                                                                    |
| ----------------------------------------------------------------------------------------------------------------------------------------------------------- | --------------------------------------------------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------- |
| A. None. Phrase = intersect the term postings, re-scan survivors, gated on the catalog's (ino, mtime, size) so a since-modified file fails rather than lies | One open + read per surviving candidate, on phrase queries only. Extracted documents re-scan from the text cache when that tier exists. | Positions are 2–4× the freq-only index (R6). On the architecture's 81 MB T1 estimate, 160–240 MB saved. |
| B. Positions on the body field                                                                                                                              | The full multiplier.                                                                                                                    | Phrase never touches the file; highlighting from the index.                                             |

**Recommendation:** A — it is the research's top-ranked density lever and the
most direct instantiation of "density over speed". The fact that would change
it: post-intersection candidate sets in the tens of thousands on phrase queries,
which the bench in D6 can measure.

> Dave: Ok, A is what I just said above. I'm not quite sure I understand the
> reason for splitting D6 and D7 and the distinction.

**Merged into D6 (2026-09-23).** No real distinction: positions are one more
candidate-narrowing structure in the same experiment.

## D8 — Regex at first ship

**Question:** Regex over content is a hard requirement. At first ship, is it
answered by a scan, by a trigram tier, or by per-file filters (architecture Q1)?

| Option                                                                                                                                                                           | Costs                                                                                                                                  | Buys                                                                                                          |
| -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | -------------------------------------------------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------- |
| A. Scan path: walk the catalog (already exclusion-filtered, no directory traversal), run the regex over file bytes in parallel; ripgrep-class, 50 ms warm on the measured corpus | Cold cache is unmeasured. No index-side narrowing.                                                                                     | The requirement is met in slice 1 with no new structure. The verifier is needed by every other option anyway. |
| B. Per-file binary fuse filter over trigrams as a gate in front of A                                                                                                             | ~31 MB on the measured corpus; the Cox regex→trigram derivation has to be written (the `research/grok/cox-trigrams` course covers it). | Skips most files without opening them — the cold-cache win at 12% of the trigram tier's cost.                 |
| C. Full trigram postings (T2)                                                                                                                                                    | ~252 MB estimate, 3.1× the entire term index; a selectivity estimator and cost model.                                                  | Candidate narrowing inside surviving files.                                                                   |

**Recommendation:** A in slice 1, with B as the first experiment row after the
stage-0 cold measurement. The fact that would change it: a cold scan of the 1.6
GB text tier above ~5 s makes B a slice-1 item; under ~1 s, B is dropped too.

> Dave: I'm pretty sure A is not possible. Running ripgrep on my home directory
> takes more than a few minutes to run. I didn't wait for it to complete. Are
> you suggesting we could run it in seconds by restricting to text files
> perhaps? I'm pretty sure we want B or C.

**Answer (2026-09-23): B or C, by experiment.** You are right, and the brief
over-read M1: the 50 ms was `~/w` _warm_, _after_ `.gitignore` pruning cut it to
5.73 GB; the same research measured the whole tree cold at 234 s. The scan
survives only as the verifier over candidates. B (per-file trigram filters)
versus C (trigram postings) is a D6 experiment row; the Cox regex → trigram
derivation is common to both.

## D9 — What a term is

**Question:** For tier-1 text (code, config, prose), what does the tokenizer
emit?

| Option                                                                                                       | Costs                                                                                                              | Buys                                                                                                          |
| ------------------------------------------------------------------------------------------------------------ | ------------------------------------------------------------------------------------------------------------------ | ------------------------------------------------------------------------------------------------------------- |
| A. Maximal runs of alphanumerics and `_`, lowercased; no stemming, no splitting; ASCII fast path; `std` only | `fooBar` and `foo_bar` are one term each; `TODO` and `todo` are one term. No prose stemming (`index` ≠ `indexes`). | Zero dependencies; deterministic; the term dictionary stays small on code, where the identifier is the query. |
| B. A plus identifier splitting: emit `fooBar`, `foo`, `bar`                                                  | Roughly 1.5–2× the postings on code (estimate — a bench row).                                                      | Sub-word search on identifiers.                                                                               |
| C. Language-aware (tree-sitter)                                                                              | A large dependency with per-language grammars; a plugin-tier concern.                                              | Symbols versus comments versus strings.                                                                       |

**Recommendation:** A, with the tokenizer version in the doc identity (D4) so B
is a reindex of tier 1, not a format change. The fact that would change it: the
query log, once it exists — if sub-identifier queries are common, B.

> Dave: I'm pretty sure we want B here, particularly for filenames. It's _very_
> common for me to name files with camelCase or TitleCase and I'd want to be
> able to search those by individual words.

**Answer (2026-09-23): B**, for filenames and content alike: emit the whole
token and its camelCase / TitleCase / snake / digit-boundary parts
(`parseHTTPRequest2` → `parsehttprequest2`, `parse`, `http`, `request`, `2`).
The postings cost is a bench row.

## D10 — Which roots

**Question:** Configured roots, `$HOME`, or `~/w`? (Architecture Q2.)

| Option                                                                           | Costs                                                                                                 | Buys                                             |
| -------------------------------------------------------------------------------- | ----------------------------------------------------------------------------------------------------- | ------------------------------------------------ |
| A. Configured roots, default `$HOME` minus a built-in denylist plus `.gitignore` | One config file. `$HOME` is unmeasured: R8 counted ~1.55M directory entries, 2.7× `~/w`'s file count. | Right on both a dev tree and a document archive. |
| B. `~/w` only                                                                    | Every number in the research applies, and the answer is that grep already wins.                       | Certainty.                                       |

**Recommendation:** A, and run M1's census over `$HOME` in the stage-0 day
(D3/C). The fact that would change it: nothing — but the census decides whether
the extracted-document tier is empty or the product.

> Dave: yes, configurable roots, and as mentioned above, we want our own
> .ferretignore file to override .gitignore, but generally we should respect
> .gitignore. We should have global overrides too. e.g. node_modules, which
> won't be .gitignored when not in a git repository but still needs to be
> ignored.

**Answer (2026-09-23): A.** Precedence and implementation are D13.

## D11 — `unsafe` posture and the intpack dependency

**Question:** The architecture set `unsafe_code = "forbid"` workspace-wide with
a single mmap leaf as the exception, and rejected SIMD decode for v1. intpack
now exists, uses SIMD intrinsics and an `asm!` pin, and carries a tested
toolchain ledger for them. What is the posture?

| Option                                                                                                                                                                       | Costs                                                                                         | Buys                                                                                                                                 |
| ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | --------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------ |
| A. Workspace `forbid`; intpack (git dependency, path dependency during development) and a future mmap leaf are the named exceptions, each with a ledger in the intpack style | Two exceptions instead of one; intpack must be pinned by revision until its format is stable. | The core stays `forbid`; the `unsafe` lives in crates whose whole job is the hot loop, with the re-measure discipline already built. |
| B. Vendor intpack into `crates/`                                                                                                                                             | Two copies of one crate; the bench harness points at the other.                               | One repo.                                                                                                                            |
| C. Publish intpack to crates.io now                                                                                                                                          | Its format and API are not stable; a published 0.x is a promise.                              | A normal dependency line.                                                                                                            |

**Recommendation:** A; C when the segment format freezes. The fact that would
change it: if intpack turns out to be the only consumer-facing codec crate and
never changes again, C immediately.

> Dave: I want to remain flexible on this one. It's quite possible that we'll
> want to vendor in intpack, and I want to use SIMD wherever it makes sense,
> espcially if it's not already being done by regex scanning for example.

**Answer (2026-09-23): stay flexible.** No workspace-wide `forbid`; `unsafe` and
SIMD are allowed where a measurement justifies them, each recorded in a
toolchain ledger in the intpack style. intpack starts as a git dependency and
may be vendored.

## D12 — Licence

**Question:** "As open a licence as possible" — which?

| Option                   | Costs                                                                              | Buys                                                         |
| ------------------------ | ---------------------------------------------------------------------------------- | ------------------------------------------------------------ |
| A. `MIT OR Apache-2.0`   | Attribution required.                                                              | The Rust convention; matches intpack; Apache's patent grant. |
| B. `0BSD` or `Unlicense` | No patent grant; some corporate policies reject public-domain-equivalent licences. | No attribution, nothing to comply with.                      |

**Recommendation:** A, for consistency with intpack and intpack-bench. The fact
that would change it: a stated wish for public-domain-equivalent terms.

> Dave: Let's go with A.

**Answer (2026-09-23): A.**

## D13 — Ignore rules: precedence, and whose matcher

**Question:** `.gitignore` is respected, `.ferretignore` can force-include or
force-ignore over it, and global rules (e.g. `node_modules` outside any repo)
apply everywhere. What is the precedence, and do we write the matcher?

Proposed precedence, most specific wins (git's own model, with ferret's file one
level above git's at each directory):

1. `.ferretignore` in the directory or nearest ancestor (`!pat` force-includes)
2. `.gitignore`, `.git/info/exclude` — only inside a git work tree
3. user global rules, `~/.config/ferret/ignore`
4. built-in defaults (`node_modules/`, `target/`, `.venv/`, `__pycache__/` …, a
   size cap, binary detection)

So `!node_modules/` in a `.ferretignore` beats the built-in default, and a
repo's `.gitignore` beats the global rules inside that repo. One gitignore
limitation to decide: git cannot re-include a file under an excluded directory.
Proposed: `.ferretignore` can, since force-include is its point — the walker
descends an excluded directory only when a force-include pattern could match
beneath it.

| Option                                                                                                                                    | Costs                                                                                                                                                  | Buys                                                                |
| ----------------------------------------------------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------ | ------------------------------------------------------------------- |
| A. `ignore` crate (BurntSushi, MIT/Unlicense): custom ignore filenames with precedence over `.gitignore` built in, plus a parallel walker | A dependency with transitive `globset` et al. (`regex` is needed anyway). Re-inclusion under an excluded directory is unsupported and needs a wrapper. | ripgrep's matcher and the semantics users already expect, in a day. |
| B. Own matcher over `globset`                                                                                                             | Gitignore's edge cases (anchoring, `**`, trailing `/`, negation order) are a known bug farm.                                                           | Exactly the semantics above, re-inclusion included.                 |
| C. Own matcher and glob engine                                                                                                            | B plus a glob compiler.                                                                                                                                | Zero dependencies.                                                  |

**Recommendation:** A for the first slice, behind one
`should_index(path, meta) -> Decision` function with a golden-file test corpus,
so B can replace it with no caller noticing. The fact that would change it: if
re-inclusion under excluded directories is common in your trees, go straight to
B.

> Dave: Agree. Use the bang syntax to unignore something that was ignored by an
> upper .ferretignore or to override a .gitignore.

**Answer (2026-09-23): A**, with `!pat` in a `.ferretignore` overriding both an
ancestor `.ferretignore` and any `.gitignore`, including re-inclusion beneath an
excluded directory.

> Dave: A .ferretignore that appears in a .gitignore excluded directory would
> never get found. That's fine. It would need to be overridden at the same level
> or higher.

**Clarified (2026-09-23):** an excluded directory is never read, so a
`.ferretignore` inside it has no effect. Re-including it takes a `!` pattern in
a `.ferretignore` at the excluded directory's level or above.

**Landed (2026-09-23, `76725e8`).** The API is a per-directory `DirRules`
(`root`, `enter`, `traverse`, `decide`) rather than one `should_index` function,
so the crawler carries rules down the walk and the policy stays pure. An
excluded directory is walked in a re-include-only mode (`traverse`: not
catalogued, its ignore files unread) only when an **anchored** `.ferretignore`
`!` pattern could match something beneath it, checked one path component at a
time with gitignore glob rules. So `!/target/doc/**` and
`!/target/*/report.html` reach into `target/`, while `!*.pdf` and `!**/x` never
cause an excluded directory to be walked. Inside a traversed directory only
`.ferretignore` `!` patterns re-include; `.gitignore` and the global file
cannot, as in git. A bad pattern line is dropped and reported, and the rest of
its file still applies.

**Revised (2026-09-25): the defaults are a file, not a layer.** Setup writes
`DEFAULT_IGNORE` to `$XDG_CONFIG_HOME/ferret/ignore` (default
`~/.config/ferret/ignore`) once, in commented sections, and never overwrites it.
From then on it is an ordinary global ignore file: deleting a line or adding
`!pat` below it re-includes, by gitignore's last-match rule. The built-in layer
and `Config::defaults` are gone, since a second copy would keep matching after
the user edited the file. Precedence is unchanged, because the defaults already
sat directly below the global file. Two things follow. Defaults added in later
versions do not reach an existing file, as with git's own global ignore. And
with no global file nothing is excluded, so the CLI must not crawl before setup
has run.

**Later decisions that change this one.** How the matcher is built — whole-path
matching against bucketed layers, or per-directory derived rule sets that would
replace `reinclude.rs` and the re-include-only walk above — is **D19**, with
measurements. Where a work tree starts is **D22**: a root inside a work tree
applies the `.gitignore` files and `info/exclude` above it, but not a
`.ferretignore` above the root.

## D14 — Filename search: scan the names, or index them

**Question:** Is "much faster find" answered by scanning the catalog's names or
by an index over them?

| Option                                                                                                                        | Costs                                                                                                                                  | Buys                                                                                  |
| ----------------------------------------------------------------------------------------------------------------------------- | -------------------------------------------------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------- |
| A. Scan: names stored contiguously in the catalog; substring/glob/regex is one SIMD pass over ~1M names (~20–30 MB, estimate) | Every query touches every name: single-digit ms warm, more cold (estimate — a bench row). Word matching means splitting at query time. | No index at all; any pattern shape, including regex. The Everything/FSearch approach. |
| B. Term postings over name tokens (D9 splitting)                                                                              | A second postings set, over names.                                                                                                     | Word search (`request` finds `parseHTTPRequest.rs`) at postings speed, and ranking.   |
| C. A for substring/glob/regex, B for words                                                                                    | Both.                                                                                                                                  | Each query shape on the structure that suits it.                                      |

**Recommendation:** C, with A first — it is the first slice's whole filename
search — and B once the postings machinery exists. The fact that would change
it: if A answers word queries fast enough by splitting at query time, B is never
built.

> Dave: Agree. Also, we can avoid the cold cache by running a permanent ferret
> daemon which I think we should do at least optionally.

**Answer (2026-09-23): C, scan first.** An optional resident daemon holds the
catalog (and hot index files) in memory so name search never starts cold; the
CLI works with or without it.

## D15 — Result unit: per path or per document

**Question:** When one content (a doc) has three names, is that one result or
three?

| Option                                                 | Costs                                                                             | Buys                                                             |
| ------------------------------------------------------ | --------------------------------------------------------------------------------- | ---------------------------------------------------------------- |
| A. Per path: three results                             | Duplicate content crowds the list (vendored copies, backups).                     | Matches `find` and `rg`; every result is something you can open. |
| B. Per doc, names grouped under it                     | A result is not a path; the JSON output and the agent skill must model the group. | Duplicates collapse — the point of the chain.                    |
| C. Per path by default, `--group` collapses to per doc | A flag.                                                                           | A for scripts and agents, B on request.                          |

**Recommendation:** C. The fact that would change it: if your trees hold many
duplicates, B as the default.

---

> Dave: Per path. Depending on the view we might change this though. I could
> imagine doing image search later and wanting an image to come up once even if
> there are duplicates.

**Answer (2026-09-23): per path** in the CLI. Grouping by document is a property
of a view, not of the index, so the index keeps both available.

## D16 — Replace `ignore` with our own gitignore matcher

**Question:** `ignore` (D13) brings 11 crates into `ferret-policy`. Do we write
our own gitignore matcher and drop it?

What the tree actually is (`cargo tree -p ferret-policy`, 2026-09-24): we use
only `ignore::gitignore::{Gitignore, GitignoreBuilder}` and `Match` — build from
lines, then `matched(path, is_dir)`. `crossbeam-deque`/`-epoch`/`-utils`,
`walkdir` and `same-file` exist for ignore's parallel walker, which we do not
call; they are not Windows support (that is `winapi-util`, `cfg(windows)`, never
built here). `globset` compiles each glob to a regex and matches the set through
`regex-automata`, with fast paths for literal basenames, extensions, prefixes
and suffixes; that brings `aho-corasick`, `bstr`, `memchr`, `regex-syntax` and
`log`. The part we would replace is `gitignore.rs` (885 lines with tests) plus a
glob matcher. Git's own matcher is a backtracking wildmatch with no regex
engine, so this is independent of whether we build our own regex (D8).

| Option                                                                                                                                                              | Costs                                           | Buys                                                                                 |
| ------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ----------------------------------------------- | ------------------------------------------------------------------------------------ |
| A. Timeboxed experiment: own gitignore module on a wildmatch-style glob matcher, checked against the golden corpus and `git check-ignore`, benched against `ignore` | ~600–900 lines; about half a day of agent time. | 11 fewer crates; the semantics are ours; the regex question stays independent.       |
| B. Keep `ignore`                                                                                                                                                    | Nothing now.                                    | A mature matcher, ripgrep's semantics.                                               |
| C. Own gitignore rules over `globset` directly, drop `ignore`                                                                                                       | Small: only the gitignore layer is rewritten.   | Drops the walker's crates (crossbeam ×3, walkdir, same-file); the regex crates stay. |

**Recommendation:** A. It forecloses nothing — if it loses, `ignore` stays. The
fact that would change it: if the bench shows ours well behind globset at ~1M
paths with realistic rule sets, and only a regex-style set matcher closes the
gap, C is the stopping point.

**Answer (2026-09-24): A**, run as:

1. **First implementation** (codex Sol), tests first, written from
   `gitignore(5)` with `git check-ignore` as a black-box oracle. Git's
   `wildmatch.c` is GPL-2 and was **not** ported or read (D12); ignore's and
   globset's sources were not read either.
2. **Review** (codex Astra): exponential backtracking (6.9 s on one 40-byte
   name), globstar and class divergences, a benchmark fed paths on the wrong
   base. A **core rewrite** (codex Sol) followed: bounded component matching, a
   parser shared with `reinclude.rs`, measured fast paths.
3. **Conformance pass** (codex Luna): ignore's and globset's tests (181 cases)
   copied in temporarily; 24 failures, every one a case where `ignore` disagrees
   with git and we follow git. The ported tests were deleted; the output was a
   list of behaviours, not tests.
4. **Blind tests** (Claude): written from that list without seeing upstream
   tests; every table row is checked against git by a permanent test; 28
   deliberate mutations of the matcher, all caught.
5. **Final review** (codex Astra): five more git-verified divergences
   (re-inclusion skipped file normalisation, escaped separators, CR/NUL order,
   bracket corner cases, an oracle pipe deadlock), fixed by codex Luna.

**Adopted (2026-09-25).** `ferret-policy` has no dependencies. Matching is
O(pattern bytes × path bytes) with no recursion; a step-count test guards it at
sizes up to 1,024. Warm matching, 500k real paths from `~/w`, each ignore file
against paths relative to its own directory, ns/path:

| Rule set                         | Ours | `ignore` |
| -------------------------------- | ---: | -------: |
| built-in defaults                |   63 |       76 |
| aic-edit `.gitignore` (48 lines) |   89 |      420 |
| CPython `.gitignore` (171 lines) |  238 |      252 |

Build of the largest set: 131 µs against 705 µs. This is a matching
micro-benchmark, not an end-to-end crawl measurement. It is the `ferret-bench`
binary `gitignore_vs_ignore`, on branch `bench/gitignore-vs-ignore`: an
experiment that needs a dependency `main` has dropped lives on its own branch,
so `main` does not carry `ignore`.

## D17 — Whose regex engine, and when

**Question:** Does Super Ferret run regex on the `regex` crate, or on its own
engine — and does that work start now, so D16's gitignore matcher can share it?

Where regex runs: content search (S3) derives a trigram query from the regex
(Cox), takes candidate files from a trigram structure, then **verifies** each
candidate by running the real regex over its bytes (`ferret-verify`). Name
search (D14) runs glob and regex over the name heap. The trigram derivation
needs a parsed regex (a syntax tree), whichever engine executes it; the verifier
and name search need an executor. DESIGN today says `ferret-verify → regex`.

What the gitignore matcher needs is not a regex engine. `ignore` is fast where
globset avoids its regex set: literal names, extensions, prefixes and suffixes
by hashing and byte comparison. D16's gap (~710 vs ~600 ns/path on real files,
measured against the wrong base path) comes from general patterns taking a
recursive backtracker; the fixes are more of those fast paths and a small
linear-time glob automaton. Globs are a strict subset of regex with
path-component rules, so they can lower onto a shared automaton later if one
exists.

| Option                                                                                                                                                          | Costs                                                                                                                                                                                                                       | Buys                                                                                                                                                              |
| --------------------------------------------------------------------------------------------------------------------------------------------------------------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| A. `regex` / `regex-automata` executes; `regex-syntax`'s parse tree feeds our Cox derivation; the verifier sits behind a narrow trait                           | The same ~8 regex crates D16 is removing from `ferret-policy` come back in `ferret-verify`. The engine is not ours to learn from.                                                                                           | ripgrep-class verification on day one. S3's effort goes into the index, which is where the density goal lives. The trait leaves the executor replaceable.         |
| B. Own parser and syntax tree (Cox derivation, name search and the error messages are ours); `regex-automata` executes, fed by translating our tree             | A parser (~2–3k lines) and a translation layer; still the regex dependency.                                                                                                                                                 | The part that touches the index is ours; the hard performance work (lazy DFA, SIMD literal prefilters) stays borrowed; C remains possible one executor at a time. |
| C. Own engine end to end: parser, Thompson NFA, a linear-time simulation (Pike VM), then a lazy DFA and literal prefilters; globs and gitignore compile onto it | Large and open-ended. A correct linear-time engine is weeks; matching `regex-automata` on hard patterns is the years of work that crate represents. Delays S1, which D3 put first so you can use the tool and collect data. | Zero regex dependencies anywhere; one automaton core for globs, names and content; the deepest learning payload.                                                  |

**Recommendation:** A now, choosing between A, B and C at S3 with a measurement.
The deciding number is **verification's share of end-to-end regex latency** on
`$HOME`: with a selective trigram filter, the verifier touches few files and its
speed barely matters, and an own engine loses little; if verification dominates,
only a highly optimised engine competes. Don't start an engine now for D16's
sake: the gitignore matcher doesn't need one to match `ignore`, and pausing S1
for it delays the data. The fact that would change it: if you want the regex
engine itself as a learning goal, like the index structures (build, not adopt),
choose C deliberately and schedule it as its own stage after S1, not as a
dependency of D16.

**Answer (2026-09-24): A.** `regex` executes, behind a narrow verifier trait;
revisit at S3 with the verification-share measurement. D16's matcher proceeds
without a regex engine.

**S3 measurement (2026-10-08): the trigger fires, but screening comes first.**
M5's rule was to reopen D17 if verification exceeds half of regex wall time and
the engine exceeds half of verification. Warm rare literals cross both lines at
p90: verification is 97% of wall and the engine is 57% of verification. Warm
identifiers sit at 90% and 47%. Cold rare literals are 99.5% and 5%, so reading
dominates there. The regex-volume diagnostic says why so much is verified. A
rare literal reads about 1.38 GB per query, against 8 MB that actually matches.
Over 99% of that is documents that hold every required trigram but not the
literal. A faster engine would only speed up rejecting those documents; better
screening would avoid reading them at all. **Recommendation:** keep `regex` (A)
for now. Measure stronger in-document screening first, then re-time whatever
verification survives, and choose A, B or C on that number. **Fact that would
change it:** if screening cannot cut warm verified bytes by more than half, the
engine share decides, and B or C comes back.

## D18 — Symlinks: catalogue as links, and what they match

**Question:** How does the catalog represent a symlink, and when does one appear
in results?

The crawler never follows a link: `Decision::Catalog(Reason::Symlink)`
catalogues it and reads nothing through it. That stays. What was open is what
the link means afterwards.

**Direction (Dave, 2026-09-26, in conversation):** symlinks are represented as
symlinks in the catalog, with a reverse mapping. They appear in results in two
cases: (1) the search matches the link's own name; (2) the search matches the
content of the file it links to. Directory links appear only in case 1. Pulling
files outside every root into the index through a link is a later enhancement;
for now a file not under a root is not indexed.

**Answer (2026-09-26): deferred, with the structure prepared now.** The target
model, for when it lands:

- A link is a catalog entry holding the text `readlink` returned. Its **next
  hop** is that text resolved against the link's directory, and never the final
  canonical path: a canonical pointer goes stale, unnoticed, when an
  intermediate link is retargeted.
- A **reverse map** from target path to the links naming it, keyed by path
  rather than by entry, so a dangling link binds when its target appears.
- Case 2 lookup: doc → inodes → paths → the links naming each path, repeated
  through chains, with a visited set against cycles and a hop limit (40, as
  Linux).
- A case-2 row is the link's path, shown with `→ target` so the user sees why it
  matched (D15: one row per path).
- Change detection needs nothing extra: a link's content comes from its target
  at query time, and retargeting a link replaces its inode, which `lstat`
  already shows.

**Known gap:** a file link whose target path goes _through_ a directory link
(`notes/r → ../proj/report.md` with `proj → ~/w/proj`) is not reached from a hit
on `~/w/proj/report.md`. Handling it means recording the hop as (directory link,
remaining suffix) and checking directory links against each hit's path prefixes;
left out until it is seen to matter.

**Later, and separate — links that pull content in.** Following a link out of
every root would need: the target catalogued once at its real path, which makes
the catalog the visited set, so cycles cost nothing extra; membership by
**reachability** from a root, via a simple mark-and-sweep, because reference
counts leak on two external trees linking to each other; and a target directory
ruled as its own root: its own ignore files plus the global file (agreed as
option A). No generational collector; the work is in when it runs and over what.

**What changes now:** the catalog stores each link's target text (the `links`
table in DESIGN.md § The catalog), captured by one `readlink` per link during
the crawl. That keeps D18 an addition over data already on disk, with no
re-crawl and no format change.

## D19 — Ignore matching: whole paths, or per-directory rule sets

**Question:** Should `DirRules` keep matching each entry's whole relative path
against every layer, or carry a per-directory set of the patterns still live
there, derived on `enter`?

**Today** (`rules.rs`, `gitignore/mod.rs`): `enter` does no pattern work; it
clones a few `Arc`s and adds a layer only when the directory holds an ignore
file. `decide` asks each layer, closest first, to match the entry's path
relative to that layer's directory. Inside a layer, patterns sit in buckets:
basename patterns (`*.txt`, `db.sql`, `node_modules/`) are a hash lookup on the
basename or extension, independent of depth and pattern count; anchored patterns
(a `/` before the last character) are bucketed by the path's first one or two
bytes and run through the component matcher over the whole path. Patterns with
no literal prefix (`**/__log/log.txt`, `*/x`) land in `anchored_any` and are
tried against every entry in the tree.

**Proposed** (Dave, 2026-09-26): each directory carries the patterns that can
still match beneath it. Entering `docs/` turns `docs/*.md` into `/*.md`,
`**/docs/my-notes.md` into `/my-notes.md` (keeping the original too), and drops
`logs/*.json`. Basename patterns are unchanged by entering, so they stay as
today's shared buckets; only anchored patterns are derived. A pattern of k
components can sit at no more than k positions, so `**` cannot blow up the set
(an NFA over path components). Two rules the derivation must keep: a derived
pattern stays anchored to its directory (`docs/*.md` must not match
`docs/sub/x.md`), and it keeps its file, band and line number, so last match
wins and closest file first still hold.

What the rules look like here (every `.gitignore` under `~/w`, 2026-09-26, with
a leading `**/` counted as basename): 866 basename patterns, 273 anchored (24%).

| Option                                                    | Costs                                                                                                                                                                                                                 | Buys                                                                                                                                               |
| --------------------------------------------------------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | -------------------------------------------------------------------------------------------------------------------------------------------------- |
| A. Keep whole-path matching                               | Nothing now. Anchored patterns cost O(depth) per entry, and `anchored_any` ones are tried everywhere. `reinclude.rs` stays a second piece of logic (`reaches_below`, `is_superseded_by`).                             | One matcher, verified against git as a black box on whole paths. `enter` is free.                                                                  |
| B. Derive anchored patterns per directory; buckets as now | Anchored matching is rewritten to match one component at a time; `enter` builds a small derived set wherever an anchored pattern's first component matches. The differential test against git must go through a walk. | Anchored patterns are only tried where they can match. `Traverse` becomes "a whitelist survives in the derived set", which deletes `reinclude.rs`. |

**Recommendation:** A until measured, then B if the numbers favour it. B is the
better model, but nothing has shown matching to matter: a walk of ~1M files is
dominated by `getdents` and `lstat`, and D16's matcher is 63–238 ns/path warm.
The fact that would change it: a real walk where `decide` is a noticeable share
of walk time, or a prototype of B that is faster on real trees and agrees with A
on every decision; either makes B the next change. A bug in `reaches_below`
would too, since B removes it.

**Measured (2026-09-26).** A prototype of B, `DerivedRules`, is on branch
`bench/policy-derive` (grok, commit `ac40047`), with the same `root` / `enter` /
`traverse` / `decide` API. Basename patterns stay in shared buckets; anchored
ones become per-directory cursors stepped by the existing component matcher, and
a cursor that can match an entry here is projected to a basename pattern for
this directory only. No second glob engine.

Agreement: the golden corpus runs both engines through one driver; unit tests
cover the discriminating cases (anchoring after derivation, `**` at zero and
many levels, last-match-wins in both orders, a nested `.gitignore` beating a
derived pattern, superseded re-includes); and on `~/w` B equals A at all 85,948
entries (5,925 directories, 156 `.gitignore`, 36 `info/exclude`), also with the
CPython `.gitignore` added to the global file. No `.ferretignore` exists under
`~/w`, so live-tree `Traverse` is covered only by the corpus and unit tests.

Timing, warm, release, best of three, in-memory replay of the same walk, ns per
entry (load 1.20, no other build running):

| Rule set                     | Engine | Whole replay | `decide` | `enter` |
| ---------------------------- | ------ | -----------: | -------: | ------: |
| real (defaults + files read) | A      |          545 |      393 |      27 |
| real                         | B      |          434 |      264 |      41 |
| stress (+ CPython globally)  | A      |          651 |      495 |      29 |
| stress                       | B      |          535 |      353 |      46 |

The walk itself (`read_dir` plus `lstat`, same directories) is 1,763 ns/entry;
the S1 walker's warm `~/w` walk is 0.219 s. So policy is about a fifth of a warm
walk (A 31% of the bare walk, B 25%), and B saves about 110 ns/entry, 10 ms on
`~/w`, about 5% of the walker. The saving is not from `anchored_any`: only 30
such patterns were read. A second data point, from the walker (2026-09-26): once
it read `info/exclude` through `.git` files, `~/w`'s 87 linked work trees
contributed patterns such as `**/.claude/…`, which have no literal prefix, and
the warm walk went from 0.219 s to 0.275 s with identical decisions; all of the
difference is user time in matching. That is the pattern class B is built for,
and the replay above did not include it, because it was collected before the
fix. Where A's `decide` time goes is not yet profiled; probing every layer's
buckets for each entry is the likely cost.

**Profiled and fixed in A (2026-09-26, `02f24fa`).** Two thirds of the warm
walk's user time was `decide`, and none of it was the model: std's
`Path::strip_prefix` parsing components for every layer on every entry (about a
fifth of user time), SipHash in the bucket maps, and the work trees'
`**/.claude/<name>` excludes scanned against every entry from `anchored_any`. A
new bucket keys anchored patterns by a literal last component (a path can only
match one if its basename is that literal), the maps use a small multiplicative
hasher, and layer bases are stripped as bytes. Warm walk of `~/w` 0.28 s → 0.205
s, then 0.185 s with D21's handles; user time 0.14 s → 0.05 s. Decisions
identical on all 86,243 entries, also with CPython's `.gitignore` added
globally. On `$HOME` (430k entries) the walk is 1.07 s, of which user 0.31 s and
policy about a fifth of that.

That is the fact the recommendation named: the cost was a fixable bucket miss in
A. Matching is now under a tenth of the walk; B would save a share of that
tenth. **Revised recommendation: A; park B** (the prototype stays on
`bench/policy-derive`). The fact that would change it: a real rule set whose
anchored patterns have neither a literal first nor a literal last component, in
numbers large enough to show in a profile.

> Dave: I still think we should look at B. Once that per-directory check is
> cached, inotify rules can be checked much more efficiently. It's not just the
> speed I'm concerned about. If I run a process that touches hundreds of files,
> e.g. prettier fix, then I want to use as few cycles as possible. Please push
> back if you think it won't make a difference.

**Pushback (2026-09-27): it won't make a measurable difference, for three
reasons.**

1. **Caching per directory is not B's alone.** A's `DirRules` is already a
   per-directory object; the walker holds one per directory and a daemon would
   cache one per watched directory the same way. An event then costs one
   `decide` against the cached value under either option. What B adds is pruning
   anchored patterns that can no longer match below the directory.
2. **`decide` is already a small share of an event.** The single-worker warm
   walk spends 0.045 s of user time on 86k entries, about 0.5 µs per entry for
   everything in user space, `decide` included. One `stat` alone is about 2 µs
   in the kernel (strace, same walk). A changed file that is re-indexed also
   costs an open, a read, a hash and tokenizing: tens of microseconds to
   milliseconds.
3. **B's saving is a fraction of that share.** On the replay B saved about 130
   ns per `decide`, measured against A **before** A's fixes, which cut the same
   costs B was built to avoid. For a prettier run touching 500 files, that is at
   most 65 µs in total, next to milliseconds for the stats and re-index.

Where B would matter is a rule set with many anchored patterns that have neither
a literal first nor a literal last component: the fact already named above. The
cheap way to settle it is to re-run the replay, A on current `main` against B on
`bench/policy-derive`, which is about an hour of agent time. If B still saves
more than about 10% of `decide` on the real rule set, I would build it for the
daemon. Otherwise, keep A.

> Dave: run the replay

**Replay (2026-09-27, `bench/policy-derive` at `0d437dd`): B is now slower than
A, so A stays.** The branch was merged with `main`, and A's code there is
exactly `main`'s. The harness was fixed in two ways:

- It now reads the linked work trees' excludes, as the walker does: 126 exclude
  files, not 36.
- It adds a "cached" timing, in which each directory's rules are built once, as
  a daemon would keep them, and then `decide` runs in one loop.

A and B agree on all 86,415 entries. Warm, five runs, median ns per entry on the
real rule set:

| Engine | Replay `decide` | Replay `enter` | Cached `decide` | Cached `enter` |
| ------ | --------------: | -------------: | --------------: | -------------: |
| A      |             217 |             35 |             182 |             44 |
| B      |             282 |             73 |             257 |             99 |

B is 30% slower on the replay's `decide` (21–43% across runs) and 41% slower
with cached rules. It costs about 100 ns more per entry once its `enter` is
counted, about 40 µs more over a 500-event burst. Two causes:

- B probes two full matchers per layer, where A probes one. Most of B's old lead
  was A's per-layer overhead (`strip_prefix`, SipHash), which `main` has
  removed.
- The work trees' `**/.claude/<name>` excludes keep a cursor alive in every
  directory below them, which doubles B's `enter`. A handles that class with one
  lookup on the last path component.

**Answer (2026-09-27): A, by the agreed rule.** B stays parked on
`bench/policy-derive` with the fair harness. The fact that would reopen it: a
real rule set on which a re-run shows B ahead.

### Reopened: B-flat, Dave's algorithm (2026-09-27)

The B measured above was not the algorithm Dave meant. His version is
**B-flat**: each directory keeps a single flat list of basename patterns, with
every rule from every layer projected onto names in that directory. The list is
ordered global, `info/exclude`, `.gitignore` root to here, then ferret root to
here. `decide` scans the list from the end, and the last matching pattern
decides. There is no chain and no path, only the name. The state needed to
derive a child's list is kept beside the list and read only by `enter`.

It is built as `FlatRules` on `bench/policy-derive` (`ea3cbdf`). It is a
deliberately plain linear scan, with no hash buckets. It agrees with A on all
86,416 entries under both global rule sets. Median of three runs, ns per entry:

| Rules  | Engine  | Replay `decide` | Cached `decide` | Cached `enter` |
| ------ | ------- | --------------: | --------------: | -------------: |
| real   | A       |             218 |             182 |             46 |
| real   | B-chain |             285 |             255 |            102 |
| real   | B-flat  |             120 |              95 |             95 |
| stress | A       |             329 |             295 |             41 |
| stress | B-flat  |           1,554 |           1,542 |            139 |

The stress global is the default rules plus cpython's `.gitignore`. Lists hold
20 rules on average (max 54) with the real rules, and 63 (max 97) with the
stress rules.

- **Real rules:** B-flat's `decide` is half of A's. Most global rules are
  directory-only literals, which a file rejects without running the glob, so
  each rule costs about 5 ns.
- **Stress rules:** B-flat is 5x slower than A. Only 0.8% of entries match any
  rule, so almost every `decide` scans the whole list. Most of cpython's lines
  are `*.ext` globs, each about 25 ns through the generic matcher, where A finds
  them with one hash lookup on the extension.
- **`enter`:** B-flat's is twice A's, because every directory rebuilds its list.
  A daemon builds it once per watched directory, not once per event.

For scale, 100 ns per entry is 50 µs over a 500-event prettier burst.

The ferret band and the traversal re-include were tested by unit tests only,
because `~/w` has no `.ferretignore`.

### Question: what next for D19?

B-flat's shape wins, but its cost grows with the number of glob rules in a
directory's list. Which way should it go next?

1. **B-flat with A's buckets.** Index each directory's merged list the way A
   indexes a layer: literals and extensions in hash maps, and the remaining
   globs scanned. Identical lists are shared, since most directories inherit the
   same global rules plus their repository root's `.gitignore`.
   - Buys: `decide` should fall below A on both rule sets, because it is one
     indexed probe where A probes each layer.
   - Costs: building hash maps is dearer than building a `Vec`, unless lists are
     shared. About one agent run to measure.
   - Also answers the space question, since shared lists are not copied.
2. **B-flat as one automaton.** Compile each distinct list into a single
   anchored alternation in reverse order, with leftmost-first matching, so the
   winning pattern number is the last matching line. `decide` costs one pass
   over the name, however many rules there are.
   - Costs: compiling is far dearer than building a list or hash maps, so it
     depends even more on sharing.
   - Needs a benchmark-only regex dependency, since the project builds its own
     engine.
3. **Stop and keep A.** A is within 90 ns per entry of B-flat on the real rules,
   and 5x better on the stress rules.
   - Costs: nothing now.
   - Forecloses the per-directory cache for inotify until something reopens it.

**Recommendation: 1**, measuring how many distinct lists `~/w` actually has as
part of the same run. That count decides whether 2 is ever worth trying. The
fact that would change it: if nearly every directory's list is distinct, sharing
buys nothing, and A's per-layer indexes are already the cheap answer, so 3.

> Dave: measure 1 vs 2

### Options 1 and 2 measured (2026-09-27)

Both are built on `bench/policy-derive` at `fcafaff`, as `FlatIndexed` for
option 1 and `FlatRegex` for option 2, with a fully compiled DFA as a third row.
All three agree with A on every entry under both rule sets. The regex crate
(`regex-automata`) is a dependency of `ferret-bench` only.

Identical lists are shared. The 6,035 directories in `~/w` have 334 distinct
lists, about 18 directories each. The key is where each rule came from; keyed by
rule text, the same rule in two clones would count once, and there would be
only 88. Median of three runs, ns per entry. Cached `enter` includes compiling
each distinct list once:

| Rules  | Engine         | Cached `decide` | Cached `enter` | Replay `decide` |
| ------ | -------------- | --------------: | -------------: | --------------: |
| real   | A              |             183 |             42 |             215 |
| real   | B-flat         |              97 |            110 |             124 |
| real   | B-flat-indexed |              81 |            142 |             117 |
| real   | B-flat-regex   |              50 |            536 |             196 |
| real   | B-flat-dense   |              58 |          4,601 |              97 |
| stress | A              |             302 |             41 |             328 |
| stress | B-flat         |           1,599 |            170 |           1,614 |
| stress | B-flat-indexed |             149 |            257 |             183 |
| stress | B-flat-regex   |              99 |          1,601 |             674 |
| stress | B-flat-dense   |              88 |         95,638 |             154 |

| Engine, rules   | Compile per list, mean | Heap for all lists |
| --------------- | ---------------------: | -----------------: |
| indexed, real   |                   7 µs |     878 KiB (est.) |
| indexed, stress |                  23 µs |   2,919 KiB (est.) |
| regex, real     |                 105 µs |          5,896 KiB |
| regex, stress   |                 342 µs |         17,595 KiB |
| dense, stress   |                  25 ms |         54,647 KiB |

- **Indexed** halves A's `decide` under both rule sets. Its `enter` is about 3x
  A's.
- **Regex** has the fastest `decide` once its lazy DFA is warm: about 30–50 ns
  better than indexed. It costs 15x indexed's compile time and 6x its memory.
  Its replay `decide` is worse than indexed's, because the first searches
  against each new list fill the lazy DFA.
- **Dense** is dominated: seconds of compiling, and tens of MB, for about 10 ns
  on stress.
- On a 500-event prettier burst, indexed saves about 50 µs of `decide` over A,
  and regex about 15 µs more.
- The merged-list fix was real: `best_at` returns the source line number, which
  is wrong once lists from several files are merged. Lists are now renumbered by
  list position, and a test catches the old bug.

### Question: adopt B-flat-indexed?

1. **Adopt B-flat-indexed for both the walker and the daemon**, sharing lists by
   rule text.
   - Buys: `decide` about 2x faster than A. One engine for both uses. Shared
     lists (88 in `~/w`) keep memory near 1 MB.
   - Costs: rewriting `DirRules` onto the bench code, with the git-oracle and
     golden tests as the gate. On a single cold walk it roughly ties with A:
     `enter` plus `decide` is 223 against A's 225 on the real rules, and worse
     on stress (406 against 343).
2. **Adopt it for the daemon only, and keep A for the walker.**
   - Buys: each use gets its fastest engine.
   - Costs: two matchers to keep in agreement for ever.
3. **Adopt B-flat-regex.**
   - Buys: about 30–50 ns better warm `decide` than option 1.
   - Costs: 6x the memory. The regex dependency stays forbidden, so it waits for
     our own multi-pattern engine.

**Recommendation: 1.** A daemon keeps its lists, so `decide` is what repeats,
and one engine is worth the tie on a cold walk. Sharing by rule text should also
cut its `enter`. What would change it: if our own regex engine ends up with
cheap multi-pattern compilation, then 3, as a matcher swap behind the same
shared lists.

> Dave: Agree. Let's go with 1.

**Answer (2026-09-27): 1.** B-flat-indexed replaces whole-path matching for both
the walker and the daemon, with lists shared by rule text.

## D20 — Walk across mount points, or stay on the root's device

**Question:** When a configured root contains a mount point, does the walker
cross into the other file system?

Today it does (`ferret-crawl`, 2026-09-26): it never compares `st_dev` with the
root's. `~/w` is one device, so the measurement so far shows no cost either way.
DESIGN already lists "multiple devices and bind mounts under one root" as not
yet designed.

| Option                                   | Costs                                                                                                                                                | Buys                                                                                              |
| ---------------------------------------- | ---------------------------------------------------------------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------- |
| A. Cross mount points (current)          | A root of `$HOME` indexes any disk mounted beneath it, and bind mounts of other file systems.                                                        | A project bind-mounted under the root is indexed; no extra stat, no new rule. Forecloses nothing. |
| B. Stay on the root's `st_dev` (`-xdev`) | Misses a deliberately mounted file system unless it is added as its own root. One `lstat` of the root, and a rule for a directory on another device. | A backup disk or network mount under the root is never indexed by accident.                       |
| C. Configurable, default A               | A flag and a test matrix before any measurement says the default is wrong.                                                                           | Both behaviours.                                                                                  |

**Recommendation:** A, until a real root crosses into a file system that should
not be indexed. The fact that would change it: a crawl of the configured root
(likely `$HOME`, D10) whose time or catalog is dominated by another mounted file
system.

> Dave: I'm happy with your recommendation to stay with A on D20.

**Answer (2026-09-27): A.** The walker crosses into mounted file systems below a
configured root, as it does today. The walk example's `devices` count shows when
a root spans more than one.

## D21 — Walk by path, or by directory handle

**Question:** Should the walker open and list directories by path, as it does
now, or through directory handles (`openat`, `fstatat`, `O_NOFOLLOW`) that
cannot be redirected mid-walk?

Found by codex review (2026-09-26), reproduced: after `Descend` is emitted, the
walker calls `read_dir` on the path. If `dir` is replaced by a symlink in
between, the walk follows it and reports files outside the root as `Index`.
Children are also `lstat`ed by path, so a swap after listing reaches outside
too. Codex's round 2 found the same race on ignore files: `.git` is classified
by `lstat`, then `.git/info/exclude` is opened by path, so a `.git` swapped for
a symlink in between applies an exclude from outside the root. And a symlink's
`lstat` and `readlink` are two observations of a name that can change between
them. std offers no directory-relative calls; closing the race needs `rustix`
(or `libc` and `unsafe`, which D11 allows only when measured).

| Option                                                                                                                        | Costs                                                                                                                          | Buys                                                                                                                 |
| ----------------------------------------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------ | -------------------------------------------------------------------------------------------------------------------- |
| A. Path-based, with an identity check                                                                                         | The race narrows but stays: re-`lstat` the directory after listing and drop the listing if `(dev, ino)` or type changed.       | std only. Enough for a user's own tree, where the swap needs a process racing the crawler.                           |
| B. Directory handles via `rustix` for every operation (listing, child `lstat`, ignore reads, `readlink`, later content opens) | One dependency (`rustix`, Apache-2.0/MIT, no `unsafe` in our code); DESIGN's crate graph gains an edge; the walk is rewritten. | The race is closed. No per-entry path resolution, which may be faster; it is also the shape a parallel walker wants. |
| C. A now, B when the walker is parallelised or measured slow                                                                  | A now; B later.                                                                                                                | Defers the dependency until something else needs it.                                                                 |

Option A has **not** landed: the walker has no identity check yet, pending this
decision. A narrows the race, it does not close it: a swap after the check still
redirects child operations.

**Recommendation:** C. The exposure is a race inside the user's own tree, and
the consequence is indexing a file outside a root, not writing anything. The
fact that would change it: roots shared with other users (a writable shared
directory), where the race is an attack rather than an accident; then B now.

> Dave: let's switch to rustix now unless you think we can get away with never
> using it.

**Answer (2026-09-26): B, now.** There is no version of the walker that avoids
it: std has no directory-relative calls, so only handles close the race; the
`statx` fields DESIGN gives `ferret-crawl` (birth time, a cheaper mask) are not
in std either; and a parallel walker wants a handle per directory anyway.
`ferret-crawl → rustix` joins the crate graph.

**What the handles guarantee, after review (2026-09-26).** An independent review
of the parallel walker found the remaining path-based reads of a gitfile's
exclude and fixed them to use handles. It also named the limit of the design.
Past 128 open descriptors, a waiting directory listing closes its handle and
later reopens from the root by checked `O_NOFOLLOW` steps, matching the saved
`(dev, ino)`. An inode number can be recycled, so a directory deleted and
recreated at the same path between the two could pass that check. The reopened
directory is still at the same path inside the root. That is identity anchoring
within the root, not a guarantee that the listing is the same directory object,
and it indexes nothing an attacker in the tree could not have placed there
anyway. Left as is. `statx` birth time would close it, where the file system
reports one, if it ever matters.

## D22 — A root inside a git work tree

**Question:** When a configured root sits inside a work tree whose `.git` is
above it (root `repo/src`), do the `.gitignore` files between the work tree's
top and the root apply?

Today they don't: `DirRules::root` documents that such a root is treated as
outside a work tree, so `repo/.gitignore` and `repo/src/.gitignore` rules are
ignored and git-excluded files are indexed. Codex confirmed with
`git check-ignore` (2026-09-26). D13 promises git's semantics inside a work
tree, so either the code or D13 is wrong.

| Option                                                                 | Costs                                                                                                                                                                                                                                             | Buys                                                                                            |
| ---------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ----------------------------------------------------------------------------------------------- |
| A. Keep it; narrow D13 to "a work tree that starts at or below a root" | A doc change. A root inside a repo indexes what git ignores there.                                                                                                                                                                                | Nothing to build; roots stay self-contained.                                                    |
| B. Discover the enclosing work tree                                    | The walker looks up from the root for `.git`, then builds rules from the work tree's top down to the root: each ancestor's `.gitignore` and `info/exclude`, no `.ferretignore` above the root. A `DirRules` constructor for a chain of ancestors. | Git's semantics wherever the root is. Matches what a user expects from `ferret index repo/src`. |

Two things B has to settle, from codex's round 2. Ancestor patterns match
relative to their own directory while the walk's paths stay root-relative, so
each ancestor layer needs its base expressed above the root; that is a
`DirRules` change, not only an upward search. And for root `repo/src`,
`repo/.ferretignore` is an ancestor in D13's wording: B as written leaves it
out, which is a choice to make explicitly.

**Recommendation:** B, with `.ferretignore` above the root left out: a
`.ferretignore` is ferret configuration for the tree it sits in, and a root is
where the user said ferret's view begins. A few file reads per root. The fact
that would change it: if roots are always at or above work trees in practice
(`$HOME`, `~/w`), A costs nothing real, and the extra constructor isn't worth
it.

> Dave: agree with your recommendation.

**Answer (2026-09-26): B.** Discover the enclosing work tree and apply its
`.gitignore` files and `info/exclude` from the top down to the root; a
`.ferretignore` above the root does not apply.

**Revised (2026-09-27): A.** Dave: configuration above a root is ignored, full
stop. Nobody here needs a root inside a work tree, and there is no way to know
whether such a user would expect the ancestors' rules to apply, so the simpler,
self-contained root wins. D13's git semantics hold for work trees that start at
or below a root. The walker's ancestor discovery (`discover` in `walk.rs`) is
removed.

## D23 — Recording work trees, so duplicate results can be hidden

**Premise (Dave, 2026-09-26):** the catalog records that a file is in a git work
tree, and which, so results can hide linked work trees. A project with a dozen
work trees open would otherwise show every match a dozen times.

**Question:** What does the catalog record, and where?

The walker already tells the three kinds of `.git` apart: a `.git` directory
starts a repository's main work tree; a `.git` file whose gitdir has a
`commondir` is a **linked** work tree of that common directory; a `.git` file
without one is a submodule, a separate repository and not a duplicate. The
repository's identity is the common directory, which for a linked work tree
usually sits outside the root (`~/w/super-ferret/.git` for every
`super-ferret-wt/*`).

| Option                                                                                                                                         | Costs                                                                                                                                                                                      | Buys                                                                                                                                                                           |
| ---------------------------------------------------------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ |
| A. A sixth table, `worktrees`: the top directory's `InoId`, kind (main / linked / submodule), repository id (the common directory's path text) | One row per work tree (tens of bytes; ~100 under `~/w`). The walker reports the kind and common directory at each `.git`. Membership is derived at load by the parent chain, as paths are. | "Hide linked work trees" and "group a hit with the same repo-relative path in other work trees" are both expressible, for names and content alike. Nothing per file is stored. |
| B. A work-tree id on every `inodes` row                                                                                                        | ~4 B per inode, 4 MB at 1M files, and redundant with the parent chain; moving a directory between work trees rewrites every row below it.                                                  | Filtering without an ancestry walk — which A gets anyway from a map derived at load.                                                                                           |
| C. Record nothing; collapse results by `DocId` at query time                                                                                   | Nothing stored. Only identical content collapses; a file edited in one work tree still shows once per work tree for a name match, and nothing says which copy is the main one.             | Works for any duplicate, work tree or plain copy, from data the catalog already has (`DocId → [InoId]`).                                                                       |

**Recommendation:** A, with C as the default presentation on top: a hit shows
once with "+N identical in other work trees", and a copy that differs shows
separately, since a divergent work tree is usually the one you want. Hiding
linked work trees entirely is then a query flag. The walker change follows the
current D21/D22 slice, since that rewrites the `.git` probe. The fact that would
change it: if the name scan (D14) needs the work-tree filter inside its inner
loop and the derived map is too slow there — then a per-directory bit, still not
B.

> Dave: Agree with this recommendation

**Answer (2026-09-26): A, with C as the default display.** DESIGN § The catalog
gains the `worktrees` table.

## Settled without a brief (object if wrong)

- The CLI emits JSON lines behind a flag from the first slice, with stable exit
  codes and byte offsets in hits, because the agent skill is the second consumer
  and the first that cannot read prose.
- A local query and timing log exists from the first slice; the opt-in upload is
  a later slice over the same records.
- One bench run at a time on msa2; measurement hygiene as in
  `intpack-bench/README.md`.
- No `git push` and no `Cargo.toml` edits by offloaded agents.

## D24 — How many walk workers by default

**Question:** What worker count should `ferret index` pass to `walk_parallel`
when the user sets none?

Measured warm, best of five, on this machine (Ryzen 9 9955HX, 16 cores / 32
threads, ext4 on NVMe), final walker `32bb2ed`; CPU is user + sys seconds for
the whole walk:

| Workers | `~/w` (86k) wall | CPU   | `$HOME` (436k) wall | CPU   |
| ------: | ---------------: | ----- | ------------------: | ----- |
|       1 |          0.176 s | 0.175 |             1.007 s | 1.002 |
|       4 |          0.053 s | 0.198 |             0.275 s | 1.086 |
|       8 |          0.029 s | 0.209 |             0.154 s | 1.198 |
|      16 |          0.018 s | 0.234 |             0.097 s | 1.451 |
|      32 |          0.024 s | 0.320 |             0.090 s | 2.013 |

Wall stops improving past 16. CPU rises steeply past 8, and most of the rise is
kernel time: sys goes from 0.87 s at 8 workers to 1.56 s at 32 on `$HOME`. Why
the kernel costs more under concurrency is not profiled.

| Option                               | Costs                                                         | Buys                                                                                                                  |
| ------------------------------------ | ------------------------------------------------------------- | --------------------------------------------------------------------------------------------------------------------- |
| A. `min(8, available_parallelism)`   | 57 ms more wall than 16 on a 436k-entry `$HOME`, warm.        | About 83% of the CPU that 16 uses, and the least disturbance to a laptop's other work. Scales down on small machines. |
| B. `min(16, available_parallelism)`  | 0.25 s more CPU than A per full `$HOME` crawl.                | The fastest warm walk measured on `~/w`, and within 7 ms of 32 on `$HOME`.                                            |
| C. `available_parallelism` (32 here) | 0.56 s more CPU than B for 7 ms, and slower than 16 on `~/w`. | Nothing measured; a larger queue depth might help cold reads (unmeasured).                                            |

**Recommendation:** A, with a config key and a `--jobs` flag for the rest. A
full crawl is rare once the daemon (D14) keeps the catalog current, and 0.15 s
against 0.10 s is invisible to a person, while the CPU difference is paid on
every crawl. The fact that would change it: a **cold-cache** crawl, which is the
one a person waits for after boot, that is markedly faster at 16 or 32 because
NVMe rewards queue depth. That needs a `drop_caches`, which needs root, so it
has not been measured.

> Dave: Agree. Cold, one run each after `drop_caches`, `$HOME`: 8 workers 3.079
> s, 16 workers 2.500 s, 32 workers 2.599 s.

**Answer (2026-09-27): A, `min(8, available_parallelism)`.** The cold numbers
arrived with the answer, and they are the fact named above: cold, 16 workers is
0.58 s (19%) faster than 8, and 32 is no better than 16. Warm, 16 costs 0.25 s
more CPU per full crawl. So the question comes back once, briefly:

| Option               | Costs                                                | Buys                                      |
| -------------------- | ---------------------------------------------------- | ----------------------------------------- |
| Keep A (8, answered) | 0.58 s more waiting on the crawl after boot.         | 0.25 s less CPU on every warm full crawl. |
| B (16)               | 0.25 s more CPU per warm full crawl, about 20% more. | The fastest measured, both cold and warm. |

**Recommendation:** B, because the cold crawl is the one a person waits for, and
full crawls are rare once the daemon runs. The fact that would change it: a
repeat of the cold runs (these are one each) in which the 8 and 16 figures
overlap.

> Dave: Go with 16

**Answer (2026-09-27): B, `min(16, available_parallelism)`.** It is
`ferret_crawl::default_workers()`; the `walk` example uses it when no count is
given. A config key and `--jobs` come with `ferret index`.

## D25 — A configured root that git ignores

**Question:** When the configured root, or a directory between it and the top of
its work tree, is excluded by the work tree's rules, does Ferret index the root?

Found by review of D22's implementation. With `repo/.gitignore` containing
`/src/` and root `repo/src`, git ignores everything below `src`, including a
file that `src/.gitignore` re-includes with `!keep.txt`: `git check-ignore`
reports all three test files as ignored by `/src/`. The walker never asks
`decide` about the root or its ancestors, so today it indexes all of `src`, and
applies the closer `!keep.txt` as though `src` were live. The global ignore file
behaves the same way: a root inside `node_modules/` is walked.

| Option                                                                           | Costs                                                                                                                                                         | Buys                                                                                                                    |
| -------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------- | ----------------------------------------------------------------------------------------------------------------------- |
| A. Follow git: an excluded root or ancestor excludes the whole root              | A root the user named explicitly yields an empty catalog, unless a `.ferretignore` `!` re-includes (D13). Needs decisions for each ancestor from top to root. | Exactly git's answer, which is the premise of D22.                                                                      |
| B. A configured root is always walked; D22's rules apply only below it (current) | Differs from git for this one case. Closer negations inside the root apply although git would not reach them.                                                 | The user's explicit choice wins, consistent with how the global ignore treats a root. No code; document it and test it. |
| C. Follow git, but report it: walk nothing and emit an event naming the rule     | The event plumbing for a new kind of report.                                                                                                                  | No silent surprise either way.                                                                                          |

**Recommendation:** B. Naming a root is the most specific instruction Ferret
gets, more specific than any ignore file, and it matches the global-ignore
behaviour. D22's purpose was to make `ferret index repo/src` agree with git
**inside** `src`, which B keeps. The fact that would change it: roots chosen by
something other than a person, such as auto-discovering every repository under a
directory. Then a root git ignores is more likely an accident than a request,
and A or C is right.

> Dave: Yes, a specified root overrides .gitignore

**Answer (2026-09-27): B.** A configured root is always walked, and the
enclosing work tree's rules apply only below it. To do: say so in DESIGN and add
a test that a root excluded by an ancestor `.gitignore` is still indexed.

---

## The catalog slice (S1), 2026-09-27

DESIGN § The catalog fixes the tables, the ids and the hash. These questions
stay open before code. Measured to size them: the default-ignore walk of `$HOME`
on this machine catalogues **434,846 entries** (76,789 directories; 354,383
files the policy sends to the index, before the binary sniff moves some to
catalogue-only; 3,674 already catalogue-only), with **10.2 MB of name bytes,
23.4 B per name**, and no I/O faults and no traversed directories. Full relative
paths would be 43 MB. Everything else below marked an estimate is one.

Reviewed by Sol (gpt-6-sol, three rounds) and Astra (gpt-6-astra) before
reaching you; their findings are folded in.

Settled without a brief (object if wrong):

- **Hashing uses the `blake3` crate** (`CC0-1.0 OR Apache-2.0`), in
  `ferret-crawl`, as D4 recommended and its restatement confirmed. That adds a
  `blake3` edge to DESIGN § Crates in the same commit. Only files the policy
  sends to the index are hashed; catalogue-only files (binary, over the size
  cap) have no `DocId`.
- **Hashing and sniffing stay in `ferret-crawl`**, as DESIGN § Crates has it;
  the catalog receives results, never file contents.
- **`DocId` is never renumbered by the catalog.** Postings are keyed by it (D4);
  only an index merge may reclaim a dead doc, and that is S2's business. In S1
  dead rows are simply dropped and the counter never goes back (D36).
- **New `DocId`s are assigned at the merge, in merge order.** Assigning them in
  one place means two workers that hash equal content concurrently still produce
  one `DocId`; the walk gives no cross-worker order to preserve, and keeping one
  would need a global sequence number per hash. D4 expected the first crawl to
  be close to path-sorted; `walk_parallel` gives no cross-worker order, so it is
  not. Revisit if an S2 postings measurement shows the lost locality costs
  bytes.

## D26 — A re-run: rebuild the snapshot, or mutate it through a log

**Question:** In S1, does `ferret index` on an existing catalog write a fresh
snapshot from the walk, or apply the differences to the old one as log records?

DESIGN describes a snapshot plus an append log, which is the shape the daemon
(D14) needs to apply one inotify event without rewriting the catalog. S1 has no
daemon: every update is a full re-crawl, which already visits every entry.

A walk fault is a hazard for both: an entry the walk did not report may still
exist. A directory whose listing fails keeps only the entries read before the
fault (`list` in `walk.rs`); a directory that cannot be opened reports none; an
`lstat` or `readlink` failure emits `Event::Io` and no `Decided`, so the entry
vanishes; and an unreadable ignore file changes the rules applied below it.
`Event::Io` carries no operation tag, so the catalog cannot tell these apart to
repair only the affected subtree.

Not every fault is a coverage fault. An entry that vanished between listing and
`lstat` (`NotFound`) is a deletion, which the new snapshot records correctly. A
file that cannot be hashed, or changes while it is hashed (D33), leaves the
namespace intact; only its content is unknown.

| Option                                                                                                                                                                                                                                                                     | Costs                                                                                                                                                                                                                                                                                                                                                                           | Buys                                                                                                                                                                             |
| -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| A. Rebuild and swap: the walk writes a new snapshot. A file's hash and `DocId` carry over when `(dev, ino, size, mtime, ctime)` match the old row. A directory whose listing faulted keeps its old children and their subtrees.                                            | Writes the whole snapshot every run: about 50 MB for `$HOME` and 114 MB at 1M entries (estimate: 64 B inode rows as the walker's fields pack, 12 B name rows, 24.4 B of terminated name, and 128-bit hashes; 48 B rows need a compression scheme). Transient maps over the old snapshot for `(dev, ino)` and hash lookups, whose peak RSS must be measured against D5's budget. | No log format, no replay, no sweep. Deletion is absence, except under a faulted directory. The log arrives with the daemon, over a snapshot format already settled and measured. |
| A′. As A, but a crawl with a coverage fault (a listing, open or ignore-file read that failed, any `readlink` failure, or an `lstat` failure other than `NotFound`) publishes nothing and keeps the old snapshot. A content fault publishes, with that file marked unhashed | A root with one permanently unreadable directory (common under `/`, zero under `$HOME` today) never updates.                                                                                                                                                                                                                                                                    | The simplest correct rule.                                                                                                                                                       |
| B. Mutate: diff the walk against the catalog, append a record per change, compact when the log grows                                                                                                                                                                       | The log format, replay, torn-tail recovery, compaction, and a sweep for entries the walk did not visit, which needs the same faulted-directory rule as A. All written in S1 and exercised only by full re-crawls until S5.                                                                                                                                                      | One mechanism from the start. A re-run that changed little writes little.                                                                                                        |

**Recommendation:** A′ for S1, moving to A once measured fault frequency
justifies typed faults and subtree reconciliation. Telling a deletion from a
coverage fault needs the walker's help: `NotFound` from an entry's `lstat` is a
deletion, but the same error from reopening a saved directory (`reopen`) or from
ancestor discovery loses a subtree or its rules. So `Event::Io` gains an
operation tag, one of the D33 walker additions, and only `lstat` `NotFound` is
exempt. Until the tag exists, every `Event::Io` blocks publication. The log is
the daemon's requirement, and designing it before the daemon means designing it
without the workload that shapes it (small inotify bursts, not whole-tree
diffs). A forecloses nothing: B's log is an addition over A's snapshot. A′ is
the only variant that is correct with today's untyped faults; A's carry-forward
needs each fault to say which operation failed on which entry. The facts that
would change it: roots with permanent faults (anything under `/`), which make A′
never publish and so make A necessary; or a measured snapshot write that a
person would notice on a re-run, over about a second at 1M entries, which
favours B.

> Dave: Agree with A′. I hope we get to incremental indexing before moving to A
> makes sense.

**Answer (2026-09-27): A′.** Rebuild the snapshot each run with hash carry-over;
every `Event::Io` blocks publication until the walker tags its faults, after
which only an entry's `lstat` NotFound is exempt and a content fault publishes
the file unhashed. Move to A when typed faults exist, and ideally only once
incremental indexing makes it worth it.

**Amendment (2026-10-02, find milestone 1):** the walker now tags operations.
EACCES while opening, listing or reopening a directory is permanent user-chosen
state: publish its directory row without traversing it, with an unknown entry
count. Every other coverage fault still blocks publication, including EACCES
from an ignore-file read. An entry's lstat NotFound remains a deletion; content
faults remain publishable as unhashed files.

S1+ carries this EACCES amendment unchanged under the log: a covered opaque
directory retires its old children and subtree in the same final set, including
a partially listed prefix. New and initial denied roots publish as opaque roots;
repeated identical denials publish no generation. Other typed coverage faults
now follow M5's checked subtree retention (or block without a valid anchor),
including EACCES from an ignore-file read.

**Amendment (2026-10-25, stale work trees):** a `.git` file whose gitdir, or
the directory its `commondir` names, does not exist (`ENOENT`) is a known
state, not a coverage fault: the repository was moved or pruned, as after
removing a main checkout whose worktrees stayed behind. The directory is still
a work tree for `.gitignore`, contributes no exclude rules and records no work
tree, which is what rg does with no error. Three stale worktrees under `~/w`
had kept a 74 GB root from ever publishing. Every other gitdir failure (EACCES,
EIO, a path through a file, an unreadable or empty `commondir`) still faults:
those leave exclude rules unknown, not absent. The crawl already registers the
missing path as a policy input, so the gitdir reappearing refreshes the root.

## D27 — `InoId` and `NameId`: stable, or renumbered each snapshot

**Question:** When an entry is deleted, what happens to its dense ids?

| Option                                                             | Costs                                                                                                         | Buys                                                                                                                                                            |
| ------------------------------------------------------------------ | ------------------------------------------------------------------------------------------------------------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| A. Renumbered on every snapshot write, in the snapshot's own order | Nothing outside the catalog may store an `InoId` or `NameId`. The query log records paths, not ids.           | Dense ids with no holes and no free list. The snapshot chooses its row order (D28) for scan and lookup speed. With D26 A, this falls out of a rebuild for free. |
| B. Stable, with a free list for reuse                              | A free list in the format, and a reused id can be mistaken for its previous owner by anything that cached it. | Ids that survive across runs, which only something outside the catalog would want.                                                                              |
| C. Stable, never reused; holes until compaction                    | Holes in every `InoId`-indexed array until the next compaction.                                               | The same as B, without the reuse hazard.                                                                                                                        |

**Recommendation:** A. `DocId` is the only id another structure holds, and it
stays stable regardless. The fact that would change it: a structure outside the
catalog that must key by inode. The daemon's watch table is the likely one, and
it can key by `(dev, ino)` instead.

> Dave: Agree, but assume C when we get to incremental index. I'm not sure how
> this will work for metadata indexes though. For example, if I change the
> permissions on a file, the permissions will need to be reindexed. However, we
> don't want to re-index the contents for such a small change, so creating a new
> docId doesn't make sense. I _think_ the docId will change or remain stable
> depending on the type of change.

**Answer (2026-09-27): A now; C once indexing is incremental.** A metadata
change (permissions, mtime, owner, a rename) touches only inode and name rows. A
`DocId` changes only when content does, so metadata predicates never go through
documents.

**S1+ interpretation (2026-10-03):** [D52](#d52--d27-c-ids-across-compaction)
reads C as never reused within a checkpoint epoch, renumbered at compaction.
Proceeding on that recommendation; Dave may veto. This does not amend the
recorded answer above or weaken the independent DocId rule.

## D28 — Name layout: raw bytes sorted by parent, or front-coded

**Question:** How are the name bytes laid out in the snapshot?

Two readers want different things: the name scan (D14) reads every byte, and a
re-run or a path lookup wants a directory's children by name. A plain byte
search over concatenated names can match across a boundary, so each option needs
one.

| Option                                                                                                                                                     | Costs                                                                                                                                                                   | Buys                                                                                                                                                                                                |
| ---------------------------------------------------------------------------------------------------------------------------------------------------------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| A. Raw bytes, NUL-terminated, one heap; `names` rows sorted by (parent, name) with a `u32` offset each, so a directory's children are one contiguous range | 23.4 B per name as measured, plus 1 B terminator and 4 B offset in the row: about 28 MB at 1M names for the heap and offsets (estimate).                                | A name cannot contain NUL, so a substring search over the whole heap cannot match across names, and a hit's offset finds its row by binary search. Children are a slice, binary-searchable by name. |
| B. Front-coded per directory (shared prefix length + suffix)                                                                                               | Each name is decoded before matching, so a SIMD search over the heap no longer applies directly. Saving unmeasured; sibling names in source trees share short prefixes. | Fewer bytes, if siblings share enough prefix, as in `IMG_0001.jpg`-style directories.                                                                                                               |
| C. Raw bytes in walk order, with a separate sorted child index                                                                                             | 4 B more per name for the index, and walk order is not reproducible across runs with parallel workers.                                                                  | Nothing A lacks.                                                                                                                                                                                    |

The terminator settles literal substring search only. A glob or regex run over
the whole heap could consume a terminator or anchor to the heap's ends, so those
are evaluated per name slice, after a literal prefilter where the pattern has
one. Names are bytes, never re-encoded: a name with a newline or invalid UTF-8
is stored and matched as the kernel returned it, and only display escapes it.

**Recommendation:** A, and measure B on `$HOME` before the format is frozen: net
snapshot bytes and scan latency together, since density comes first in this
project. The fact that would change it: B saving enough of the whole snapshot to
matter at a scan cost that stays within the name-query target.

> Dave: I think this one is worth discussing. Since this is going to be in
> memory anyway and since we've already invested time into inpacking algorithms,
> what about a suffix array where each element references the name in the
> catalogue. So the catalogue has an entry for every indexed file or directory
> which includes metadata like permission byte, created at or last-updated
> timestamps, etc. These are referenced by docIds. We create a giant suffix
> array which I _think_ makes super fast search possible for all or most of the
> different query types, including regex. Otherwise, I agree with your
> recommendation.

**Answer (2026-09-27): A.** Raw NUL-terminated heap sorted by (parent, name);
measure front-coding before the format freezes. A suffix array, FM-index or name
trigrams over the heap is a later experiment: the heap is exactly the text such
an index is built on, so A forecloses none of them.

**Measured (2026-09-27).** `$HOME`'s 434,846 names from the walker dump,
committed through the real writer (`ferret-catalog`'s `names` example). The raw
heap is 10,591,121 B (24.36 B per name with its NUL). Front-coded per directory
against the previous sibling (a LEB128 prefix length, the suffix, a NUL), it is
7,592,302 B (17.46 B): 3.0 MB saved, 28% of the heap but 5.9% of the 51.2 MB
snapshot, whose largest part is the 64 B inode rows (27.8 MB). Warm scans, one
scalar first-byte `position` plus a compare of the rest, median of 30, counting
matching names: `ferret` (115 names) 6.8 ms raw vs 15.2 ms front-coded, `.js`
(62,297) 7.8 vs 15.5 ms, `q` (18,102) 4.8 vs 13.3 ms. Front-coding doubles to
nearly triples the scan, because each name is rebuilt before it is searched, to
save 6% of the file. A stands; the format is not switched.

## D29 — How a parallel walk tells the catalog each entry's parent

**Question:** How does the catalog writer learn the parent directory's row for
each entry `walk_parallel` reports, including under directories it does not
catalogue?

Events carry the root-relative path, decision and stat, but no parent
identifier. One worker lists a directory, but the saved children can be
processed by another worker when it takes over the parent (`run_worker` in
`walk.rs`), so a directory's children are neither contiguous nor confined to one
visitor. Two more gaps: the root emits no event, and a `Traverse` directory is
by definition not catalogued, yet a file re-included below it needs a parent
chain to the root.

| Option                                                                                                                                                                                                                               | Costs                                                                                                                                                                                                      | Buys                                                                                                |
| ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | --------------------------------------------------------------------------------------------------- |
| A. The walker carries a small value per directory: the visitor returns it for the root, a `Descend` and a `Traverse`, and the walk passes it back with each child. A traversed directory gets a structural row, excluded from search | An API change in `ferret-crawl` (a field in the walker's `Job`, 4–8 B each). One structural row per traversed directory, zero on `$HOME` today. The value is minted on whichever worker saw the directory. | No lookup by path at all. Workers build rows independently; one merge at the end assigns dense ids. |
| B. The visitor looks up the parent by path, in a map shared across workers                                                                                                                                                           | A map from path to row: the 43 MB of relative paths, or a hash of them, behind a lock or a concurrent map, hit once per entry. Traversed directories still need rows.                                      | No walker change.                                                                                   |
| C. Workers send owned events to one writer thread                                                                                                                                                                                    | A copy of each path (43 MB of allocation over `$HOME`) and a channel. The writer is still B inside.                                                                                                        | The simplest writer.                                                                                |

**Recommendation:** A. The walker already keeps a job per directory, so the
value costs a field, and the catalog never handles a path. The value is a
per-worker local id (worker, sequence), resolved to a dense `InoId` at the
merge. The fact that would change it: a measured merge cost that makes a single
writer (C) cheaper overall.

**Answer (2026-09-27): A,** as recommended (no comment).

## D30 — What `ferret find` builds when it opens the catalog

**Question:** Are the lookup structures DESIGN says are "derived at load" —
`hash → DocId`, `DocId → [InoId]`, `InoId → [NameId]`, and each directory's work
tree — rebuilt by every reader, or stored in the snapshot?

The name scan's target is single-digit milliseconds warm (estimate), and every
`ferret find` without the daemon pays whatever opening costs.

| Option                                                                                                                                                                                                            | Costs                                                                                                                                                                                            | Buys                                                                                                                        |
| ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ | --------------------------------------------------------------------------------------------------------------------------- |
| A. Rebuild all of them on open (DESIGN now)                                                                                                                                                                       | A hash map with one entry per document ever assigned, live or dead (D36) and three inverse arrays on every invocation: tens of milliseconds at 1M (estimate), which would dominate a name query. | The smallest file. Nothing to keep consistent.                                                                              |
| B. Store the query-path structures as arrays in the snapshot (`DocId → [InoId]` and `InoId → [NameId]` as offset-plus-list arrays, the work tree per directory); build the `hash → DocId` map only in the indexer | About 14–16 B per entry more on disk for the two inverses as offset-plus-list arrays (estimate), all sequential and mapped rather than read.                                                     | Opening is an `mmap`; a name-only query touches the heap and the rows it hits. The hash map is paid only by `ferret index`. |
| C. Build each lazily, on the first query that needs it                                                                                                                                                            | The first content query pays A's cost for its structure.                                                                                                                                         | A name-only query pays nothing, without growing the file.                                                                   |

**Recommendation:** C for S1, plus one persisted array. A name-scan hit
identifies its `NameId`, whose row gives the parent directory's `InoId`; but an
inode row holds no `NameId`, so walking up needs a directory's own name. With
D31 C every directory except a configured root (which has a `roots` row instead)
has exactly one. Directory inode rows are numbered first, so their `InoId`s are
the dense range `0..directories`, and a directory-only `InoId → NameId` array (4
B per directory, 0.3 MB for `$HOME`) is the whole path structure, and a
name-only `find` needs nothing else. `DocId → [InoId]` and the full inverse
serve `stats` and, from S2, content results; they are built when first needed,
and one is persisted when a measured query shows its build dominating. The fact
that would change it: a measured S1 query whose latency is mostly that build,
which moves that structure to B.

> Dave: I'm not sure where it fits in the schedule but I want this to be fast on
> devices where the daemon isn't running so we should measure the options. Speed
> to search from scratch should be one fasctor in deciding on the catalogue disk
> format. I'd err on the side of simplicity though. Otherwise, I'm fine with C
> for now.

**Answer (2026-09-27): C.** Nothing derived at open beyond the persisted
directory `InoId → NameId`. Cold `ferret find` without a daemon is a first-class
S1 measurement, in both forms (empty page cache; warm cache, fresh process), and
a factor in choosing the on-disk format. Prefer the simpler format when the
numbers are close.

**Measured (2026-09-27).** The same 51,159,311 B `$HOME` snapshot, opened by a
fresh process that reads the whole file and validates it (the `names` example's
`open-bench`). Warm, over 30 processes: read 17.8 ms, validate 0.9 ms, the whole
process 22.2 ms, median. Validating sibling order and exact name spans, added in
review so `lookup` agrees with iteration on any accepted file, raised validation
to 3.9 ms and the process to 25.9 ms. Cold is per-file eviction
(`dd iflag=nocache count=0`, checked with `fincore`), not `drop_caches`, which
needs root: read 23–33 ms and the process 29 ms over two runs of 20, on NVMe
with a load average near 2.5 from other work. So opening is reading, and reading
costs more than the name scan itself (5–8 ms). The name sections (names, heap,
directory names, traversed, roots, strings) are the first 16.1 MB of the file;
reading only them took 5.0 ms warm and 16.8 ms evicted. See D38.

## D31 — One inode, several names

**Question:** When hard links or a bind mount give one `(dev, ino)` several
names, is that one `inodes` row or one per name?

DESIGN already implies one row with several `names` edges, but also says a
directory has exactly one name, which a bind mount below a root breaks (D20: the
walk crosses mounts). The prevalence of either on `$HOME` is not measured.

| Option                                                                 | Costs                                                                                                                                                                                                                   | Buys                                                                                                                                    |
| ---------------------------------------------------------------------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | --------------------------------------------------------------------------------------------------------------------------------------- |
| A. One row per `(dev, ino)`, several `names` edges                     | The merge (D29) must unify rows by `(dev, ino)` across workers. A directory with two names has two parent chains, so a path is no longer unique per directory row; a path walk must start from the name, not the inode. | One stored row and one content association per inode; a doc's liveness counts distinct inodes. DESIGN's model as written for files.     |
| B. One row per observed name                                           | Several rows for one inode, each hashed unless the carry-over catches it, and doc liveness counted per name.                                                                                                            | Every row has exactly one path, so directories keep the one-name rule.                                                                  |
| C. A for files; B for directories (a directory seen twice is two rows) | Two rules instead of one.                                                                                                                                                                                               | Hard-linked files share one row and one stored hash, and a directory row keeps one path, so work-tree membership stays one parent walk. |

**Recommendation:** C. Hard-linked files are what dedup is for; aliased
directories are rare and mostly bind mounts, where two independent subtrees are
the honest result (D15 is per path). This identity rule governs D26 and D29: the
merge deduplicates **files** by `(dev, ino)`, which is also the hash carry-over
key; directory rows are not unique by `(dev, ino)`, and an old directory row is
found by its path edge (parent row and name) plus its identity. The fact that
would change it: a census showing many aliased directories **and** a way to give
each alias its own path context under one row; without the second, A cannot
represent them.

Two names of one file can reach `Index` on different workers at the same time,
so the merge, not the walk, guarantees one row and one `DocId`: each worker
hashes what it sees, and the merge unifies rows by `(dev, ino)`. A hard-linked
file may therefore be read more than once in a run. A shared in-flight cache
would prevent that, at the cost of a key, a hash and a lock-protected state for
every hashed inode held for the whole crawl, which the density goal argues
against while hard-link prevalence is unmeasured. The fact that would add the
cache: a census showing enough hard-linked bytes that the duplicate reads show
in crawl time.

> Dave: Would a central cache really make that much of a difference to
> performance? I imagine it would be quick enough not to have much impact while
> having a net complexity benefit. Happy either way, but I think this is worth
> measuring unless you're confident this isn't the case.

Without the cache, two workers can hash one inode on either side of an edit,
each passing D33's stat bracket. The merge never combines one observation's
metadata with another's hash: differing metadata or hashes for one `(dev, ino)`
in a run are a D26 content fault, and the inode is published with one coherent
metadata observation and no `DocId` until the next run.

**Answer (2026-09-27): C, with a shared per-run cache after all.** Workers probe
and insert into a sharded `(dev, ino)` map directly (there is no cache thread);
a probe is about 100 ns and hashing stays on the worker, so the cost is peak
memory only: the map holds inodes hashed this run, about 20 MB on `$HOME`'s
first run and 50 MB at 1M (estimates), small on re-runs. Each inode is observed
once per run, so the conflicting-observation rule below is no longer needed.

## D32 — A reader while `ferret index` runs

**Question:** What does `ferret find` see while `ferret index` is writing?

| Option                                                                                                                                                                                    | Costs                                                                                                                                                                          | Buys                                                                                                             |
| ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ | ---------------------------------------------------------------------------------------------------------------- |
| A. Generations: the writer takes a lock, writes a new snapshot file with fsync, renames it into place and fsyncs the directory; a reader maps whichever generation it opened and keeps it | A reader holds the old file's pages until it exits. In S2 the catalog and the postings need one shared generation boundary, so the index commits against a catalog generation. | Readers never wait and never see a half-written file. Two concurrent `ferret index` runs are refused, not raced. |
| B. A reader-writer lock over the catalog directory                                                                                                                                        | `ferret find` waits for the whole index run, seconds cold.                                                                                                                     | Simpler, with no old generations to keep.                                                                        |

**Recommendation:** A. It is the same rename sequence DESIGN already specifies,
plus a lock file. The fact that would change it: readers pinning old generations
long enough to exceed the disk or memory budget, as a resident daemon might,
which would call for a reader handshake before the old file is released.

> Dave: Agreed

**Answer (2026-09-27): A.**

## D33 — What the walker must also hand the catalog

**Question:** The catalog needs two things the walker does not expose: an open
file to hash and sniff, and each work tree's kind and repository for the
`worktrees` table (D23). Does the walker expose them, or does the catalog reopen
by path?

| Option                                                                                                                                                                                                                     | Costs                                                                                                                                                                                                                                                        | Buys                                                                                                                     |
| -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ | ------------------------------------------------------------------------------------------------------------------------ |
| A. Extend the walker: an `Index` event lends the parent directory's handle so the visitor opens the file with `openat(O_NOFOLLOW)` and checks `(dev, ino)`; a `.git` probe emits the work tree's kind and common directory | Two additions to the public walker API. The walker distinguishes a `.git` directory from a `.git` file and follows `commondir` to read `info/exclude`, but keeps neither the kind nor the common directory's path; those must be produced, not only exposed. | Hashing gets the race safety D21 bought for the walk. The `.git` classification the walker already does is not repeated. |
| B. The catalog reopens each file by its full path and re-probes `.git`                                                                                                                                                     | A path join per hashed file, a path resolution the kernel repeats, and a window in which a swapped symlink along the path is followed. A second `.git` parser.                                                                                               | No walker change.                                                                                                        |

A handle check proves the name still names the inode; it does not prove the
inode held still while it was read. So in A the hash is taken between two
`fstat`s of the open file, and the first must also match the stat in the
`Decided` event, since that is what the catalog row records. Any mismatch
retries once, then is a content fault (D26): the file is published unhashed for
this run, rather than with a hash that matches neither version.

**Recommendation:** A, with the stat-bracketed hash. It keeps D21's guarantee
end to end, and the walker's `.git` probe is the natural place to classify work
trees. The fact that would change it: hashing inside a walker callback
measurably defeating crawl throughput, which would change how handles are passed
(a queue of opened descriptors for hashing workers) while keeping
handle-relative opens.

> Dave: Agreed

**Answer (2026-09-27): A.**

## D34 — Which roots a re-run replaces

**Question:** After `ferret index /a /b`, does `ferret index /a` refresh `/a`
and keep `/b`, or replace the catalog with `/a` alone?

| Option                                                                                                                                           | Costs                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                            | Buys                                                                                                                  |
| ------------------------------------------------------------------------------------------------------------------------------------------------ | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | --------------------------------------------------------------------------------------------------------------------- |
| A. The invocation's roots are the whole set: anything not named is dropped                                                                       | Forgetting a root on the command line deletes it from the catalog.                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                               | One rule, no ownership bookkeeping.                                                                                   |
| B. Named roots are refreshed and others kept; removal is explicit (`ferret roots remove`); with no arguments, every configured root is refreshed | Each root owns its directory rows and name edges, and a rebuild copies the untouched roots' forward. File inodes and docs cannot be owned by one root, since a hard link can join two disjoint roots (D31); they are global, live while any retained name references them, and a fresh observation in this run supersedes a carried one. An overlapping pair (`~` and `~/w/x`, where D25 lets the inner root expose what the outer one ignores) needs an owner for the shared subtree, and the walker a way to stop at it: `walk_parallel` has no root-boundary parameter today. | Refreshing one tree is cheap and safe; the configured roots in `config` are the authority, not the last command line. |

**Recommendation:** B, with overlap resolved by the innermost configured root
owning its subtree: the outer root's walk is given the inner roots as boundaries
and stops at each, and the inner root's rules and D25 behaviour apply there.
Adding or removing a root that overlaps another changes which rules govern that
subtree, so the affected region is re-walked before the new root set is
published, never copied. The fact that would change it: roots never overlapping
in practice, which would let B drop the ownership rule but not the explicit
removal.

> Dave: Interesting question. I think you should be able to name an index
> location with an optional ENV VAR override. In any case, asking to index a
> directory should update the roots in the index config (either default location
> or specified location). There should also be a CLI command for removing a
> root, and `ferret index` should index the current roots or prompt the user to
> specify if there are none. Let me know if I'm missing the point of this
> question.

**Answer (2026-09-27): B.** The index lives in `$XDG_DATA_HOME/ferret` by
default, overridden by `--index <dir>` or `FERRET_INDEX`, and its roots are
stored with it. `ferret index <dir>` adds the root and refreshes it;
`ferret roots remove <dir>` drops one; bare `ferret index` refreshes every root,
prompting at a terminal when there are none and failing with a message
otherwise. The innermost root owns its subtree. Roots are independent: removing
`~` leaves `~/w/x` indexed, and adding `~` does not absorb `~/w/x`.

## D35 — Work-tree context for a root inside a repository

**Question:** For root `repo/src` (D22), the work tree's top directory is above
the root and has no row, yet D23 keys `worktrees` by that row. Where does the
context live?

| Option                                                                                                                 | Costs                                                                                                                    | Buys                                                                                                                                            |
| ---------------------------------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------ | ----------------------------------------------------------------------------------------------------------------------------------------------- |
| A. The `roots` row carries inherited context: kind, repository id, and the root's path relative to the work tree's top | A special case in `roots`, a few tens of bytes per root.                                                                 | Grouping a hit across work trees compares repository-relative paths, which needs exactly that prefix; nothing outside a root becomes an entity. |
| B. Synthesize rows for the ancestors up to the work tree's top                                                         | Rows for directories outside every root, which the name scan must then skip, and which D34's ownership must account for. | `worktrees` stays uniform.                                                                                                                      |

**Recommendation:** A. The fact that would change it: ancestors outside roots
becoming searchable, which D18's later "links that pull content in" might bring.

> Dave: I'm inclined to say that a root within a repo loses all context outside
> of that repo so it would be treated as outside of a git repo, let alone know
> anything about worktrees. Tell me that isn't even an optoin here.

**Answer (2026-09-27): moot.** D22 is revised to A, so a root inside a work tree
is treated as outside one and carries no work-tree context. `worktrees` covers
only work trees whose top is at or below a root.

## D36 — Dead documents before there is an index to merge

**Question:** In S1 nothing reclaims a dead `DocId` (only an S2 merge may), so
every previously unseen content adds a `docs` row forever; a revert reactivates
an existing one. What bounds that?

| Option                                                                                       | Costs                                                                                                                     | Buys                                                                                                         |
| -------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------ |
| A. Keep dead docs; content that reappears (a revert, a checkout) reactivates its old `DocId` | 16 B hash plus a live bit per historical content, growing with churn until S2's merge exists. Unmeasured on this machine. | Stable ids across a revert, which is what S2's postings want, and the S1 data shows how much churn there is. |
| B. Keep only the ordinal high-water mark; drop dead rows at each rebuild                     | Content that reappears gets a new `DocId`; the id space has holes.                                                        | The table holds live content only.                                                                           |
| C. Allow an explicit reset that renumbers everything                                         | Invalidates every `DocId`, which is harmless in S1 (no index) and expensive after S2.                                     | Bounded storage by command.                                                                                  |

**Recommendation:** A, and record dead-doc count and bytes in `ferret stats` so
the budget is measured rather than guessed. Reactivation holds only while the
dead doc's postings exist: once an S2 merge reclaims them, reappearing content
gets a new `DocId` and is indexed again. The fact that would change it: measured
churn making the history a material share of the catalog before S2's merge
lands, in which case B.

> Dave: When would you even find out a document is dead? For S1, I thought we
> were doing a full reindex for each update and the index was otherwise static?

**Answer (2026-09-27): B for S1.** A document is dead when no inode in the new
snapshot carries its hash, which each rebuild finds. S1 has no postings, so
nothing needs a dead `DocId`: drop the rows and keep the next-id counter so ids
are never reused. Whether a revert reactivates its old `DocId` is an S2
decision, where postings make it worth something.

## D37 — Remembering why a file has no document

**Question:** `DocId = none` means too large, binary, or failed to hash. After
the size cap is raised, or the sniffer changes, which unchanged files are read
again?

| Option                                                                                                                           | Costs                                                                                                                                                                                                                                                | Buys                                                                                                                                    |
| -------------------------------------------------------------------------------------------------------------------------------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | --------------------------------------------------------------------------------------------------------------------------------------- |
| A. Re-sniff every row without a document on every run                                                                            | A head read of every binary file each run: every image, archive and object file, warm or cold.                                                                                                                                                       | No extra state.                                                                                                                         |
| B. A content state per inode (not content-eligible / binary / hashed / fault), plus the sniffer's version in the snapshot header | 2 bits per inode in a side array (0.25 MB at 1M), since the 64 B row has no spare byte. A sniffer-version change must refresh every root before the header advances, or D34's partial refresh would carry old classifications under the new version. | An unchanged binary is not re-read; a new cap re-evaluates only rows whose state the cap affects; a new sniffer version re-sniffs once. |

**Recommendation:** B. Current policy decides eligibility each run; the stored
state only says what the last observation found. The fact that would change it:
a measured re-sniff of `$HOME`'s binaries cheap enough to ignore.

> Dave: B is cheap. Let's do it.

**Answer (2026-09-27): B.** A 2-bit content state per inode plus the sniffer
version; a version change refreshes every root.

## D38 — Reading the snapshot: whole file, or by section

**Question:** Should a reader load the whole snapshot at open, or only the
sections a query needs?

Measured on `$HOME` (D30): opening a 51.2 MB snapshot costs a fresh process
about 18 ms warm and 23–33 ms evicted, nearly all of it the read, against a 5–8
ms warm name scan. A name-only `find` needs only the first 16.1 MB. The file
already puts those sections first and every section is located by the table, so
this is a reader change, not a format change.

| Option                                                                                                        | Costs                                                                                                                                                                                                                                                                      | Buys                                                                                                                             |
| ------------------------------------------------------------------------------------------------------------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | -------------------------------------------------------------------------------------------------------------------------------- |
| A. Read the whole file, then validate (now)                                                                   | 18 ms warm per fresh process at `$HOME`'s size, growing with every inode and document row a name query never reads.                                                                                                                                                        | One read, one validation pass, every accessor infallible after open.                                                             |
| B. Read the header and table, then each section on first use with a positional read into its own buffer (std) | Validation splits per section, and cross-section checks (a name's child is below the inode count) move to when both are loaded. Accessors of a later section take a load step. A generation replaced mid-open is still safe: the reader holds the open file, not the path. | 5.0 ms warm and 16.8 ms evicted for a name-only query (measured), and a content query pays for the inode and doc rows only then. |
| C. `mmap` the file                                                                                            | `unsafe` or a dependency (D11), and a file truncated under the mapping is SIGBUS rather than an error; validation then touches every page anyway unless it too is split as in B.                                                                                           | No copy: warm cost falls to page-table setup plus the pages touched.                                                             |

**Recommendation:** B, in slice 5 when the name query exists to measure it end
to end. It is std-only, keeps reads-are-errors semantics, and captures most of
C's gain for a name query. The fact that would change it: B measured at more
than a few milliseconds above C for a warm name query, which reopens C under
D11.

**Answer (2026-09-27): B, through D40.** Build for 10M; section reads land in
slice 5.

## D39 — A checksum over the snapshot

**Question:** Should the snapshot carry a checksum, so corruption in a value
nothing indexes by (a time, a hash, a name's bytes) is detected?

Validation now proves every offset, index and ordering safe to follow, so a
corrupt file never panics or loops (tested with every truncation and every
single-bit flip of a sample). A flip in a stored value still reads back as a
different value: a wrong mtime means one needless re-hash; a wrong content hash
means a file's content is misattributed until it changes.

| Option                                               | Costs                                                                                                                  | Buys                                                             |
| ---------------------------------------------------- | ---------------------------------------------------------------------------------------------------------------------- | ---------------------------------------------------------------- |
| A. No checksum; structural validation only (now)     | Silent value corruption, which on a checksummed filesystem or ECC memory is rare.                                      | Nothing added to open, which is already the dominant cost (D38). |
| B. One checksum over the file, checked at open       | Hashing all 51 MB at open (unmeasured, several ms at scalar speed), and it forces B of D38 back to reading everything. | Any corruption is detected.                                      |
| C. A checksum per section, checked when it is loaded | 8 B per section in the table; the cost is paid per section read, which fits D38 B.                                     | Detection without reading sections a query never uses.           |

**Recommendation:** A for S1. The snapshot is rebuilt each run (D26), so value
corruption lives one run, and the writer already decodes what it wrote before
publishing it. Revisit with C when D38 B lands, since the table is the natural
place for it. The fact that would change it: a corrupt snapshot seen in
practice.

> Dave: Agreed. S1 is about having something to measure so we can pick the right
> data structures. Once we have incremental indexing, we can start thinking
> about things like checksumming.

**Answer (2026-09-27): A.** No checksum in S1, which exists to measure what the
right data structures are; checksumming is revisited once incremental indexing
lands.

## D40 — What entry count S1 is built for

**Question:** Everything through D39 assumed about 1M files. Dave expects his
file count to grow 10–100× with agentic coding and his knowledge base
(`~/w/lab`, `~/w/kb`), possibly before this machine is replaced. What count
should S1 be built for?

These are measured at `$HOME`'s 435k entries and scaled linearly, so every
figure past 435k is an estimate:

|                                      | 435k   | 10× (4.3M)     | 100× (43M)          |
| ------------------------------------ | ------ | -------------- | ------------------- |
| Snapshot                             | 51 MB  | 0.5 GB         | 5 GB                |
| Name scan, warm (naive / SIMD-class) | 5–8 ms | 50–80 / ~10 ms | 0.5–0.8 s / ~100 ms |
| Open by reading the whole file       | 22 ms  | ~0.2 s         | ~2 s                |
| Full rebuild per run (D26)           | 0.66 s | ~7 s           | ~70 s, writing 5 GB |

The catalog is derived data, so a later format change costs a re-index, not a
migration. Much of the growth is also build output and linked work trees, which
default ignores and D23 already absorb.

The choices that do not scale are:

- reading the whole file on every `find` (D38);
- a rewrite on every run (D26);
- a linear scan without a daemon (D14);
- the 64 B inode row.

| Option                                          | Costs                                                                                                                                               | Buys                                                                   |
| ----------------------------------------------- | --------------------------------------------------------------------------------------------------------------------------------------------------- | ---------------------------------------------------------------------- |
| A. Keep ~1M; measure 10× and 100× synthetically | Likely rework of D26, D38 and the scan within months                                                                                                | S1 ships soonest                                                       |
| B. Build for ~10M; measure at 100×              | Section reads (D38 B) and a SIMD-class scanner in slice 5; a synthetic 10M build under ~2 GB peak in slice 4; the incremental catalog next after S1 | `find` stays interactive without a daemon at the size expected soonest |
| C. Build for ~50M now                           | Name trigrams or a suffix array, incremental writes and narrower rows, all in S1; weeks, designed before any usage data                             | No redesign later                                                      |

**Recommendation:** B. A rebuildable catalog makes deferring 100× cheap, but not
10×. The fact that would change it: the `ferret stats` census showing catalogued
entries growing faster than about 2× a quarter, in which case move to C sooner.

> Dave: Let's go for B. Build for 10M. Now that I think about it a little more,
> I think that's a more realistic cap anyway.

**Answer (2026-09-27): B.** S1 is built for about 10M catalogued entries and
measured at about 40M. This also answers D38: B, section reads, in slice 5.
After S1 the incremental catalog (D26 B, the change log) moves ahead of the
daemon.

## D41 — The name scanner: `memchr::memmem`, or our own

**Question:** At 10M entries (D40) the name heap is about 230 MB. A substring
scan must run at SIMD-class speed, 5 GB/s or more, to keep `find` near 50 ms
warm without a daemon. Today's naive scan manages about 1.5 GB/s. Whose scanner
should it be?

Case-insensitive matching is the likely default, and it is what separates the
options. `memmem` compares bytes exactly. A case-insensitive search through it
needs a lowercased second heap (+10 MB at `$HOME`, +230 MB at 10M) or a
different algorithm. A scanner of our own can fold ASCII case inside its
candidate filter, testing two needle bytes each in both cases, at little cost.

| Option                                                                                                                                                                      | Costs                                                                                                                                    | Buys                                                                                                                                                   |
| --------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ---------------------------------------------------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------ |
| A. `memchr::memmem` (MIT / Unlicense). It is already in the tree through `regex`                                                                                            | A direct `ferret-query → memchr` edge in DESIGN. Case-insensitive search needs a folded copy of the heap: density lost, or a slower path | Well-tuned and SIMD with runtime detection, with nothing to write                                                                                      |
| B. Our own: a two-byte candidate filter with ASCII case folding. An AVX2 arm through `core::arch`, allowed per D11 with a toolchain-ledger entry, plus a safe SWAR fallback | A few days' work, plus `unsafe` on one audited item and its ledger row                                                                   | Case-insensitive search at full speed with no second heap. It is ours to learn and tune, and the same filter serves glob literals and regex prefilters |
| C. Our own, safe SWAR only (u64 bit tricks, std)                                                                                                                            | About 2–4 GB/s (estimate), below the target at 10M                                                                                       | No `unsafe`, simple                                                                                                                                    |

**Recommendation:** B. Benchmark it in slice 5 against `memmem` as the control,
using the D40 query mix. The fact that would change it: B falling more than
about 30% behind `memmem` on case-sensitive queries. In that case use `memmem`
for the case-sensitive path and keep B for case-folded search.

> Dave: Agree, and I like your reasoning.

**Answer (2026-09-27): B.** Our own case-folding two-byte candidate filter, with
an AVX2 arm under D11 plus a toolchain-ledger row and a safe SWAR fallback,
benchmarked in slice 5 against `memchr::memmem` as the control.

**Measured (slice 5a, 2026-09-28, `ferret-bench scan`, warm).** The AVX2 arm
runs 16–33 GB/s on the synthetic 10M heap (244 MB) and 16–32 GB/s at 40M (974
MB); SWAR runs 3.2–3.6 GB/s everywhere, which is why the AVX2 arm is in the
ledger. Against `memmem` on case-sensitive search, the arm takes 2–18% longer at
10M and 19–31% longer at 40M (`Flamegraph` 30.6 against 25.7 ms). The worst
case, `test` at 40M with 592k hits (43.5 against 33.1 ms), is on one side of the
30% line or the other depending on the metric: 31.4% longer in time, but 23.9%
lower in throughput. "Falling more than about 30% behind" in the recommendation
above does not say which, so the measurement neither clearly triggers nor
clearly clears it. The `memmem` fallback was not added: it would put `memchr` in
a product crate for about 10 ms on a query that spends 730 ms loading sections
(D43). Whether that stands is put to Dave to confirm. Folded, the arm is 16–24
GB/s; `memmem` on a lower-cased copy of the heap is faster (23–36 GB/s) but
needs the second heap. A byte-frequency table taken from real name heaps, in
place of the English one that picks the probe pair, is the obvious next tuning
step.

> Dave: Definitely happy to keep our own version here. Tuning is not a high
> priority right now. We can look at that later.

**Answer (2026-09-30): own scanner, no `memmem` fallback.** Tuning the probe
pair from real name heaps waits.

## D42 — A re-run's memory: the previous generation held through the walk

**Question:** A re-run holds the whole previous catalog in memory while it
walks, for hash carry-over and for copying kept roots forward. Should that be
cut?

Measured on synthetic trees (slice 4):

| Build       | 10M entries                | 40M entries   |
| ----------- | -------------------------- | ------------- |
| First build | 1.66 GB peak, 5.5 s commit | 6.4 GB, 23 s  |
| Re-run      | 2.56 GB peak, 7.6–8.9 s    | 10.1 GB, 24 s |

On `$HOME` a first run peaks at 78 MB and a re-run at 110 MB.

| Option                                                                                                                                       | Costs                                                                                                                                                              | Buys                                                                            |
| -------------------------------------------------------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------ | ------------------------------------------------------------------------------- |
| A. Accept it                                                                                                                                 | Nothing now; 2.56 GB at 10M, about 10 GB at 40M                                                                                                                    | Nothing foreclosed                                                              |
| B. Compact carry table: at `begin`, extract `(dev, ino, size, mtime, ctime, state, hash)`, run `keep`, then drop the catalog before the walk | About a day; `previous()` narrows                                                                                                                                  | Re-run peak about 1.9 GB at 10M (carry table about 0.45 GB)                     |
| C. `mmap` the snapshot, for readers and for the writer's previous generation                                                                 | `unsafe` under D11 with a measurement to justify it; SIGBUS if the file is truncated under the map (the writer only renames); cuts across D38 B's positional reads | File-backed, reclaimable pages; fixes writer peak and reader open time together |

**Recommendation:** A for S1, then C when S2 designs the reader's open path,
since C fixes writer and reader together. The fact that would change it: a real
10M tree whose re-run peak matters on an 8–16 GB desktop, which makes B,
self-contained, worth doing now.

> Dave: I'm not sure I understand why we're keeping the previous run around,
> when it seems like we're not saving time by doing so. Perhaps you could
> explain that. Why do we need hash carry-over and what does it mean to copy
> kept roots forward. Is that because we don't have a roots config yet? That
> seems like a minor issue. You could read the roots and then throw away the old
> generation before even start the new generation.

**Reply (2026-09-30).** The previous generation does two jobs during a run, and
both save time. Neither is about the roots config, which is already stored with
the index (D34).

- **Hash carry-over.** A file whose `(dev, ino, size, mtime, ctime)` is
  unchanged keeps its hash and `DocId` without being read. That is the whole
  difference between a first run and a re-run on `$HOME`: 29.1 s and 10.3 GB
  read cold, against 0.74 s. Without it every re-run re-reads every file.
- **Kept roots.** `ferret index ~/w` walks only `~/w`. Because S1 rewrites the
  whole snapshot (D26 A′), every other root's rows are copied from the previous
  generation rather than re-walked.

Dropping the old generation before the walk loses both. What _is_ held longer
than it needs to be is everything else in it (names, heaps, links): only the
carry table, about 45 B per inode, has to live through the walk, which is option
B. S1+ (the incremental catalog) removes the question, because the previous
generation becomes the base a change log applies to rather than a source to copy
from, and compaction (D48 A) shrinks it in the meantime. Recommendation
unchanged: A, with B only if a re-run's peak bites before S1+.

## D43 — A 10M name query costs its section loads, not its scan

**Question:** At 10M entries a name query spends about 190 ms warm loading the
name sections and under 15 ms scanning. Should the reader's open path change in
slice 5b?

Measured in slice 5a with `ferret-bench open` and `query`, the whole path from
`Catalog::open` to the last row. Loads use positional reads into owned buffers
(D38 B).

| Catalog             | Name sections | Load, warm | Load, evicted | Rare word, warm | Scan alone |
| ------------------- | ------------- | ---------- | ------------- | --------------- | ---------- |
| `$HOME`, 441k names | 16.3 MB       | 4.6 ms     | 17.7 ms       | 5.1 ms          | 0.3 ms     |
| synthetic 10M       | 371 MB        | 196 ms     | 329 ms        | 206 ms          | 7.4 ms     |
| synthetic 40M       | 1.48 GB       | 729 ms     | 1,349 ms      | 763 ms          | 30.6 ms    |

The 10M load, split by a second measurement (reading the same 371 MB from the
file into a fresh buffer, then into the same buffer again): about 95 ms is page
faults on the fresh buffers, 27 ms the copy, and about 74 ms, by difference,
per-section validation. At 40M the same split is 354, 107 and about 270 ms.
These are a subtraction between two measurements, not a measured `mmap`. D38
said B stands unless it measures "more than a few milliseconds above C for a
warm name query". The 122 ms of fault and copy at 10M is an **upper bound** on
what C (`mmap`) could save, not a saving: a mapped file still faults its pages
in as validation and the scan traverse them, cheaper per page than faulting
fresh anonymous memory and copying, but not free. What settles it is an `mmap`
open measured with the same validation.

| Option                                                                                                          | Costs                                                                                                                                                               | Buys                                                                                                                                                                |
| --------------------------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| A. Keep section reads (now)                                                                                     | About 200 ms warm per query at 10M and 760 ms at 40M, four times D41's 50 ms target. Nothing to write                                                               | Std only, reads are errors, every accessor infallible after load                                                                                                    |
| B. `mmap` the snapshot (D38 C, D42 C), keeping per-section validation                                           | `unsafe` under D11 with a ledger-style note; SIGBUS if the file is truncated under the map (the writer only renames). Validation still touches every page it checks | Removes the copy and the faults into fresh memory, at most 122 ms of the 196 at 10M, less the cost of faulting the mapped pages, which is unmeasured. Fixes D42 too |
| C. Validate at use: check a row's offsets and ids when a query follows them, rather than whole sections at load | A branch per followed row, in accessors that become fallible or clamp; the "a loaded section is sound" invariant becomes per-access                                 | About 74 ms at 10M; combines with A or B                                                                                                                            |

**Recommendation:** B and C measured together in slice 5b, as an experiment
against today's reader, before the CLI's timings are published: an `mmap` open
with today's validation, then with validation at use. B alone leaves about 74 ms
of validation at 10M, which is still above the target, so C is what gets a warm
10M name query near 50 ms. The fact that would change it: a measured `mmap` open
whose page-table and fault cost is not well below the 122 ms it replaces, in
which case A plus C is the safe answer.

> Dave: I think mmap make sense in some cases, but I'm not sure how much
> difference it will make here. The name heap needs to be read in full to do a
> full scan, and I think it should stay resident in memory in the daemon. The
> would change if we had an index. Also, looking at the data structures, I see a
> lot of opportunities for compression. I think I'd rather address that first,
> and possibly even before that, establish the optimal index format.
>
> On the topic of better compression, I think we should introduce bitfield. If
> we know how many files we have, we know how many bits we need for the InoIds.
> Surely mtime and ctime don't need 12 bytes each. UID and GUID only need two
> bytes I believe. If I know the maximum file size, I know how many bits I need
> for the size. etc. My estimate is that we'd be able to halve the size of
> Inodes section.
>
> So in summary, yes to mmap, but we need to look at better optimising the size
> of data structures on disk and in memory to ensure we can read the full
> catalogue into memory, and we need to look at indexing for larger cataloges so
> that we don't need to read the full name list into memory to search it. Then
> looking at mmap makes more sense to me.

**Answer (2026-09-30): B later, after compaction and a name index.** The order
is: compact the catalog's structures on disk and in memory, then index names for
large catalogs so a query need not read the whole name list, then `mmap`, mainly
for indexes. D48 puts the next step to Dave.

Notes for the compaction slice, from today's layout. An inode row is 64 B: dev
8, ino 8, size 8, mtime s 8, ctime s 8, mtime ns 4, ctime ns 4, mode 4, uid 4,
gid 4, `DocId` 4. On `$HOME` Inodes are 65 of the catalog's 106 B per name.

- Fixed-width bit-packed columns, each width taken from the catalog's own
  maximum (the bitfield idea), keep O(1) row access, which single inode reads
  rely on. Variable-length codes would not.
- `dev`, `mode` and `(uid, gid)` have a handful of distinct values per catalog,
  so a per-catalog dictionary and a few bits beat fixed narrow ints. Two bytes
  is not enough for uid/gid in general: rootless containers' subordinate ranges
  start at 100000, and such files are under `~/.local/share/containers`.
- Times as an offset from the catalog's earliest, plus nanoseconds, which change
  detection needs on ext4 and btrfs.
- A rough estimate: about 26 B per row against 64, better than half. The
  compaction slice measures it rather than trusting this.

## D44 — Single inode reads against an evicted catalog

**Question:** Should a name query's choice between single inode reads and
loading the Inodes section depend on whether the catalog is in the page cache?

5a gave `run` a limit. A query reads result rows one at a time until it has read
`max(inode_count / 64, 256)` rows (156,273 at 10M), then loads the section. The
limit was tuned warm, and 5a's note asked 5b to revisit it for a cold file.
Slice 5b measured it through the `ferret` binary on the synthetic 10M catalog,
against the D38 B reader:

| query        | rows    | single reads              | evicted  | fresh process, warm |
| ------------ | ------- | ------------------------- | -------- | ------------------- |
| `flamegraph` | 115     | 115                       | 390 ms   | 233 ms              |
| `ext:jpg`    | 358,570 | 156,273, then the section | 970 ms   | 576 ms              |
| `test`       | 157,527 | 156,273, then the section | 2,307 ms | 634 ms              |

`test` pays for both paths: 156k scattered reads on a cold file, then the 640 MB
section anyway. `ext:jpg` pays for the same count, but its rows are clustered,
so its reads probably land on pages already read (inferred, not measured).

| Option                                                                                                                   | Costs                                                                                                                                     | Buys                                                       |
| ------------------------------------------------------------------------------------------------------------------------ | ----------------------------------------------------------------------------------------------------------------------------------------- | ---------------------------------------------------------- |
| A. Keep the limit                                                                                                        | A cold broad query at 10M costs up to 2.3 s. An evicted query that only loads the section (`size:>100M`) takes 0.6 s                      | Nothing to write                                           |
| B. A lower fixed limit, such as `inode_count / 1024` (about 10k at 10M)                                                  | Warm, broad-but-rare queries pay the section load sooner: 5a measured `case:test` at 306 ms by single reads against 525 ms by the section | Bounds the cold worst case near one section load. Std only |
| C. Ask the kernel: `cachestat(2)` (Linux 6.5+) on the Inodes range, and pick the path by residency                       | A syscall with no std wrapper: `rustix` in `ferret-catalog`, a new edge in the crate graph. Falls back to A or B on older kernels         | The right path both warm and cold, with no tuning          |
| D. Let D43 decide: with an `mmap` reader, a single read is a page touch, and a load costs only its faults and validation | Waits on D43                                                                                                                              | The limit may vanish rather than being tuned               |

**Recommendation:** D, then B if D43 keeps section reads. The limit exists
because a section load costs 640 MB of reads and copies, which is exactly what
D43 B would remove. The fact that would change it: D43 settling on A. In that
case C is worth its dependency, since a daemonless CLI is often cold.

> Dave: I've kind of answered this above. Yes to mmap. I'd like this to be fast
> when running a find and we don't have a daemon running, but I'd like to
> optimise for daemon use. In fact, perhaps we simplify things for ourselves if
> we say the only mode of operation is with a daemon. Most of these questions
> seem to expect that. Otherwise, every search is a read from disk. So what we
> end up tuning is how much memory we allow ferret to use, and how we optimise
> our mmap cache swapping. I.e. go we read the names and inodes into memory in
> full, and only mmap indexes. I'd guess this would be the right answer, at
> least up to 1M files.

**Answer (2026-09-30): superseded by D46.** Whether cold reads get tuned at all
depends on whether the daemon is the only mode, which D46 asks.

## D45 — Query text in the local log

**Question:** Should the local query log keep query text by default, with no way
to turn it off?

Slice 5b's log (`$XDG_STATE_HOME/ferret/log.jsonl`, mode 0600) keeps each
query's atoms as typed, with the plan, counts and times. No field holds a result
path, a root path or an id: results and roots would be the most revealing data,
and the planned experiments do not need them. Query text stays, because the
experiments are about which atom classes people use and what they cost, and an
atom's class is only knowable from its text. That text is user data, and it can
itself be a path: `find path:/home/dave/private` logs `/home/dave/private`, and
a word or glob logs the name searched for. So the log is not path-free, only
free of paths ferret chose to write.

| Option                                                                                   | Costs                                                                               | Buys                                                            |
| ---------------------------------------------------------------------------------------- | ----------------------------------------------------------------------------------- | --------------------------------------------------------------- |
| A. As built: keep the text; opt-in upload later redacts or asks                          | Searched-for names sit in a 0600 file in the user's own state directory             | The log is useful to Dave now, as the S1 "something to measure" |
| B. A. plus a config switch (`log = false` / `log = "shape"`) when the config file exists | One setting; the config file does not exist yet                                     | A user who minds can turn it off, or keep only shapes           |
| C. Log only the query's shape: atom kinds, literal lengths, the plan                     | Loses the text, so a regression on one real query cannot be reproduced from the log | Nothing identifying is ever written                             |

**Recommendation:** A now, B with the config file. Redaction belongs to the
upload step (S6), which has to decide what leaves the machine regardless. The
fact that would change it: a plan to sync or upload the state directory before
S6. In that case C should be the default.

> Dave: Agree

**Answer (2026-09-30): A.** The log keeps query text; a config switch (B) comes
with the config file.

## Slice 5b's CLI calls (answered 2026-09-29, on the decisions page)

- **`stats --json`: yes.** Dave: "always make choice to optimise the tool for
  agents." That is a standing rule for CLI choices, not only this one.
- **Bare `index` with no roots: not an error.** Once there is a config file,
  prompt for roots; without a terminal, index `$HOME`.
- **`find -0`:** folded into D47, since `ferret find` is to be a drop-in for
  find(1), which has `-print0`.
- **`index_change`:** an internal API with no user-visible effect. The CLI hands
  the crawl "add these roots, remove those" rather than a whole new list, so two
  `ferret index` runs started together cannot lose each other's new root. It
  needed no decision.

## D46 — Is the daemon the only mode of operation?

**Question:** Dave, on D44: "perhaps we simplify things for ourselves if we say
the only mode of operation is with a daemon". Is the daemon required, or the
primary mode with a direct read kept as a fallback?

D14 and D30 today say the daemon is optional and a cold `find` without one must
be fast. D43 and D44 are both questions about that cold path.

| Option                                                                                                                     | Costs                                                                                                                                                                                   | Buys                                                                                                                                           |
| -------------------------------------------------------------------------------------------------------------------------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ---------------------------------------------------------------------------------------------------------------------------------------------- |
| A. Optional, both paths tuned (today)                                                                                      | Every layout question is answered twice, cold and resident; D44-style tuning goes on                                                                                                    | Works anywhere with no background process                                                                                                      |
| B. Daemon primary; direct read kept correct but untuned: without a daemon, `find` reads the catalog itself, as S1 does now | Daemon lifecycle work (a socket, and a systemd user unit or spawn on first use); a second path that is frozen, not deleted                                                              | Tuning targets resident memory and the budget Dave describes; agents in sandboxes, containers and CI still get correct answers; D44 is dropped |
| C. Daemon required: `find` is a client, and without a daemon it starts one or fails                                        | A sandboxed agent may be unable to reach the user's socket or to leave a background process running, so the tool fails exactly where agents run, against the "optimise for agents" rule | One code path; results are always as fresh as the watcher                                                                                      |

**Recommendation:** B. The daemon becomes the design centre, and S5 stops being
optional and moves up, after S1+ since a watcher needs incremental updates
(D40). The fact that would change it: if agents always run where the user's
daemon socket is reachable, C, with its single path, is simpler.

> Dave (2026-09-30): I haven't had issues with agents being able to reach my
> daemons in other tools. The CI is a good point though. For testing, could we
> possibly open it in a kind of script mode where we pass a number of queries
> and collect the output in a single run. So it's still opening the same way as
> it would in daemon mode, i.e. it reads the whole catalogue into memory. CI is
> unlikely to have that much data in the index, so reading the full catalogue
> into memory for every test might not be that onerous, but I'd still like it to
> run as fast as possible.

**Answer (2026-09-30): C, plus a batch mode.** The daemon is the mode of
operation, and there is one engine: open the catalog the resident way (names and
inodes read in full, indexes mapped) and answer queries from memory. It runs in
two hosts:

- **`ferretd`**, which keeps the engine resident and serves the CLI over a
  socket.
- **A batch run** (`ferret batch`, name open), which builds the same engine in
  process, reads many queries (JSON lines on stdin or a file), writes one tagged
  result block per query, and exits. This is how CI and the test suites run: one
  load per run, not per query, through the code path the daemon uses.

The S1 direct reader is not kept as a separately tuned path; D44's cold-read
tuning is dropped. Opening fast still matters, since every batch run and every
daemon start pays it, so compaction (D48 A) and a load that is one sequential
read per section are the budget. What a one-shot `ferret find` does with no
daemon running is D49.

## D47 — `ferret find` as a drop-in for find(1)

**Question:** Dave wants `ferret find` to be a drop-in replacement for `find`,
matching POSIX at minimum, with agent-driven fuzz testing against it once it is
implemented. What shape gets there?

POSIX.1-2024 (Issue 8) `find` is `find [-H|-L] path... [expression]`, with
primaries
`-name -iname -path -type -size -mtime -ctime -atime -newer -perm -user -group -nouser -nogroup -links -xdev -mount -prune -depth -print -print0 -exec -ok`
and the operators `( ) ! -a -o`. `-size n` counts 512-byte blocks (`c` for
bytes). `-maxdepth` is GNU, not POSIX. Output order is unspecified, so a fuzz
oracle compares sorted output.

POSIX is the floor, not the target. Dave (2026-09-30): "If people are using
`-maxdepth`, we should support that too." So the supported set is POSIX plus
whichever GNU extensions real scripts use, ranked by use. Likely candidates are
`-maxdepth`, `-mindepth`, `-regex`, `-newermt`, `-empty`, `-delete` and
`-printf`; the corpus decides.

**Testing (Dave's proposal):** mine GitHub for `find` invocations in shell
scripts and run them as differential tests against GNU `find`. Three things make
that work:

- **A corpus of commands, not scripts.** Extract each `find` command line
  (through `gh` code search) with its source recorded, and keep only the
  commands. The primary counts from the same corpus rank the GNU extensions.
- **Trees to run them on.** A script's paths refer to its own repository, so
  each command runs against generated trees, and against the cloned repository
  where that is cheap.
- **A sandbox.** `-exec`, `-ok` and `-delete` have side effects. Run every
  command in a throwaway copy inside a user namespace with no network, and
  compare the effects as well as the output.

Four conflicts with S1 as built:

1. **Syntax.** S1's `ferret find` takes bare words and `ext:`/`size:>100M`
   atoms. That cannot also be `find path... expression`.
2. **Ignored trees.** `find` sees `target/`, `node_modules/` and `.git/`, which
   ferret does not catalog (D13). An index answer silently misses them.
3. **Missing fields.** `-atime` and `-links` need atime and the link count,
   which the catalog does not keep.
4. **Freshness.** `find` reads the disk now; ferret answers as of the last index
   run, unless a daemon is watching (D46).

| Option                                                                                                                                                              | Costs                                                                                                                        | Buys                                                                                               |
| ------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ---------------------------------------------------------------------------------------------------------------------------- | -------------------------------------------------------------------------------------------------- |
| A. `ferret find` takes POSIX syntax; S1's grammar moves to another verb. Any subtree the index cannot answer (ignored, outside every root, or stale) is walked live | A live-walk path inside `find`; the catalog gains nlink (and atime, or `-atime` walks live); the S1 grammar needs a new name | A true drop-in: same answers as `find`, faster wherever the index covers. The fuzz oracle is exact |
| B. One verb, told apart by shape: a leading path or a `-primary` means POSIX syntax                                                                                 | Ambiguity at the edges: a search word that starts with `-`, or a bare word that is also a directory name                     | Keeps one command name                                                                             |
| C. Index-only find syntax: POSIX primaries over what is indexed, ignored trees excluded, and documented as different                                                | Not a drop-in; scripts that rely on `find` seeing `node_modules/` break silently                                             | Simplest; no live walk                                                                             |

**Recommendation:** A, with the index as an accelerator and a live walk as the
fallback, because a drop-in that silently omits whole trees is not a drop-in.
The fact that would change it: if Dave wants ignored trees excluded even in
`find` mode, the result cannot match `find`, and C is the honest shape. The
corpus and harness are cheap-agent work, and can start before `find` itself is
built, since the extension ranking feeds its scope.

> Dave (2026-09-30): D47, let's go with C and keep it simple for now. We want to
> match find's behaviour _except_ that we respect .gitignore and .ferretignore.

**Answer (2026-09-30): C.** `ferret find` takes find syntax (POSIX floor, GNU
extensions by use) over the index, and matches `find` exactly **except** that
paths excluded by the ignore rules (D13: global rules, `.gitignore`,
`.ferretignore`) do not exist to it. No live walk.

That makes the test oracle exact after one filter: GNU `find` on the same tree,
with its output passed through `git check-ignore` (git as a black-box oracle
only) and the ferret-only rules, must equal `ferret find`. Commands whose
effects depend on an ignored path are excluded from the corpus or flagged.

Three follow-on calls, recorded as defaults (object if wrong):

- **The S1 grammar** (bare words, `ext:`, `size:>100M`) moves to
  `ferret search`, which in S2 composes content terms with exactly those
  predicates. `ferret find` becomes find syntax only.
- **`-links`:** the link count goes into the catalog, costing a few bits a row
  after compaction.
- **`-atime`:** stat the candidates live after every other predicate has
  filtered them, rather than storing atime, which goes stale on every read and
  which the watcher does not see change.

> Dave (2026-09-30), on the ignore-sensitive cases: `-empty`, do whatever is
> faster. If we must stat directories to know they're empty anyway, match find;
> if the index answers faster, respect the ignore rules — unless inotify on the
> directories we index can keep an up-to-date count of their contents at little
> cost, which I'd prefer, so `-empty` is correct even when every child is
> ignored. `-links` and `-size` on directories: the same, fastest response.
> `-newer` should fail if the file is not in our index. `-delete` should fail if
> the directory is not empty. Generally, match find where it doesn't cost us
> much, erring towards performance. We can also test against fdfind. Corpus
> licensing is a good point: build the corpus in a separate private repo, and
> use what it teaches to write our own tests, which need not reuse other repos'
> commands.

**Answer (2026-09-30):** all three directory predicates match `find` at almost
no cost, so they match it:

- **`-links` and `-size` on a directory** are the directory's own `st_nlink` and
  `st_size`. The crawler already stats every directory (`observe.rs` reads
  `st_nlink`), and ignored children are counted in both, as `find` sees them.
  The catalog stores them (nlink joins the row; size is already there).
- **`-empty` on a directory** needs the count of all its entries, ignored ones
  included. The walker already reads every entry of each directory it enters,
  before policy drops any, so the crawl records the raw count for nothing. The
  daemon keeps it current: an inotify watch on an indexed directory reports
  creates and deletes of every child, ignored or not, so no ignored tree needs
  watching. `-empty` on a file is its size.
- **`-newer REF`** fails, exit 1 with a message, when REF is not in the index.
- **`-delete`** of a directory that still holds entries fails, as `find` does
  for a non-empty directory; ignored contents are what keep it non-empty.

The corpus and its differential runner live in a private repo,
`~/w/find-compat`, never published. Super Ferret's own find tests are written
from what it teaches, not copied from it. Targets: GNU find (the oracle), bfs,
busybox, fd (for its gitignore handling and speed; fd's syntax differs, so its
adapter translates the subset it can express), and ferret.

**Amendment (2026-10-02, compatibility handoff):** find has one evaluator over
catalog and live entry sources. Default mode respects ignore rules; `-I` /
`--no-ignore` selects GNU-exact behaviour. Ignored means uncatalogued beneath an
opaque marker, not nonexistent: an explicitly named ignored start walks live
without nested ignore rules, and ignored reference operands resolve normally.
Re-included descendants have ordinary catalogued directory ancestors. Special
files get name/stat rows without content. Ignored regular files get name-only
rows; ignored directories with nothing visible beneath get opaque markers. The
global setting making `-I` the default is deferred to milestone 4. Docs at that
milestone must explain that a pasted `find ... -delete` skips ignored files and
still exits 0. Milestone 1 builds the live engine and rejects default mode until
those catalog and policy changes land.

**4a encoding brief (2026-10-02, D40/D43).** The handoff's fixed-width 12-byte
name row predates S1a: child is now blocked frame-of-reference, 128 rows per
block. Does its high type sentinel justify a different physical encoding?

- **A, fixed high sentinels (retained):** seven values below NONE, one reserved
  tombstone, no inode for an ignored name. Mixed blocks widen to 32 bits.
  10.449M names: 594,837,226 B, 10.71 s build wall, 1,641 MiB peak RSS. Build
  load 7.86/4.22/3.22. Tags stay fixed when stable inode ids are added.
- **B, tags adjacent to inode_count (measured trial):** keep real ids unchanged,
  put seven type codes immediately after them, translate in the reader. Same
  names/inodes/docs: 593,660,890 B, 10.69 s wall, 1,639 MiB peak RSS. Build load
  9.98/7.00/4.56. Saves 1,176,336 B (0.20%), but tags move as the inode count
  changes, complicating the future incremental writer.

**Recommendation: A.** The size difference is small and B has no consistent
query win. Full listing, warm: A 1,369 ms vs B 1,354 ms; baseline 1,340 ms.
ROADMAP § S1c summarises the section and query numbers; the full tables, with
their loads, are in git history as `docs/FIND-M4A.md`. The fact that would
change it: a much more ignored-heavy 10M fixture where nullable child/type or a
separate ignored range saves materially more space or query time. Those two
options were not implemented or measured here. There is no departure from the
settled sentinel encoding. All seven ignored kinds have name-only rows; visible
special kinds use a sparse 8-byte-per-row Specials section so name search loads
no stat columns. Re-included ancestors are ordinary for find but keep the
existing search-suppression bit to preserve search counts (D29 compatibility).
Visible special files are available to the find source but suppressed by search,
which keeps its existing regular-file/directory/symlink result domain.

**4b fallback and configuration (2026-10-03).** Default find needs an index
covering each explicit start. Options: refuse with status 1; walk live with
policy (a second policy crawl and changed costs); or walk as `-I` with a warning
(scripted actions then reach ignored data). Chosen: **refuse** missing or
incompatible catalogs and unresolved explicit starts, with re-index/`-I`
guidance. **New names observed while listing are visible**, using live kinds and
lstat; an uncatalogued directory walks live as visible, without nested ignore
rules. This replaces 4b's initial refusal of new names: actions such as
`-exec touch {}/new \;` must not invalidate their own walk. Dave selected this
behavior after the full-corpus run. The tradeoff is that a name created after
indexing that the ignore rules would hide shows up until the next index. Deleted
catalog names disappear from listings or are dropped when live lstat fails.
Metadata changes are read live, not treated as index invalidation. A snapshot
cannot detect changes inside ignored opaque subtrees, and does not watch for
policy-file edits: re-index after changing ignore rules. A policy-aware live
source sharing the crawler's rules could remove the new-name tradeoff without
duplicating traversal.

Directories cache their live stat observation before descending, including in
`-depth` and implicit depth from `-delete`; child actions must not change the
mtime seen by the directory's later predicates. Regular-file metadata stays
lazy. `-empty` checks current on-disk contents, including ignored entries,
rather than the catalog's snapshot child count: deleting visible children may
make the directory empty during the walk, while ignored children still prevent
emptiness. Real CatalogSource regressions cover both timings and new names
created by directory `-exec` actions.

Q4's setting is **`find_no_ignore = true`**, in `$XDG_CONFIG_HOME/ferret/config`
(default `~/.config/ferret/config`). Its boolean name states the behavior
directly and matches `--no-ignore`. The smallest shape is one `key = true|false`
line, optional whitespace, blank lines and `#` comments; missing/empty means
false. Unknown keys, duplicate keys and malformed values fail with status 1. No
dependency or general-purpose format parser. Config lookup resolves only its XDG
base, independent of unused state/cache bases. Explicit `-I` bypasses both
config and index; help/version bypass them too.

The catalog's name order cannot reproduce readdir order. 4b retains live name
listings solely for traversal order and intersects them with catalog entries.
This preserves `-quit` and directory-local command batches without using stale
stat columns. ROADMAP § S1c records the cost.

A pasted `find … -delete` skips ignored files and still exits 0 when the
selected deletions succeed. Deleting a visible directory that still contains
ignored files can fail with ENOTEMPTY and exit 1, as a live deletion does.

**4b stat freshness brief (2026-10-03).** Does the measured stat cost justify
changing the settled lazy-live policy? A: live lstat, current metadata but one
syscall per matching entry (size 443 ms, mtime 459 ms). B: snapshot columns,
size/mtime about 170 ms with the same ordered source, but stale metadata and
missed deletions. C: the later watch-backed stat cache, current within the
watched set but requiring daemon work. **Recommendation: retain A**; Dave must
choose any move to B. The fact that changes it: an explicit acceptable freshness
contract. The trial patch was removed in M5a; its tables, with loads, are in git
history as `docs/FIND-M4B.md`. Eager size/mtime decoding costs about 5 ms on the
name/type controls. B alone still misses fd's roughly 51 ms stat median.

**4b order/speed brief (2026-10-03).** How should exact current traversal order
coexist with the speed gate? A: live name listings (implemented), preserving
order but costing 160–220 ms on cheap queries; fd is 23–39 ms. B: persist
traversal order during indexing and keep it current through watches, adding
storage/crawl/daemon work but making catalog traversal possible. C: allow
catalog order for read-only plans without quit/commands, fast for names but
changing plain output order and the brief's exact-order contract. Existing
catalog-only search takes 12–21 ms here, under a different output/order
contract. **Recommendation: investigate B and measure its storage/freshness
costs**, or explicitly choose C if that ordering relaxation is acceptable. No
ordering relaxation is selected. The fact that changes it: Dave accepting
catalog order for those plans, or evidence that persisted order can stay current
cheaply. The functional implementation therefore does not complete the speed
gate.

**Superseded in part (2026-10-03, D50).** F7, F8, F10 and F12 replace 4b's live
listing order, its new-name visibility, its live stat and its live `-empty`:
default mode is now a pure index query in catalog order, with stored metadata,
and `-empty` subtracts the walk's own `-delete` removals. The config key and the
refusal without a covering index stand. [FIND.md](FIND.md) is the current
contract.

## D48 — The next move after S1

**Question:** S1 is done. What comes next?

| Option                                                                                                                                                                                                  | Costs                                                                                        | Buys                                                                                                                                                                |
| ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | -------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| A. Catalog compaction (D43): bit-packed fixed-width columns, per-catalog dictionaries for dev, mode and uid/gid, then names. Measure bytes per name, open and query time on `$HOME` and 10M, against S1 | One slice and a bench; the format changes again later for S1+'s change log                   | Dave's stated first priority. It fixes the layout that the incremental log, the daemon's resident set and `mmap` all build on, and S1's tooling already measures it |
| B. S1+ incremental catalog first, as ROADMAP now says                                                                                                                                                   | Built on a layout about to change                                                            | Cheap re-runs sooner; the daemon's prerequisite                                                                                                                     |
| C. A name index for large catalogs first (trigram, or D28's suffix array), as a `ferret-bench` experiment                                                                                               | Its value depends on D46: with a resident daemon, a scanned heap may do up to about 1M names | Queries that do not read the whole name list at 10M                                                                                                                 |
| D. Settle D46 and D47, then re-plan ROADMAP before any code                                                                                                                                             | A pause in code                                                                              | The daemon and `find` answers reorder several milestones                                                                                                            |

**Recommendation:** A now, with D46 and D47 answered alongside it, since
compaction does not depend on either. Then C or S1+, depending on D46. The fact
that would change it: if "establish the optimal index format" (Dave, on D43)
means settling the content index (S2) format before the catalog layout, then S2
design comes first.

> Dave (2026-09-30): Agreed. By optimal index, I meant whether we want to
> include a suffix array or term and possibly trigram indexes for the names. If,
> after we build full find support, we can still read a 10M file catalogue into
> 1Gb of memory, they're probably not necessary.

**Answer (2026-09-30): A, catalog compaction next.** A name index (suffix array,
terms, trigrams) is decided by measurement, not built on speculation: once
`find` is complete, if a 10M catalog loads resident in under 1 GB and scans fast
enough, there is no name index. Today's 10M catalog is 1.18 GB on disk and
`find` peaks at 358–970 MB, before compaction and before nlink and entry counts
join the row.

## D49 — A one-shot query with no daemon running

**Question:** With the daemon as the mode of operation (D46 C), what does a
single `ferret find` do when no daemon is running?

| Option                                                               | Costs                                                                                                | Buys                                                                 |
| -------------------------------------------------------------------- | ---------------------------------------------------------------------------------------------------- | -------------------------------------------------------------------- |
| A. Start the daemon, then query it                                   | Spawn logic and a wait on the first query; the first query pays the load anyway                      | Every later query is resident; nothing to set up                     |
| B. Build the engine in process for this one query, as a batch of one | Every query pays the full load until a daemon exists (tens of ms at `$HOME`, about 1 s at 10M today) | No background process appears unasked; one code path with batch mode |
| C. Fail with a message saying to start `ferretd`                     | A bare install does not answer anything                                                              | Simplest; the user decides when a process runs                       |

**Recommendation:** A where a user session exists (spawn on first use, or a
systemd user unit), falling back to B when a background process is not allowed
or `FERRET_NO_DAEMON` is set, which is what CI and sandboxes get. B alone is the
cheap first step, since batch mode already builds it. The fact that would change
it: if you would rather a daemon never starts without being asked, B plus an
explicit `ferretd` is the whole design.

> Dave (2026-09-30): Agree.

**Answer (2026-09-30): A.** Start the daemon on first use; build the engine in
process when a background process is not allowed or `FERRET_NO_DAEMON` is set.

## D50 — The `ferret find` stretch's calls (answered 2026-10-03)

The find stretch (ROADMAP § S1c, milestones M1–M5c) asked its questions on an
HTML decisions page rather than here, numbered F1–F13. Each is recorded below
with its options, Dave's answer and the reason. The contract they produced is
[FIND.md](FIND.md). Dave's standing rule for the stretch (2026-10-03): take the
fastest answer and the simplest to maintain over time; more work now, and
harness changes, are fine; discuss only where fastest and simplest disagree.

F1, F2, F6 and F9 are changes to the private find-compat harness, not to ferret.
They are recorded because they decide what "zero differ" means. After M4 the
full corpus stood at `-I` 67,394 agree / 20 differ / 6 harness failures and
default 52,909 / 28 / 3, and every one of those rows was F1, F2 or F9.

### F1 — The sandbox root's link count

**Question:** ferret's sandbox binds its XDG and index directories under
`/ferret`, giving `/` one more link, so `ls -la` of the start's parent differs
(20 rows). How should the harness stop that?

- A. An empty `/ferret` in every target's sandbox: one line, identical
  sandboxes; older bfs/busybox/fd results stop being comparable on those rows.
- B. Bind ferret's directories under a directory that already exists: no new
  root entry, but the harness paths change and a safe directory must exist.
- C. Leave it and record a known artefact: every run report carries the list.

**Answer: A** (harness change). Smallest change, identical sandboxes, no
exception list.

### F2 — Commands that hang on the tree's FIFO

**Question:** Three commands `cat` or `grep` every entry and block on the FIFO
in two trees, so both GNU and ferret time out (6 rows). How should such a row
score?

- A. A separate status, outside both differ and error, listed beside the gate.
- B. Score a both-sides timeout as agree: one clean number, but it calls two
  outputs equal without comparing them.
- C. Remove the FIFO: loses the coverage that FIFOs are catalogued.

**Answer: A** (harness change, the `both-timeout` status). A timed-out run's
partial stdout depends on scheduling, so B would be a claim the harness cannot
make.

### F3 — Parallelism in the live walk

**Question:** Should `ferret find -I` walk directories in parallel, given that
its output order would then stop matching GNU's? After M3b, single-threaded `-I`
beat GNU on every timing row and was at or below `bfs -j1` on all but
`-maxdepth 2`; the gap to default bfs and to fd was threads.

- A. Keep `-I` sequential; parallelise only default mode's live fallback.
- B. `-I` goes parallel when the expression does not depend on order, which
  needs a classifier for order-sensitive expressions.
- C. Parallel only behind a flag or config key.

**Answer: superseded by F10 B.** The live walk is parallel in every mode, with
no order classifier; F11 settles actions.

### F4 — A stray file in the repository root

**Question:** May the untracked, empty `1T`, apparently a stray shell redirect,
be deleted? A: delete it. B: leave it in `git status`.

**Answer: A.** Deleted.

### F5 — Walking relative to each directory's fd

**Question:** Should `-I` open and stat entries relative to the parent
directory's fd, as GNU does, instead of by full path? Two shapes outside the
corpus differ: a tree deeper than PATH_MAX, and an ancestor renamed mid-walk.

- A. Stay path-based: no cost; those two shapes stay wrong.
- B. Go fd-relative: a medium slice touching every metadata call site, with no
  measured speed gain.

**Answer: A.** Same speed and less code. FIND.md lists both shapes as known
differences.

### F6 — The harness's catalogue listing used a retired command

**Question:** find-compat's control 3 listed the catalogue with
`ferret find --json -- '*'`, the atom grammar that moved to `ferret search`.
What should it run?

- A. `ferret search --json -- '*'`: one word, but search keeps its old
  file/directory/symlink domain, so special files never appear.
- B. Default-mode `ferret find / -print` once M4b landed: lists everything the
  find source sees, special files included.

**Answer: B** (harness change). Control 3 then passes for the right reason.

### F7 — Output order in default mode

**Question:** M4b listed each visible directory live to keep GNU's order, which
made default mode as slow as the live walk: `-name '*.c'` took 185 ms against
fd's 25 ms, while catalog-only search answered in 12 ms. May default mode print
in catalog order?

- A. Catalog order whenever the plan cannot observe order.
- B. Persist readdir order in the catalog: exact and fast, but a permutation
  column, crawl work, and staleness on every create and rename.
- C. Keep the live listing: exact, and 4–7 times behind fd.

**Answer: A, unconditionally** (under F10 B). Default mode prints in catalog
order for every expression, `-quit` included; live listing is used only below
opaque markers. It was the only option that met the speed goal without daemon
work.

### F8 — Where default mode gets stat data

**Question:** Should default mode answer `-size`, `-mtime` and the other stat
tests from stored columns instead of a live lstat? In M4b's ordered source,
`-size +1024c` took 443 ms with live stat and 170 ms with stored columns; fd
took 51 ms.

- A. Live lstat, in parallel: current metadata; new code, speed unmeasured.
- B. Stored columns: well below fd, with the index's freshness, like `locate`.
- C. Stored columns only inside a daemon-watched set: fast and current, but
  waits for the daemon.

The recommendation was A now, C with the daemon.

**Answer: B.** Default mode is a pure index query: catalog order, stored
metadata, and a live walk only below opaque markers. It will not be advertised
until the daemon keeps the index current; C follows with the daemon. Cold reads
get less weight in performance work, since the daemon is the normal mode (D46,
D49); D30 is amended to match. This replaces D47's 4b freshness brief, which
retained live lstat.

### F9 — Oracle skips that scored as differences

**Question:** In default mode, 12 rows came out as "skipped: command not
allowlisted" for the ferret oracle while ferret ran them, and scored as differ.
How should the harness score them?

The cause was found in the harness: its allowlist rewrite was not idempotent, so
the oracle's second pass rejected its own output. B was to fix the rewrite
(about three lines) and test that it is idempotent. The page did not preserve
the other option's text.

**Answer: B.** "It is a small change."

### F10 — What order `ferret find` promises

**Question:** When an expression can observe walk order, which order must
`ferret find` produce? GNU's sibling order is readdir order, which on ext4 is
hash order: arbitrary, and changed by copying the tree. fd and bfs do not keep
it. The harness sorts line output, but order still shows in `-quit` (829 corpus
commands), in `-printf` with no separator, and in `-exec +` argument order.

- A. Free order unless the expression can observe it: zero differ stays
  reachable, but a classifier must recognise every order-sensitive shape.
- B. Free order everywhere, with only the structural rules: the simplest code,
  fastest on `-quit` too; about 70 harness rows differ on order alone, and the
  harness must learn to classify them.
- C. GNU order always: no classifier; `-I` stays 3–5 times behind fd, and
  default mode needs F7 B.

**Answer: B.** Free order everywhere; the harness learns to classify order-only
rows. The structural rules are a parent before its children, children first
under `-depth` and `-delete`, and `-prune` stopping descent.

### F11 — Actions under a parallel walk

**Question:** When the walk is parallel, may `-exec`, `-execdir` and `-delete`
run concurrently? `-exec` is a test, so a worker would otherwise wait on a
single action thread for its exit status.

- A. Concurrent actions, fd's model: workers run commands themselves; each
  child's stdout is buffered and written whole; `-ok` prompts take a lock.
  Commands with clashing side effects can leave a different tree from run to
  run.
- B. Serial actions: a deterministic tree for a given order, but workers stall
  on every `-exec` used as a test, and a hand-off channel is more code.

**Answer: A.** Both of Dave's criteria point at it. H1, on find-compat's
`ferret-harness` branch, takes F1, F2, F6, F9 and order-only rows; M5a makes
default mode a pure index query; M5b, the parallel walk, follows.

### F12 — Index answers inside commands that change the tree

**Question:** M5a's full corpus found 85 rows where actions went wrong. Opening
directories and dropping vanished children, only when the expression has an
action, fixed 77. The other 8 were `-depth … -type d -empty -delete`: the index
still says a directory has children after the walk deleted them. Should
effectful expressions read the disk?

- A. Expressions with actions read stat live: one rule, which also stops a stale
  `-mtime` deciding what `-delete` removes.
- B. Live `-empty` only, when the expression has actions.
- C. Index answers everywhere: accepts the 8 rows.
- D. The walk counts the children it deleted itself: `-empty` is the indexed
  count minus this walk's own `-delete` removals; no live reads, and it cannot
  see `-exec rm` or `-exec rmdir`.

A was recommended. A current daemon would not fix the 8 rows, because the walk
reads index state before it starts and the daemon's updates arrive
asynchronously; 91 of the corpus's 163 `-empty` commands also delete.

**Answer: D.** `find . -depth -type d -empty -exec rmdir {} \;` leaving an
emptied parent in default mode is accepted: use `-delete`, or `-I`. Revisit only
if a user brings a case that neither solves.

### F13 — Stat fields the catalog does not store

**Question:** Should the catalog add atime and allocated-block columns, so that
`-atime`, `-amin`, `-used`, `%a`, `%b`, `%k` and `-ls` stop doing a live lstat?
Over 300k entries the fallback cost about 350 ms against about 40 ms stored. At
10M: atime 31.1 MB on this fixture (up to about 77 MB with fully varying times)
and blocks 6.1 MB, against 553 MB of sections.

- A. Keep the lazy fallback: no format change; these primaries run at `-I`
  speed; a stored atime is usually stale, since reading a file changes it.
- B. Add both columns: index speed; about 37 MB at 10M and a migration.
- C. Blocks only: 6 MB for index-speed `-ls` and `%k`.

**Answer: A.** Atime, allocated blocks, birth time and device numbers stay lazy.
The fact that would change it: query logs showing `-ls` or `%k` in normal use
(then C).

### README — which mode it leads with

**Question:** Should README lead with `-I` instead of default mode, given F8's
"not advertised until the daemon keeps the index current"?

- A. Lead with `-I`, which is current without a daemon.
- B. Keep README as it is, leading with default mode.

**Answer: B.** Dave, 2026-10-03: "not advertised" means not posting about ferret
online. The README may lead with default mode.

### M4b's own calls, recorded in D47

The config key `find_no_ignore = true`, and refusing default mode without a
covering index, were decided inside M4b and are recorded under D47 (4b fallback
and configuration). The page asked Dave to say if either should change; no
change is recorded.

The two follow-ups below were asked and answered during M5b and M5c, in this
file, and refine F11.

### Find M5b — quit while an action is running (answered)

**Question.** Does F11's “stops every worker promptly” require terminating a
command or interactive prompt already started on another worker, or only
cancelling further traversal and expression evaluation? The M5b implementation
does the latter, flushes collected batches, and reaps children. Dave answered
the clarification in M5c, as recorded below.

| Option                       | Behavior                                                                                                                   | Tradeoff                                                                                                                                                       |
| ---------------------------- | -------------------------------------------------------------------------------------------------------------------------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| A. Finish started actions    | Cancel new traversal/evaluation immediately; flush collected batches; await already-started commands/prompts               | Preserves completed child output and external effects with the existing process interface; exit can wait for a slow command or unanswered prompt               |
| B. Terminate started actions | Add cancellation to process-group execution, pipe collection and prompt input; terminate running commands and stop prompts | Bounds cancellation latency, but can leave partial external effects/output and changes command completion semantics; needs a cancellation-aware host interface |

**Answer (2026-10-03): A.** Dave's M5c instruction settles the policy: already
running commands finish and are awaited, with no forced termination. Quit is an
output latch: the first entry reaching it commits under the output lock; entries
finishing later discard their buffered output. Side effects already performed
remain. Collected batches flush at exit. This accepts exit latency from a slow
command or unanswered prompt and avoids cancellation machinery.

### Find M5c — measured batching and many-start costs (answered)

**Question.** The initial implementation sequences all starts and runs full
shared batches while holding their mutex. Fifteen warm samples on the 300k tree
showed many-starts default/live 21.565/130.593 ms versus M5b 17.261/26.694 ms;
batch-all 189.923/243.461 ms versus 76.342/48.539 ms. The original brief
attributed too much of this to append contention and treated read-only starts as
effectful.

| Option                                           | Benefit                                           | Cost                                                                                             |
| ------------------------------------------------ | ------------------------------------------------- | ------------------------------------------------------------------------------------------------ |
| A. Shared batch, execute outside its mutex       | Minimal partition; collect while full batches run | Selected entries still synchronize on append                                                     |
| B. Bounded worker staging into that shared batch | Amortize append synchronization                   | Partial stages must merge at task completion and quit; shared batch remains the sole partitioner |
| C. Special scheduling for narrow effectful roots | Reduce short-task barriers                        | New scheduler policy must earn its cost in measurements                                          |

**Answer (2026-10-03): A, then measured B; overlap read-only starts.** Dave
requires full batches to detach under the lock, then spawn and wait outside it.
Concurrent full-batch commands are allowed by F11 A. Reuse M5a's `has_actions`
for scheduling, extending it to all file-output actions as well as exec variants
and delete. Only effectful starts sequence. Dave accepts 0.4–2.3% default-mode
timing regressions as noise.

After A, append contention remains on batch-all (144.411/188.290 ms versus
paired M5b 75.811/49.490 ms), so Dave's conditional authorization for B applies.
Staging merges at 32 paths or 4 KiB; only the shared batch fills/partitions
argv. Partial stages merge even after quit, before shared exit flush. Staging
recovers M5b throughput. A cheap narrow-root donation guard made effectful
starts slower (148.730 versus 135.290 ms live), so it was removed. The pool
already persists across starts; no new scheduler policy is kept. ROADMAP § S1c
records the measurements; per-cell loads and the syscall profile are in
find-compat's m5c scratch.

The final full corpus (2026-10-03, 135,693 rows, zero errors) showed one gap in
overlapping read-only starts: with `-quit`, a later missing start reported
ENOENT and exit 1 before the first start quit, where GNU exits 0 silently.
Starts now also sequence when the expression contains `-quit`.

## D51 — Compaction while the watcher is busy (answered 2026-10-05)

**Status: answered, A (Dave, 2026-10-05).** S1+ M0, 2026-10-03;
[design](S1PLUS.md).

**Question:** May an incremental checkpoint pause the writer for seconds, or
must S1b keep applying bursts while it compacts? Queries keep their pinned
generation in either case. This is a disagreement between fastest updates and
the simplest implementation to maintain, not a question about query locking.

The current v3 10.45M synthetic checkpoint is measured at 594.8 MB and a 10.71 s
build (ROADMAP S1c). With D52's epoch ids, document counts and coverage, the new
10M checkpoint is an estimated 602.0 MB, down from 761.7 MB with lifetime maps;
estimated compaction is 9–20 s. Removing the maps saves about 0.56 s of model
read/write/checksum work, not the whole seconds-long pause. All new-format
time/RAM figures below are estimates, to be measured in M7.

| Option                                            | Costs                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                        | Buys                                                                                                                          |
| ------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ | ----------------------------------------------------------------------------------------------------------------------------- |
| A. Idle-boundary checkpoint under the writer lock | Incoming updates wait 9–20 s at 10M when compaction runs; sustained churn eventually requires a pause. About 0.7 GB temporary disk and 0.2–0.7 GB transient RAM including validation, plus pinned generations.                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                               | One writer, one generation publication proof, no catch-up protocol. Queries continue on the old view.                         |
| B. Concurrent checkpoint with suffix replay       | Build a pinned view at sequence S while the writer appends. At cutover, take the lock, rebase S's suffix into the new epoch, write/checksum the new log, sync both files and publish one new manifest. Retain about 80 MB of transient old-to-new inode/name maps plus suffix births; map every parent/child/own-name/root/aux reference and reset epoch allocation counters while keeping DocIds. Raw suffix copying is invalid. At 1,000 unique-content edits/s for 20 s, each committed separately, the suffix is 2.72 MB payload + 3.84 MB framing = about 6.6 MB total, with an estimated 30–120 ms replay budget plus about 10 ms byte/check cost before sync; rebasing overhead is unmeasured and must be included in the cutover measurement. Extra pinned source/overlay RAM may be 0.1–0.8 GB, besides those remaps; requires a bounded catch-up policy, new-epoch writer lookups prepared in the background and tests for two epoch pairs. Queued old-epoch requests retry before id dereference. | Ordinary bursts retain small-commit latency while the snapshot is built; cutover scales with the suffix rather than 10M rows. |
| C. Defer checkpoints until the watcher is quiet   | No compulsory pause during activity, but replay and overlay costs become unbounded: 5M records cost an estimated 2.5–10 s replay and roughly 200–600 MB overlay RAM, risking the 1 GB resident goal.                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                         | Smallest scheduler and no catch-up path.                                                                                      |

**Recommendation:** A for M7, with a measured checkpoint pause and explicit
backlog/freshness reporting before S1b ships. It is the smallest complete
mechanism and does not change D32's reader guarantee. Do not choose C: deferred
work still needs a bound. B is justified if sustained activity makes A's pause
unacceptable; design its suffix handoff as a separate review slice then. Epoch
renumbering does not remove the pause and adds a rebasing protocol to B; the
smaller checkpoint helps both options, so the recommendation stays A.

**Fact that would change it:** a required worst-case freshness lag below 10 s
during sustained churn, or M7 measuring checkpoint pauses beyond the accepted
lag even on an idle-priority run. Either favours B. S1b's observed burst rate
and the permitted lag, rather than snapshot size alone, decide it.

> Dave (2026-10-05): agreed with the recommendation, A. Compaction pauses the
> writer at an idle boundary; M7 measures and reports the pause.

## D52 — D27 C: ids across compaction

**Status: answered, B (Dave, 2026-10-05).** S1+ M0 round 2, 2026-10-03;
[design and consumer check](S1PLUS.md#epoch-scoped-ids).

**Question:** Does D27 C's “stable, never reused; holes until compaction”
require InoId/NameId to survive compaction, or only remain stable within its
epoch? The original C buys the same external stability as B. Dave's comment
requires metadata edits to preserve DocId; it does not specify a compaction
namespace. This brief records the interpretation, without claiming Dave has
answered it.

No concrete current, S1b or S2 consumer needs inode/name ids across compaction.
DESIGN's `CandidateSource`/`DocCursor` and content structures, ROADMAP S2/S3,
and `ferret-index`'s boundary use DocIds. Catalog inverses belong to a pinned
view. S1b watch locations can use checked paths or rooted `(dev, ino)` identity;
queued requests already have expected_generation and can retry/re-resolve. No
separate external metadata index with a durable inode-id key is specified.

Numbers below are **estimates at exactly 10M names**, normalised from the
**measured** 594,837,226 B / 10,448,739-name v3 artifact in
[S1PLUS's sourced cost model](S1PLUS.md#cost-model-at-10m). They include
document reference counts and the all-none RetainedAt column; incremental
timings are not measured. Both options retain identical 360 B metadata / 456 B
unique content-replacement commits and independent stable DocIds.

| Option                                                | Costs                                                                                                                                                                                                                                                                                                                                                                                                               | Buys                                                                                                                                                                                                                                                                                                               |
| ----------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ |
| A. Lifetime-stable logical ids, private physical rows | About 159.7 MB of forward/paged inverse maps at dense high water: 761.7 MB checkpoint, roughly 727 MiB encoded resident buffers and a conservative 450 MB name-load set. Translation at boundaries and map/bijection validation; sparse pages needed for churn, and lifetime high-water exhaustion remains. Roughly 100M historical ids/10M scattered survivors raise map storage toward 200 MB plus page overhead. | Inode/name handles survive compaction without re-resolution, useful only if an external consumer needs them. Physical rows can still pack densely.                                                                                                                                                                 |
| B. Epoch-scoped ids: id equals checkpoint row         | About 602.0 MB checkpoint, roughly 574 MiB resident buffers and 291 MB name-load set. Compaction renumbers and invalidates old-epoch requests/caches; about 80 MB transient reference-remap arrays at 10M, plus bounded births. Concurrent checkpointing must rebase its suffix (D51).                                                                                                                              | Removes about 159.7 MB (21% of A), all persistent id maps/validation, scan translation and inode/name history growth. Dense BFS base ids preserve D29; log births/tombstones preserve ids and holes within the epoch. Compaction resets their high water. Pinned old readers retain their old namespace under D32. |

**Recommendation: B; proceeding on the recommendation; Dave may veto.** It is
both fastest and simplest for the identified consumers, so it is not another
fastest-versus-maintenance question. A metadata edit retains its epoch InoId and
live DocId; an epoch change can renumber the inode without content-index work.
Generation must include `(incarnation, checkpoint, sequence)`: compaction can
leave the sequence unchanged, so sequence-only comparison is insufficient.
Reject stale scopes before interpreting ids; retain a locator or old pinned view
for re-resolution. Keep DocId and next DocId across compaction.

**Fact that would change it:** a concrete external structure or measured
workload that requires persistent InoId/NameId references and cannot reasonably
pin, rekey by path/identity, use DocId or retry across epochs. Its measured
re-resolution/rebuild or freshness cost must outweigh A's extra 159.7 MB,
translation and map maintenance. No such consumer was found in current code or
the S1b/S2 plans. D51 remains open about the permitted writer pause.

> Dave (2026-10-05): agreed with the recommendation, B. Epoch-scoped
> InoId/NameId, renumbered at compaction; DocId stays stable.

## D53 — Cold-open overlay validation (answered 2026-10-05)

**Status: answered, A (Dave, 2026-10-05).** S1+ M5 Part A, 2026-10-04;
[design](S1PLUS.md).

**Question:** Should a cold name reader replay and semantically validate the
published overlay, trust the writer's validated transactions, or load a
persisted materialised namespace? Fastest and simplest to maintain disagree.

The post-merge 10M opens are about 330 ms at 0% and 558 ms at 1%. The earlier
profile attributes 236 ms to namespace load, including 93 ms in addition checks;
the rest includes required query construction. All 2% and alternate-reader
numbers below are **estimates**, not new benchmark results. They assume a
similar transaction mix and dirty-name slope. Checksums cover accidental
corruption, not authentication or a writer checksumming its own invalid graph.
The ordinary writer validates its successor before publication using the shared
reader code.

| Option                                   | Costs                                                                                                                                                                                                                                                                                                                                                                     | Buys                                                                                                                                                                     |
| ---------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------ |
| A. Keep semantic validation              | About 550–560 ms at 1%, 770–790 ms at 2%. Rebuild and check dirty namespace history on each cold name load.                                                                                                                                                                                                                                                               | No new code or publication proof; independently rejects invalid, correctly checksummed history at load, except bugs shared with writer validation. Simplest maintenance. |
| B. Trust checksummed writer transactions | Estimated 420–470 ms at 1%, 510–610 ms at 2%; child-key/tombstone/name-heap construction remains. Lose independent graph/reference/count checks on correctly checksummed writer mistakes; retain safe decode/index checks and termination protection. Requires factoring trusted query construction from publication validation and amending the reader contract/tests.   | Avoid part of duplicate validation without an extra durable file, but neither the fastest nor the smallest change.                                                       |
| C. Persist a materialised namespace      | Estimated 350–400 ms at 1%, 370–470 ms at 2%. Roughly 600–1,200 production lines plus comparable tests, not a prototype count. New checked index format and generation binding, an extra durable artifact before manifest publication, retention/cleanup and crash tests. Per-burst rewrite risks O(dirty-total) write amplification; cached prefixes need suffix replay. | Expected fastest cold name opens: no replay/sort of prefix history, just checked final-state buffers and any suffix.                                                     |

**Recommendation:** A for M5/M6; evaluate C with M7's compaction/derived-state
budgets if the cold-open requirement warrants its writer cost. Do not implement
B or C in M5. C is expected fastest, A simplest, so this brief requires Dave's
choice rather than silently exchanging the existing reader corruption contract.
The current section reader owns checked buffers; adding actual mmap is a
separate mechanism, not an existing format feature. Part A's detailed analysis
is in `/home/dave/w/super-ferret/.ai/s1plus-m5-done.md`.

**Fact that would change it:** a required cold name open below roughly 0.8 s at
2%, or a C prototype meeting that goal with acceptable publication latency,
write amplification and code cost. Measure query-only construction for B before
crediting it with the full 220 ms gap.

> Dave (2026-10-05): agreed with the recommendation, A. Keep semantic validation
> on cold open; evaluate C alongside M7's budgets.

## D54 — In-memory names: interning, postings, row order

**Status: answered, B after S1+ merges, after a prototype of the scoped-query
check (Dave, 2026-10-05). The prototype passed; see the end of this entry.**
Measured 2026-10-05 in the private bench repo `~/w/name-index-bench`
(github.com/dbalmain/name-index-bench); its README holds the method and full
tables.

**Question:** Should the in-memory name catalog keep D28 A's raw heap in BFS
order, intern names and add row postings and a term index, or also move rows to
DFS preorder?

The bench models each layout over two walker dumps, `$HOME` (459,713 rows,
133,749 distinct names) and `/nix/store` (4.2M rows, 357k distinct). It runs 46
query shapes mined from 315 agent `find` calls, plus fuzzy and term queries.
Everything is bit-packed with intpack, and every strategy's output equals a flat
full-path reference. "Interning" means a sorted table of distinct names plus a
bit-packed name id per row. A name query scans the distinct table, then either
passes over the ids or, with **row postings** (name id → rows), jumps to the
rows. The **term index** is a token FST with postings. The memory figures for
interning were measured in DFS order; B's BFS figures are **estimates** (same
ids and table, BFS first-child array in place of the DFS tree arrays).

| Option                                                               | Costs                                                                                                                                                                                                                                                                                                                                             | Buys                                                                                                                                                                                               |
| -------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| A. Keep the raw BFS heap (D28 A)                                     | 25.4 B/row on `$HOME`, 17.1 on nix (11.7 / 72.0 MB); ~171 MB at 10M at nix density. Selective name queries 0.4–3 ms on `$HOME` and 2–22 ms on nix; no term or Levenshtein search beyond a 100–760 ms scan.                                                                                                                                        | No work. Children stay one slice; nothing in S1+ changes.                                                                                                                                          |
| B. Intern names, add row postings and the term index; keep BFS order | ~16–19 B/row on `$HOME` and ~9–10 on nix, estimated (≈ 8.7 / 43 MB; ~100 MB at 10M). A distinct-table build and a postings build at compaction; name lookup by path goes through the table (binary search) rather than the heap. Scoped name queries over big subtrees stay a walk or a global hit list filtered by ancestor (unmeasured in BFS). | Selective name queries 0.1–0.8 ms on `$HOME`, 0.3–7 ms on nix (3–10× faster than A); terms 0.7–10 ms, Levenshtein 0.8–8 ms (30–150×). 25–250× faster than warm `fd -H`. Children stay a slice.     |
| C. B, and rows in DFS preorder                                       | Measured: 18.9 B/row on `$HOME`, 10.2 on nix (8.7 / 42.8 MB). Every place that assumes `(parent, name)` order changes. Children are found by hopping subtrees, not a slice, so path lookup and re-runs cost more (unmeasured).                                                                                                                    | A scope is one contiguous row range: scoped name queries 0.2 ms against 2.8 ms (BFS walk) in a 79k-row monorepo; listings 1.3–1.8× faster; basename-driven path scans 1.7–2.5× faster than in BFS. |

Across all three, path patterns usually imply a basename
(`*/.github/workflows/*.yml` → names ending `.yml`). Scanning for it and
verifying the path beats a walk by 10–40×, while anchored patterns must walk.
That is a planner rule and needs no format change. Trigrams, a suffix array and
an FST of names did not pay: none beat a scan of the distinct table by more than
~20%, at 1.4–20 B/row.

Fuzzy full-path search is the weak spot in every option: 60–90 ms on `$HOME` and
200–400 ms on nix, only 4.6× faster than `fd -H | fzf`. Per-directory subtree
byte masks (1.4–2.2 B/row) speed long queries 2–3×. That belongs to a fuzzy
brief when fuzzy search is scheduled, not to this one.

**Recommendation:** B, after S1+ merges. Fastest and simplest disagree only on
order. B takes most of the win — 2–2.5× less memory, and the 3–150× speed-ups —
without touching the `(parent, name)` order S1+ is built on, and C's further
gains are on queries that are already a few milliseconds.

**Fact that would change it:** a B prototype where a scoped name query over a
large subtree (the 79k-row monorepo, a nixpkgs checkout) misses ~10 ms, or the
writer finding DFS order cheaper to maintain across compaction. Either would
favour C.

> Dave (2026-10-05): B, after S1+ merges — prototype the scoped-query check
> first.

**Prototype (2026-10-05, name-index-bench `916825a`):** interned names in BFS
order with BFS row postings, every output checked against the flat reference.

- **Scoped check passes.** The slowest scoped name query is 7.4 ms (nixpkgs
  `-name default.nix`, 13,935 hits; DFS 4.4 ms), under the 10 ms bar. Always
  walking the scope stays under 10 ms on every scoped query (7.5 ms at worst).
  The faster plan for a selective name in a large scope uses the global
  postings, keeping the rows under the scope by walking parent ids up
  (memoised). For example, nixpkgs `*ripgrep*` takes 0.63 ms that way against
  7.5 ms walking. A planner chooses between them from the stored posting counts.
  The wrong choice costs up to 10×: 69 ms for `default.nix` through the
  postings.
- **Measured memory for B** (the brief estimated it): int-bfs plus postings plus
  terms is 8.6 MB on `$HOME` (18.6 B/row) and 39.0 MB on nix (9.3 B/row), the
  smallest configuration measured. BFS's first-child and parent arrays are
  cheaper than DFS's subtree arrays (5.4 MB on nix).
- **Correction to the brief's figures:** the bench originally found a BFS parent
  by predecessor search over first-child, about 0.5 µs per hit, where ferret
  keeps a `NameParent` column. With a per-row parent column, raw-bfs is 2–3×
  faster than the brief's table (`$HOME` `-name '*.rs'` 7.3 ms, not 22 ms). B
  matches or beats DFS on whole-index name queries: nix `Cargo.toml` 2.4 ms
  against 3.3, and `libc.so.6` 0.46 ms against 2.3 today. B's speed-up over
  today is therefore 2–5× on selective names, not the brief's 3–10×. The
  100–150× from the term index is unchanged.

## D55 — Storing mtime as an order rather than a value

**Status: answered, D + F with an exact per-block max (Dave, 2026-10-05).**
Raised by Dave, 2026-10-05. Measured the same day with
`scripts/mtime_columns.py` in the private `~/w/name-index-bench` (`d543475`),
over a live walk of `$HOME` (4.2M entries, unfiltered, so including ignored
paths) and `/nix/store` (4.5M), rows in ferret's order: directories breadth
first, then files, siblings by name. **Revised the same day:** the first draft
sized ranks as a plain permutation, 23 b/row; Dave pointed out they deserve the
same blocked packing as the seconds, and packed that way they cost 13.2 b/row on
`$HOME`. Options D and E come from that, and F from asking why ctime kept its
nanoseconds.

**Question:** Should the catalog store each inode's mtime as its rank in mtime
order, rather than as a value, to save space and make time queries cheap?

Dave's proposal: on disk, each inode holds its rank in mtime order. In memory,
inodes are held in mtime order, in a structure that moves an inode to the top
cheaply. An updated mtime is nearly always the newest, so only the last few
seconds' actual values need keeping. `-newer file` finds the reference's rank
and lists everything after it; sorting is free. An absolute time (`-mtime -1`,
`-newermt DATE`) binary-searches the order, stating files on disk at each probe.

A rank packs well when blocked, because siblings made together have nearby
ranks: a block's width is the log of how many other times fall between its own.
A **dense** rank, where equal times share a rank, is also an index into the
sorted table of distinct times, so keeping that table (sorted, it packs as
blocked deltas) makes the rank a lossless coding of the value. That is an
ordered dictionary, blocked.

What each encoding costs, in bits per row:

| Encoding                                                        | `$HOME`                | `/nix/store`        |
| --------------------------------------------------------------- | ---------------------- | ------------------- |
| Today: mtime seconds + nanoseconds, each blocked                | 10.9 + 25.7 = **36.6** | 0.6 + 0.6 = **1.2** |
| Rank, a plain permutation                                       | 23                     | 23                  |
| Rank, unique (ties broken by row), blocked                      | 14.7                   | 7.6                 |
| Rank, dense over (sec, ns), blocked                             | 13.2                   | 0.6                 |
| Dense rank over seconds, blocked, + sorted distinct seconds     | 8.0 + 0.4 = **8.4**    | 0.6 + 0.0 = **0.6** |
| Dense rank over (sec, ns), blocked, + sorted distinct (sec, ns) | 13.2 + 14.7 = **27.9** | 0.6 + 0.0 = **0.6** |
| Today: ctime seconds + nanoseconds, each blocked                | 8.5 + 27.1 = 35.6      | 5.0 + 27.3 = 32.3   |
| ctime: dense rank over (sec, ns) + sorted distinct (sec, ns)    | 12.1 + 13.7 = 25.8     | 8.8 + 0.6 = 9.4     |
| ctime: dense rank over seconds + sorted distinct seconds        | 6.5 + 0.3 = 6.8        | 2.9 + 0.0 = 2.9     |

How find's time predicates are used, from the find-compat corpus (24,461 `find`
occurrences): `-mtime` 1,789, `-mmin` 1,234, `-newerXY` 1,207, `-newer file`
597, `-printf` with a time 230, `-ls` 211, ctime 170, atime 157. Absolute
windows outnumber relative-to-a-file ones about 7 to 1.

With a stored table (D, E), every time query works as today against ids instead
of values: an absolute window or a reference file's time becomes an id threshold
by binary search of the table, then the scan compares ids, at the same cost as
comparing values. Sorting by time is sorting by id. A printed time is a table
lookup. A new time the table lacks lives in the in-memory overlay until
compaction, which rebuilds the table and the ids. New times nearly always sort
after every stored one, as Dave says, so the rebuild seldom moves old ids,
though correctness does not depend on that.

| Option                                                        | Costs                                                                                                                                                                                                                                                                                                                                                                                                                                                                                               | Buys                                                                                                                                                                                                                |
| ------------------------------------------------------------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| A. Keep both mtime columns as they are                        | 36.6 b/row on `$HOME`.                                                                                                                                                                                                                                                                                                                                                                                                                                                                              | No work. Every time predicate and printed time answers from the index (D47).                                                                                                                                        |
| B. Rank only (Dave's proposal), blocked                       | 13.2 b/row on `$HOME`, 0.6 on nix, with no values kept. Listing in mtime order needs the inverse permutation in memory, a further ~23 b/row that does not pack. New times are often not the newest (`tar x`, `cp -p`, `rsync -a`, `unzip`, `curl -R`, `touch -d`), so the in-memory order must insert anywhere. Absolute queries, the common case, cost ~23 stats each, leave the index (D47) and fail on files deleted, unreadable or on an unmounted drive; printed times cost a stat per result. | Newest-first and `-newer file` in O(results) with no scan; exact sub-second order.                                                                                                                                  |
| C. Drop the mtime nanoseconds column; key carry-over on ctime | 10.9 b/row on `$HOME`. Sub-second mtime is gone: a printed mtime has whole seconds unless the result is stated, and `-newer file` must stat files in the reference's second to break that tie (find compares full timestamps). D26's carry-over key becomes `(dev, ino, size, ctime)`.                                                                                                                                                                                                              | −70% on `$HOME`, no new coding. The key loses nothing: any change to mtime, by a write or by `utimensat`, sets ctime to the current time, which userspace cannot set, so a matching ctime implies a matching mtime. |
| D. C, with seconds coded as a blocked ordered dictionary      | 8.4 b/row on `$HOME`, 0.6 on nix. C's costs, plus a new column coding (sorted delta-packed table, blocked ids) and an indirection to print a time.                                                                                                                                                                                                                                                                                                                                                  | −77% on `$HOME`; exact seconds; every predicate from the index.                                                                                                                                                     |
| E. Blocked ordered dictionary over full (sec, ns)             | 27.9 b/row on `$HOME`, 0.6 on nix. The new coding, as D.                                                                                                                                                                                                                                                                                                                                                                                                                                            | −24% with no semantic change: exact sub-second times, carry-over untouched, `-newer` needs no stat.                                                                                                                 |

The same coding applies to ctime. Under C and D, ctime is carry-over's only test
that a file changed since it was hashed, and the question is whether it needs
its nanoseconds:

- **Why keep them:** a file written again in the same second it was stated and
  hashed, at the same size, matches `(dev, ino, size, ctime)` in whole seconds.
  Its old hash would be carried over and stay wrong until the next change. This
  is git's racy-index problem; nanoseconds make the collision practically
  impossible.
- **Git's fix needs no nanoseconds:** a file whose ctime second is not before
  the second it was observed has an untrusted hash. It is recorded stale and
  rehashed at the next crawl. That is one check at observation and no stored
  field. The stat bracket around each read (`ferret-crawl` `observe.rs`
  `bracket`) is unaffected: it compares two fresh stats in memory, at full
  precision.

| ctime option                                           | ctime b/row, `$HOME` / nix     | Costs                                                                                                                                                                                                                                                                                                                                            | Buys                                        |
| ------------------------------------------------------ | ------------------------------ | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ | ------------------------------------------- |
| E. Ordered dictionary over (sec, ns)                   | 25.8 / 9.4 (today 35.6 / 32.3) | The new coding.                                                                                                                                                                                                                                                                                                                                  | Carry-over exactly as today.                |
| F. Ordered dictionary over seconds, plus the racy rule | 6.8 / 2.9                      | One check when a file is observed; files written in the second before observation are rehashed at the next crawl. A network filesystem whose clock lags ours could stamp a later write with an earlier second and slip past the rule; treating ctime within a few seconds of the observation as racy covers modest skew for a few more rehashes. | −81% on `$HOME`, −91% on nix against today. |

Per-block minimum and maximum, implied by each block's base and width, let
newest-first and narrow windows skip blocks under any of these. That is a scan
optimisation for S1b, not part of this decision.

**Recommendation:** D for mtime and F for ctime. Together the time columns go
from 72.2 to 15.2 b/row on `$HOME` (−79%) and from 33.5 to 3.5 on nix (−90%),
with every query still answered from the index. (D with E for ctime: 34.2 and
10.0.) B gets most of D's space saving but none of its values, and its speed
edge (newest-first) is the case block skipping covers.

**Fact that would change it:** if exact find parity on `-newer` without stating
anything matters more than ~19.5 b/row on `$HOME`, take E for mtime as well.
Agent query logs dominated by newest-N with printed times rare would favour B. A
filesystem ferret indexes whose ctime is not reliably updated on mtime changes
(some FUSE mounts) would make the ctime-only carry-over key unsafe there; those
roots would need E for mtime. Indexing network mounts with badly skewed clocks
would favour E for ctime on those roots.

**Time bench (2026-10-05, private `~/w/time-index-bench` `804e592`,
github.com/dbalmain/time-index-bench):** Rust, run by an Opus 5.5 subagent and
spot-checked here. Load, per-event update, compaction and query costs for eight
in-memory mtime structures under simulated events (single edits, a hot set,
archive extracts with old mtimes, `npm install` ties, cargo build/clean, git
checkout, `cp -p` restores, future mtimes, `rm -rf`, a mixed day), on `$HOME`,
`/nix/store` and `$HOME` tiled ×22 (10.1M rows). Every answer is checked against
a brute-force reference after every scenario, and a positive control proves the
check catches a non-strict `-newer`, an ignored overlay and wrong tie order.

| At 10M                            |    b/row |  `-mmin -5` | `-mtime -365` (5.6M hits) |   newest 20 | update p99, single edit | compaction |
| --------------------------------- | -------: | ----------: | ------------------------: | ----------: | ----------------------: | ---------: |
| today's columns (sec + ns)        |     44.7 |      5.5 ms |                    8.0 ms |             |                         | 265–297 ms |
| C, seconds column                 |     14.1 |             |                           |             |                         |  97–132 ms |
| D with block skipping             |     11.6 |     1.03 ms |                   6.45 ms |     2.65 ms |                ~0.57 µs | 148–192 ms |
| **D with an exact max per block** | **11.8** | **0.70 ms** |                **6.0 ms** | **0.42 ms** |                ~0.57 µs | 148–192 ms |
| D + sorted permutation            |     35.6 |        1 µs |                    6.5 ms |      1.4 µs |                ~0.57 µs | 282–329 ms |
| D + binary heap                   |     75.6 |             |                   ~230 ms |             |                  2.8 µs | 183–230 ms |
| B-tree                            |    157.0 |        1 µs |                     28 ms |             |                  2.0 µs | 346–385 ms |

- No ordered structure earns its memory. The sorted permutation turns selective
  windows and newest-k from sub-millisecond into microseconds for +24 b/row (3×
  D's total) and 153 ms to build at load; the heap only answers newest-k; the
  B-tree is 13× D's memory. Keeping any of them current is cheap in CPU
  (microseconds per event); the cost is memory. Heap and B-tree still pay at
  compaction, because renumbering InoIds relabels every id-keyed order.
- An exact maximum per block costs 0.25 b/row and gets the cheap part of the
  permutation's win (newest 20: 2.65 → 0.42 ms).
- D's base is 10.6 b/row at 10M against the 8.4 estimated above: ~0.44 is
  ferret's 16 B block-table entry, the rest the tiled corpus's 27× distinct
  seconds.
- Combining with a name side, choosing between scanning the time column and
  probing it per name-side id matters more than the structure: a probe costs ~11
  ns per id with D (~5 ns with the plain seconds column), and the crossover at
  10M is a name side of ~50–100k ids.
- Block skipping weakens after compaction scatters recent edits (the bench
  scatters them uniformly, which is pessimistic): `-mtime -1` 2.0 → 5.0 ms,
  newest 20 0.42 → 2.3 ms. A real edit trace would settle how much.

> Dave (2026-10-05): Agree with the recommendation.

**Answer (2026-10-05):** D + F, with D's blocks carrying an exact maximum.

- mtime: whole seconds as a blocked ordered dictionary (sorted distinct seconds
  as packed deltas, a dense rank per inode in 128-row blocks), each block with
  its exact maximum for skipping. The mtime nanoseconds column is dropped; a
  printed mtime has whole seconds, and `-newer file` stats only files in the
  reference's second to break that tie.
- ctime: whole seconds in the same coding. D26's carry-over key becomes
  `(dev, ino, size, ctime)` in seconds, with git's racy rule: a file whose ctime
  is within a few seconds of its observation is recorded stale and rehashed at
  the next crawl.
- No in-memory ordered structure (sorted permutation, heap, B-tree). Time
  queries scan with block skipping, and the planner chooses between scanning and
  probing per name-side id from the name side's size.

## D56 — Where an indexed find with actions executes

**Status: answered, A (Dave, 2026-10-05).** S1b M0, 2026-10-05;
[design](S1B.md#cli-and-find-effects).

**Question:** Should an indexed `find` with commands, deletion, prompts or file
outputs execute in the client's instance of the shared engine, or keep its
resident traversal in the daemon and call the client for observations/actions?
Fastest for small selective queries and simplest to maintain disagree.

F8 B/F12 D still bind both options: default predicates use stored metadata; only
this invocation's own `-delete` corrects stored child counts. Commands are
Boolean tests, actions run concurrently (F11 A), observed parent handles protect
delete/execdir, and prune/depth/quit/start sequencing remain visible. A filtered
path stream plus client commands cannot implement that contract. The client owns
cwd, environment/PATH, umask, stdin and tty. The daemon must never substitute
its launch context for them.

S1+ M3 measured full reader opens at **709 / 1,012 / 1,351 ms** for clean / 1% /
2% overlays on the actual 10.45M fixture (`2e12d7b`, warm OS cache, fresh
process). D54-integrated opens are **estimated 1–4 s**, not measured. The socket
has no measurement yet; **0.05–0.3 ms** per small request/response is a planning
range. At 100 dependent callbacks that adds **5–30 ms**; at one million it adds
**50–300 s** if serialised. B can overlap workers and batch independent work,
but it cannot batch away a command's truth value before evaluating its branch.
Those products describe serial work, not predicted whole-query elapsed time.

| Option                                                                   | Costs                                                                                                                                                                                                                                                                                                                                                                                                                            | Buys                                                                                                                                                                                                                                                              |
| ------------------------------------------------------------------------ | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| A. Execute action plans locally through the shared resident engine       | A one-shot action query pays another full query-only open, currently 0.7–1.35 s before D54, plus roughly 0.6–0.8 GiB additional process RSS while the daemon remains resident. Proposed routing exception to D49; no separate cold-query implementation. Batch pays its open once.                                                                                                                                               | Existing evaluator, live source, observed handles, command batching, tty/stdin/PATH and per-query deletion accounting stay in one process. No action RPC, capability transfer or replay proof. Fastest for dense cheap actions once loaded; simplest to maintain. |
| B. Cooperative resident traversal with client-local observations/actions | New continuation protocol, descriptor/cwd capability transfer, per-worker action replies and shared batching, delete-count feedback, prompt serialization, output commit/cancellation and no-effect-replay proof. Additional safe syscall dependency/features need review. Estimated 0.05–0.3 ms per dependent callback, plus bulk output/observation bytes. Code/latency unmeasured; count them in a prototype before choosing. | Avoids full client open and duplicate reader RSS; fastest expected for selective commands with few callbacks, where 5–30 ms beats a 0.7 s load. Actions still use the client's own context.                                                                       |

**Recommendation: A.** The correctness surface is materially smaller, and batch
amortises the load for agent suites. This is a recommendation awaiting Dave, not
permission to alter D49 or the answered find semantics. Read-only find/search
daemon queries and local batch actions can proceed independently; shipping
action-client routing waits for this answer. `-I` remains the existing local
live mode under either option.

**Fact that would change it:** production agent traces dominated by selective
indexed action queries, with the extra 0.7–4 s open visible in their latency,
and a B prototype preserving the complete find action/mutation suite with
measured callback and resident-memory savings large enough to justify its
continuation protocol. Measure dense delete too; command spawning can hide RPC
cost while deletion cannot.

> Dave (2026-10-05): agreed with the recommendation, A.

## D57 — Share JSON lines with the socket, or add binary framing

**Status: answered, A (Dave, 2026-10-05).** S1b M0, 2026-10-05;
[design](S1B.md#endpoint-and-request-context).

**Question:** Should the daemon use batch's tagged JSON-lines codec, or a second
binary socket codec for query/output blocks? Binary is faster for large byte
streams; one shared codec is simplest to maintain.

Batch requires JSON input under D46. The current `ferret` crate only writes
JSON; it has no general parser. Both options propose the external
**`ferret → serde_json`** edge for batch input. The engine stays in the existing
`ferret` library, with no new engine crate or internal dependency edge. Update
DESIGN's enforced graph with the dependency in the implementation commit; M0
changes no manifest. This brief records the required proposed edge rather than
smuggling a hand-written general JSON parser into the batch host.

Find output can contain arbitrary bytes. Base64 is exactly **4/3** of input
length rounded to four-byte groups before JSON framing: **800 MB → 1.067 GB**.
Binary could save at least **267 MB** on that example. At an illustrative **1
GB/s** wire rate that is **267 ms** of transfer, before codec CPU. This is
arithmetic, not a benchmark. Small connected JSON request/replies have an
**estimated 0.05–0.3 ms** total transport/framing budget; no local JSON-versus-
binary timing exists. Search's UTF-8 rows need not pay base64 for every path.

| Option                                          | Costs                                                                                                                                                                                                                            | Buys                                                                                                                                                                    |
| ----------------------------------------------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| A. One tagged JSONL codec for batch and socket  | Base64's 33% byte expansion on arbitrary find output, JSON field/framing and encode/decode CPU. Bulk throughput must be measured; small-message budget is estimated 0.05–0.3 ms. Requires bounded lines/parts and a real parser. | One schema, byte convention, decoder, compatibility rule and end-status/cancellation test suite. Easy agent/CI inspection. Simplest maintenance.                        |
| B. JSONL batch plus length-framed binary socket | Two encoders/decoders, equivalent schemas/version negotiation, truncation/length/cancellation tests and dual harness adapters. Small-message savings unknown.                                                                    | 25% fewer payload bytes than base64 on byte output, avoiding its codec work. Expected fastest for bulk find/printf streams; no need to turn arbitrary output into text. |

**Recommendation: A**, pending Dave. Bound frames and stream them; measure
first/final row and wire/CPU cost before building B. There is no socket timing
yet that justifies another codec, but B's byte advantage is real, so fastest and
simplest do not automatically coincide. Batch and its required parser edge are
common to both answers; the daemon codec slice waits for the answer.

**Fact that would change it:** JSON encoding/transfer dominates measured query
CPU or adds material end-to-end latency on the real agent workload, with a
binary prototype reducing that cost enough to justify a second protocol. A large
synthetic listing alone is useful evidence, not evidence that most agent queries
have that shape.

> Dave (2026-10-05): agreed with the recommendation, A.

## D58 — Batch input: adopt `serde_json`, or parse the request by hand

**Status: answered, B (Dave, 2026-10-05).** S1b M1 review, 2026-10-05. Raised
because S1b M2, the batch host, would add the workspace's first serde
dependency. D57 records `ferret → serde_json` as common to both of its options.
It does not offer the alternative.

**Question:** `ferret batch` reads JSON-lines requests (argv, cwd, mode and
options for each query). Should the `ferret` crate depend on `serde_json` to
parse them, or parse the request schema with a small hand-written reader?

Today nothing in the workspace uses serde. Every JSON byte `ferret` writes is
produced by its own `json.rs`. The external dependencies are `rustix`, `blake3`,
`regex` (confined to `ferret-verify`) and intpack.

| Option                                                                       | Costs                                                                                                                                                                                                                                                                                          | Buys                                                                                                                                |
| ---------------------------------------------------------------------------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ----------------------------------------------------------------------------------------------------------------------------------- |
| A. `serde_json` (with `serde` for derive, or `serde_json::Value` without it) | Two to five new crates in the lockfile: `serde`, `serde_json`, `itoa`, `ryu`, `memchr`, plus `serde_derive`, `proc-macro2`, `quote` and `syn` if derive is used, which adds compile time. A DESIGN graph line.                                                                                 | A parser that has been fuzzed for years; request types added later cost one struct each. D57's socket codec, if A there, reuses it. |
| B. A hand-written reader for the request schema only                         | About 200–300 lines: strings with escapes and `\u` surrogates, arrays, objects, integers and booleans, with a depth bound. It needs its own property test (round-trip through `json.rs`'s writer, plus rejection of malformed lines) and a fuzz target. Every new request field is hand-wired. | No new dependency. It matches how output is already written. Errors can name the exact field in ferret's own terms.                 |

**Recommendation: A, `serde_json::Value` without derive**. That is four small
crates, no proc-macro build cost, and a parser nobody has to maintain. "Build,
not adopt" is about the index structures, which are the part worth learning; a
JSON reader isn't. Fastest is a wash, since parsing a request is noise next to
the query.

**Fact that would change it:** if you want the dependency list kept minimal for
its own sake (audit surface, offline builds), B is a bounded, testable job, and
the batch schema is small enough that B stays small.

S1b M2 waits for this answer. M3 (D54's names) doesn't depend on it, so it goes
first.

> Dave (2026-10-05): I feel like B might be the right answer here. It gives us
> more scope to optimise for our usecase. The thing that would steer me toward
> serde would be if you think it's likely other tools might want to make request
> to the daemon.

**Answer (2026-10-05): B, a hand-written reader.** Other tools sending requests
does not steer toward serde. The wire format is JSON lines either way (D57 A),
so a client in any language uses its own JSON library, and the daemon's choice
of parser is invisible to it. What other clients change is robustness: the
reader must take any input. So B ships with a fuzz target and a property test
that round-trips through `json.rs`'s writer and rejects malformed lines, with
bounded depth and line length. The one case that would favour serde is a Rust
client library shared with other tools. That would be a separate crate, and it
could choose its own parser.

## D59 — How intpack enters the workspace

**Status: answered, A (Dave, 2026-10-05); intpack pushed at `6423815`.** S1b M3,
2026-10-05.

**Question:** S1b M3 is the first code to use intpack, through D54's packed
names and row postings in `ferret-catalog`. D11 said intpack "starts as a git
dependency and may be vendored". In what form should it enter for good?

**Facts, checked on 2026-10-05:**

- DESIGN listed `ferret-index → intpack`, but no manifest or lockfile had it.
  The layering test only checks that external dependencies are allowed, not that
  they are present.
- `~/w/intpack` `main` is **20 commits ahead of `origin/main`**, and those
  include the pfor128 and skip-table speedups. The D54 prototype
  (`~/w/name-index-bench`) uses it as a `path = "../intpack"` dependency.
- So a git dependency pinned to what is on GitHub today would build against code
  older than what D54 measured.

**What M3 is doing meanwhile:**

- The edge `ferret-catalog → intpack` is approved. It is within D11 and DESIGN's
  ownership table, since catalog owns the name dictionary and row postings.
- The dependency is `path = "../intpack"`, with a symlink at
  `~/w/super-ferret-wt/intpack` so worktrees resolve it too.
- There is no Nix package build of ferret, so nothing else breaks.

| Option                            | Costs                                                                                                                          | Buys                                                                                                                     |
| --------------------------------- | ------------------------------------------------------------------------------------------------------------------------------ | ------------------------------------------------------------------------------------------------------------------------ |
| A. Git dependency pinned by `rev` | You push intpack's 20 commits first. Every intpack change is a push plus a rev bump. Offline builds need the git cache.        | One copy. intpack stays its own repo, with its bench and ledger. D11's stated default.                                   |
| B. Vendor into `crates/intpack`   | Two copies that drift. intpack's own bench harness keeps pointing at the other one. Its toolchain ledger moves into this repo. | Builds anywhere with no network and no sibling checkout. Changes to the codec land in the same commit as their consumer. |
| C. Keep the path dependency       | Builds only where `~/w/intpack` sits beside the checkout. CI and other machines break.                                         | Zero friction while both are moving fast.                                                                                |

**Recommendation: A**, after you push intpack, and C until then. It is D11's
plan, and it keeps one copy of the codec.

**Fact that would change it:** if intpack's API is going to churn alongside
ferret for a while, so that most ferret changes need an intpack change, B
removes the push-and-bump loop.

> Dave (2026-10-05): We vendor in iff we want to make changes specific to our
> project. Remember that we have plans for other indexing projects like a small
> log search "Splunk in a single pod" utility. So, ideally, we'll keep adding
> improvements that can be used in other projects and eventually, we'll push it
> up to crates.io. Please do push it to GitHub though.

**Answer (2026-10-05): A, a git dependency pinned by rev.** intpack's `main` was
pushed on 2026-10-05 (`5e352f4..6423815`). General improvements go into intpack
itself so that other indexing projects get them. Vendoring happens only for a
change specific to Super Ferret. crates.io comes later, when the format settles.
The `path = "../intpack"` dependency M3 started with switches to
`git = "https://github.com/dbalmain/intpack", rev = "6423815…"`.

## D60 — Reactivate a dead DocId when its content returns

**Status: answered, B now, A′ as the upgrade (Dave, 2026-10-11).** S2 M0,
2026-10-06; [design](S2.md#segments-are-disjoint-docid-ranges). D36 B left this
to S2: "Whether a revert reactivates its old `DocId` is an S2 decision, where
postings make it worth something."

**Question:** When content that died comes back (a revert, a branch switched
away and back), should it get its old DocId, whose postings may still sit in an
unmerged segment, or a new one?

Today the catalog drops a dead document's row and keeps only the counter (D36
B), so returning content gets the next DocId. In S2 the index follows the
catalog and tokenizes every new DocId
([S2 § Writer and daemon seam](S2.md#writer-and-daemon-seam)). The crawl reads
and hashes the returning file either way, so reactivation saves only the
tokenizing and the new postings, never the read. All figures below are
**estimates**: a branch switch of 5,000 files at the research's 21.6 KB mean is
108 MB, which is about 0.36 core-s at the tokenizer's 300 MB/s/core target, plus
about 7.3 MB of new segment bytes at the docs-only 6.8% (`108 MB × 0.068`), held
until a merge purges the dead copy.

| Option                                      | Costs                                                                                                                                                                                                                                                                                                                                                                      | Buys                                                                                                                                                                   |
| ------------------------------------------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| A. Reactivate while postings exist          | The catalog keeps dead rows (D36 A's 16 B hash plus id per dead document) until the index purges their postings, so the index must tell the catalog which dead DocIds a merge purged. That is a new edge from index state into catalog state, against DESIGN's ownership table, and a new record type. Merges must keep their purge list until the catalog has applied it. | A flip-flopping branch costs no tokenizing and no new postings until merge; fewer holes in the DocId space (slightly smaller gaps, so marginally denser doc-id lists). |
| B. Never reactivate (D36 B carried into S2) | Returning content is tokenized again: about 0.36 core-s and 7.3 MB of segment bytes per 5,000-file switch (estimate), read from the page cache. Until merge, the dead copy's postings sit beside the new ones; the 25% dead-fraction merge rule bounds that.                                                                                                               | No code. Liveness stays one-way (live → dead), so a segment never needs to know whether a dead document might return, and the catalog stays ignorant of postings.      |

**Recommendation: B.** It is both the simplest and, within the noise, as fast:
the read that dominates is paid either way. Fastest and simplest do not disagree
here by more than a fraction of a core-second per switch, so this brief exists
only because D36 deferred it by name.

**Fact that would change it:** M5's steady-state measurement showing follow
passes after branch switches costing seconds of CPU or noticeable postings churn
on Dave's real repositories, for example a monorepo whose switches touch tens of
thousands of large files.

> Dave: This is definitely an experiment I'd like to run. We should make sure
> our metrics capture the cost of branch switches. You say that read dominates -
> I think we should explore using the git index to avoid that read. i.e. when
> the user switches branches, we detect the branch switch and use the same
> algorithm git uses to determine if a file has been modified from what is in
> the index. I assume this would mean keeping a mapping of git hashes to docIds.

### Measured update: the git-index experiment (2026-10-08)

**Status: open, revised 2026-10-08** after the branch-switch experiment
(`ferret-bench git-switch`, branch `wt/d60`). The figures below are **measured**
unless marked otherwise.

**Question:** When content that died comes back (a revert, a branch switched
away and back), should it get its old DocId back? And should the crawl use git's
index to recognise that content without reading it?

**What the experiment found.** The original brief said "the read dominates and
is paid either way". That was wrong, and so is the premise behind the git-index
idea. A checkout has just written the returning files, so they sit in the page
cache, and the crawl's read plus BLAKE3 runs at about 1.1 GB/s. The expensive
part of a switch is the follow pass, which tokenizes and builds a segment for
every new DocId. On a 5,139-file, 60 MB return (synthetic, A → B → A):

| Part of the returning switch                  | Cost                                     |
| --------------------------------------------- | ---------------------------------------- |
| Crawl read + hash                             | 50 ms wall, 57 ms CPU (4% of the switch) |
| Rest of the crawl refresh                     | about 100 ms wall                        |
| Follow pass (tokenize 0.39 s + segment build) | 1.17 s CPU, 1.20 s wall                  |
| New segment bytes                             | 3.35 MB                                  |
| Dead postings until merge                     | +7.1 M (7.95 M → 15.0 M)                 |

Dave's largest real repository has only 1,897 tracked files. No repository under
`~/w` is larger, so the synthetic one stands in for a big switch. Its 1,479-file
real return costs 16 ms of read+hash against 324 ms of follow CPU and 2.15 MB of
segment.

The git index does recognise returning content correctly. Every returning
eligible file was skippable: 1,479 of 1,479 real and 5,139 of 5,139 synthetic.
The check found **0 BLAKE3 mismatches across 6,739 skips**. One condition
applies: that holds only under a **nanosecond** racy-clean rule. Git's own
default compares whole seconds, and under that rule **zero** files are skippable
immediately after a switch, because the checkout writes them in the same second
as the index. Once a later git command such as `git status` rewrites the index,
they all become clean.

| Option                                                                           | Costs                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                   | Buys                                                                                                                                                                                                                                                                      |
| -------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| B. Never reactivate (D36 B carried into S2; today)                               | Each returning file is tokenized again under a new DocId: 1.17 s CPU, 3.35 MB of segment and +7.1 M dead postings per 5,139-file return (synthetic). The large real repository's return costs 0.32 s and 2.15 MB. The 25% dead-fraction merge rule bounds the dead postings.                                                                                                                                                                                                                                                                                                                            | No code and no new state. Liveness stays one-way, and the catalog stays ignorant of postings.                                                                                                                                                                             |
| C. Git-assisted identity, no reactivation: skip the crawl's read, still tokenize | A `.git/index` reader in `ferret-crawl`, written from the format docs; it is std-only, so no new crate edge. It has fallbacks for filters, attributes, sparse checkout, split index, submodules and SHA-256. A persistent `blob id → DocId` catalog section at 24 B/entry (SHA-1 plus DocId; 36 B under SHA-256), about 240 MB at 10M git-tracked entries before table overhead. A nanosecond racy rule that is less conservative than git's default and depends on 1 ns filesystem timestamps. Switch detection costs one stat per repository per burst (0.6 µs), plus a parse at about 0.45 µs/entry. | Saves 57 ms of read+hash per 5,139-file return (net about 44 ms after the 2.8 ms parse and 9.7 ms decision), about **3%** of the switch. The follow pass, which is 85–90% of the cost, is untouched. D63's re-hash saving for git repositories is real but just as small. |
| A. Git-assisted identity plus reactivation (the original D60 A, keyed by git)    | C's costs, plus the original A's: dead catalog rows retained until a merge purges their postings (16 B hash + 4 B DocId per dead document), and a purge list that the index hands back to the catalog. That is a new flow of index state into catalog state against DESIGN's ownership table, though no new `Cargo.toml` edge, since `ferret` relays it. Merges keep their purge list until the catalog applies it.                                                                                                                                                                                     | Skips the read **and** the follow pass for returning content while its postings exist: 1.24 s of the switch's 1.35 s, 3.35 MB of segment and 7.1 M postings per 5,139-file return. 1,405 of the large real repository's 1,479 returning files mapped to a dead DocId.     |
| A′. Reactivation keyed by content hash, no git                                   | The original A's retention and purge-list costs, with no git reader, no blob map and no switch detection. The crawl still reads and hashes the returning files (57 ms per 5,139). The catalog build looks the hash up among retained dead rows, in the sort it already does to assign DocIds.                                                                                                                                                                                                                                                                                                           | **96%** of A's saving: the follow pass, the segment and the dead postings. It also works for every way content returns, not only git: editor undo, `stash pop`, rsync, a restored backup. It adds no git surface for correctness to depend on.                            |

**Recommendation: B now; A′, not A, when reactivation is wanted.** Drop the
git-index idea: it is sound, but it buys about 3% of a switch for a format
reader, a 24 B/entry map and a racy rule that is stricter in what it assumes
than git's own default. Fastest and simplest disagree only between B and A′. On
Dave's real repositories a returning switch costs at most about 0.33 s of follow
CPU, which is under the original brief's "seconds of CPU" trigger. So B stays,
with A′ as the named upgrade. D63 A also stands: git does not remove the re-hash
in any way that matters.

**Fact that would change it:** follow passes after real switches costing seconds
of CPU, at the measured 51–64 MB/s per core. That means switches of about 100 MB
or 10,000+ files on a repository Dave works in; then A′. For the git half: a
working tree where the read is genuinely expensive, such as a switch larger than
free memory, a network filesystem, or a cold cache that was not measured here.
Even then A′ comes first, and C goes on top only if the read is measured to
dominate.

> Dave (2026-10-11): Agree: B now, A′ when reactivation is wanted, and drop the
> git-index idea.

## D61 — Term frequencies in S2's postings

**Status: answered, B (Dave).** S2 M0, 2026-10-06;
[design](S2.md#what-a-posting-holds).

**Question:** Should S2's postings store a term frequency per (term, document),
or doc ids only?

D6 made the index a candidate filter: it decides which documents to verify, not
how to order them. Frequencies are ranking's input (BM25 and relatives), and
nothing in S2 ranks. Sizes are from
[S2 § Bytes per content byte](S2.md#bytes-per-content-byte-estimate): doc ids
**9.69 bits/posting, measured** (intpack `pfor128skip`, `words.docs`) and
frequencies **4.33 bits/posting, measured** (intpack `pfor128`, `words.freqs`);
the per-content-byte totals are **estimates**.

| Option               | Costs                                                                                                                                                                                                                                                                         | Buys                                                                                                                                                                                                                                   |
| -------------------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| A. Store frequencies | A parallel unsorted stream per list: +45% on doc-id bytes, +33% on the whole index (6.8% → 9.0% of text). At 30 GB of text, about +0.68 GB. One more stream to write, merge and check.                                                                                        | BM25-style ranking later without reindexing. A "most occurrences first" order for agents.                                                                                                                                              |
| B. Doc ids only      | Ranking later needs a format version and a full re-follow: one pass over all text, about 16 min per 30 GB under the background pacer plus about 400 core-s (estimate, [S2 § Cost model](S2.md#cost-model-at-10m)). Until then, results come in DocId order (near path order). | The densest index, matching the goals' "index size takes priority". One stream per list. A verifier can still count matches in the documents it reads, which gives exact frequencies for phrase and regex candidates at no index cost. |

**Recommendation: B.** Fastest (fewer bytes to read) and simplest (one stream)
agree; this is a product-scope question rather than an engineering one, which is
why it is here. The reindex that A avoids is the same code path as any tokenizer
change.

**Fact that would change it:** ranked results scheduled for S4's agent skill or
the TUI, which would make the reindex a certainty rather than an option; then A
now.

> Dave: Agree. This is a good experiment for later, but my gut says doc ids only
> for now.

## D62 — `ferret search` syntax for content atoms and booleans

**Status: answered, A (Dave).** S2 M0, 2026-10-06;
[design](S2.md#syntax-open-d62).

**Question:** How does `ferret search` spell a content atom, and how does it
spell OR, NOT and grouping?

Today a query is a conjunction of atoms, one per argument, and a bare word is a
**name substring** (the `ferret-query` crate doc). Agents already call it, and
S4's skill will document it. find(1) spells booleans as whole arguments (`-o`,
`!`, `(` `)`), which agents quote correctly.

| Option                                                                       | Costs                                                                                                                                                                                                  | Buys                                                                                                                                                                                                               |
| ---------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ |
| A. A `text:` prefix for content; `OR`, `NOT`, `(` and `)` as whole arguments | Two more characters per content atom. `OR`/`NOT` become reserved words: a file literally named `OR` needs `name:OR`-style quoting, which needs a spelling (proposed: any atom prefix, e.g. `path:OR`). | S1 queries keep their meaning. Mixed queries read naturally: `ferret search ext:rs text:serde NOT text:"derive serialize"`. `case:text:Foo` composes with the existing `case:` prefix. One grammar for both sides. |
| B. Bare words search content and names together (a union)                    | Changes every existing S1 query's meaning and cost: `ferret search parse` becomes a content query over the whole index. Agents' and tests' expectations change.                                        | The shortest spelling for the most common content query.                                                                                                                                                           |
| C. A separate verb (`ferret grep TERMS [-- name predicates]`)                | A second grammar and parser, and two answers to "how do I combine them".                                                                                                                               | grep-like familiarity.                                                                                                                                                                                             |

For the booleans, the alternative to whole-argument words is Google-style
`-atom` for NOT and `|` for OR: shorter, but `-` collides with option parsing
and with names that start with `-`, and `|` needs quoting in every shell.

**Recommendation: A.** Fastest and simplest are indifferent here; this is
interface taste and compatibility, and it is Dave's interface.

**Fact that would change it:** the query log or S4's agent traces showing bare
words used mostly with content intent, which would argue for B's default with
`name:` as the explicit form.

> Dave: Agree with A, but let's run a side investigation into the different
> query languages used elsewhere. I'd like to include elastic search, splunk,
> jira, datadog, dynatrace, and anything else in this space you think is worth
> while. Give grok now, which has plenty of usage this week.
>
> As for `ferret grep`, that is also a must. I want a drop-in replacement for
> grep, similar to our drop-in replacement for find. Decisions about how close
> we match should follow the same patterns. i.e. match ripgrep's syntax for
> compatability. Please ask grok or an opencode model to get started on the
> ~/w/grep-compat harness, looking at ~/w/find-compat for comparison.

## D63 — Tokenize in a follow pass, or in the crawl's hashing read

**Status: answered, A (Dave).** S2 M0, 2026-10-06;
[design](S2.md#the-content-flow-d63).

**Question:** Should the index tokenize new content in its own pass after the
catalog commits (reading each new file a second time), or during the crawl's
existing read, which already sniffs and hashes every new or changed file?

The crawl worker reads a file, hashes it, and only the catalog build, later,
decides which hashes are new and assigns their DocIds (DESIGN § The catalog, by
sorting). Tokens therefore cannot be keyed by DocId on the worker. All sizes and
times below are **estimates**: 30 GB of text, the S1b M6 background pacer of 32
MiB/s, 1 GB/s unpaced sequential reads (S1PLUS's modelling input), and postings
per content byte from [S2](S2.md#bytes-per-content-byte-estimate).

| Option                                                                                                                                    | Costs                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                           | Buys                                                                                                                                                                                                                                                                                               |
| ----------------------------------------------------------------------------------------------------------------------------------------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| A. Follow pass: after each catalog publication, the index reads the live documents it has not covered, in DocId order, and tokenizes them | New content is read twice. First build: one more pass, about 16 min per 30 GB paced, or about 30 s unpaced. Steady state: the second read is of a file the crawl read moments earlier, so it should come from the page cache (M5 measures the hit rate).                                                                                                                                                                                                                                                                                                                                        | No change to `ferret-crawl` and no new crawl edge. Postings are appended in DocId order: no sort, no remap, a bounded 64 MiB buffer. One code path for the first build, a tokenizer change, a deleted index and the steady state. The index commits independently, with coverage covering the gap. |
| B. Fused: the crawl worker tokenizes from its read buffer, for hashes not already live                                                    | `ferret-crawl → ferret-text`. Either the whole file is buffered until its hash is known (8 MiB size cap × 16 workers = 128 MiB), or every read file is tokenized, including duplicates already indexed (linked work trees). Tokens are keyed by hash on the worker, then remapped to DocIds and sorted at commit: about 1.25G postings per 30 GB, an external sort of roughly 5–10 GB of pairs. A pre-commit hook in both the checkpoint build and the log-transaction paths. A first build is a different code path from steady state and from a tokenizer change, which still needs A's pass. | One read of each new byte: the first build saves one pass, about 16 min per 30 GB paced.                                                                                                                                                                                                           |

**Recommendation: A.** Simplest by a wide margin, and in steady state, which is
where the daemon spends its life, it should cost no extra I/O. Fastest and
simplest disagree only for the first bulk build, which B speeds up at the price
of a second, separate code path that A still needs anyway.

**Fact that would change it:** M5 measuring the second read's page-cache hit
rate in steady state below roughly 90%, or the first build's wall-clock on
Dave's real tree, from M1's census, being long enough that he calls it a product
problem.

> Dave: agree, but D60 may change this, at least for files with in a git repo.
> i.e. we may not need to re-hash a file during branch switching.

## D64 — How segment bytes reach a query: positional reads or `mmap`

**Status: answered, A (Dave).** S2 M0, 2026-10-06;
[design](S2.md#cost-model-at-10m).

**Question:** Should queries read postings from segment files with positional
reads into owned buffers, or map the segments?

The catalog reads checked sections into owned buffers (D38 B), and its resident
view is about 574 MiB at 10M (S1PLUS, estimate). The content index is
**estimated** at 0.68–6.8 GB at 10M depending on text volume
([S2](S2.md#bytes-per-content-byte-estimate)), so it cannot be resident under
D48's 1 GB line in either option. Syscall and copy costs below are
**estimates**: about 1–2 µs per warm `pread`, copies at about 10 GB/s.

| Option                                                                                                                        | Costs                                                                                                                                                                                                                                                  | Buys                                                                                                                                                                  |
| ----------------------------------------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ | --------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| A. Positional reads: block indexes resident (~0.4 B per term per segment), each list read with `pread` into per-query scratch | One syscall per (term, segment): a rare term across ~50 segments pays about 50–100 µs. Long lists are copied: a 1 MB list costs about 0.1 ms. A one-shot client and the daemon each copy what they read (the page cache itself is shared).             | No `unsafe`; the same checked-read contract as the catalog, so a corrupt or truncated segment is an error, never a signal. Resident memory is only the block indexes. |
| B. `mmap` each segment, read lists in place                                                                                   | One `unsafe` item under D11, with its ledger and the measurement that justifies it. A segment truncated or a device error under the mapping is SIGBUS in the daemon, not an error; immutability of segments makes truncation unlikely, not impossible. | Zero-copy reads and no syscall per list: the fastest rare-term lookups. Simple code once the mapping exists.                                                          |
| C. Read whole segments resident, as the catalog does                                                                          | 0.68–6.8 GB resident at 10M.                                                                                                                                                                                                                           | The fastest queries. Ruled out by D48's line; listed for completeness.                                                                                                |

**Recommendation: A.** Simplest and safe; B is faster per lookup, so fastest and
simplest disagree, but the difference is tens of microseconds on a query whose
row emission costs milliseconds. Revisit with numbers, as D11 asks.

**Fact that would change it:** M4 measuring `pread` and copying above about 20%
of rare-term or phrase-intersection latency at 10M, with an `mmap` prototype
removing most of it.

> Dave: Agree - avoid the unsafe and extra complexity unless we can back with
> real numbers. Revisit when we have metrics and can run experiments.

## D65 — The term dictionary is 87% of the index

**Status: answered.** Static dictionary layout: front-coded blocks routed by a
resident block-prefix FST (Dave, 2026-10-11; see the last update below). H's
codec question: **C** (Dave, 2026-10-11), measured as three arms, C1–C3, below.
Slice C: codec 1 stays the only term codec; token policy: T1, blob `text:` terms
through trigrams, no date index (Dave, 2026-10-12). S2 M2b, 2026-10-07;
[measurements](S2.md#m2--segment-writer-and-reader).

**Question:** The first real build is 12.7% of text, not the 6.8% S2 M0
estimated, and the term dictionary is 86.8% of it. What should S2 do about the
dictionary?

All figures are from one build over Dave's `$HOME`: 4.00 GB of text, 59 unmerged
segments, `ferret-bench segment-build --breakdown` on `wt/s2` at `6d514e2`. Rows
marked **derived** are arithmetic over measured sums.

- **Index:** 509.5 MB (measured). The dictionary is 442.3 MB, the doc-id
  postings 65.3 MB at 8.32 bits/posting, and the rest is head and sums.
- **Distinct terms:** 18.46M (M1c). They become 34.5M dictionary entries,
  because a term is repeated in every segment it appears in (h = 1.87).
- **Singletons:** terms with df = 1 in their segment are 78% of entries and
  **359.7 MB, 81% of the dictionary** (measured). By type:
  - hash-like: 66.2 MB;
  - `jsonl`: 114.1 MB;
  - no extension: 155.5 MB.
- **`jsonl` (agent transcripts):** 29% of text. Terms seen only in `jsonl`
  account for 99.6 MB of the dictionary.

What each lever leaves, in absolute bytes. M2b's S2.md block divides option (b)
by the reduced text, which makes dropping `jsonl` look worse than doing nothing,
so these figures use absolute bytes instead:

| Lever                                                                    |      Index |                                       vs 509.5 MB |
| ------------------------------------------------------------------------ | ---------: | ------------------------------------------------: |
| Drop hash-like singletons                                                |   443.4 MB |                                              −13% |
| Drop `jsonl` documents                                                   | ≤ 409.9 MB | −20% or more (shared terms' postings not counted) |
| Drop all per-segment singletons                                          |   149.8 MB |   −71%, before the fallback structure's own bytes |
| Full merge to one segment (rough, **derived**: each term once at 12.8 B) |   ≈ 303 MB |                                              −40% |

The singleton entries are long and share little with their neighbours: about
8.35 B of string per term after front coding. An FST saves least on exactly
those terms.

| Option                                                                                                                                                       | Costs                                                                                                                                                                                                                                                                                                                                                          | Buys                                                                                                                                                                                         |
| ------------------------------------------------------------------------------------------------------------------------------------------------------------ | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| A. Keep front-coded blocks; let M3's merge policy cut duplication; re-measure the merged index                                                               | The index stays at roughly 7–8% of text after merging (derived, rough): about 2.3 GB at 30 GB of text.                                                                                                                                                                                                                                                         | No new structure. Every term stays exact in S2: a rare-term lookup reads one block. The format already carries a dictionary codec id, so C or B stays possible later without a format break. |
| B. An FST dictionary                                                                                                                                         | A new structure to build, merge and checksum, and slower lookups than one block decode. Expected saving is small, because 81% of the bytes are the least-shared terms.                                                                                                                                                                                         | Possibly 10–25% of the dictionary (guess, unmeasured). The textbook answer.                                                                                                                  |
| C. Keep df = 1 entries out of the dictionary: a term missing from a segment's dictionary means "maybe", answered by S3's trigram structure and then verified | Couples S2 to S3, which isn't built or measured. Its trigram bytes add back an unknown share of the 71% saved. Every rare-term query, the most selective kind and common for agents (identifiers, error strings, hashes), becomes a trigram intersection plus a verify read instead of one block read. The dictionary can no longer say "absent" for any term. | The densest index by far: about 3.7% of text before trigram bytes. The structure that answers the dropped terms is one S3 must build anyway, because regex is a hard requirement.            |
| D. Drop hash-like whole-run terms only                                                                                                                       | −13%. Hash lookups (commit ids, UUIDs) are a real agent query; they would need C's fallback anyway. The tokenizer still emits their alphanumeric parts, so part of the noise stays.                                                                                                                                                                            | A one-line rule.                                                                                                                                                                             |

Excluding `jsonl` transcripts is not listed as an option. It is a crawl policy
(what to index), not a dictionary format. It is worth about 20% of the index by
itself, and it could be added to any of A–D, so it should be its own question if
you want it.

**Recommendation: A for S2, with C re-decided in S3 using measured trigram
bytes.**

- **Simplest:** A is simplest, and B is ruled out on the numbers.
- **Densest:** C is far denser, but its true cost depends on S3's trigram
  postings, which nobody has measured. Choosing it now would put S2 to wait on
  S3.
- **Why A loses nothing:** the dictionary codec id makes C a codec change later,
  not a format break.

**Fact that would change it:** two possibilities.

- M3 measuring the merged index above about 8% of text would make the A-only
  path unattractive; then move C's experiment forward to the start of S3.
- If index size must come down now regardless of query cost, choose C and accept
  that S2's rare-term queries wait for S3.

**Update, S2 M3 (2026-10-07, `wt/s2` at `b4a04e1`): the fact that would change
it has happened.** Measured with `ferret-bench index-build` over the same 4.0
GB:

- **First build:** 49 segments, 0.1254 B per content byte. 99 s wall and 94
  CPU-s unpaced, peak RSS 282 MiB.
- **After the merge policy settles:** 13 segments and **0.1108 B/B**. The
  dictionary is 81.9% of the index; h = 1.39; singletons are 72.9% of entries.
- **Forced full merge:** 1 segment and **0.0981 B/B**. The dictionary is 74.5%
  and singletons are 68.3%.

Both are above the ~8% line, so merging alone does not bring A down to it.

The revised recommendation is still A for S2, because S2 should not wait on S3,
but C's experiment now moves to the **first** slice of S3. That slice measures
trigram bytes for the singleton terms and the latency of a rare-term query
routed through them, then reopens D65 with C costed.

If you want the size down before S3, say C now, and S2's rare-term queries will
go through a verify scan until S3 lands.

### D65 amendment — H replaces C as the singleton experiment

**For the existing D65 brief, to append.** S3 M0, revision 5, 2026-10-08;
[design](S3.md#experiment-d65-rare-terms-without-dictionary-strings).

**C as written is unsound, and so is C′.** Three problems, the first two from
Astra and confirmed against the code:

- **False negatives.** The trigram fallback checks a normalised term against the
  document's raw bytes. A document whose `Kelvin` is written with the Kelvin
  sign emits the term `kelvin`, but its bytes hold no `kel` trigram, so C misses
  it.
- **Merging loses documents.** Merging a dropped singleton into a segment that
  keeps the term means a dictionary hit no longer lists every document.
- **Every lookup pays.** A dictionary miss stops meaning absent, so every term
  lookup pays the trigram fallback.

**The replacement option, H.** Singletons move to a searchable section of
(32-bit fingerprint, DocId) records, and every lookup unions that section with
the dictionary. It is sound by construction, and it needs no trigrams.

Records never return to the dictionary, so the section starts with every
**first-build** singleton: about 26.3M records on `$HOME` (derived from M3),
about 134 MB. A record persists while its document remains live. A purge removes
records of dead documents, and demotes into the section dictionary terms left
with live df 1. Its saving therefore shrinks as merging removes the control's
own singletons. The figures below are lifecycle **estimates, not bounds**.
Removing an entry can lengthen its front-coded successor, so M2's real encoding
at each state decides the saving.

| State       | Control (measured, M3) | H (estimate) | Change |
| ----------- | ---------------------- | ------------ | ------ |
| first build | 500.7 MB               | ≈ 283 MB     | −43%   |
| steady (13) | 442.3 MB               | ≈ 326 MB     | −26%   |
| full merge  | 391.9 MB               | ≈ 357 MB     | −9%    |

An idealised conversion of only the merged index's singletons would save about
105 MB, but H does not build that.

**Latency.** Every fingerprint hit is Maybe and costs one verification read. A
term carries one record per first-build segment in which it was a singleton (up
to 49 on `$HOME`), plus one per later purge that left it with live df 1. Each
lookup also pays one or two extra block reads per segment.

**Proposed rule.** M2 builds H through its own lifecycle, encodes each state for
real, and runs S2's churn trace to report records added by demotion, records
removed by purge, and the net population. H replaces dictionary codec 1 if, **at
steady state**, all of these hold:

- term-index bytes fall by at least 15%;
- warm p90 for df = 1, df 2–100 and absent terms stays under 20 ms;
- the mid and common medians stay within 1.5× the control's;
- build CPU stays within 1.2×.

Otherwise A stands. The full-merge figure is reported beside it.

**Fact that would change it:** H passing at steady state but saving under 15% at
full merge, which is what the estimate predicts. That matters only if the index
spends its life near full merge. If S2's merge policy keeps `$HOME` near 13
segments, take H. If long-running daemons drift toward one segment, H's
advantage erodes, and A is the simpler choice.

### D65 update — M2 measured H (S3 M2, 2026-10-08)

**Measured on a frozen snapshot**: 107,354 readable documents, 3.51 GB. The home
tree had changed since S2. The two arms are fresh real follows and real merges.
Every H query returned exactly the control's rows (750/750).

| State  | Segments | Control bytes |  H bytes | Saving |
| ------ | -------: | ------------: | -------: | -----: |
| first  |       45 |      471.3 MB | 202.9 MB |  56.9% |
| steady |        9 |      409.7 MB | 192.5 MB |  53.0% |
| full   |        1 |      368.6 MB | 193.2 MB |  47.6% |

The saving is roughly twice the estimate. The estimate missed 2.49M terms: each
of them occurs in more than one document overall, but only ever as a singleton
within a segment, so H never stores their strings. Records cost about 4.06 B
each, against an estimated 5.1 B. Build CPU is 1.03× (follow plus steady
merges). Peak RSS is 266 MB against 290 MB.

**The rule as written fails, and the control fails it too.** Steady warm p90 for
df1, df2–100 and absent is 24.3 / 33.7 / 23.7 ms; the control's is 23.4 / 24.8 /
22.9 ms. Both sit on a ~23 ms floor that is not the index. The isolated catalog
is stale: 5,656 of its 113,010 live documents are unreadable, so every query
tries 22,708 fallback name aliases. With that floor removed and nothing else
changed, the control is 0.08 ms everywhere. H is 1.44 / 10.99 / 0.18 ms p90, and
mid and common medians are 1.30× and 0.99×, so **every gate passes**. That
diagnostic still needs a refreshed catalog before it counts as evidence.
df2–100's p95 is 46 ms: the p90 rule does not bound large-file tails.

**A cost the estimate did not foresee.** On S2's synthetic 5% churn trace, H
ends **10.4% larger** than the control (13.07 MB against 11.84 MB), with write
amplification 3.47 against 3.00. Records never return to the dictionary, and for
recurring short vocabulary a 4-byte record costs more than a compact posting.
Sol names two fixes, neither built: delta-code the DocIds within an
equal-fingerprint run, or compress long runs as postings. Either keeps every hit
Maybe.

**Question: should H replace codec 1?**

| Option                                                               | Cost                                                                                        | Buys                                                                                                |
| -------------------------------------------------------------------- | ------------------------------------------------------------------------------------------- | --------------------------------------------------------------------------------------------------- |
| A. Keep codec 1 and drop H                                           | Nothing more to build.                                                                      | The index stays roughly twice the size H measured: 410 MB against 192 MB at steady state.           |
| B. Adopt H as it stands                                              | The 10% churn penalty on long-lived daemons. Every rare-term hit costs a verification read. | 53% off the term index on `$HOME` now.                                                              |
| C. Adopt H after a run-codec fix, re-measured on a refreshed catalog | One more slice: the run codec, plus re-running M2's bench and churn trace.                  | B's saving without the churn penalty. The latency verdict is taken without the stale-catalog floor. |

**Recommendation: C.** S3 keeps building on codec 1, with codec 2 opt-in, so
nothing blocks. H's density win is the largest S3 has measured. The churn result
is the only thing against it, and a run codec is a direct answer to it. **The
fact that would change it:** on a refreshed catalog, small-class p90 stays over
20 ms with full coverage. Then H's verification reads are the cost, and A
stands.

**Answered: C (Dave, 2026-10-11), measured as three arms.** H's records are
one-way: a dictionary term demoted to a record never returns, because a record
keeps only a 32-bit fingerprint. Under churn, recurring vocabulary therefore
piles up at about 4 B per document against about 1 B in a postings list. The
next slice measures, against codec 1:

| Arm | Fix                                                                                                                                               | Extra cost                                        |
| --- | ------------------------------------------------------------------------------------------------------------------------------------------------- | ------------------------------------------------- |
| C1  | Run codec: equal-fingerprint runs store the fingerprint once and delta-code DocIds                                                                | none                                              |
| C2  | C1, plus graduation: a merge that holds a term's text pulls matching records into its postings after reading each document to rule out collisions | merge-time document reads                         |
| C3  | C1, plus each record's first byte offset, used to verify graduation and query hits cheaply                                                        | about 3 B per record (+80 MB on `$HOME`, derived) |

Each is measured for bytes at first build, steady state, full merge and under
S2's churn trace; rare-term latency cold and warm, split by the verified file's
size; and merge CPU and I/O. The latency verdict uses a refreshed catalog, so
the stale-catalog floor is gone.

### D65 update — the static dictionary's layout (2026-10-10 to 2026-10-11)

**Answered: blocks on disk, routed by a resident FST over block keys (Dave,
2026-10-11).** [DESIGN.md](DESIGN.md) § Static term dictionary holds the result.
Measured in its own repo, `dbalmain/term-dict-bench` (private), on msa2, geekom
and p16s; slice 1 was reviewed by Astra (`results/review-1-astra.md`). The
questions were kept on a
[decisions page](https://claude.ai/artifact/EeBPfDj46NVpnGNW6Ub4h9).

The question was how the fully merged dictionary should hold the terms left
after H moves rare terms (df = 1) to fingerprint records. Option B above guessed
an FST was slower and saved little. Both guesses were made before H, when 81% of
the bytes were singletons.

| p16s `$HOME`, 10.3M terms          | Codec 1 (A) | Full FST, term → offset | Blocks + block-prefix FST |
| ---------------------------------- | ----------: | ----------------------: | ------------------------: |
| Dictionary on disk                 |    127.9 MB |                133.3 MB |                  133.8 MB |
| Resident                           |     16.6 MB |                134.5 MB |                   17.4 MB |
| Open, cold / warm                  |  30 / 10 ms |             266 / 94 ms |                 23 / 6 ms |
| Exact lookup, warm p50             |      4.3 µs |                  1.9 µs |                    4.4 µs |
| Levenshtein (`config`, distance 1) |      821 ms |                  6.7 ms |                     40 ms |
| Two-word alternation (`get\|set`)  |      826 ms |                  1.7 ms |                    3.7 ms |
| Substring (`.*timeout.*`)          |      1.09 s |                  2.01 s |                    1.09 s |

The block-prefix FST is 3.2 MB of the 17.4 MB resident. All arms return
identical ranges for every kept term and identical pattern sets. Timings are
single runs. msa2's `$HOME` (6.0M terms) ranks the same way.

**Decided along the way (Dave):**

- **An FST, not plain codec 1** (2026-10-10). Term-level regex and fuzzy
  expansion is best effort, because rare terms are not in it. Trigrams stay, as
  an optional feature.
- **Blocks plus a block-prefix FST, not a resident full FST** (2026-10-11). The
  full FST's open cost grows with the corpus: 61–84 ms cold on msa2, 266 ms on
  p16s.
- **A full FST stays possible, in memory only.** It can be built on demand from
  the blocks, which hold the terms sorted: about 7 s of background CPU and 134
  MB resident on p16s, no extra disk. Add it if measurements show term-level
  automaton queries are common enough for 40 ms → 7 ms to matter.
- **Term ordinals kept open.** They are implicit with fixed 32-entry blocks.
  DESIGN.md lists when to use them.

**Measured and not pursued: permanent term ids.** Ids given when a term reaches
df 2, stable across merges, would let segments drop their own term strings. On
p16s the ids add 39 MB to the global dictionary. Dropping segment strings pays
for that only at 16 unmerged segments (net +38 MB); at 4 segments the net is −42
MB, and at 1 it is −79 MB.

**Corrections.** The ordinal arm's first premise, "about 18 MB of per-entry df
and list-length bytes", was wrong: on msa2 it is 12.0 MB, 6.0 MB each. And the
ordinal arm's apparent saving was mostly the df it left out: a fair saving is
0.7% (Astra). Codec-1 postings lists carry no document count, so any layout that
drops the entry's df must store it somewhere else.

### D65 update — slice C

**Status: answered, A (Dave, 2026-10-12).** Codec 1 stays the only term codec.
Hapaxes get no special treatment, and codecs 2–7 stay on branch `wt/d65c`; this
record and [D65C.md](D65C.md) come to main without the code.

2026-10-12, refreshed catalog; [measurements and soundness](D65C.md). 112,382
live documents, 112,376 readable frozen representatives (3.98 GB), six
changed/unreadable representatives. The codec-1 control's steady rare-term warm
p90 is **0.12 ms**, so the stale-catalog floor is gone. All arms are real
follows and merges. All 7,500 experimental queries agree with the control.

**Question: which measured arm should replace codec 1?** The original rule
requires ≥15% steady saving, rare/absent warm p90 <20 ms, mid/common medians
≤1.5× control, and follow-plus-steady CPU ≤1.2×.

| Option                                         | Cost                                                                                           | Buys                                                                                |
| ---------------------------------------------- | ---------------------------------------------------------------------------------------------- | ----------------------------------------------------------------------------------- |
| A. Keep codec 1                                | 450.28 MB steady term index.                                                                   | Passes latency; no graduation reads. Existing default.                              |
| B. H0, codec 2                                 | df2 p90 23.83 ms; mid 4.07×; full churn +10.8%; CPU 1.152×.                                    | 52.6% steady saving.                                                                |
| C1. Run codec, codec 3                         | df2 p90 25.88 ms; mid 4.48×; full churn +0.9%; CPU 1.162×.                                     | 50.3% steady saving; nearly removes compact churn penalty.                          |
| C2. Runs + verified graduation, codec 4        | CPU 2.896×; steady merge reads 9.57 GB; peak RSS 815 MiB.                                      | 51.7% steady saving; full churn −2.5%; df2 p90 17.15 ms and all latency gates pass. |
| C3. Runs + first offsets + graduation, codec 5 | CPU 3.698×; steady merge reads 9.33 GB across 2.22M reads; peak RSS 743 MiB; full churn +1.3%. | 36.1% steady saving; rare warm p90 0.09 / 0.14 ms; all latency gates pass.          |

**Recommendation: A for the default; keep C1–C3 opt-in.** C1 answers most of the
compact churn cost, but verification latency still fails the rule. C2 and C3
solve enough latency to pass, but graduation exceeds the CPU budget. Offsets
make typical queries cheap; they do not make graduation cheap on this corpus.
C3's df2 p95 is still 32.91 ms, and its steady state has three segments versus
twelve for the other arms under today's paired merge policy.

The one-way-record diagnosis is confirmed. Today's ordinary paired churn state
makes H0 10.3% smaller; the 10.8% penalty appears at full merge. C1's run
headers add 30.42 MB at first build despite adding no offset payload. C3's
first-build increment is 94.39 MB (3.12 B/record), above the derived 80 MB
because this refreshed corpus contains more records.

**Fact that would change it:** a measured C3 implementation with bounded
graduation buffers and coalesced/budgeted reads that meets the original ≤1.2×
CPU rule while retaining these size, latency and soundness results; or Dave
explicitly choosing a higher CPU budget for the measured density gain. Any
skipped or failed verification must leave a record intact. The current buffers
grow with the merge input. All required collision, changed-file, offset-fallback
and repeated-merge soundness tests pass; all eleven original H tests exercise
every arm. Workspace gates pass **938 tests**, versus 928 on main, with zero
warnings.

### D65 update — offsets without graduation

**Status: answered, A (Dave, 2026-10-12).** Codec 1 stays; C4b is not adopted,
as part of the slice C answer.

2026-10-12, same refreshed catalog and frozen corpus as slice C;
[measurements](D65C.md#offsets-without-graduation-c4). Astra's review found
graduation cannot soundly avoid document reads and proposed C3's offsets with
every merge-time verification turned off. Two arms test that, both C3's on-disk
format: **C4a** (codec 6) keeps C3's full read when a dictionary term is demoted
to a record, to recover its offset; **C4b** (codec 7) reads nothing during a
merge and gives a demoted record the unknown-offset hint, which queries answer
with a full check. Neither graduates, so every record stays a Maybe hit that a
query verifies. All 1,875 C4b queries matched the codec-1 control.

**The read split.** A C3 re-run reproduced slice C's bytes and read counts
exactly, now counted by purpose. On `$HOME` **all 9.33 GB of steady reads, and
all 2.99 GB of full-merge reads, are graduation**: a frozen corpus has no
deaths, so nothing is purged and nothing is demoted. Demotion exists only under
churn. On S2's trace it is 0.74 GB of C3's 2.92 GB (329k of 2.45M reads); C4a,
which keeps more records to demote, reads 1.15 GB for it.

| Steady unless noted             | Codec 1               | C3                    | C4a               | C4b                   |
| ------------------------------- | --------------------- | --------------------- | ----------------- | --------------------- |
| Term index                      | 450.28 MB             | 287.91 MB (36.1%)     | 317.95 MB (29.4%) | 317.95 MB (29.4%)     |
| Full merge                      | 398.12 MB             | 271.75 MB (31.7%)     | 305.18 MB (23.3%) | 305.18 MB (23.3%)     |
| Follow + steady CPU             | 269.05 s              | 998.29 s (3.71×)      | 287.14 s (1.067×) | 284.17 s (1.056×)     |
| Merge document reads            | 0                     | 9.33 GB               | 0                 | 0                     |
| df1 / df2–100 / absent warm p90 | 0.12 / 0.15 / 0.12 ms | 0.09 / 0.14 / 0.05 ms | as C4b            | 0.21 / 0.51 / 0.21 ms |
| Mid / common warm median        | 1.88 / 26.26 ms       | 1.05× / 1.00×         | as C4b            | 1.51× / 1.06×         |
| S2 churn, ordinary / full       | 19.77 / 11.84 MB      | −4.2% / +1.3%         | −5.3% / +18.2%    | −5.9% / +17.3%        |
| S2 churn merge CPU              | 50.11 s               | 120.56 s              | 79.28 s           | 43.75 s               |

C4a and C4b write byte-identical sizes on `$HOME`, because neither demotes
anything there; their latency is the same index, so only C4b's was timed.
Astra's estimate of about 318 MB steady was right to the megabyte.

**The mid median sits on the gate.** The paired run gives 2.84 / 1.88 ms, 1.51×.
Two more C4b runs gave 2.81 and 2.82 ms; a rebuilt control gave 1.94 and 1.92
ms. Pooled, that is 1.48×. Mid terms keep a median of six records each, and
where an offset window cannot prove the token (its enclosing run leaves the
1,280 B window) the query reads the whole file: mid p90 is 15.2 ms against 5.5
ms, and p95 about 99 ms. At full merge mid is 1.55×. The rule tests medians.

**Question: should C4b replace codec 1?**

| Option          | Cost                                                                                                                                                         | Buys                                                                                            |
| --------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------ | ----------------------------------------------------------------------------------------------- |
| A. Keep codec 1 | 450.28 MB steady term index.                                                                                                                                 | Nothing to change; every gate passes by definition.                                             |
| B. C4b, codec 7 | Mid median 1.48–1.51× (at the gate), mid p90 2.8×; full-merge churn 17.3% larger than codec 1, because records are one-way again and each carries an offset. | 29.4% steady saving at 1.056× CPU with zero merge-time reads; rare/absent p90 under 0.6 ms.     |
| C. C4a, codec 6 | B's costs, plus 1.15 GB of demotion reads and 81% more merge CPU than B under churn.                                                                         | Known offsets on demoted records, which only churned indexes have; unmeasured on query latency. |
| D. C3, codec 5  | 3.7× CPU and 9.33 GB of merge reads.                                                                                                                         | 36.1% saving and the best latency.                                                              |

**Recommendation: B, as the opt-in candidate, with codec 1 staying default until
the mid median is settled.** It is the first arm to pass size, CPU and rare-term
latency together, and it does so with no merge-time reads at all, so it carries
no version-race or memory-growth risk at merge. C over B buys nothing
measurable: `$HOME` never demotes, and under churn it costs reads for offsets
the query would otherwise get by a full check. **The fact that would change
it:** mid medians on a second machine or corpus landing clearly above 1.5×,
which would make the window-miss fallback the cost and point at a cheaper fix
there (coalescing a document's windows, or widening a failed window before a
full read) rather than at graduation. A long-lived daemon on a churning tree
would also see the +17% full-merge penalty; if that matters more than the
`$HOME` saving, A stands.

Workspace gates pass **939 tests** (938 + one new: a real merge for codecs 5–7
counting graduation and demotion reads, which asserts C4b's reader is never
called). Every H soundness test that looped codecs 2–5 now loops 2–7.

### D65 update — token policy

**Status: answered (Dave, 2026-10-12).** Question 1: **C**, T1 on by default,
with `text:` terms that look like blobs answered through trigrams and
verification. Question 2: **A**, no date index; T2 is held. Question 3: **A**,
mtime and ctime stay as they are. The experiment code stays on branch
`wt/tokens`.

**Follow-up, answered A (Dave, 2026-10-12): a slow `text:<blob>` is accepted.**
Routed through trigrams, a blob query returns the same rows as before T1. On 300
sampled blobs it took 2.4–15.6 s median and read about 1.4 GB, against about 0.1
ms as a dictionary lookup. Base64 trigrams are close to random, so blob-heavy
documents hold all of them
([TOKENS.md](TOKENS.md#screening-routed-blob-text-queries)). The rejected
options were indexing each whole blob run as one hashed term (about 1.95M
one-off terms back) and dropping T1. What would reopen it: query logs showing
blob lookups are a habit.

2026-10-12, same catalog and corpus as slice C;
[method, results and semantics](TOKENS.md). Two opt-in tokenizer policies were
tried, over codec 1 and over C4b.

- **T1** drops base64-like runs from the term index.
- **T2** moves timestamps to a date index, keeping their date pieces as terms
  and dropping the time of day and keyed epoch numbers.

Trigrams are untouched, so `grep:` still sees every byte. Every query matched
the codec-1 control apart from rows the policy explains: the document held the
term only inside a suppressed span.

| Steady, against codec 1 | Term index                       | CPU      | Mid median                | Rows dropped (mid / common) |
| ----------------------- | -------------------------------- | -------- | ------------------------- | --------------------------- |
| codec 1, no policy      | 450.28 MB                        | 269.05 s | 1.88 ms                   | —                           |
| codec 1, T1             | 221.21 MB (50.9%)                | 1.026×   | 0.88×                     | 11,227 / 23,417             |
| codec 1, T2             | 442.63 MB (1.7%) + 3.3 MB dates  | 1.016×   | 1.04×                     | 2,008 / 2,894               |
| codec 1, T1+T2          | 215.62 MB (52.1%) + 3.3 MB dates | 1.060×   | 0.88×                     | 13,424 / 26,393             |
| C4b, T1+T2              | 189.94 MB (57.8%) + 3.3 MB dates | 1.134×   | 1.43× (1.63× same policy) | as above                    |

Hapaxes fall from 13.33M to 7.39M under T1 and to 12.56M under T2. On a
hand-checked sample, T1's false suppressions were about 6 of 150 code runs
(identifiers built from short capitalised words and digits), plus about 13 runs
of SVG path data. Prose had none. A has-suppressed-runs flag that restored exact
`text:` would send 2.24 GB of documents to verification.

**The brief's headline question, "does T1+T2 bring C4b inside the gates?",
answers no.**

- The mid median passes on paper, at 1.43×, only because the policies removed
  13,424 mid rows, about 8% at the median of 3,363 rows a query. Against codec 1
  under the same policy, C4b is 1.63×, and mid p90 is still 2.6×.
- The churn penalty is unchanged byte for byte. The S2 trace is synthetic and
  holds no blobs or timestamps.

So the policies are a separate decision from C4b. T1 alone halves codec 1's term
index at 2.6% CPU.

**Question 1: should the term index adopt T1?**

| Option                                                | Cost                                                                                                                                                                                         | Buys                                                                                                      |
| ----------------------------------------------------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | --------------------------------------------------------------------------------------------------------- |
| A. No policy                                          | 450 MB steady; 13.3M hapaxes, 44% of them blob fragments.                                                                                                                                    | Exact `text:` for any token.                                                                              |
| B. T1, plus a manifest field and version check        | `text:` stops matching blob pieces: 21–30% of rare control terms, and 24 of 50 mid queries lose rows (11k), all inside blobs. About 4% of reviewed code runs are real identifiers. 2.6% CPU. | 50.9% steady saving (229 MB) on codec 1, and better rare and mid latency.                                 |
| C. T1, routing blob-looking `text:` terms to trigrams | B's costs, minus the blob-term misses; a planner branch on the detector.                                                                                                                     | B's saving, with `text:<blob>` still answering through trigrams. Short words inside blobs stay unmatched. |

**Recommendation: C.** The saving is the largest any D65 arm has found, and it
costs no merge-time reads. What it drops is almost all noise. Option C keeps the
one query a user would notice, pasting a blob into `text:`. **The fact that
would change it:** a second corpus where the code-run false positives clearly
exceed about 5%, or where users search `text:` for short words inside keys.

**Question 2: should timestamps go to a date index, and in which layout?**

T2 is a capability, not a saving. It removes 7.6 MB of terms and adds 3.3 MB of
dates. Following Dave's framing, dates are stored as u32 UTC seconds,
deduplicated per (time, doc).

| Option                          | Cost                                                  | Buys                                                                                                                    |
| ------------------------------- | ----------------------------------------------------- | ----------------------------------------------------------------------------------------------------------------------- |
| A. No date index                | Dates stay terms; no range queries.                   | Nothing to build.                                                                                                       |
| B. Bucketed by day, 1 s offsets | 3.29 MB, 2.62 B/entry (17 offset bits, 3.4 doc bits). | Day, local-day and month exact; a point query verifies 2 docs / 4 MB / 8 ms (only for its fraction); month lookup 2 ms. |
| C. Time-major, 1 s              | 3.88 MB, 3.09 B/entry (17 of 24 bits are doc ids).    | The same exactness; simpler; 0.1 ms day lookup.                                                                         |
| D. Day buckets, 1 min offsets   | 1.57 MB, 2.27 B/entry.                                | Half the bytes; a point query reads 16 MB / 47 ms (p90 88).                                                             |
| E. Day buckets, no offset       | 0.38 MB.                                              | Smallest; a point query reads 171 MB / 328 ms, and a local day 241 MB.                                                  |

**Recommendation: B, if date search is wanted at all.** At second resolution
uniqueness costs only bits. Day buckets delta-code their doc lists to 3.4 bits,
where time-major cannot compress doc ids. B is 15% smaller than C with the same
exactness. Under T2, `text:2026` still matches, because date pieces stay as
terms. **The fact that would change it:** point queries turning out rare next to
day queries, which would favour D. Or a corpus whose timestamps spread across
far more documents per day, which would erode B's doc-list advantage and favour
C.

**Question 3: where do mtime and ctime belong?** They are one u32 per inode, and
the catalog already stores both as columns.

| Option                                  | Cost                                                                            | Buys                                                                             |
| --------------------------------------- | ------------------------------------------------------------------------------- | -------------------------------------------------------------------------------- |
| A. The existing catalog column, scanned | 4 B/inode, already paid; 0.32 ms per range at 394k inodes (about 0.8 ms at 1M). | No new structure; fastest for month and week ranges, which match 7–9% of inodes. |
| B. A sorted index in B's layout         | 4.2 B/inode more (bucketed); single-valued keys lose the doc-list compression.  | 0.06 ms instead of 0.32 for a one-day range.                                     |
| C. Time-major sorted permutation        | 2.9 B/inode more.                                                               | 0.16 ms day ranges; slower than a scan for month ranges.                         |

**Recommendation: A.** No measured range needs anything faster than a column
scan. **The fact that would change it:** catalogs of about 10M inodes, where a
scan nears 10 ms and selective "changed today" queries dominate. Then C.

## D66 — Phrase queries read too much: positions, or not

**Status: answered, D: a per-document Bloom 8 pair filter (Dave, 2026-10-11).**
S2 M4c, 2026-10-07;
[measurements](S2.md#m4--cursors-planner-and-ferret-search-text).

**Question:** Two-word phrase queries on your `$HOME` take 0.7 s at the median
and 1.7 s at p90. Should S2 store word positions to answer them from the index,
or accept the cost of verification?

D6, which absorbed D7, rejected positions with a trigger: "post-intersection
candidate sets in the tens of thousands on phrase queries", read as also
covering verification time above about 100 ms. Measured with
`ferret-bench content-query` on the fully merged `$HOME` index (113k documents,
4.0 GB of text), 50 two-term phrases picked automatically from adjacent parts:

- **Candidates per query:** median 2,510, p90 9,059. The count half of the
  trigger **does not fire**.
- **Latency.** M4b tokenized every candidate byte. M4c tokenizes only windows
  around the rarest unit's matches, with a soundness proof and a 30,000-case
  property test.

  | Latency                 |       M4b |        M4c |
  | ----------------------- | --------: | ---------: |
  | median                  |    12.1 s | **0.72 s** |
  | p90                     |    21.9 s | **1.73 s** |
  | bytes tokenized, median | 1,254 MiB |     32 MiB |

  The time half **fires**.

- **Reading alone:** the candidate files average 1.25 GB per query, which is 126
  ms at the median and 250 ms at p90. No matcher change can bring p90 under 100
  ms; only reading fewer bytes can. Of the remaining ~585 ms, about 190 ms is
  tokenizing and the rest is two whole-document regex scans. One combined scan
  would cut part of that; that is a cheap fix, not a question.
- **Which files the bytes come from** is not measured. Large documents dominate;
  whether those are the `jsonl` transcripts is a guess.

| Option                                                                                                                                                                                                                                  | Costs                                                                                                                                                                                                                  | Buys                                                                                                                                                                                                           |
| --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| A. No positions. Keep windowed verification; merge its two scans into one                                                                                                                                                               | Phrases stay around 0.3–0.7 s median and 1–2 s p90 on this tree (derived from M4c, after the merged scan). Cold cache adds whatever reading 1.25 GB costs from disk.                                                   | The index stays as it is, against D65's size problem. One code path. Every phrase answer is checked against the current bytes.                                                                                 |
| B. Full positions: a delta-coded position stream per posting                                                                                                                                                                            | 889.8M occurrences (M1c, measured) at ~1–1.5 B each comes to **0.9–1.3 GB** against a 392 MB merged index, about 3–4× the index and 33–43% of text (**estimate**). Positions must be merged, checked and kept current. | Phrase queries never read files: milliseconds. Highlighting comes from the index.                                                                                                                              |
| C. A coarse chunk map for large documents only. For each (term, document) over a size line, say 1 MiB, a bitmap of which 64 KiB chunks contain the term. Verification reads only chunks where every unit appears, plus their neighbours | A second postings stream for large documents. Size unmeasured: bounded by (large documents × distinct terms in each × a few bytes). Reads become `pread`s of chunks rather than whole files.                           | Reads shrink in proportion to how concentrated a phrase is inside big files, which is where the bytes are. Small files stay as A. The same "read fewer bytes" shape as D65 C, so S3 can measure both together. |

**Recommendation: A for S2, with C as an S3 experiment beside D65's C.**

- **Fastest and simplest disagree.** B is fastest, but it roughly quadruples an
  index that D65 already finds too big, against your "density over speed". A is
  simplest.
- **C** targets the measured cost, large files, at a fraction of B's bytes, but
  its size is unknown.
- **The 100 ms line** was mine, not yours. If 1–2 s at p90 for a phrase is
  acceptable to you and the agents, A is simply done.

**Fact that would change it:** two possibilities.

- Agent traces showing phrase queries are frequent, and latency-sensitive in a
  loop, would move C forward into S2.
- A measurement that most verified bytes come from `jsonl` transcripts would
  make a `jsonl` policy (D65's side question) the cheaper fix than any
  structure.

### D66 update — the chunk-map census (S3 M6, 2026-10-08)

**Heuristic estimate, not a bound** (S3.md § Experiment: D66 C). M4c's 50 phrase
queries were regenerated with the same seed, against a fresh index of the S2
bench catalog. Map bytes assume **1 B per (term, document, chunk)**, S3.md's
cheapest guess; its own table expects 1.5–4 B at 256 KiB.

| Chunk   | Estimated p90 read | Actual p90 read | p90 cut | Map bytes (1 B/triple) | vs index |
| ------- | -----------------: | --------------: | ------: | ---------------------: | -------: |
| 1 MiB   |            1.98 GB |         2.16 GB |      8% |                32.6 MB |    +8.3% |
| 256 KiB |            1.83 GB |         2.16 GB |     15% |                41.9 MB |   +10.7% |
| 64 KiB  |            1.68 GB |         2.16 GB |     22% |                58.2 MB |   +14.9% |

**Corrected after review** (Astra, S3 sprint review round 1). The first run's
estimate left out reads of files under 1 MiB, which no chunk map changes. Its
actual figure included them, so it reported cuts of 43% / 55% / 61%. The map
also counted uncapped tokens rather than stored terms. Both are fixed
(`wt/s3-m6` `192c4e0`). Counting large files only, the cuts are 5% / 24% / 34%.

The rule is a p90 cut of at least 50% for at most 10% more index. **No chunk
size comes near the cut, so D66 C is parked, not closed.** Two caveats:

- 1,320 forced whole reads include files whose stat differs from the stale bench
  catalog. That understates the cut, but not by the 28+ points needed.
- The anchor term is the one the real matcher picks: the longer unit.

Recommendation A stands. **Fact that would change it:** phrase reads
concentrated in a few huge files, for example after a `jsonl` policy change.
Then the large-file cut would decide it, so re-run the census.

### D66 update — per-document pair fingerprints (2026-10-09 to 2026-10-11)

**Answered: D, below (Dave, 2026-10-11).** Measured in its own repo,
`dbalmain/pair-fp-bench` (private), on msa2, geekom and p16s. The questions and
answers were kept on an
[options page](https://claude.ai/artifact/J8m565JvH3q7nkCrRnHRPg) and are
summarised here.

**Option D.** Each document stores a Bloom filter of its adjacent-unit pairs.
Units are the tokenizer's identifier parts, case-folded and capped; each pair
hashes to 64 bits. The filter has 8 bits per distinct pair and k = round(8 ln 2)
= 6 probes. A phrase query probes each candidate's filter for the phrase's pairs
and verifies only the documents that pass. It is sound: a document that contains
the phrase contains all its pairs. Verification stays as in A.

**Measured.** Workload C is 200 real multi-word patterns mined from agent
transcripts, one pinned core, filters held in memory, p90 per query:

| Workload C, p90                   | geekom warm | geekom evicted | msa2 warm | msa2 evicted |
| --------------------------------- | ----------: | -------------: | --------: | -----------: |
| Verify every candidate (A, today) |      922 ms |         3.18 s |    2.46 s |       8.13 s |
| Bloom 8, then verify              |      121 ms |         691 ms |    293 ms |       1.20 s |
| Transcripts out, verify every one |      748 ms |         1.97 s |    1.02 s |       3.12 s |
| **Transcripts out, Bloom 8**      |   **19 ms** |      **69 ms** | **61 ms** |   **171 ms** |
| Transcripts out, binary fuse 8    |      8.8 ms |          52 ms |     39 ms |       139 ms |

- **Size:** Bloom 8 is 236 MB on msa2's `$HOME` (112k documents, 3.9 GB of
  text), 60% of the 392 MB merged index. That fails D66's "at most 10% more
  index", and the bar is relaxed for it: the p90 cut is 85–98%.
- **Probing** costs under 0.6 ms a query. Verify costs 1.4–1.7 ms/MB warm and
  4.4–4.5 ms/MB evicted on both machines.
- **Build:** 41 ns/byte for pair keys plus Bloom 8 in the bench, an upper bound
  because its fold allocates per unit (see Q6 below).
- **Accuracy:** within 7–9% of a perfect pair filter at p90. Bloom 8's measured
  false-positive rate is 2.2%, as the formula predicts.

**Sub-decisions** (Dave, 2026-10-10 and 2026-10-11):

- **Filter: Bloom 8.** Binary fuse 8 reads 2–5% less at p90 for 25–30% more
  bytes (1.25–1.30 B/key measured, against Bloom 8's 1.00). It wins at equal
  bytes, but Bloom 8 is chosen for size. **Fact that would change it:** index
  size ceasing to matter; then take fuse 8.
- **Agent transcripts are not in the content index for now** (`.codex/sessions`,
  `.grok/sessions`, `projects` under `.claude*`). They are large append-only
  JSONL files where nearly every pair occurs, so whole-file filters barely
  reject them. Structured logs get their own index and query language later, in
  the planned VictoriaLogs alternative. Chunking transcripts into 64 KiB runs of
  lines was measured as evidence for that index. It cut transcript reads 2.4–6×
  and lost no matches across 9,250 A and 34,489 C matches on p16s.
- **Pairs only.** Adding triples removes long-phrase sequence false positives
  (workload B, 3 units: p90 1.13 GB → 64 MB). But it costs 2.5× the keys, and
  real queries gain only 4–16%.
- **Pair keys come from a streaming fold.** Each unit is hashed as it is read,
  and only the previous unit's hash is kept, so there is no per-unit allocation.

**Measured and not pursued:**

- **Filtering only large files:** files of 64 KiB and up still need 81% of the
  keys, and C's p90 gets 10% worse. The bytes worth rejecting are in files of 64
  KiB to 1 MiB.
- **df pruning:** dropping pairs in 1,000+ documents saves 8.7% of keys and more
  than doubles A's p90.
- **A per-document bitset for the 64–4,096 most common pairs,** taken only where
  it saves bytes: 0.14–1.97% smaller (0.41% at 256 bits). The key mass is in the
  long tail.

B (positions) stays rejected on size, and C (chunk map) stays parked. **Fact
that would change D:** a hard index-size budget near the 392 MB index. Then take
A, and make the verifier parallel first.

## D67 — How `ferret search` spells a content regex, and when it refuses

**Status: answered, (a) A2, (b) i (Dave, 2026-10-11).** (a) `grep:REGEX`,
**case-sensitive** by default, with `(?i)` inline for case-insensitive; `^` and
`$` match at line boundaries. This changes S3's shipped case-insensitive
default, so `grep:` gives the same answers as `ferret grep`. The grep-usage
survey found `-i` on 4.1% of 21,815 agent searches, and `grep:` as the only
content-regex prefix in real `ferret search` commands (2 uses; `re:` and `/…/`
0). (b) i as built. S3 M0, revision 3, 2026-10-08;
[design](S3.md#query-surface-d67).

**Question (a):** What prefix makes an argument a regex over file content, and
with what line and case semantics?

**Question (b):** When the planned expression could verify most of the tree,
does the query refuse, at what threshold, and through which flag?

`re:` already means a regex over **names** (S1), and D62 A gave content the
`text:` prefix. DESIGN's query section sketched `/regex/`. Agents call
`ferret search` from shells, so quoting cost matters.

**(a) Spelling.**

| Option                                     | Costs                                                                                                                                                    | Buys                                                                                                                      |
| ------------------------------------------ | -------------------------------------------------------------------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------- |
| A. `grep:REGEX`                            | One more prefix to learn.                                                                                                                                | It reads as what it does, and agents know grep. `case:grep:` composes as `case:` does elsewhere. `re:` keeps its meaning. |
| B. `text:/REGEX/`                          | It changes `text:` for an argument that starts and ends with `/`, which today is a term (`text:/usr/` finds `usr`). A regex containing `/` needs a rule. | No new prefix: `text:` covers all content.                                                                                |
| C. Bare `/REGEX/`, as DESIGN sketched      | `/` already makes a bare word a path substring (S1), so `/usr/` would change meaning.                                                                    | The shortest spelling.                                                                                                    |
| D. `re:` searches content as well as names | It changes every existing `re:` query's meaning and cost, which is D62's argument against its B.                                                         | One regex prefix.                                                                                                         |

Semantics under any option:

- **case-sensitive by default** (A2, answered 2026-10-11), with `(?i)`
  for Unicode case folding; `case:grep:` is accepted as a no-op to preserve
  existing queries. `text:` and `re:` keep folding by default;
- **multi-line:** `^` and `$` match at line boundaries, as grep users expect,
  and `(?-m)` restores whole-document anchors;
- `.` does not match `\n`, the `regex` default.

The alternative is the `regex` defaults, with whole-document `^` and `$`. Those
are right for names but surprising for content. A pattern the verifier cannot
compile, whether from a syntax error or from the regex crate's compiled-size
limit, is reported as a pattern error.

**(b) Refusal.** Before reading any list, the planner computes M, an **upper
bound** on the live documents the whole expression leaves Maybe. It is taken
from dictionaries only, and follows each atom's real certainty rules:
`case:text:`, capped terms and phrases are Maybe, as are H's fingerprint
records. It covers `NOT` and `OR`: `NOT grep:\d+` and `grep:\d+ OR text:rare`
both leave every document Maybe.

To stay an upper bound, `NOT e` is assumed to admit every live document (P =
live). That has a cost. An unindexed regex narrowed **only** by a `NOT`, such as
`NOT text:common grep:'\d+'`, is estimated at every live document and refused,
even when few documents actually lack `common`. A query with any positive
narrowing atom (`ext:rs`, `text:serde`) is not affected.

S2 already refuses when uncovered documents exceed 10,000, with the flag
`--scan-uncovered`.

| Option                                                                                                                                                      | Costs                                                                                                                                                                                                              | Buys                                                                                                            |
| ----------------------------------------------------------------------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ | --------------------------------------------------------------------------------------------------------------- |
| i. Refuse when M > live / 4 with `ScanRequired`; one flag `--scan` overrides both this and S2's uncovered refusal, with `--scan-uncovered` kept as an alias | Some broad but legitimate queries need the flag. `grep:the` likely does on `$HOME`, and so does a regex narrowed only by `NOT`. M is an upper bound, so it sometimes refuses a query that would have been cheaper. | No accidental 4 GB read, with a bound that is proved, not hoped for. One rule an agent can learn, and one flag. |
| ii. As i, but with a tighter `NOT` that uses an exact count of e's Yes documents, which needs reading e's lists before deciding                             | A list read before every refusal decision on a `NOT` query, and a second estimate path to keep correct.                                                                                                            | Fewer refusals of `NOT`-narrowed regexes.                                                                       |
| iii. Never refuse; report M in `explain()` and the JSON stats                                                                                               | Agents pay the full read before learning the cost.                                                                                                                                                                 | Nothing to learn.                                                                                               |

**Recommendation: (a) A, with multi-line on; (b) i.** On (a), fastest and
simplest are indifferent: this is interface taste and compatibility, as D62 was.
On (b), i is the simplest sound rule, and it follows S2's precedent. The
`NOT`-only over-refusal is narrow, and `--scan` answers it.

**Fact that would change it:** S4's agent traces. If agents reach for `/…/` or
for `re:` on content, make B or D the spelling, with the old one as an alias. If
agents hit `ScanRequired` on `NOT`-narrowed queries they meant, take ii. If they
hit it on broad positive queries, raise the fraction.

## D68 — Per-document filters: a screening stage, terms and trigrams separately

**Status: answered, B (Dave, 2026-10-11).** Neither filter is built: no layout
passed M1b's screen. Term filters could return only as a density opt-in, under a
new brief. S3 M0, revision 3, 2026-10-08;
[design](S3.md#per-document-trigram-filters).

**Question:** ROADMAP says S3 builds per-document trigram filters beside trigram
postings, and benches per-document term filters against postings (D6, D8).
Should either filter go into the index, or is a screening measurement enough to
decide?

A binary fuse filter's size follows from its key count, and each key is one
posting, so its bytes can be computed exactly from the documents' key sets.
Figures are **estimates** unless marked:

- **Trigrams:** about 500 MB of filters against about 351 MB of complete trigram
  files at the central estimate. The comparison flips if postings average much
  above 9 bits per pair. Term lists measured 8.32 (M2), and trigram lists should
  be denser.
- **Terms:** 89.7M pairs (**measured**, M1c) give about 101 MB of filters,
  against S2's 392 MB merged index (**measured**, M3). That is 3.9× smaller, so
  term filters are a real density option.
- **Latency:** a filter is probed once per candidate document. Unnarrowed, that
  is every live document: 0.13–2.7 s warm at 10M, against a rare term's measured
  1.94 ms warm median (M4b). Narrowed by a selective name atom or a rare term,
  the probe cost shrinks with the narrowing.
- **D8's premise** that filters cost 12% of postings is an arithmetic error in
  the research budget: 248 Mbit should have been 248 MB.

| Option                                                                                                                                                                                                                                                        | Costs                                                                                                                                  | Buys                                                                                                                                                   |
| ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | -------------------------------------------------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------ |
| A. Build both filters into the index and bench them, as ROADMAP says                                                                                                                                                                                          | A filter builder in follow and merge, a probe arm, coverage and tests: about one slice of work before knowing whether either survives. | Measured end-to-end numbers, including cold page-cache behaviour of real filter files.                                                                 |
| B. Screen first: layout bytes from exact key sets in M1a; real filters on matched workloads in M1b, after M3 (unnarrowed; narrowed by 1%, 10%, 50% name atoms; narrowed by a rare term), terms and trigrams separately; integrate only a layout that survives | A layout that survives screening still needs the integration slice later. Screening does not measure follow or merge cost.             | Each structure is decided on measured bytes and probe cost, before any index code. A surviving density/latency trade comes back here with its numbers. |
| C. Build term filters only, as a density opt-in, without screening                                                                                                                                                                                            | A second term structure to keep correct; unnarrowed rare-term queries 100–1000× slower when chosen.                                    | The smallest term index, for users who value bytes over everything.                                                                                    |

**Recommendation: B.** Fastest and simplest agree on screening first: it costs
bench commands, and it can only remove work. I expect trigram filters to fail
screening, being larger and slower than postings. Term filters will likely
survive as a trade, and come back here as a question about the opt-in.

**Fact that would change it:** screening showing a filter layout within about
1.2× of postings on the narrowed workloads, and smaller on bytes. Then build it
(A), for that structure only.

**Update after M1b (measured, `wt/s3-m1` `98b6fa9`).** Real binary fuse filters
were built from exact per-document key sets over the `$HOME` catalog.

- **Bytes:**
  - trigram filters are 354.3 MB, against 296.4 MB of `.tri` (**1.20×**, 10.4
    bits per key);
  - term filters are 122.7 MB, against S2's 391.8 MB merged index (**0.31×**,
    11.0 bits per key).
- **Latency,** filters against postings, by median:
  - **Unnarrowed,** trigram filters are 25–267× slower warm. Term filters are
    38× slower for common terms and up to 6,343× for rare terms.
  - **Narrowed to 1% of documents,** term filters stay 5–15× slower when probed
    from a file. They come within 1.2× only when held in memory, for mid and
    common terms.
  - **Evicted,** filters lose by 11–482× everywhere.
  - Filters win only when a rare term with its own postings narrows the query
    first, which is a hybrid, not a filter layout.

No layout passes the screen. **Recommendation unchanged: B**, and neither filter
is built. Term filters could return only as a density opt-in, under a new brief.

## D69 — Saturated large documents in trigram postings

**Status: answered, A (Dave, 2026-10-11).** Every document's trigrams are
indexed. Trigram indexes are optional, so B's saving is not worth taxing every
regex. S3 M0, revision 3, 2026-10-08;
[design](S3.md#bytes-per-content-byte-estimate).

**Question:** Large documents hold most of the bytes and many of the common
trigrams. Should every document's trigrams be indexed? Or should a document
above some distinct-key threshold be marked "Maybe for every trigram atom"
instead, and skip trigram postings?

Figures are **estimates** until M1a and M1b. On `$HOME`, 53% of the text is in
files of 1 MiB or more (**measured**, S2 M1), perhaps 800 files. At the central
estimate they contribute about 160M of 444M trigram pairs, about 120 MB. A
selective regex likely excludes most of them, but a broad one includes them all.
A high distinct-key count does not prove that a document holds the keys queries
actually use, so the threshold is swept against real regex candidates, not
guessed. M1b's sweep takes its candidate queries from M3's derivation.

| Option                                                                                                                                                    | Costs                                                                                                                                                                                           | Buys                                                                           |
| --------------------------------------------------------------------------------------------------------------------------------------------------------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------ |
| A. Index every document's trigrams                                                                                                                        | The full trigram files: about 351 MB central, about 36% of it from large documents. Bytes grow with text past a few GB.                                                                         | Every regex narrows over every document. One rule.                             |
| B. Above a threshold chosen from M1b's sweep (16k–256k distinct keys), put the document in a per-segment "saturated" bitmap, Maybe for every trigram atom | Every regex verifies every saturated document: up to about 800 files and 2.1 GB on `$HOME` at a low threshold. Warm, that is a few hundred ms of reading (M4c) on every regex; cold, much more. | Up to about a third of trigram bytes, at the threshold the sweep picks.        |
| C. A, plus a trigram chunk map                                                                                                                            | Likely larger than the trigram postings themselves (estimate).                                                                                                                                  | Large documents both narrowed and partly read. Not costed further this sprint. |

**Recommendation: A**, the fastest and the simplest. B is denser but makes every
regex pay a cost that the phrase work just removed. Fast and dense disagree, and
that is why this is a brief.

**Fact that would change it:** M1b's sweep finding a threshold where the
saturated documents hold more than about half of all trigram pairs, _and_
verifying them measures under 100 ms warm, with the cold figure reported, on the
M5 regex classes. B's cost is then small enough to buy its bytes.

**Update after M1b (measured, `wt/s3-m1` `98b6fa9`).** Documents above 16k
distinct keys hold **54%** of trigram pairs (2,852 documents), but verifying the
candidates they add costs 154–437 ms warm for three regex classes, and 0.7–1.5 s
evicted. At 64k and above, verification stays under 100 ms warm, but those
documents hold only 13–17% of pairs. No threshold meets both conditions.

**Recommendation unchanged: A.** Measured in bytes rather than pairs, a 64k
threshold would save 38% of `.tri` bytes (113 MB) for at most 84 ms warm. If you
would rather judge D69 in bytes, that is the case for B at 64k.

## D70 — How large the trigram follow buffer may grow

**Status: answered, B (Dave, 2026-10-11).** The shared budget should become
configurable, so a user who wants A's segment sizes can raise it. Fully merged,
A and B are the same size (688.1 MB), so B's extra 1–2% at steady state comes
only from having more segments. S3 M0, revision 5, 2026-10-08;
[design](S3.md#memory-live-structures-not-serialized-bytes).

**Question:** During a follow pass, should trigram pairs count against S2's 64
MiB flush budget, ending segments sooner? Or should they get their own bound, so
that segment size is still set by terms alone?

**Accounting.**

- **Terms** use S2's `Inverter::memory()`, a capacity estimate.
- **Trigrams** use a pair buffer allocated once at a fixed capacity: 8 B per
  pair, plus 8 B of radix scratch per buffered pair during a flush, plus a 2 MiB
  bitmap. A document is admitted only if `len + (doc_bytes − 2)` fits, so the
  buffer never reallocates.

Two rules hold under every option:

- an empty segment always accepts the next document, so follow always makes
  progress;
- a document larger than the whole capacity grows the buffer for itself, is
  flushed alone, and the buffer is then reset to its capacity.

Allocated bytes follow the capacity formula under every option:
`term.memory()` + 8 B × trigram capacity + 8 B × len of scratch during a flush

- 2 MiB. The buffer is reused, so its whole capacity counts once touched.
  Neither A nor B is an RSS limit. Encode buffers and allocator overhead are not
  reserved for, and M1a measures combined peak RSS separately.

Figures are **estimates** until M1a, unless marked measured:

- **Pairs per segment.** At S2's 49 build segments (**measured**, M3), a segment
  holds about 9.1M trigram pairs (central), with a range of 4.1–28.2M.
- **S2's own build peak** was **288,616 KiB** RSS (**measured**, M3).
- **What M1a measures.** It feeds both inverters from each read under A and B,
  and measures combined peak RSS, both structures' bytes, segment counts and
  merge work.

| Option                                                                                                                                     | Flush condition, checked before each document                                                                                 | Costs                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                       | Buys                                                                                                                                                                                           |
| ------------------------------------------------------------------------------------------------------------------------------------------ | ----------------------------------------------------------------------------------------------------------------------------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| A. Separate bounds                                                                                                                         | `term.memory() ≥ 64 MiB`, or the document may not fit the trigram capacity, which M1a sizes so that the term rule fires first | Follow's allocated memory is about 145 MB higher (central), 66–451 MB across the envelope, plus encode buffers. It is held while follow runs, though follow is paced.                                                                                                                                                                                                                                                                                                                                       | Segment count and term-dictionary duplication (h) unchanged from S2's measured figures.                                                                                                        |
| B. One shared 64 MiB flush charge, with a fixed trigram reservation of 48 MiB at 16 B per pair (capacity 3,145,728 pairs, a 24 MiB buffer) | Flush charge `term.memory() + 16 B × len ≥ 64 MiB`, or the document may not fit the capacity                                  | About 3.2× as many segments at the central estimate: the charge fires at about 2.87M pairs, against 9.1M per S2-sized segment. More term-dictionary entries are duplicated across segments, so the term index grows (by an amount M1a measures), and there is more merge work to reach steady state. The charge is not the allocation: a flush allocates about 68 MiB (central), 82 MiB in Astra's example of 48 MiB terms and 1M pairs, and at most about 90 MiB plus one document, before encode buffers. | A smaller, fixed trigram buffer: at most about 50 MiB on the trigram side (24 MiB buffer, 24 MiB scratch, 2 MiB bitmap), against A's buffer sized to a whole S2 segment, about 145 MB central. |
| C. Spill sorted trigram runs to a scratch file within a range, merged at flush                                                             | Either of the above, with spilled runs not counted                                                                            | New code, and a scratch file in the index directory to clean up.                                                                                                                                                                                                                                                                                                                                                                                                                                            | Both: S2's segment sizes and S2's memory.                                                                                                                                                      |

B's reservation is 48 MiB so that the flush charge, not the capacity, decides
when a segment ends. A 16 MiB reservation, about 1M pairs, would end segments on
capacity at roughly 9× S2's count, which is a different policy.

**Recommendation: A**, if M1a measures follow's combined peak RSS under 512 MiB
on `$HOME`. It is the fastest and the simplest. B is the most frugal with
memory, about 90 MiB at most against A's roughly 200 MiB central, but costs
density, which outranks memory here. C is the most code.

**Fact that would change it:** M1a measuring A's combined peak above 512 MiB, or
B's term index growing by less than 3% at steady state. Then take B. If neither
A's memory nor B's density is acceptable, take C.

**Update after M1a (measured, `wt/s3-m1` `6d63c27`).**

Both inverters were fed from each read over the `$HOME` catalog (112,898
readable documents, 3.97 GB). Each policy ran as its own process. Both segments
were bounded by allocated capacity, not by S2's legacy estimate. B was built as
one shared 64 MiB capacity bound, which is stricter than revision 5's 48
MiB-reservation flush charge.

|                           | A (term 64 MiB, trigram own bound) | B (shared 64 MiB)      |
| ------------------------- | ---------------------------------- | ---------------------- |
| Follow peak RSS           | 458.3 MiB                          | 247.5 MiB              |
| Build segments            | 73                                 | 177                    |
| Steady segments           | 10                                 | 15                     |
| Steady term index         | 449.3 MB                           | 454.3 MB (+1.1%)       |
| Steady trigram index      | 323.2 MB                           | 329.2 MB (+1.9%)       |
| Full-merge term + trigram | 688.1 MB                           | 688.1 MB               |
| Steady merge work         | 86.9 CPU-s, 7 merges               | 182.9 CPU-s, 18 merges |

A is under 512 MiB, and B's term index grows by less than 3%. Both of the
recommendation's conditions hold, so the stated rule picks **B**: half the
memory for 1.1% term density at steady state. B also pays 96 more CPU-s of
merging while the index settles. The trigram index is 7.9% of text at steady
state and 7.5% fully merged, inside the 20% gate.

**Revised recommendation: B**, with revision 5's 48 MiB reservation, which ends
fewer segments than the variant measured here. **Fact that would change it:**
treating merge CPU during settling as more costly than 210 MiB of follow memory.
Then A.

## D71 — Agent habits `ferret search` silently gets wrong

**Status: answered, A (Dave, 2026-10-11).** Phrases containing "or", "and" or
"not" must keep working: `text:"this or that"` is a phrase, not an error. So the
embedded-operator error fires only when the argument also holds another atom
prefix, as in `"text:foo OR text:bar"`.

**Raised 2026-10-08** by the D62 side investigation:
[query languages](research/grok/query-languages.html), a survey by grok. It
upholds D62 A and D67 A. It also finds three spellings agents use that
`ferret search` accepts and quietly answers wrongly, with exit status 0:

- **Lowercase operators.** `text:foo or text:bar` is a name search for `or`,
  ANDed with both text atoms. KQL, Sourcegraph, JQL, Kusto and Zoekt use
  lowercase operators.
- **The whole query in quotes.** `"text:foo OR text:bar"` is one `text:` phrase
  of four tokens. Nearly every surveyed tool takes the query as one string.
- **`content:`.** GitHub, Sourcegraph and Everything call the body `content:`,
  which `ferret search` reads as a name substring.

`-term` for negation already errors, which is the right failure.

**Question: should the grammar catch these?**

| Option                                                                                                                                          | Cost                                                                                                                                                                | Buys                                                                                                 |
| ----------------------------------------------------------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ---------------------------------------------------------------------------------------------------- |
| A. Accept `and`/`or`/`not` in any case as operators. Alias `content:` to `text:`. Error when one argument contains a space-separated `OR`/`NOT` | `and`, `or` and `not` stop being bare name words; `name:or` is the escape, as for `OR` today. Two names for one atom. One help line and the grammar comment change. | All three silent misses become correct answers or errors. No query that works today changes meaning. |
| B. Only the error on an embedded operator                                                                                                       | Smallest change.                                                                                                                                                    | Catches the quoted-query habit. Lowercase `or` and `content:` stay silent misses.                    |
| C. No change; document it in the help                                                                                                           | Nothing.                                                                                                                                                            | Agents that read the help are right. Those that guess get wrong answers with exit 0.                 |

**Recommendation: A.** It fits "optimise for agents", and every change either
turns a silent miss into a right answer or turns it into an error. The survey
also recommends keeping AND tighter than OR, fold-by-default with `case:` (no
smart-case), and the GitHub-shaped glob. A says nothing new about any of them.

**The fact that would change it:** the query log showing real searches for files
named `and`, `or` or `not`. Then B.

## D72 — Does the grep CLI automatically scan broad queries?

**Status: answered, A (Dave, 2026-10-11).** Any pattern the index cannot narrow,
including a literal shorter than 3 bytes, scans the selected files. Trigram
indexes are optional, and `ferret grep` works without them by scanning. The
documentation says plainly that without trigram indexes it should not be
expected to beat ripgrep. The grep-usage survey (`dbalmain/ferret-survey`,
21,815 agent searches) found 8.7% of patterns cannot be narrowed at all; under B
those would have needed `--scan`.

**Question:** Should a valid `ferret grep` command ever need `--scan` merely
because its regex cannot narrow the indexed files?

| Option                                                                     | Costs                                                                                                                                                    | Buys                                                                                                              |
| -------------------------------------------------------------------------- | -------------------------------------------------------------------------------------------------------------------------------------------------------- | ----------------------------------------------------------------------------------------------------------------- |
| A. Automatically scan the selected visible files; keep refusal in `search` | At 10M entries, a broad command can read the entire searchable tree. D8's historical cold full-tree scan was 234 s. `-v` is used by 433 rg repositories. | A habitual `rg -v foo`, short pattern or `\d+` gets an answer. One grep interface; no threshold-dependent errors. |
| B. Keep S3's M > live/4 and uncovered > 10,000 refusals; require `--scan`  | Legitimate rg commands fail based on index statistics. H's estimate can over-refuse small catalogs by up to 255 records per segment.                     | Prevents an accidental full content read. The existing planner's refusal policy can be reused unchanged.          |
| C. Automatically scan only below a byte/file budget                        | Another default and estimator; known size is not reliable for stdin or changed files. Above the budget, the same command still fails.                    | Protects whole-root queries while retaining convenient small-directory use.                                       |

**Recommendation: A (provisional).** Fastest favors B; compatibility and the
simplest enduring grep interface favor A. Automatic scan is permission to read,
not permission to change visibility, size policy or snapshot freshness. Use
postings only when useful; broad/inverted commands enumerate through the
existing catalog walk. No extra index structure or stored positions.

**Fact that would change it:** measured automatic scans dominating agent query
time on broad accidental commands, together with traces showing agents can
reliably retry with `--scan`; then B. Measure before introducing C's budget.

## D73 — What freshness does indexed grep promise?

**Status: answered, A+D (Dave, 2026-10-11):** snapshot by default, plus
`ferret grep --live`. The grep-usage survey found 35.3% of agent searches follow
an edit within 5 calls, but only 0.25% search for bytes the edit just wrote.

**Question:** May indexed grep retain S2/S3's snapshot exclusions while reading
current candidate bytes, or must every invocation search the current tree?

| Option                                                                             | Costs                                                                                                                                                                                       | Buys                                                                                                                                                            |
| ---------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| A. Snapshot membership and posting absence; verify candidates now                  | A new file or a changed file newly containing the needle can be missed until refresh/follow. This is not exact over the current tree.                                                       | Matches find/S2's existing model, generation pins and caches. Selective queries avoid checking every file.                                                      |
| B. Live enumerate and stat all selected files; add changed/new files to candidates | O(selected entries) metadata work on every query; at 10M entries this may dominate a rare lookup. Needs sound detection of replacements and policy changes.                                 | Current-tree completeness for observed stable files, while postings can still skip unchanged contents.                                                          |
| C. Refresh and follow to a query barrier first                                     | Refresh/follow can read the entire tree and block on writer backlog; S3 measured refresh 24 ms versus S2's 7.4 ms on its small trace. A barrier still is not an atomic filesystem snapshot. | Stronger freshness with one writer model; useful once the daemon has a proved barrier.                                                                          |
| D. Opt-in current-tree search retaining ferret ignore policy                       | The opt-in walks selected names, loads current policy rules and reads selected bytes; latency includes that work. Host must apply policy/pruning over find's existing live source.          | Separates freshness from visibility: new files/matches are found without searching ferret-excluded trees. Default selective queries keep A's low metadata cost. |

**Recommendation: A+D (provisional, rechecked after round 1).** Retain A as
indexed default, add `ferret grep --live` in M6. This is a real correctness
tradeoff for Dave to accept, not something the fastest/simplest rule settles. D
is the missing alternative: `--no-ignore` changes visibility and cannot serve as
the policy-preserving freshness switch. `--live` loads covering catalog roots
only as policy anchors, reads current rules from those roots, and walks current
names with D13 size/binary/ignore policy plus grep filters. No stale
name/posting exclusion; outside-root directories still need indexing or the
separately chosen unrestricted mode. Explicit starts retain their existing
exceptions. It is not an atomic filesystem snapshot.

A disappeared candidate is dropped; changed bytes stay path-local, never cached
under the old DocId. Checking emitted hits cannot repair snapshot No.

**Fact that would change it:** if agent traces require current-tree completeness
by default, promote D or measure B/C and include enumeration/barrier latency. If
`--live` is rarely used and a proved change barrier eliminates misses at low
cost, C may replace it. Unrestricted live mode is no longer presented as the
only way to request freshness.

## D74 — May visibility flags recover ferret-excluded paths?

**Status: answered, A (Dave, 2026-10-11).** Possible later feature, not
scheduled: respect `.ignore` and `.rgignore` files. The grep-usage survey found
13 uses of `--hidden` and none of `-u` or `--no-ignore`.

**Question:** Should rg's positive globs and visibility switches reconstruct
paths absent from the ferret catalog's visible universe?

| Option                                                                                              | Costs                                                                                                                                             | Buys                                                                                                                    |
| --------------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------- | ----------------------------------------------------------------------------------------------------------------------- |
| A. Intersect indexed mode with D47 visibility; explicit starts and unrestricted live mode bypass it | `-g` cannot resurrect ignored trees. `--hidden` reveals only hidden paths ferret holds. `.ignore`/`.rgignore` are not part of the indexed policy. | One catalog universe shared with find; no query-time policy walk. `--no-ignore` reuses find's unrestricted live source. |
| B. Apply full rg policy live, merging missing paths with indexed candidates                         | Extra policy inputs, walking and precedence logic on queries; a second policy universe to maintain. May search ferret's excluded build trees.     | Closer answers for the 1,031 rg repositories using `-g` and 362 using `--hidden`.                                       |
| C. Make indexed grep refuse every widening flag                                                     | Even `--hidden` fails although ferret ordinarily catalogs hidden files.                                                                           | A very small surface with explicit failures rather than partial widening.                                               |

**Recommendation: A (provisional).** It follows Dave's ignored-path contract;
the user-visible meaning of rg's overrides deserves confirmation. `--no-ignore`
(and `-u`, `-uu`) chooses the unrestricted live walk, like find's `-I`; `-uuu`
refuses until its binary-search semantics are supported; `-I` in grep means no
filename, never live mode. Explicit ignored directory starts use find's live
fallback. No fallback to an external rg executable.

**Fact that would change it:** visibility disagreements on ordinary agent
commands accounting for most real misses; B would need a separate policy review
and measurements before implementation. This slice does not authorize that
change to D13/D47.

## D75 — Raw bytes or BOM-aware input?

**Status: answered, D (Dave, 2026-10-11).** UTF-16 is a known difference from
rg. The BOM census (`dbalmain/ferret-survey`) found 1,389 files with a UTF-8 BOM
and 832 with a UTF-16LE BOM, most of the latter `.json` in one archive.

**Question:** May grep keep UTF-8 BOM bytes and decline UTF-16 BOM detection,
including on explicit current files where freshness does not explain a miss?

rg 15.2.0 defaults to BOM detection. Untimed synthetic probes: `^needle$`
matches UTF-8-BOM and UTF-16 LE/BE BOM inputs with auto encoding (exit 0/count
1), and misses all three with `--encoding=none` (exit 1). Corpus rank 42 is
`--encoding`, used by 20 rg repositories / 34 occurrences; six rendered commands
carry its feature label. These counts do **not** measure implicit BOM/UTF-16
usage. The invalid-byte fixture deliberately avoids a UTF-16 BOM; its binary
observations cannot settle encoding compatibility.

| Option                                                                                | Costs                                                                                                                                                                                                                                                                                                                                                             | Buys                                                                                                                                                |
| ------------------------------------------------------------------------------------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | --------------------------------------------------------------------------------------------------------------------------------------------------- |
| A. Raw input everywhere, as initially proposed                                        | Anchored matches can miss behind a UTF-8 BOM; UTF-16 matches can miss or become binary skips even on explicit files. Dave must accept that divergence.                                                                                                                                                                                                            | One original-byte profile for matching, offsets and trigram soundness; no transcoding or extra enumeration.                                         |
| B. rg's BOM-only auto detection everywhere; arbitrary named encodings remain deferred | Strip UTF-8 BOMs; decode UTF-16 LE/BE before NUL detection. Need malformed-input rules, byte-offset mapping and bounded conversion. Indexed UTF-16 lacks ordinary text postings: include catalog-visible BOM text through enumeration/live verification, never negative raw-trigram shortcuts. Measure resulting binary-head reads; no numeric cost measured yet. | Fixes default encoding misses on both explicit and recursive input; closer drop-in behavior without a general encoding dependency.                  |
| C. BOM handling only for direct files/stdin and policy-preserving live mode           | Matching profiles and offsets differ by source; indexed UTF-16 still misses, and switching modes changes answers. Later B must unify those rules.                                                                                                                                                                                                                 | Addresses explicit-current-file failures without probing every catalogued binary file in indexed mode.                                              |
| D. Strip an initial UTF-8 BOM consistently for all inputs; defer UTF-16               | UTF-16 still misses.                                                                                                                                                                                                                                                                                                                                              | Removing the prefix keeps the needed substring trigrams in the original bytes, so it needs no binary enumeration. It is cheap and separable from B. |

**Recommendation: D.** Apply UTF-8 BOM stripping consistently to all inputs and
defer UTF-16. D fixes the cheap UTF-8 prefix miss while retaining original bytes
and trigram soundness without binary enumeration. It keeps UTF-16 as an explicit
known miss and remains separable from B. If that remaining miss is unacceptable,
prefer B over C: C leaves an input-source-dependent dialect that builders and
agents must remember. Under B, transformed matches must satisfy the necessary
query over original indexed bytes; where unproved (notably UTF-16), bypass raw
postings and enumerate the relevant selected names. If B is chosen, specify
offset semantics before implementation, along with malformed decoding rules,
bounded conversion and the selected-name universe. D/B changes the planned
reader/profile scope and M1/M6 positive fixtures; schedule the chosen option
before authorizing builders. No extra crate edge or index-format change is
presumed here.

**Fact that would change it:** ordinary agent commands encountering UTF-16 BOM
text, or measurements proving B's indexed UTF-16 enumeration and conversion cost
acceptable. Then choose B and specify original-byte versus decoded offset
semantics before building. With D, keep positive UTF-8 BOM fixtures and UTF-16
oracle fixtures; label the UTF-16 miss as the approved encoding disagreement,
never freshness or binary as a substitute cause.

## D76 — Rare-literal regex screens: word 4+5, the term screen, or nothing

**Status: C chosen and built (2026-10-13, branch `wt/par-verify`); a
follow-up question is open below.** Measured in [SCREENS.md](SCREENS.md) on the D65C corpus
(3.98 GB, 112,376 readable documents, 250 case-insensitive regexes). Times
are median ms, cold / warm; `rg` is the full scan on 32 threads.

**Question:** Should regex search get a second screen for rare literals
— the 1.5 GiB word 4+5 index, or a term-index screen — or neither?

| Class | rg | trigrams | word 4+5 | term screen (sound) |
| --- | ---: | ---: | ---: | ---: |
| rare literal | 5,182 / 168 | 2,966 / 249 | 9.9 / 0.56 | 75 / 57 |
| identifier | 6,135 / 172 | 6,356 / 501 | 4,544 / 402 | 7,211 / 595 |
| alternation | 5,226 / 179 | 8,272 / 542 | 5,485 / 302 | 6,347 / 428 |
| code | 5,142 / 169 | 11,137 / 5,316 | 5,638 / 2,552 | 6,594 / 3,012 |

| Option | Costs | Buys |
| --- | --- | --- |
| A. Opt-in word 4+5 index | 1,533 MiB beside 286 MB of trigrams (38% of corpus bytes); 884 CPU s to build; a second extractor kept in step with follow and merge. | Rare literals 526× faster than a cold scan and 298× warm; sub-ms warm. Other classes 0.07–1.35× a scan, so no help there. |
| B. Term screen with a flat dictionary and a residue store | About 484 MB new (154 MB flat dictionary, 330 MB of lost whole runs); every regex with a literal scans both (56 ms warm, linear in residue); new writer paths for both in follow and merge; substring lookups in the planner. | The same rare-literal volume as A (8.9 vs 9.3 MB median), sound in both cases; 69× a cold scan, 2.9× warm. Reuses the term index; a third of A's bytes. |
| C. Neither; parallelise verification first | No new index. Rare literals stay at trigram speed (1.7× a cold scan, slower than a warm one) until verification is threaded. | Verification is single-threaded today and is most of every class's time; threading it is the lever for all five classes (estimate: near a warm scan), and it composes with A or B later. |

Rejected on the numbers: the term screen as briefed (no unit is safe under
case-insensitive search; 7/50 rare literals case-sensitive), the term screen
with a blanket fallback (65% of corpus bytes admitted; slower than
trigrams), and hashed 4/5-grams at 15–17 bits (180–400 MB for at most 1.5×).

**Recommendation: C now, then B if rare literals still matter.** Four classes
of five are bound by single-threaded verification of 1.4–3.6 GB, and no
screen here moves their volume more than about 2×. For rare literals B
reaches A's volume at a third of the bytes and is already 69× a cold scan;
A's 56 ms warm saving buys little against a 170 ms scan.

**Fact that would change it:** agent traces showing rare literals are a large
share of grep time *and* warm latency below ~10 ms matters for them; then A,
since only A is sub-ms warm. If the residue store would grow with blob
content past roughly the size of A's index, also A.

### Update 2026-10-13: C built and measured

Content verification now runs on a pool of threads. `FERRET_SEARCH_THREADS`
sets the count, which defaults to every core (every core up to 16 since
D77). For files that do not change
during the query, and without cancellation, output is byte-identical at any
thread count, and tests compare 1 thread against N. Workers reserve at most
64 MiB of documents at once, by catalogued size, plus 1 MiB of kept buffer
each; files that grew since indexing hold more than they reserved, and the
calling thread's buffer sits outside the budget. The one order that changes is
that a document can be read up to 64 rows per thread (since the scan
work, up to 16 batches of up to 256 positions per thread) before its
predecessors are emitted, so a result callback that changes a later file
may not change that file's row
([SCREENS.md § Parallel verification](SCREENS.md#parallel-verification)).

Measured through the product path in
[SCREENS.md § Parallel verification](SCREENS.md#parallel-verification):

| Class | rg cold / warm | trigrams, 1 thread | trigrams, 32 threads | best warm (threads) |
| --- | ---: | ---: | ---: | ---: |
| rare literal | 5,182 / 168 | 3,119 / 250 | 900 / 72 | 47 (8) |
| identifier | 6,135 / 172 | 6,848 / 514 | 1,738 / 128 | 93 (16) |
| alternation | 5,226 / 179 | 8,617 / 606 | 2,061 / 165 | 143 (16) |
| broad | 5,208 / 172 | 11,160 / 965 | 2,778 / 349 | 275 (8) |
| code | 5,142 / 169 | 11,395 / 5,340 | 1,690 / 240 | 240 (32) |

Cold, every class now beats a scan by 1.9–5.8×, because the reads are
bound by the device at about 1.7 GB/s, twice `rg`'s rate. Warm, rare
literals, identifiers and alternations beat it by 1.25–3.6×. Broad and code
do not.

Warm stops scaling at 8–16 threads, for a different reason on each class:

- Memory-bound classes are limited by copying out of the page cache.
- Broad is limited by the calling thread's per-row work: 181 of 258 ms at
  8 threads.
- Code is limited by the regex itself.

Threading also changes option B's case. B's term screen measured 57 ms warm
on rare literals, and threaded trigrams now take 47–72 ms. B still wins
cold, 75 ms against 900.

**Question:** What should come next for regex latency: A, B, or more
product work on the verifier?

| Option | Costs | Buys |
| --- | --- | --- |
| A. Opt-in word 4+5 index | As above: 1,533 MiB and a second extractor. | Rare literals 9.9 / 0.56 ms: 91× threaded trigrams cold, 84× warm. Nothing on other classes. |
| B. Term screen and residue store | As above: about 484 MB and two new writer paths. | Rare literals 12× faster cold (75 against 900 ms). Warm is now no better than threaded trigrams. |
| D. Take per-row work off the calling thread, and read without the copy | No index bytes. Moving residual evaluation and path building to workers touches the ordered window just built; `mmap` brings unsafe or a crate, and fault handling for a file truncated during a read. | Broad's serial ~180 ms, and the memory-bound classes' copy. These are estimates: broad could move toward 100–150 ms warm, and the light classes might gain 20–40%. Nothing here is measured. |

**Recommendation: D, then decide A or B on cold traces.** Every class pays
the caller and the copy, while A and B help only rare literals, which are
already 3.6× faster than a warm scan. D is the lever that could take broad
past a scan.

**Fact that would change it:** if agents' rare-literal searches commonly
run cold, for example as the first search after a boot or after memory
pressure, then B, or A at its size, buys 12–90× cold that no amount of
threading reaches.

### D76 addendum — a suffix array or an FM-index

**Status: open.** Measured in [FMINDEX.md](FMINDEX.md) on the same
population. Verification ran on 16 threads, the count used for threaded
trigrams from `wt/par-verify` (D76 C, cited, not re-run). Times are median
ms, cold / warm. The SA is per 1 GB segment, ASCII-folded, with lines
verified around each hit.

**Question:** Does a suffix array or an FM-index beat word 4+5 and threaded
trigrams by enough to justify its size and merge cost?

| Class | Threaded trigrams | Word 4+5 | Suffix array | FM-index s=32 |
| --- | ---: | ---: | ---: | ---: |
| rare literal | 900–930 / 49.5 | 9.9 / 0.56 | 57.9 / 0.82 | 70.5 / 1.16 |
| identifier | 1,738–1,845 / 92.8 | 4,544 / 402 | 1,447 / 114 | est. ≥ 620 warm |
| alternation | 2,061–2,356 / 142.9 | 5,485 / 302 | 878 / 53.3 | est. 438 warm |
| broad | 2,778–3,532 / 301.7 | 11,164 / 880 | 2,949 / 225 | est. 13,276 warm |
| code | 1,690–2,195 / 359.2 | 5,638 / 2,552 | 568 / 92.4 | est. ≥ 185 warm |
| bytes | 286 MB | +1,607 MB | +19,894 MB (5.0× corpus) | +3,896 MB (0.98×) |

Threaded-trigram cold is a range because only 8- and 32-thread cold rows
exist. Word 4+5's non-rare rows verify on one thread. The FM's non-rare rows
are estimates built from a measured locate cost of 3.6 µs per occurrence.

| Option | Costs | Buys |
| --- | --- | --- |
| A. Suffix array + folded text per segment | 5× corpus on disk: 20 GB here, about 50 GB at D40's 10 M entries. A merged segment is rebuilt: about 20 s and 230 CPU s per GB at 16 threads, writing 5 bytes per corpus byte. Updates go to a delta segment plus a tombstone bitmap. Each segment adds about 30 binary-search steps per literal. | Alternation 2.7× and code 3.9× faster than threaded trigrams warm (2.3–3× cold). Alternation candidates equal the oracle's documents. Rare literals 60× threaded trigrams warm, but 6× slower than word 4+5 cold. Identifier 1.2× slower warm; broad unchanged. |
| B. FM-index, s=64 | 0.92× corpus: 3.6 GB, 2.3× word 4+5. The same rebuild as A plus 37 s per GB for the transform. Locate is 8.4 µs per occurrence. | Rare literals 0.95 ms warm and 90 ms cold (lookup only). Any literal's count in about 1 ms warm. Seconds per query on every other class: no help there. |
| C. Neither: threaded trigrams, plus word 4+5 or the term screen for rare literals (D76 A/B) | Rare literals stay at 50 ms warm without a second screen. | No new index lifecycle. Word 4+5 already beats both arms on rare literals cold, at 8% of A's bytes. |

Case, all options: the index is ASCII-folded.
- Case-insensitive queries need nothing more, because `required_literals`
  emits Unicode case variants as OR alternatives.
- Case-sensitive queries use the folded screen, and verification filters.
  The extra candidates were not measured.
- An exact-case SA would double A.

**Recommendation: C.** A's best wins are on alternation and code: 90 and
267 ms saved warm, about 1.2 s cold. They cost 70× the trigram index and a
rebuild at every merge. Its rare-literal win is matched cold by word 4+5 at
a twelfth of the bytes. B pays nearly a corpus of bytes to answer only rare
literals and counts. The hypothesis held for alternation and code, and
failed on three points:
- Cold rare literals: the SA is slower than word 4+5 (58 against 9.9 ms).
- Identifiers: the literal is selective, but the regex is not. Candidates
  are 55× the oracle's bytes, and combining 2 M positions costs 51 ms.
- Literals of 1–2 bytes never reach a screen.

Line-level verification is sound for all 250 queries and independent of the
index. It needs positions, which trigrams do not keep.

**Fact that would change it:**
- Agent traces showing alternation or code-class regexes dominate grep time,
  with 5× corpus on disk acceptable: then A.
- Literal occurrence counts becoming a product feature (a frequency facet,
  say; `grep -c` counts matching lines, which the FM cannot give without
  locating): then B.

## D77 — Default verification threads: every core, or at most 16

**Status: answered 2026-10-15, C, stored. Dave took the recommendation after
asking that the count be kept in the index's configuration, so it is detected
once and can be adjusted over time. B is built and merged (0849ec4); C is
built (0c23072).** The count lives in `settings` in the index directory, a
versioned `key = value` text file (`version = 1`, `search_threads = N`) that
`ferret index` writes when it creates the catalog and never overwrites, so it
can be edited by hand; `crates/ferret/src/settings.rs` documents it. An
index made before C has no file and keeps B's rule until one is added; the
daemon cannot create an index, so it writes none. Physical cores are counted from `/sys/devices/system/cpu/*/topology`
when the index is created and stored with it. Each search takes the smaller of
that count and `available_parallelism`, so affinity and cgroup limits still
apply, and `FERRET_SEARCH_THREADS` still overrides it. If the count can't be
read, the default is `min(16, available_parallelism)`.
Measured in [SCAN.md § Threads](SCAN.md#threads) on the D65C corpus (3.98 GB,
112,376 readable documents), a pure scan through the product path, 5 queries
per class and case, after the scan work (steps 1–3). Median ms over the five
classes, as the range of class medians; 16 cores, 32 threads.

| Threads | Warm ci | Warm cs | Warm CPU s | Cold ci | Cold cs |
| ---: | ---: | ---: | ---: | ---: | ---: |
| 12 | 178–265 | 162–219 | 1.8–2.1 | 3,825–4,136 | 3,813–4,174 |
| 16 | 162–216 | 155–173 | 2.1–2.4 | 3,544–3,822 | 3,507–3,871 |
| 24 | 164–222 | 167–179 | 2.9–3.3 | | |
| 32 | 179–217 | 174–187 | 3.5–4.0 | 3,194–3,654 | 3,135–3,749 |
| `rg` (12) | 158–175 | 144–156 | 1.4–1.8 | 5,137–5,332 | 5,174–5,247 |

**Question:** Should `ferret search` verify content on every available core,
or on at most 16?

| Option | Costs | Buys |
| --- | --- | --- |
| A. Every core (32 here), the default before | 1.3–1.6 s more CPU per warm scan than B (about 60%), for 10–20 ms more wall on most classes. | The fastest cold scan: 2–12% faster than B (70–380 ms), most on alternations. |
| B. `min(16, available_parallelism)` (built) | Cold 2–12% slower than A; still 1.35–1.5× faster than `rg`. On a machine with more than 16 physical cores, leaves cores idle. | The fastest warm scan on every class and case, at 60% of A's CPU, which matters on a laptop running other work. |
| C. Physical cores, read from `/sys/devices/system/cpu` topology | A sysfs read and its fallback on every `search` process; more code to test. | B's result here, and the right count on machines whose physical core count is not 16. |

**Recommendation: B.** The warm scan is the repeated case for an agent
working in one tree, and B is fastest there at the least CPU. A wins only
cold, by at most 0.38 s of a 3.5 s scan that already beats `rg` by 1.4 s.

**Fact that would change it:** if agents' full scans mostly run cold, A,
since its cold gain (70–380 ms) exceeds B's warm gain (10–20 ms) in
absolute terms. If a machine with more than 16 physical cores is a target,
C.

## D78 — Should a one-shot `ferret search` build resident names?

**Status: answered 2026-10-15, A. Dave took the recommendation; nothing to
build.**
Measured in [SCAN.md § Remaining gap](SCAN.md#remaining-gap): a no-daemon
`ferret search --scan` over the D65C catalog (112,377 name rows) spent 26 ms in
`Engine::open` building the resident names (D54 B's interned table and postings)
before it read a document. The build now runs on up to 8 threads, with the other
sections loading alongside, and costs about 9 ms. The whole query now takes
136–155 ms warm against `rg`'s 147–180, so this no longer decides the comparison
with `rg`. These are the benchmark's `FERRET_NO_DAEMON=1` runs. By default a
`search` that finds no daemon starts one and answers that query in-process
(`daemon/client.rs`); the daemon then pays the build once, so a one-shot query
pays it only before the daemon is ready, or where daemons are disabled.

**Measured at 10M rows (2026-10-15):** building the resident names for the
synthetic 10M catalog (D40; 10,448,739 name rows) took 300 ms on up to 8
threads, with a process peak of 841 MB, after the intern table's initial
reservation was capped at 64K entries (it had reserved half the rows: 325 ms,
896 MB). One warm run of an uncommitted probe, not the bench CLI. The fixture's
limit: it replicates one real tree's dump, so it has only 133,385 distinct
names. A real 10M tree has more, and its table hashes and stores more, so 300 ms
is nearer a lower bound than a typical cost.

**Question:** Should a one-shot search keep building the resident names on open,
build them only when a query needs them, or load them prebuilt?

| Option                                                     | Costs                                                                                                                                               | Buys                                                                                                                                            |
| ---------------------------------------------------------- | --------------------------------------------------------------------------------------------------------------------------------------------------- | ----------------------------------------------------------------------------------------------------------------------------------------------- |
| A. Build on every open (today)                             | 9 ms per one-shot query here; 300 ms measured at 10M rows, on a fixture with few distinct names.                                                    | Nothing new to build or test; one catalog state per engine.                                                                                     |
| B. Build lazily, on the first query that needs name search | An engine holding the catalog in two states, and a test that every name path forces the build. Saves nothing for queries that need names.           | Up to 7 ms here off every one-shot content query that needs no name search, if such queries exist (unverified: S1 reads names for every query). |
| C. Write the interned table in the checkpoint and load it  | A storage format change, more checkpoint writing and index bytes; loading still costs a read and validation, about 2 ms for the name sections here. | Most of the build off every one-shot query, names or not: about 7 ms here, and most of the 300 ms measured at 10M rows.                         |

**Recommendation: A.** The 10M build, 300 ms, crosses the threshold this brief
first set ("hundreds of milliseconds"), but that threshold assumed every
one-shot `search` builds. By default only the queries before a daemon is ready
do, and the daemon builds once per catalog generation. At this catalog's size
the build is 9 ms of a query that already beats `rg`.

**Fact that would change it:** agents running `search` where no daemon can run
(`FERRET_NO_DAEMON=1`, or a sandbox where the client cannot start one) on
catalogs near 10M rows make C worth its format change, since each query would
pay 300 ms or more. So would a real 10M tree whose build exceeds about 1 s, with
daemons or not, since the first query after each start pays it.

## D79 — Empty absolute anchors in line grep output

**Status: answered 2026-10-19 by D98.** (Was: open, 2026-10-17; provisional.)

**Question:** should line-oriented `-o`/`-co` retain the per-line span rule in
GREP.md, or reproduce rg 15.2.0’s cross-line empty-absolute-anchor artefacts?
Owned binary probes (no oracle source): on `red\nred\nred\n`, `rg -o '\A'`
prints one empty record and then two `red` records; `-o '\A|\z'` emits four
empty records, while the contract’s per-line spans give six. On
`ox\ncedar\nx\n`, `-no '\A'` emits `1:`, `2:`, `3:x`. Ordinary nonempty
absolute-anchor matches and `^|$` do follow the expected per-line rule.

| Option | Tradeoff |
| --- | --- |
| A. Retain per-line spans | Keeps one iterator and the written contract; documented rg differences for these empty-absolute-anchor outputs. |
| B. Reproduce rg artefacts | Closer output on those probes; depends on cross-line state that is not described by the matcher’s spans, with unclear bounds and buffer sensitivity. |
| C. Refuse affected profiles | Avoids output disagreement but rejects valid regular syntax that M2 promises, including patterns outside the observed cases if detection is conservative. |

**Recommendation: A.** Keep anchors’ assertion context and byte-wise empty
advancement consistent in the shared iterator; do not infer a new span rule
from erroneous-looking cross-line output. M2 records this as provisional,
not an approved blanket compatibility waiver.

**Fact that would change it:** a documented, stable rg rule that cheaply
explains these outputs, or significant affected commands in the 3,332 audit.
Then B, with owned positive/negative cases for that rule.

## D80 — Multiline inversion with a limit

**Status: answered 2026-10-19 by D98.** (Was: open, 2026-10-17; provisional.)

**Question:** should `-U -v -m` reproduce rg 15.2.0's positive-search limit and
its gaps, or keep GREP.md's nonmatching-line selection rule? Owned probes (no
oracle source): on `foo foo\nbar\nfoo\n`, `rg -U -vn -m1 'foo(?:\nbar)?'`
prints `2:bar` and `3:foo`; unlimited prints only `2:bar`. On
`x\na\ny\na\nz\n`, `-U -vnc -m1 'a|\n'` prints 6 although its plain output
has four lines.

| Option | Tradeoff |
| --- | --- |
| A. Selected nonmatching lines | Exclude every line a multiline match intersects, then limit selected lines. Keeps the written inversion and count contract; differs from rg's multiline path on overlapping line groups and limits. |
| B. Reproduce rg's search path, keep selected-line counts | Limit positive searches, skip each matched line group and emit the remainder as a gap when the limit is exhausted. Agrees with the oracle's output, but can print lines that match the pattern. `-c` still counts selected lines (4 on the probe, where rg prints 6), so one owned count exception remains. |

**Recommendation: B** for the LF-including multiline profile; LF-excluding
execution keeps ordinary selected-line limits. The profile already separates
search, clipping and re-matching, so gap selection adds little machinery.

**Fact that would change it:** preferring predictable inversion over the rg
artefact. Then A.

## D81 — Multiline search assertions versus rendering assertions

**Status: answered 2026-10-19 by D98.** (Was: open, 2026-10-17; provisional.)

**Question:** should multiline search keep the original haystack at every
resume, or follow rg's suffix-based search while counting and rendering against
the clipped region? Owned probes: on `abc\nabc\n`, `rg -U -n '\A'` prints both
lines while `-U -c '\A'` prints 1 and `-U -o '\A'` prints nothing. A terminal
`\z` on unterminated `abc` can render the whole line even under `-o`; a
terminal match after LF can print before-context while exiting 1.

| Option | Tradeoff |
| --- | --- |
| A. One original haystack | Smaller API and intuitive absolute assertions (`\A` selects only the first line). Differs from rg's plain multiline rendering and trailing context. |
| B. Suffix search, bounded re-match | An explicit search-region iterator mode in the shared verifier; counts and spans keep their own bounds and assertion context. Slightly more machinery; matches rg without a CLI regex executor or pattern-text special cases. |

**Recommendation: B.** D79 (line-mode empty absolute anchors) stays separate.

**Fact that would change it:** a requirement for uniform whole-document
assertion truth across search and presentation, or a new pinned rg that removes
the artefact. Then A.

## D82 — Clipped multiline zero counts

**Status: answered 2026-10-19 by D98.** (Was: open, 2026-10-17; provisional.)

**Question:** when `-m` clips a multiline match so that bounded re-matching
finds nothing, should the count re-match within its bound or count the
discovered match? Owned probe: on `a\nb\n`, `rg -Ucm1 'a\n\b'` prints 1; the
boundary after `a\n` needs the `b`, which lies beyond the clipped prefix, so
M7's bounded re-match finds 0.

| Option | Tradeoff |
| --- | --- |
| A. Keep bounded re-matching | Prints nothing without `--include-zero` and 0 with it, exit 0. Keeps D81 B's separation of search and count bounds; differs from rg on this boundary case. |
| B. Count the discovered match | Reproduces rg's 1, but changes the count bounds and can disconnect counts from the separately bounded printable spans. |

**Recommendation: A.** Follows from D81; if D81 goes to A, revisit this.

**Fact that would change it:** a requirement to reproduce rg's count here.
Then B.

## D83 — Replacement presentation and terminal empty matches

**Status: answered 2026-10-19 by D98.** (Was: open, 2026-10-17; provisional.)

Question: should replacement use every ordinary line span, or reproduce pinned
rg 15.2.0's replacement-specific terminal bounds and whole-line fallback?

- **A — reuse every ordinary span.** Smallest implementation and uniform
  iteration, but adds a replacement at unterminated EOF that rg omits. It
  also replaces the line selected solely by a first terminal assertion.
- **B — retain discovery/counts, bound replacement presentation.** Keep the
  shared iterator and original capture haystack, but omit terminal EOF
  replacement on an unterminated line. A line selected solely by that terminal
  assertion retains its unchanged whole-line fallback, including under -o.
  Requires retaining source termination through line rendering, but agrees
  with the oracle and preserves M7's search/publication distinction.

Recommendation: B, provisional. On abc without LF, -or X '' emits three X
records, whereas abc-LF emits four. On unterminated abc, -or X '\z' emits abc;
with LF it emits X. Counts still describe original matches. Requiring uniform
span and replacement presentation, or a new oracle removing this artifact,
would change the recommendation to A.

## D84 — Multiline replacement records

**Status: answered 2026-10-19 by D98.** (Was: open, 2026-10-17; provisional.)

Question: should multiline replacement preserve one output record per source
line, or reproduce rg's transformed match-group records?

- **A — preserve source lines.** Smaller record model, but cannot remove an
  LF captured across lines or add correctly prefixed replacement lines.
- **B — expand bounded capture spans before projecting output lines.** Render
  each selected source group, split the result for LF-including profiles and
  retain original source end separately for context boundaries. LF-excluding
  profiles retain line-mode replacement bytes (literal replacement LF does
  not acquire another prefix). This agrees with pinned rg while adding source
  boundary metadata to cached records.

Recommendation: B, provisional. On a/b/c/d with -UnC1 -r X 'b\nc', rg emits
1-a, 2:X, 4-d without a group separator. Replacing with X-LF-Y-LF-Z emits
2:X, 3:Y, 4:Z and still 4-d. A requirement for stable source-line records, or
an oracle changing this projection, would change the recommendation to A.

## D85 — Replacement captures after multiline clipping

**Status: answered 2026-10-19 by D98.** (Was: open, 2026-10-17; provisional.)

Question: should replacement preserve M7's bounded re-match contract, or adopt
rg's replacement-specific unbounded re-match and suffix publication after -m?

- **A — bounded replacement captures.** Use the same clipped match spans and
  assertion bounds as existing output. This keeps GREP.md's explicit M7 limit
  example and prevents a replacement from publishing an otherwise unselected
  suffix, but differs from rg's replacement fast path for an optional LF branch.
- **B — reproduce rg's replacement suffix.** Re-match the source beyond the
  clipped group, potentially capture across its end, and in plain output copy
  later unmatched lines into that positive group before emitting the requested
  context again. This matches rg but weakens the existing clipping contract
  and requires independently tracking replacement's expanded publication bounds.

Recommendation: A, provisional. The owned input red-red/LF/blue/LF/gap/LF/red/
LF/blue/LF with -Unm1 -A2 -r '<$0>' 'red(?:\nblue)?' makes rg capture the
second red plus blue, then copy the entire remaining suffix into the match
record, followed by blue/gap again as context. Ferret retains two bounded red
expansions and the requested blue/gap context. Under -o rg still captures the
cross-boundary blue, without the unmatched suffix. A requirement that replacement
must override the accepted M7 clipping rule, or a pinned oracle whose bounded
behavior no longer conflicts, would change the recommendation to B.

This is an explicit owned spec/oracle conflict, not a D74 audit waiver. No
private corpus row has been licensed by this decision.

M8-C also applies to independently discovered trailing context: recheck a
cross-line tail within each source line. Mandatory LF patterns retain whole
context (matching rg); optional LF patterns retain a bounded local capture
instead of expanding a cross-line capture and publishing the suffix again.
The owned tail regression records both the agreement and the rg discrepancy.

## D86 — Inherited absolute-anchor presentation

**Status: answered 2026-10-19 by D98.** (Was: open, 2026-10-17; provisional.)

Question: should replacement inherit D79's per-line absolute-anchor spans or
reproduce rg's cross-line empty-anchor presentation state?

- **A — inherit per-line spans.** Preserves GREP.md's line-mode assertion
  scope and the existing M2/D79 recommendation, with no alternate executor.
  It differs from rg when an empty absolute-start capture is rendered across
  several lines, including ordinary replacement output.
- **B — reproduce cross-line presentation state.** Matches rg's first-only
  replacement, with unchanged whole-line fallback on later selected lines,
  but introduces a second assertion/presentation boundary in line mode.

Recommendation: A, provisional, extending the already-open D79 choice to
replacement. On three red-LF lines, -nr '<$0:$1>' '()\A' emits <:>red on every
line in ferret, versus only the first in rg. Under -nor, ferret emits three
<:> records; rg emits <:>, red, red. An answer to D79 choosing oracle artifacts
or a requirement to give replacement different assertion truth would change
the recommendation to B. Owned tests record both outputs; no output code was
changed by this final coverage addition, and no private row is waived.

## D87 — Clipped multiline JSON submatches

**Status: answered 2026-10-19 by D98.** (Was: open, 2026-10-17; provisional.)

Question: should JSON re-match beyond a clipped selected region to reproduce
rg 15.2.0, or retain M7/M8's bounded publication contract?

- **A — unbounded oracle re-match.** Reproduces rg's cross-boundary captures
  where it succeeds, but can attach spans beyond the event's source bytes and
  panics on ordinary optional-LF patterns after -m. This conflicts with the
  existing bounded capture/publication contract.
- **B — bounded event submatches.** Retain the existing scanner's clipped
  assertion/span bounds. Recheck a split trailing occurrence within each
  original source line, retaining its LF and only complete regex matches.
  Lines with no local match become JSON context, even when plain discovery
  keeps match formatting. Valid byte bounds alone cannot license a submatch.
  This avoids publishing an unselected suffix, but can omit a discovered
  boundary-sensitive submatch that rg reports after initial-region truncation.

Recommendation: B, provisional, extending D82/M8-C. On red-red/LF/blue/LF with
-U -m1 'red(?:\nblue)?', pinned rg --json panics at an out-of-range slice. On
a/LF/b/LF with -Um1 'a\n\b', rg reports the truncated a-LF span while bounded
re-matching has no span. Preserve the selected event and search status, with
zero submatches for the latter, consistently with the existing count bounds.
A requirement for JSON-specific unbounded assertion truth, or a pinned oracle
with safe clipping consistent with the existing contract, would change this
recommendation. These owned conflicts are not private-audit waivers.

Fix round 1 changes D87's tail rule explicitly: blanket span intersection was
incorrect, not an intended compatibility choice. On a/LF/b/LF/a/LF/b/LF with
--json -Um1 -A2 'a\nb', rechecking yields one a-LF-b-LF match event and two
context events, matches 1 / matched_lines 2, agreeing with pinned rg 15.2.0.
For 'a(?:\nb)?', the trailing a line retains only the complete local a match;
b becomes context. That is matches 2 / matched_lines 3; rg panics after its
initial event. Initial clipped-region assertion bounds are unchanged.

## D88 — rg 15.2 replacement metadata versus the written contract

**Status: answered 2026-10-19 by D98.** (Was: open, 2026-10-17; provisional.)

Question: should JSON ignore replacement as GREP.md explicitly requires, or
add rg 15.2.0's replacement metadata to each submatch?

- **A — honor the explicit ignore rule.** Keep source lines and submatches
  identical to JSON without -r. Small, independent publication surface, but
  differs from rg 15.2.0's optional `replacement` field.
- **B — add replacement metadata.** Matches the pinned schema extension and
  helps consumers preview transformations, but changes the explicit contract
  and requires original capture expansion at every JSON submatch boundary,
  including provisional clipping/assertion bounds.

Recommendation: A, provisional, because the current slice explicitly says to
ignore replacement. On red-LF with --json -or X red, rg retains lines red-LF
and match red but adds replacement {text:X}. Ferret emits the same event as
without -r. An instruction to adopt replacement metadata, or an updated
contract that distinguishes metadata from transformed source, would change
this to B. The better future contract is original source plus optional
replacement metadata, with capture bounds stated explicitly. No private
corpus row is waived by this owned discrepancy.

## D89 — Inherited line assertion presentation in JSON

**Status: answered 2026-10-19 by D98.** (Was: open, 2026-10-17; provisional.)

Question: should JSON use the specified per-line assertion scope, or reproduce
rg's stateful absolute-assertion spans and trailing-context classification?

- **A — retain line-scope selection and spans.** Extends D79's existing choice
  to source submatches and keeps JSON on the existing selection scanner. It
  differs from rg's later empty absolute-anchor submatches and limit tails.
- **B — reproduce stateful presentation.** Matches rg's first-only absolute
  start submatch and its context classification after -m, but requires another
  assertion/publication state alongside the established per-line contract.

Recommendation: A, provisional. On abc-LF/abc-LF with --json '\A', ferret
reports one empty submatch on each selected source line; rg reports it only
on the first. On a-LF/bc with --json -m1 -A1 '\z', ferret classifies bc as a
match without printable submatches; rg classifies it as context. This extends
the documented D79/line-scope choice rather than silently adopting a new scope.
A decision that JSON must have its own stateful assertion contract, or a D79
answer replacing per-line scope, would change the recommendation to B.

## D90 — Inherited inverted limit/context windows

**Status: answered 2026-10-19 by D98.** (Was: open, 2026-10-17; provisional.)

Question: should JSON reuse the selected-line and clipped-window contract, or
reproduce rg's additional inverted tail search and terminal suffix artifacts?

- **A — retain established selection and windows.** Keeps the written -m and
  -A rules and the existing scanner for all publication modes. It can differ
  from rg's inverted after-context path and suffix rebasing at terminal LF.
- **B — reproduce the extra tail search.** Matches rg's extra selected lines
  and their shifted context boundary, but changes existing selection/window
  behavior and adds separate JSON selection state or a broader plain fix.

Recommendation: A, provisional. On x-LF/a-LF/y-LF/a-LF/z-LF with -Uvm1 -A1 a,
ferret publishes match x and context a; rg publishes matches x, a and y. The
same plain discrepancy exists without JSON and without -U. On the same bytes
with -Um1 -A1 'a|\z', ferret publishes match a and context y; rg labels y as a
match after suffix-based terminal discovery. This is separate from bounded
submatch counting: all JSON counts still describe the emitted events. A
requirement to prioritize these oracle tail artifacts over the explicit limit/
window contract would change the recommendation to B and needs a focused
selection fix plus regressions for the existing plain path.

## D91 — Quiet JSON summary counting bounds

**Status: answered 2026-10-19 by D98.** (Was: open, 2026-10-17; provisional.)

Question: should quiet summary reuse ordinary event submatch counts, or follow
rg's separate counting iterator and inverted logical-line floor?

- **A — reuse event counts.** Smallest and uniform across -q, but reports zero
  matches for a first terminal assertion that rg's quiet summary counts, and
  undercounts inverted source regions with no printed regex occurrences.
- **B — retain separate quiet counting bounds.** Re-count each selected source
  group with the shared byte matcher. Keep a first terminal assertion, omit
  advanced EOF empties, and retain at least one logical match per inverted
  selected source line. Adds quiet-only iteration, but matches pinned summaries
  without changing selection, plain counts or ordinary JSON submatches.

Recommendation: B, provisional. On unterminated abc, --json '\z' publishes a
selected source with no submatches, while adding -q reports matches 1. On
x/a/y/a/z with -Uqv -m1 'a|\n', quiet stats are matched_lines 4 / matches 6;
with -Uqv '\z' over the same LF-terminated bytes they are 5 / 5. The owned
quiet matrix and cross-line gap case pin the first-terminal, advanced-EOF and
line-floor distinctions. A requirement that -q preserve event-span counts
exactly, or a pinned oracle removing this separate counting path, would change
the recommendation to A.

## D92 — Grep output order across files

**Status: answered, 2026-10-18.**

Question: when several threads search, should `ferret grep` publish each
file's output as soon as that file finishes, or keep today's fixed order?

Measured at 32 threads on indexed `-F -a the` over the 4 GB corpus, which
produces 2.3 GB of output from 56k files (`wt/grep-perf1` `4bc4eb3`):

| Destination | rg | ferret | ferret, never spilling |
| --- | ---: | ---: | ---: |
| `/dev/null` | 0.22 s | 0.86 s | 0.56 s |
| file on tmpfs | 0.95 s | 1.39 s | 1.14 s |
| pipe to `cat` | 0.70 s | 1.12 s | 0.91 s |

Peak memory: rg 263 MiB, ferret 254 MiB, the never-spilling build 622 MiB.
Ordered publication holds finished groups until every earlier group is
published, and each group over 64 KiB spills to a temp file and is read back.
That costs about 0.28 s here, plus about 0.12 s for one thread copying every
byte to stdout.

- **A — a shared in-memory budget (256 MiB) for finished, unpublished
  groups.** Keeps the fixed order; spills only past the budget. Peak memory
  rises by up to the budget on dense queries.
- **B — publish each file as soon as it finishes, as rg does.** Few groups
  are held, so little spills and no single thread copies all output. Output
  order across files varies between runs with more than one thread; groups
  still never interleave, and one thread keeps walk order.
- **C — keep 64 KiB per group.** No memory change; keeps the 0.28 s.

Recommendation was A, for the fixed order at a bounded memory cost.

> Dave: B, matching rg. A flag for sort order can come later.

**Answer: B.** Multi-threaded grep publishes groups in completion order.
GREP.md already promised only that files finish in any order and their
groups do not interleave. A sort-order flag (rg's `--sort path`, refused
today) is a later feature.

## D93 — Grep capture memory at high thread counts

**Status: answered, 2026-10-18.**

Question: how much memory may `ferret grep` use for captured output at 32
threads?

perf4 (`wt/grep-perf1` `d781f16`) held peak RSS under 263 MiB, a figure taken
from rg at its default 12 threads. At 32 threads rg peaks at 500–615 MiB on
indexed `-F -a the` (2.3 GB output), so the ceiling left ferret about 9 MiB of
headroom and a 256 KiB per-capture spill limit.

- **A — stay near 263 MiB.** About half rg's peak; ferret stays 2.5× behind on
  `/dev/null` and 27% on a tmpfs file.
- **B — up to rg's peak at the same thread count, about 500 MiB.** A 1 MiB
  capture limit measured 0.56–0.60 s on `/dev/null` (against 0.75 s).
- **C — no ceiling.** Never spilling: 0.56 s at 622 MiB, unbounded on larger
  outputs.

Recommendation: B.

> Dave: Yes, option 2.

**Answer: B.** Peak RSS at 32 threads may reach rg's at the same thread count,
with the capture bound stated as a formula of threads and limit.

## D94 — Grep on a broken pipe

**Status: answered, 2026-10-18.**

`ferret grep x | head` printed `writing stdout: Broken pipe` and exited 2; rg
exits 0 silently. Nothing had decided ferret's behaviour; M9 inherited it.

> Dave: make ferret match rg.

**Answer:** a broken pipe on stdout ends `ferret grep` silently with exit 0.

## D95 — Grep output: one writer, or workers writing stdout

**Status: answered, 2026-10-18.**

Question: should `ferret grep` close its last gap to rg (dense output to
`/dev/null` at 32 threads, about 358 ms against 285 ms) by letting workers
write stdout themselves?

Today workers render each file's group and pass it through a bounded queue;
one thread writes every group (D92 B keeps completion order either way). That
thread spends about 3.8 µs per group, so on `/dev/null`, where a write costs
nothing, it is the bottleneck. On a pipe or a file the kernel's copy of the
output dominates and the per-group cost overlaps it.

- **A — keep the single writer.** Ahead of rg on a pipe (755 against 845 ms)
  and a tmpfs file (999 against 1032 ms); about 70 ms behind on `/dev/null`.
  Cut its per-group cost instead (batched handoffs).
- **B — workers write directly.** perf4 measured it: `/dev/null` 0.74 → 0.69 s,
  but pipe 1.09 → 1.21 s, because a worker blocked on a full pipe stops
  searching.
- **C — hybrid.** A worker writes its own group when stdout is free and nothing
  is queued, else enqueues. Unmeasured; a second publication path.

Recommendation: A. `/dev/null` measures handoff overhead, not a destination
users have.

> Dave: Ok, then I'm happy with option 1.

**Answer: A.** One writer thread; reduce its per-group cost.

## D96 — Grep's producer thread: on top of T, or counted in it

**Status: answered, 2026-10-19.**

Question: P2 (`wt/grep-p2`) moves grep's file producer onto its own thread,
which feeds a bounded queue of at most 4 tasks per worker, so no worker calls
the producer or holds a task while it blocks. Should that thread count against
the T search threads (`-j`, `FERRET_SEARCH_THREADS`, the index `settings`
count)?

- **1 — T workers plus the producer.** Workers keep all their capacity. At
  T = 32 on a 32-thread machine one core is oversubscribed only while all 33
  threads are runnable; the serial producer's work was about 0.08 s of a 4 GB
  search after P1.
- **2 — T − 1 workers plus the producer.** The thread count is exactly as
  configured, at the cost of a worker, which matters most at small T: `-j2`
  searches with one worker.

Recommendation: 1. The fact that would change it: P2's measurements showing
slower 1-thread runs or more context switches.

> Dave: Agree, but we should consider a thread count where we do T-1 if the
> core that the producer thread is running on is close to saturated by the
> producer thread.

**Answer: 1, with a refinement to measure.** Where the producer itself nearly
saturates a core (a live walk over many small files is the likely case), use
T − 1 workers. P2's measurements record the producer thread's CPU share per
corpus shape to find where that happens.

**Measured 2026-10-20** (P2 + P5 and the D98 stack, kernel 6.18.56, turbo off):
the producer thread peaks at about 66% of one core, on a deep tree of small
files at 8 threads, and uses 14–46% on the flat 4 GB corpus. It never nears
saturation, so T − 1 is not needed for any shape measured. A live (`-u`) walk
was not measured. Separately, 32 workers run 13–26% slower than 8 on every
case; that extra CPU is in the workers, not the producer. The query matrix
will establish where the thread-count knee is.

## D97 — When GNU grep and rg disagree

**Status: answered, 2026-10-19 (a principle, not a brief).**

> Dave: when GNU grep and rg disagree, we should take the simplest, fastest
> solution. In such a case, if there is an approach that is "more correct" but
> matches neither output and is simpler than both, we take that too.

**Answer:** where the two oracles disagree, ferret's behaviour is chosen for
simplicity and speed rather than for matching either one. A third behaviour
that is more correct and simpler than both is allowed. D79–D91 are being
re-examined under this rule.

Scope: the rule also covers features only rg has (`-U`, `--json`, `-r`
replacement). Where rg's output there looks like an artefact rather than a
design, ferret takes the simplest correct behaviour instead of copying it.

> Dave: Agree, option A. We could even open a PR in ripgrep if we are
> confident it is an error. The reds example appears to be that to me.

Because of the clean room, ferret's side reports a confirmed rg error upstream
as an issue with a minimal repro, not as a source patch.

## D98 — D79–D91 under D97: four cluster answers

**Status: answered, 2026-10-19: every recommendation taken (D79 D, D83 A,
D86 A; D81 C, D80 A, D91 A, D82/D85/D87 moot; lines after `-m` are context,
D89 A, D90 A; D84 B, D88 A).** Built from a read-only review (private notes;
oracles GNU grep 3.12 and rg 15.2.0, synthetic probes, `LC_ALL=C`). D79–D91
stay as written; this brief proposes revised answers for them in four
clusters, so they can be answered in four lines.

Three findings change the earlier briefs:

1. **rg disagrees with itself.** Some outputs depend on its reader. On
   `x\na\ny\na\nz\n`, `rg -vn -m1 -A1 a` prints `1:x 2-a` from a file (mmap, as
   GNU and ferret do) and `1:x 2:a 3:y` from stdin, in plain line mode. On
   `ox\ncedar\nx\n`, `rg -no '\A'` prints `2:cedar` from a file and `2:` from
   stdin; D79's brief quoted the stdin output. The grep test fixtures feed rg
   through stdin, so "matches rg" on the M branches means "matches rg's stream
   reader".
2. **D82, D85, D87, D91 and the tails of D89/D90 exist only because `-U`
   clips its input after `-m`** and re-matches inside the clip. A single pass
   over the original input never clips, so those questions disappear.
3. **On `a\nbc` with `-m1 -A1 '\z'`, GNU and rg both print `2-bc` (context);
   ferret prints `2:bc`.** GNU's rule is that lines after `-m` are context, and
   they are never searched.

Net effect if every recommendation below is taken: about −450 / +50 lines of
production code across `multiline.rs`, `matcher.rs`, `replacement.rs`,
`events.rs` and `line_grep.rs`. Every one removes a special case or changes
nothing, and none adds work to an ordinary line-mode search. The cost is in
tests: about 22 M7–M9 test functions pin rg's stdin output and need owned
expectations, an estimated 400–600 changed test lines, in one slice on the
grep tip.

### Cluster 1: `-o` and empty matches (D79, D83, D86)

**Question:** should `-o` print empty matches at all?

| Option | Costs | Buys |
| --- | --- | --- |
| A. Per-line empty records (D79 A today) | Keeps two `-o` fallback branches. Prints records neither GNU nor rg's file reader agree on. | Closest to rg's stdin output on `\A`. |
| **D. `-o` never prints an empty record** (GNU's rule; a non-empty `-r` expansion still prints) | Differs from rg wherever rg prints an empty record. | About −35 lines; one rule. D83 becomes A (ordinary spans, about −12 more), D86 stays A. |

**Recommendation: D.** GNU and rg disagree, and D is the simpler of the two.
**Would change it:** agents relying on empty `-o` records as markers.

### Cluster 2: the multiline engine (D81, then D80, D82, D85, D87, D91)

**Question:** should `-U` search, count and render from one pass over the
original input, with no clipping after `-m`?

| Option | Costs | Buys |
| --- | --- | --- |
| A. One haystack, re-match inside the clip | Keeps the separate count and render passes and every clip question (D82, D85, D87, D91). | `\A` means the start of the file. |
| B. Suffix search (built) | 3–5 regex passes per `-U` search; `rebased_spans`; every clip question. One rough run put the count re-match alone at about +30%. | Matches rg's stdin output. |
| **C. One pass, whole matches, no clip** | After `-m`, can print one more line than rg: the rest of a match that started on the last selected line. | About −230 / +40 lines and one regex pass. D82, D85, D87 become moot; D80 becomes A; D91 becomes A by construction. |

On `red red\nblue\n`, `-Unm1 'red(?:\nblue)?'` under C prints `1:red red` and
`2:blue`. rg prints only line 1, but its own `-or '<$0>'` already shows
`<red\nblue>`, so C is rg's `-o` view made consistent.

**Recommendation: C.** **Would change it:** a requirement that `-U -m N`
never prints a line past the N-th group's last line; then A.

### Cluster 3: lines after `-m` (D89 tail, D90)

**Question:** are lines printed after the `-m` limit always context?

| Option | Costs | Buys |
| --- | --- | --- |
| **GNU's rule: always context** | Changes ferret's line-mode output on `-m N -A M` where a trailing line would match (it agrees with GNU and rg on the probe; today ferret differs from both). | Removes the tail search: about −70 lines, and no regex run on trailing lines. D89 A for `\A`, D90 A. |
| Today's re-search of the tail | Keeps `tail_selection`, `tail_match` and the `-o` tail licence. | Nothing either oracle does. |

**Recommendation: GNU's rule.** **Would change it:** none known; both
oracles already do this.

### Cluster 4: unchanged

D84 B (expanding a replacement that spans lines is what substitution means)
and D88 A (rg's `replacement` field is a feature, not an artefact; GREP.md says
JSON ignores `-r`).

**Recommendation:** take both as recommended.

> Dave: I agree with all of your D98 recommendations.

## D99 — Grep when no files were searched

**Status: answered, 2026-10-19.**

Question: when `ferret grep` is given no path, searches the current directory,
and its filters leave nothing to search, should it exit 2 with a diagnostic,
as rg 15.2.0 does ("No files were searched, which means ripgrep probably
applied a filter you didn't expect"), or exit 1 silently?

Probed with the pinned binaries: GNU grep 3.12 and rg both exit 1 silently
when an explicit path or an include glob leaves nothing to search; rg exits 2
only in the no-path case above. GNU grep with no path reads stdin, so it has
no conflicting behaviour. The random tests found about 200 differences, all
of this one shape.

- **A — match rg in that one case.** About 10 lines: a count of files
  searched, checked at exit only when no path operand was given; one test. No
  cost on the search path. An agent whose filters exclude everything learns why.
- **B — keep exit 1.** One less rule.

Recommendation: A. Would change it: scripts that run ferret with no path and
expect an empty result to exit 1.

> Dave: Agree, option A

**Answer: A.**

## D100 — Empty matches at the end of an unterminated last line

**Status: answered, 2026-10-20.**

Question: is the end of a last line that has no final LF a position where an
empty match can occur?

Built on `wt/grep-d98` as yes. Probed with rg 15.2.0, `--count-matches`:

| Input   | Pattern | rg | ferret |
| ------- | ------- | -: | -----: |
| `abc`   | `''`    |  3 |      4 |
| `abc`   | `x*`    |  3 |      4 |
| `abc`   | `$`     |  1 |      1 |
| `abc\n` | `''`    |  4 |      4 |

rg already matches `$` at that position, so its 3 for `''` is inconsistent
with its own `$`. GNU grep has no `--count-matches`: its `-c` counts lines, and
`-o` prints no empty matches, which ferret now also does (D79 D).

- **A — yes (as built).** The count is the same with or without a final LF, and
  agrees with where `$` matches. 89 exclusion rows; no special case in code.
- **B — match rg.** A rule that drops the final empty match only when the file
  has no final LF, and only for patterns that are not anchored with `$`.

Recommendation: A, under D97 (simpler, and consistent). Would change it: a
tool that depends on rg's count for unterminated files.

> Dave: Agree with all of your recommendations from D100 up.

**Answer: A.**

## D101 — Multiline `-c` for patterns that can match LF

**Status: answered, 2026-10-20.**

Question: under `-U`, should `-c` keep counting matches for a pattern that can
match LF, as GREP.md documents since M7, or count selected lines?

On `foo foo\nfoo\n` with `foo(?:\nbar)?`: ferret and rg both give 3 for
`-Uc` and 2 for `-Ucm1`. With `-Ucm2` ferret gives 3, because D98 takes whole
line groups; rg gives 2, because it stops after two matches. Patterns that
cannot match LF count lines in both tools.

- **A — keep match counts (as built).** No change; agrees with rg on `-Uc` and
  `-Ucm1`; `-c` equals `--count-matches` for this class.
- **B — always count lines.** `-c` means lines everywhere, as in GNU grep.
  Costs new differences from rg on `-Uc` and `-Ucm1`, more exclusion rows, and
  edits to GREP.md and the M7 spec test.

Recommendation: A. Would change it: wanting `-c` to mean lines everywhere more
than agreement with rg.

> Dave: Agree with all of your recommendations from D100 up.

**Answer: A.**

## D102 — A spill-file fault stops the whole search

**Status: answered, 2026-10-20.**

Question: when grep cannot create or write the temp file a capture over 4 MiB
spills to (D93), should that fail only the file being captured, or the whole
run as now?

Found by the hostile suite and reproduced: with `r/a` holding `hit small`, `r/b`
holding one 6 MiB matching line, `TMPDIR=/nonexistent FERRET_SEARCH_THREADS=1
ferret grep -uu hit r` prints only `writing stdout: create output spill …` and
exits 2. `r/a`'s match is never printed. At `ulimit -n 32` and 32 threads,
1,900 of 4,000 small files were dropped this way without any message naming
them. rg printed every file. GREP.md documents spill faults as fatal, so this
is the design working as written. The message also blames stdout.

- **A — fail the file.** Name the file in a diagnostic, publish nothing for it,
  carry on, exit 2: the same treatment as a read error. About 30 lines; the
  spill fault stops being an output fault. A full disk then fails every large
  file but still prints the small ones.
- **B — keep it fatal, fix the message.** One line. Output is still silently
  lost whenever TMPDIR is full, missing or out of descriptors.

Recommendation: A, and fix the message either way. Would change it: D103 A
removing spilling altogether, which makes this moot.

> Dave: Agree with all of your recommendations from D100 up.

**Answer: A.**

## D103 — Streaming output from one large file

**Status: answered, 2026-10-20.**

Question: should a file whose capture passes 4 MiB stream its output as it is
scanned, instead of publishing only after the closing version check?

Measured by the hostile suite on one 2 GB file, every line matching, 8 threads:
the full run takes 3.28 s and `| head -n1` takes 3.13 s, against rg's 0.01 s.
The first byte reaches the pipe after 2.97 s, and the unlinked spill file grows
to 1,935 MiB. Grep cannot notice the closed pipe until it publishes, and TMPDIR
must hold the file's whole output (or D102 applies).

- **A — stream at the head.** Once a capture passes 4 MiB, the worker waits for
  the output writer, writes what it has, and writes the rest directly as it
  scans. A closed pipe is then seen on the next write. If the closing check
  finds the file changed, what was printed cannot be withdrawn: grep says so
  and exits 2, without a retry. Buys: `| head` in milliseconds, no temp disk,
  and the spill machinery can probably be deleted (D97: simpler). Costs: other
  workers' groups wait while one file streams, up to D93's queue bound, then
  workers stall; "one version per file" weakens to "one version or a
  diagnostic" for files over 4 MiB.
- **B — poll stdout for closure.** The output thread checks fd 1 for a closed
  reader while it waits. Cheap, but it does not help `| head -n1` on one file,
  because nothing is written for head to close on until the scan ends.
- **C — keep as is.**

Recommendation: A. Would change it: a consumer that relies on never seeing a
large file's output from two versions.

> Dave: Agree with all of your recommendations from D100 up.

**Answer: A.**


**Applied 2026-10-20, by the D103 A answer's own rule:** a temp-file fault
after a file has started streaming (a before-context spool that fails on a
later long line, say) keeps the printed prefix, names the file and the fault,
continues with other files and exits 2, without a retry. Before the first
published byte, D102 A applies and nothing for the file is printed. For
`--json`, the begun group is closed and its statistics cover only what was
observed. Raised by Sol while building the slice; not a new question.

## D104 — Files that grow during the search

**Status: answered, 2026-10-20.**

Question: when a file's only change during the read is growth, should grep
search the bytes up to its first observed size instead of exiting 2?

GREP.md says "retry instability once, then continue with exit 2", and the
version compares `(dev, ino, size, mtime, ctime)`, so any append counts.
Measured, 50 runs per cell: a 64 MiB log appended at 100 lines/s exits 2 in 50
of 50, at 1 and at 32 threads, and prints nothing for that file; a 4 MiB log at
100 lines/s fails 4–40 of 50; 1 line/s never fails. rg exits 0 every time.

With a real appender, 1,347 of 2,000 checked reads saw a change; all were pure
growth (same inode, larger size), and the bytes up to the first size never
differed. A constructed truncate-and-regrow (logrotate's copytruncate) can give
the same signature with mixed bytes, but only if the reader is descheduled
mid-read; a randomised run saw 428 growth signatures and no mixed bytes.

- **A — keep exit 2.**
- **B — accept growth.** Read only up to the first observed size; accept the
  read when device and inode match and the new size is no smaller. About 20
  lines in the checked read. Costs: a copytruncate that lands mid-read can
  print a mix of old and new bytes, as rg can.
- **C — B, but only after the one retry fails.** Same risk, slightly rarer,
  and one more read of every growing file.

Recommendation: B. Would change it: logs rotated by copytruncate being common
in the trees ferret searches.

> Dave: Agree with all of your recommendations from D100 up.

**Answer: B.**

## D105 — A multi-threaded live walk for grep

**Status: answered, 2026-10-20.**

Question: should `ferret grep -uu` (the live walk, no index) walk on several
threads, as `ferret find` already does?

Measured on a deep tree of ~300k small files: one producer thread does the
whole walk, using 1.12 s CPU, which is the whole wall time. More workers do not
help: ferret stays at ~1 s from 2 threads up, about 3.5–4× rg's 0.26 s.
`ferret find -I` makes the same walk (the same ~490k syscalls) in 0.07–0.09 s
on ~15 threads.

- **A — keep the serial producer.** No code. deep-uu stays ~0.9 s; src-uu keeps
  a ~0.2 s floor.
- **B — several producers, each owning a split of the walk**, reusing find's
  `LiveWalk::split` and its donation queue, all feeding the existing queue.
  About 300–400 lines with tests; one thread keeps the serial path, and
  `--live` stays serial at first. Expected 0.15–0.3 s (estimated from find's
  walk, not measured). Output order is already completion order, so nothing
  visible changes.
- **C — workers walk their own subtrees** (rg's model, no producer). The
  largest rewrite. Gives directory locality for free, but gives up P2's rule
  that any idle worker takes any task as soon as it exists.

Recommendation: B. Would change it: `--live` (policy mode) being the main use;
then split the policy stack in the same slice, about a third more code.

> Dave: Ok, I think we want both of those (i.e. --live too) to be enhanced.
> Please try to do it orthogonally. i.e. abstract the complicated part of the
> walk and make it reusable.

**Answer: parallel walks for both `-uu` and `--live`, built orthogonally.** The
complicated part (find's sibling donation, suspended donors, the active count
and cancellation in `find/parallel.rs`) becomes a reusable walk scheduler,
generic over what each worker does with an entry. find evaluates its
expression; grep runs the checked read and search and hands the group to the
output thread. `--live` gives each split its own copy of the D13 policy state.
Neither B nor C as written: the scheduler is shared, not duplicated.

## D106 — Keeping a directory's files on one worker (index mode)

**Status: answered, 2026-10-20.**

Question: should a worker take a run of consecutive small files at once, so
that one directory's files are read by one worker?

Measured on the deep tree with its index, at 32 threads: 0.70 s wall and
8.7–12.5 s of kernel time, against rg's 0.26 s. Neighbouring files go to
different workers, and each re-opens the same directories for the checked-path
rule (D21): 514k directory opens at 32 threads against 84k at 1. This is most
of why 32 threads runs slower than 8 (see D96's measurement note). Taking runs
of 16 tasks cuts kernel time to 2–4 s; with D107 B the wall falls to ~0.50 s.

- **A — batched take.** A worker takes up to 16 consecutive tasks whose sizes
  sum to at most 64 KiB. About 40 lines. Cost: up to 15 small files (64 KiB in
  all) can wait behind one slow file, as in rg by design; the test
  `completed_group_publishes_before_a_slow_earlier_file` needs rewriting around
  that bound.
- **B — batched take plus stealing.** An idle worker steals the far end of
  another's run, so no file waits behind a slow one (P2's rule kept). About 150
  lines and a new concurrent structure to review.
- **C — share checked directory descriptors across workers.** Scheduling
  unchanged, but it needs a sharded descriptor map with an LRU against D21's
  1,024-descriptor budget. A quick global-mutex version cut kernel time but
  made the wall worse (1.24 s).

Recommendation: B. Would change it: accepting a bounded hold-back of up to 15
small files behind a slow one; then A, for a quarter of the code.

> Dave: Agree with all of your recommendations from D100 up.

**Answer: B.**

## D107 — Fewer wakeups from the producer

**Status: answered, 2026-10-20.**

Question: may the producer queue several tasks before waking a sleeping worker?

Today it wakes one per task: 374k voluntary context switches on deep-uu, 57–108k
on deep-idx (rg: under 1k at 8 threads).

- **A — as today.**
- **B — wake when 16 tasks are queued**, and wake everyone when the walk ends.
  deep-uu switches 374k → 103k, producer CPU 1.12 → 0.90 s. Alone it does not
  move deep-idx; with D106 it takes deep-idx from 0.70 to ~0.50 s. Cost: up to
  15 tasks wait while the producer finds more, longer if a slow filesystem
  stalls the walk.
- **C — wake only when no worker is awake or the backlog reaches 2.** At most
  one task waits. Smaller savings, not measured.

Recommendation: B together with D106, and only in index mode if D105 B lands,
since there the producer makes no syscalls and cannot stall. Would change it:
streaming latency on slow filesystems mattering more than CPU; then C.

> Dave: Agree with all of your recommendations from D100 up.

**Answer: B.**

## D108 — What a refused search thread does to the result

**Status: answered, 2026-10-21.**

Question: when the system refuses a search thread (for example
`RLIMIT_NPROC`, or a thread limit under load), should grep still finish the
search on fewer threads, and does the refusal change the exit status?

Astra's review of the grep stack found the answer depends on which spawn is
refused. With `pthread_create` failing on demand at four threads:

- feed workers refused, two file operands: both matches, exit 0, no message;
- the live walk's outer thread refused (`-uu`): exit 2, "starting a search
  thread", nothing searched;
- the outer thread started, nested walk workers refused: all 130 matches, exit
  0, no message.

So the output is complete wherever a fallback exists, and the one place without
a fallback reports an error. Before Sol's round-2 fix that place exited 1 with
no output at all.

- **A — degrade silently.** Every refused spawn falls back to fewer threads,
  including the live walk's outer thread, which then searches on the calling
  thread. The exit status describes the search only. Cost: about 30 lines to
  restructure the outer spawn so a refusal doesn't consume the operands; a
  resource limit that makes grep slow is invisible.
- **B — degrade and warn.** As A, plus one stderr line per run, "ferret: searching
  with N of M threads: <cause>". Exit status unchanged. Cost: A's 30 lines plus
  a latch; stderr is no longer empty on every exit-0 run, which GNU and rg
  conventions don't promise but our tests often assert.
- **C — error everywhere.** Fall back where possible so the output is complete,
  but latch the refusal and exit 2 with the message. Consistent with "anything
  on stderr means exit 2". Cost: a script sees failure for a search that
  returned every match; under load, `grep -q` can no longer be trusted to mean
  "not found" on exit 1, though it never did on exit 2 either.

Recommendation: B. A refused thread costs speed, not correctness, so the exit
status should not change, but the cause of a slow run should be visible. Would
change it: wanting the invariant "exit 0 means empty stderr" (then C), or
treating thread limits as never worth a message (then A).

> Dave: I agree with the recommendations on D108 and D109.

## D109 — Waiting behind a streaming file

**Status: answered, 2026-10-21.**

Question: while one file streams its output through the publisher (D103 A),
what should another worker do when its own capture passes 4 MiB?

Today it blocks until that stream ends, so files with heavy output publish one
at a time and the pool idles. Measured on src-uu (a source tree with 32 files
whose `-v` output totals 197 MiB) at T=32, locked, after the walk-lending fix
(dbc2cbb): `-v` 0.84 s and `--json` 0.43 s, against 0.49 / 0.29 s at 3348266
(before streaming) and rg's 0.20 / 0.11 s. A control with streaming disabled
gives 0.42 / 0.29 s, with 513 MB RSS.

- **A — keep blocking at 4 MiB.** No new code. Resident capture stays at
  (2T+1)×4 MiB + (4T+1)×64 KiB. About 1.7× slower than 3348266 on `-v` and
  1.5× on `--json` for trees with several big-output files.
- **B — one shared capture budget of D93's total.** A waiting owner keeps
  capturing past 4 MiB while the pool's (2T+1)×4 MiB total has room, and
  streams from the 4 MiB point once the publisher is free, so `| head` still
  sees output early. D93's bound is unchanged; one file may use most of it.
  Estimated 0.42–0.77 s on `-v` (unmeasured). 60–100 lines: a shared counter
  and a wait in the capture path. Multi-GiB single files stay serial: huge-uu
  `--json` stays at 11.7 s, against 3.5 s at 3348266.
- **C — spill a waiting owner's capture to a temp file.** Recovers the
  multi-GiB case too (3348266, which spilled, ran huge `--json` in 3.45 s).
  Restores the spill code removed in 938054a and its fault paths (D102: a
  full or unwritable temp directory), and writes the output's size to disk.

Recommendation: B. It keeps D93 exactly, needs no disk, and fixes the common
case of a source tree with a few big generated or log files. Would change it:
heavy output from multi-GiB single files mattering; only C fixes that.

> Dave: I agree with the recommendations on D108 and D109.

## D110 — A client that connects while ferretd is exiting

**Status: answered B, 2026-10-21.** Writer clients wait for the leaving daemon.

> Dave: Also, agree on D110.

Question: when a client connects to a ferretd that has already decided to exit,
should the daemon stop that from happening, or should the client wait and
retry?

Found while fixing the watch idle-exit flake (7b5f8e3); the evidence is from
reading the code at 7b5f8e3. Once the idle loop in `daemon.rs` breaks, ferretd
sets `stop`, joins the writer, calls `close_writer`, removes its socket and
only then drops the endpoint lock. Until the socket is removed, `connect`
succeeds: the listener thread keeps accepting, but nobody reads the channel,
and the streams are closed when `run` returns. The client sees EOF before any
hello. The same thing happens in two other places. During an explicit drain
(the build-mismatch replacement), the main loop drops `Accepted` streams and
`connection()` returns before writing a hello. A 33rd client (`CLIENTS = 32`)
is also dropped without a hello.

What the client does with that EOF (`client.rs`, `connect_owner`): the error
is `Owner`, not `NoOwner`, because it did connect. Queries then fall back to
the in-process engine, which is correct, just not served by the daemon. Writer
commands (`index`, `roots remove`) fail with exit 3 and "writer command:
daemon closed the connection". The window is usually milliseconds: the
writer is idle, so `join` returns as soon as `Intake` wakes it. But if the
writer had already taken a debounced burst when the exit was decided, the
window is the whole of that refresh, which takes seconds on a large tree. With
the production idle of 15 minutes this is rare. Drain makes it more likely,
since that is when clients are actively arriving.

Two locks rule out a second writer whatever we choose, and they were checked
for each option:

- the endpoint lock (`ferret/lock` in the runtime dir) is held from startup
  until after the socket is removed. A starter that cannot take it, and whose
  socket does not answer, retries every 20 ms until `FERRET_DAEMON_STARTUP_MS`
  (10 s) runs out, then exits with "singleton winner did not bind";
- the catalog writer flock (`<index>/lock`, taken by `WriterSession::open`) is
  held until `close_writer`. An in-process `ferret index` that finds it held
  fails with `BeginError::Locked`.

So no option can let two writers touch the index at once. They differ in what
the client sees.

- **A — remove the socket as soon as the exit is decided, before joining the
  writer.** About 10 lines in `run`: move the identity-checked
  `remove_socket` above `writer.join()`, keeping the lock until the end.
  What it buys: new connections get `ENOENT` straight away, and queries take
  the no-owner path: spawn a starter, answer in-process. The starter waits on
  the endpoint lock, polling every 20 ms, and then wins. If the join outlasts
  10 s, it gives up and the next client spawns another. What it costs: a
  writer command now reaches `NoOwner` and runs in-process, but the exiting
  writer still holds the catalog flock. So it fails with "another `ferret
  index` holds the catalog lock", which replaces one error with another.
  Streams already in the channel or the kernel backlog when the socket is
  removed still see EOF. It does nothing for drain or the 33rd client. What it
  forecloses: nothing, and it combines with B.
- **B — the client treats EOF before hello as an owner leaving, and waits for
  it to go.** About 15 lines in `connect_owner`. On EOF before hello, record
  the socket's dev/ino in `draining_socket` (the existing build-mismatch wait)
  and `continue`. Once the socket is gone or replaced, behave as if no owner
  had been seen. A writer command then finds no owner and runs in-process,
  because `close_writer` runs before the socket is removed, so the flock is
  already free. A query spawns a daemon and answers in-process as today. What
  it buys: one fix for idle exit, drain and the client cap, and no
  daemon-side change. What it costs in latency: the time until the old
  daemon removes its socket, plus up to one 20 ms poll. That is milliseconds
  when the writer is idle. In the bad case it is the rest of an in-flight
  refresh, capped at the 10 s startup deadline, after which today's
  `OwnerTimeout` message stands. Queries should keep falling back at once
  instead of waiting, so the wait applies to writers only. For the client cap
  the socket never goes away, so a writer would wait out the whole 10 s and
  then report `OwnerTimeout`. What it forecloses: a daemon can no longer
  refuse a writer "now, try later" with a silent close; it would have to say
  so in a hello.
- **C — accept it and document it.** No code. Writer commands that race an
  exit or a drain fail with exit 3, and a retry by the user succeeds. What it
  costs: a scripted `ferret index` in a cron job or a git hook fails at
  random, at a rate set by how often it lands on the idle boundary.

Recommendation: B, for writer commands only. It is the one option that turns
every no-hello close into the right outcome. It relies on an ordering the
daemon already guarantees (writer lock released, then socket removed, then
endpoint lock released), and it covers drain, which is the more likely of the
three windows. A is cheap, but it moves the writer failure from EOF to the
catalog-lock error, so on its own it does not help.

What would change it: if refreshes taken after the exit decision often run
for seconds (a large tree with heavy churn), B's wait would become visible. Then
add A as well, and have the writer refuse new bursts once `stop` is set, so the
join is always short.

## D111 — Indexed grep pays for the whole index on every call

**Status: answered A, 2026-10-21.** Directory grep goes through the daemon;
B is not done.

> Dave: I'm not sure I understand why A is more work and helps less. For any
> user that can't reach the daemon, perhaps ferret is not for them. I've had no
> issues having my agents reach the daemon. If something is heavily sandboxed,
> there is likely less data to search so they wouldn't really benefit from
> ferret over rg anyway. Using the daemon seems like the right direction to me
> in general.

Question: should a directory `ferret grep` stop paying, on every call, a
start-up cost that grows with every name in the index rather than with the
tree it searches?

Evidence. **Measured** in the impact investigation (8f72c86, warm, the same
62-file tree searched through indexes that differ only in how many other
names they hold): 2.4 ms with its own 102-name index, 11.0 ms at 64k names,
22.7 ms at 250k, 80.1 ms at 1M; rg 4.1–4.5 ms throughout. About 75 µs per
1,000 names, independent of thread count. Linear extrapolation to D40's 10M
target: **~0.75 s per call** (the report's "~210 ms" at 10M is that figure
weighted by the 28% of invocations that search a directory, not a per-call
cost). **Measured** here, same tree and 64k index, 15 runs each: content grep
11.0 ms; `grep --files` 2.9 ms; `-u` 2.6 ms; `--live` 5.0 ms. `--files` walks
the same catalog subtree, so the catalog walk itself is not the cost.
Earlier measurements: SCAN.md § Remaining gap puts "catalog residency, in
`Engine::open`" at 9 ms and "document names, live set and pin" at 4 ms on a
112k-name catalog; D78 measured the resident-name build alone at 300 ms on a
synthetic 10M catalog, with an 841 MB peak.

**Read from the code at cba36ae** (the impact report's claims hold, with one
correction):

- `grep.rs:392` `open_index` calls `Engine::open` (`engine.rs:72`) and then
  `open_content`; `ferret grep` has no daemon route at all, so
  `FERRET_NO_DAEMON` changes nothing for it. (The report places `open_content`
  inside `Engine::open`; it is called by grep after it.)
- `Engine::open` runs `Catalog::into_resident()` — loads and checks all 26
  sections and builds `ResidentNames` (D54 B's interned table) — and then
  `NameIndex::new` over the whole catalog.
- Grep never reads the `NameIndex`. Its uses of the pin are `catalog()` (the
  `CatalogSource` subtree walk), `grep_candidates` (live `DocSet` plus content
  postings), `doc_names_until` and `search_threads`. The only consumer of
  `DocNames` is `groups.rs:959`, which asks whether a document has more than
  one name (hard links); building it is one pass over every name
  (`plan.rs:211`, "2 ms at 112k names" in its comment).
- `grep --files` already opens differently (`grep/names.rs:23`): `Catalog::open`
  plus `load(CatalogSource::sections())` — Names, Links, Roots, Entries — with
  no resident build and no name index. That is the 2.9 ms row above.
- The daemon protocol (`protocol.rs` `Op`) has search, find, status, reload,
  index and roots-remove. Find already streams its output through the daemon
  as `stdout`/`stderr` frames (`find_json.rs` `FrameOutput`, rendered in
  `daemon/client.rs:442`), so a grep route has a precedent to copy.
- A section is read whole and checked whole (`read.rs:789` `load_one`,
  `format::check`). There is no row-range read.

Options:

- **A — serve directory grep through the daemon.** Add `Op::Grep`; the client
  resolves tty, colour and heading locally and sends them as explicit flags,
  with its cwd; the daemon runs the existing grep host against a frame sink;
  the client renders frames and maps the `end` exit. Stdin and explicit file
  operands stay in-process. About 500–700 lines with tests, all in the
  `ferret` crate: `grep.rs` split so the host takes a writer, cwd and pin as
  parameters rather than reaching for `io::stdout()` and `current_dir()`;
  a `batch::grep_request`; the client arm. No new crate edge, no dependency.
  What it buys: the open drops to a connect and hello while a daemon is up,
  at any index size; candidates come from a followed, fresher generation. What
  it costs: every output byte is JSON-framed in the daemon and decoded in the
  client — negligible at the median 11 output lines, unmeasured on dense
  output, where ferret is now only 0.91× rg (C3). The first call with no daemon
  still pays the full in-process open and spawns ferretd (as `search` does,
  D78). It gives nothing where no daemon can run: `FERRET_NO_DAEMON=1`, or an
  agent sandbox that cannot reach the runtime socket — and agents are most of
  the callers. Forecloses nothing.
- **B — open only what grep reads.** A grep constructor that does what
  `grep --files` does: `Catalog::open` without `into_resident`, no
  `NameIndex::new`, sections loaded by the calls that need them; and the
  hard-link check at `groups.rs:959` reads the inode's `st_nlink` (already a
  section) instead of a whole `DocNames`. About 100–150 lines in `engine.rs`,
  `grep.rs` and `grep/groups.rs`, plus a test that grep never builds resident
  names (`grep/names.rs` has the `OPENED` hook to copy). No format change,
  no crate edge, no dependency. What it buys (estimated, not measured): most
  of the 8 ms at 64k — the residual is the content open, the live `DocSet` and
  whole-section reads, so roughly 4–5 ms, against rg's 4.1; at 10M it removes
  the 300 ms name build and the DocNames pass, leaving whole-section reads of a
  ~420 MB snapshot (this index is 42 B per name) at perhaps 100–250 ms. It
  stays O(index), with a smaller constant; it works with or without a daemon.
  It reopens D78 for grep only: D78 A kept the build on every open because
  `search` reaches a daemon, and grep does not; D78's own "would change it"
  was this case. Forecloses nothing; A can come later on top.
- **C — proportional to the tree: read only the operand's subtree.** Row-range
  section reads with per-row checks, overlay records applied to a partial
  view, and a walk that reads children by range. A storage-reader change in
  `ferret-catalog` plus `ferret-query`'s walk: weeks, not days, and a second
  validation model beside whole-section checks. Buys rg-like scaling with no
  daemon. Nothing needs it if A and B both land.
- **D — leave it.** No code. Measured +6–8 ms at 64k names, about 2 ms per
  average invocation once weighted by usage, and Dave has no default-location
  index yet. Crosses 50 ms around 600k names; ~0.75 s at 10M.

Recommendation: B now, A when indexes grow past about 1M names. **Fastest and
simplest disagree here:** A is fastest (near zero open while a daemon runs, at
any size) but is the larger change and does nothing for sandboxed or
no-daemon agents; B is the simplest (a grep-only open path that copies
`--files`) and helps every call, but stays linear in the index. B's
estimates need one measurement after it lands; if the residual at 1M names is
still over 50 ms, A becomes the real fix rather than a later one.

What would change it: evidence that agent sandboxes reach the daemon socket
and that real indexes will approach 10M soon; then do A first, and B only for
the no-daemon path.

## D112 — Grep on a directory no index covers

**Status: answered B, with rg's ignore behaviour, 2026-10-21.**

> Dave: go with D112 B, matching rg's ignore behaviour

Read as: an operand no index covers is searched live, anchored at the
enclosing work tree, with rg's ignore rules rather than D13's. So the
fallback reads `.gitignore` files from the work-tree top down, plus
`.git/info/exclude`, the global git excludes file, `.ignore` and `.rgignore`;
skips hidden entries; applies no `DEFAULT_IGNORE` and no 8 MiB size cap; and
always searches an operand it is given by name, even an ignored one, while
still applying the parent directories' rules inside it. Checked with rg
15.1.0 (2026-10-21): with `.gitignore` holding `target/` and `*.log`,
`rg hit target` searches `target/a.txt` and `target/sub/b.txt` but not
`target/c.log`. Behaviour is pinned by differential tests against the dev
shell's `FERRET_RG`, not by reading rg's source. A's message fixes land too.

> Dave: I agree with what you said above.

(Confirms that reading, including dropping the 8 MiB cap in the fallback.)

Question: when `ferret grep PATTERN DIR` names a directory that no index
covers, should it fail as it does today, or search the directory, and with
which ignore rules?

Evidence. **Measured** with the 8f72c86 binary and isolated config:

- No index at the default location (Dave's state: `~/.local/share/ferret`
  does not exist): `ferret grep -n parse_date src` exits 2 with "no index in
  …/ferret; run ferret index DIR; live modes arrive in M6". The last clause is
  stale; M6 merged.
- An index exists but the operand is outside its roots (`grep -n fn /tmp`):
  exit 2, "start is outside the catalog or the index is stale; run ferret
  index DIR or use -I; index this directory or use --live / --no-ignore". Two
  hints are wrong: `-I` in grep means no filename (D74), and `--live` fails
  outside roots too ("directory is outside configured roots; index it or use
  --no-ignore").
- On a real Rust repo with a 13 GB `target/` (`~/w/tally`), anchored at its
  top: `ferret grep --live --files .` and `rg --files` list the same 75 files;
  `-u --files` lists 38,225.
- On a toy git repo (`.gitignore`: `*.log`, `gen/`; `src/.gitignore`:
  `secret.txt`; plus `src/node_modules/` and an unignored `target/`),
  `-l needle`:

  | run                                    | files reported                                                   |
  | -------------------------------------- | ---------------------------------------------------------------- |
  | `rg src`                               | `src/a.rs`, `src/node_modules/m/i.js`                            |
  | `--live`, root at the repo top, op src | `src/a.rs`                                                       |
  | `--live`, root at `src` itself         | `src/a.rs`, `src/gen/b.rs`, `src/secret.txt`, `src/x.log`        |
  | `-u src`                               | all five under `src`, `node_modules` included                    |

  A root at a subdirectory of a work tree reads no `.gitignore` at all, not
  even its own: D22 A treats it as outside any work tree.
- Speed on the 62-file `tally/src`: `--live` 5.0 ms, `-u` 2.6 ms, rg 4.1 ms
  (impact report; U2, 9 MB tree: `--live` 7.0 ms, rg 5.3).

**Read from the code:** `--live` (`grep/live.rs`) uses the catalog only for
its list of root paths (`Anchors { roots, faults, global, index }`); `start`
walks from the covering root down to the operand, applying D13 rules at each
step. With no global ignore file it falls back to `DEFAULT_IGNORE`
(`.git/ node_modules/ vendor/ .venv/ target/ dist/ build/ .next/ result
.cache/ __pycache__/ .direnv/`), so it needs no setup. A synthetic root is
therefore just a path in that list. In `grep.rs` today the engine opens
before operands are checked, so a fallback must check coverage first (the
Roots section only), or every uncovered call would pay D111's open and then
walk anyway.

Options:

- **A — keep exiting 2; fix the messages.** About 20 lines across `grep.rs`,
  `grep/live.rs` and `grep/names.rs`: drop the M6 clause and the `-I` hint,
  stop suggesting `--live` where it cannot work, and name both working
  commands (`ferret index DIR`; `-u`, which searches ignored files). What it
  buys: no change to D73's model. What it costs: the first directory search
  in any uncovered tree fails; an agent then retries, usually with `-u`
  (the only flag that works without an index), and gets the `-u` output below.
  Every directory call fails until someone indexes, and the investigation
  found the owner has no index. Forecloses nothing.
- **B — fall back to `--live` anchored at the enclosing work tree.** If no
  index covers an operand, search it live with D13 policy, anchored at the
  nearest ancestor holding `.git`, or at the operand when there is none. About
  100–150 lines in the `ferret` crate: an `Anchors` constructor from synthetic
  roots, an upward `.git` probe (a few `stat` calls), coverage checked before
  the engine opens, per-operand routing, and tests; no crate edge, no
  dependency. What it buys: rg's file set on a plain repo — identical on
  `tally` — at 5 ms against rg's 4 on a small tree. Remaining differences
  against rg: `DEFAULT_IGNORE` directories are skipped even when git tracks
  them (a committed Go `vendor/`, a source directory named `build/` or
  `dist/`, `node_modules/` above); files over 8 MiB are not searched;
  `.ignore` and `.rgignore` are not read (D74). Anchoring at the operand
  instead would be about 30 lines less and would search everything the repo's
  `.gitignore` excludes, as in the table. The upward probe does not
  contradict D22 A, which governs configured roots; it does reintroduce a
  small `.git` search that D22's revision removed from the walker, here used
  only to choose the anchor. Forecloses: a missing index becomes silent, so
  nothing tells the user that indexing would make the call faster; covered
  directories answer from a snapshot (D73 A) while uncovered ones answer
  live, which is a visible difference in freshness between two directories.
- **C — fall back to the unrestricted walk (`-u`).** About 10 lines. No ignore
  rules at all: on `tally` it lists 38,225 files including 13 GB of `target/`
  where rg lists 75, so a pattern that appears in build output prints
  thousands of lines rg never shows, and the walk reads gigabytes the first
  time (U3: 65 ms per warm GB). Matches neither rg nor D74 A's intent that
  `-u` is an explicit request. Simple but wrong output.
- **D — index on first use.** Run `ferret index DIR` from inside grep. Cost:
  the first call pays a crawl and content index (0.13 s for `tally`'s 80
  files; seconds to minutes for a large tree), and a read command adds roots
  and daemon watches as a side effect, so an agent grepping `/tmp/x` or a
  throwaway worktree registers it permanently. Buys indexed speed on every
  later call. Forecloses grep being read-only; better offered as a hint in
  B or A than done implicitly.

Recommendation: B, anchored at the enclosing work tree. **Fastest and
simplest disagree here:** A is the simplest (20 lines) and B the fastest in
the sense that matters, one call instead of a failure, a retry and possibly
`-u`'s 38,000-file output. B also needs D111's ordering fix (check coverage
before opening the engine) or its fallback calls pay the open for nothing.
Fix A's messages in either case, since the outside-roots hint names a flag
that means something else.

What would change it: Dave preferring that ferret never silently differ from
an indexed answer (then A, with the corrected messages), or agents mostly
calling grep in trees whose committed sources sit under `vendor/`, `build/` or
`dist/`, where B's default ignores would hide real matches (then B with
`DEFAULT_IGNORE` limited to what git ignores when a work tree is found).

Note (2026-10-22): the shipped fallback also reads `.ignore` and `.rgignore`
from every ancestor above the work tree, up to `/`, and those rules stay in
force inside a nested work tree. Each outranks every git source, including a
closer `.gitignore`. `.gitignore`, `.git/info/exclude` and the XDG global git
excludes (`$XDG_CONFIG_HOME/git/ignore`, or `~/.config/git/ignore`) apply
only inside a work tree. Ferret does not read `core.excludesFile`.

## D113 — Indexed grep and files over the size cap

**Status: answered, yes, 2026-10-21.**

Question: should indexed `ferret grep` search catalogued files that are over
the 8 MiB content-index cap, reading them live, rather than leaving them out?

Evidence. The cap (`ferret-policy` `DEFAULT_SIZE_CAP`, "a judgement, not a
measurement") exists to keep big files out of the content index; grep
inherited it only so its answers agree with the index. Today indexed grep
skips over-cap files, and a changed candidate that has grown past the cap
reports an error (GREP.md, "Indexed recursive content"). rg has no cap.
**Measured** on Dave's `~/w` (2026-10-21, `rg --files` plus `stat`): 67,584
files rg would search, 4.8 GB; 90 over 8 MiB, 1.97 GB, nearly all build
output under one repo's unignored `target/debug` (binary; ferret's index
leaves `target/` out anyway). Over-cap files with no NUL in their first
8 KiB: 6 files, 110 MB, all JSONL corpora (`grep-compat`, `find-compat`),
which are the files an agent would grep. At U3's 65 ms per warm GB, reading
all six costs about 7 ms, paid only by searches whose operands reach them.

Options:

- **Yes — search them live.** The catalog already lists over-cap files with
  their sizes; grep reads them directly beside the index's candidates, with
  rg's binary handling, and the grown-past-the-cap error goes. Matches rg.
  Costs the bytes read per call in trees that hold such files.
- **No — keep the cap.** No change; ferret silently misses matches in large
  text files that rg finds.

> Dave: Should we be live grepping those files even when they don't show up
> in the index? I think yes.

Fact that would change it: trees where over-cap text files are common and
large enough that every grep reads gigabytes; then a cap configurable per
root, not a fixed exclusion.

## D114 — A content merge blocks catalog refreshes in the daemon

**Status: answered 2, 2026-10-21.** Pacing fix now; then merges move off the writer thread.

> Dave: go with D114 option 2

Question: once the merge-pacing bug is fixed, should content merges move off
the daemon's writer thread so that a file edit becomes searchable while a
merge runs, and should the merge policy stop rewriting a mid-sized segment on
every few edits?

Evidence (**measured** on a 120k-file synthetic tree, daemon running, a loop
of one-file edits each followed by a `text:` search; report in the session
handoff, `stall-diagnosis.md`):

- Normally an edit is searchable in 0.23 s (the watch debounce plus a
  refresh). One edit in nine took 5.7 s (11.0 s for the first merge).
- Each edit's content follow writes a one-document segment. The merge policy
  groups every segment under 5.6 MiB, so every nine edits it rewrites the same
  5.8 MB, 13.4k-document segment together with nine one-document ones.
- Watch traffic cancels a merge twice; the third attempt is protected from
  cancellation (`daemon/writer/content.rs`, so merges cannot starve) and holds
  `Engine.writer` for its whole run. The refresh that publishes the edit waits
  behind it.
- The merge took ~6 s, of which 5.68 s was pacing: every postings read is
  rounded to a checksummed 4 KiB chunk and charged to the 32 MiB/s limiter
  each time, so 5.77 MB of input charged ~180 MB. Unpaced: 0.29 s. That is a
  bug and is being fixed now regardless of this decision ("fix A"); with a
  throwaway version the worst case fell to 0.49 s.
- Controls: protection disabled, worst case 0.24 s but 29 consecutive
  cancelled merges and segments growing 10 → 39 (the starvation the protection
  prevents); merges suppressed, 0.44 s; content follow suppressed, every edit
  searchable in 0.22–0.25 s, because an edited file gets a new DocId the
  content index does not cover and the query verifies its bytes directly. A
  catalog refresh alone gives a correct, fresh text answer.
- After fix A a protected merge still blocks refresh for about 0.1 s per MiB of
  merge input (**estimate**): a merge into a 40 MB segment would be a 4–6 s
  window. Under a steady edit loop every merge becomes protected after two
  cancellations.

Options:

- **1 — fix A only.** No further work. Worst case 0.5 s at this merge size;
  larger merges keep a multi-second window, proportional to merge input.
- **2 — fix A, then B: merges off the writer thread.** Run `merge_content` on
  its own thread holding only the content-index mutex and a pinned catalog
  view; refreshes stay on the writer and proceed beside it; follow skips (try
  lock) while a merge holds the content index, and uncovered documents are
  verified directly meanwhile (bounded by the 10,000-document
  `IndexIncomplete` limit). Watch traffic then never interrupts a merge, so the
  cancellation counter and the protection go away. Costs a concurrency slice
  (Sol-class): a second nice-19 thread; closing a publication race in
  `Engine::select`, which reads the previous pin before taking the write lock
  and could overwrite a just-published merged content view with a stale one;
  status reporting two concurrent operations. Merges and refreshes share CPU
  and limiter tokens. Expected 0.23 s at any merge size (**estimate**, backed
  by the two controls).
- **3 — option 2 plus G: small segments merge among themselves.** In the merge
  policy's `choose`, a group whose top is above `TIER_FLOOR` stops absorbing
  floor-level segments, so nine one-document segments merge into one ~10 KB
  segment instead of rewriting the 5.8 MB one. A few lines in `merge.rs`, plus
  correcting the policy's documented "Lucene's span" rationale. Routine
  edit-loop merges become milliseconds and write amplification in an edit loop
  falls by roughly 500× (**estimate**); costs up to ~9 extra small segments per
  level in steady state, slightly more query fan-out. Once A and B land, G no
  longer affects latency, only disk writes.

Recommendation: **2**. Fastest and simplest disagree: 1 is the simplest and
removes about 92% of the measured stall; 2 is the fastest and makes edit
latency independent of merge size, which is the agent loop's worst case. G is
worth doing for write amplification but is not needed for latency.

What would change it: a merge-size trace from the daemon on a real tree under
agent traffic showing merges never much above ~6 MB; then 1 is enough and B
can wait.

## D115 — Should ferret honour git's `core.excludesFile`?

**Status: answered B, 2026-10-22.**

> Dave: Let's go with your recommendation for now.

Done, 2026-10-23 (b768ad8). Uncovered grep picks its global excludes file as
rg 15.2.0 does, measured as a black box: the first `excludesFile = VALUE` line
in `$GIT_CONFIG_GLOBAL`, `~/.gitconfig`, then the XDG git config, in any
section. rg's line grammar is narrower than git's (one word of value, nothing
after it), and a named file it cannot read means no global excludes. GREP.md
has the rules; a 19-case differential test pins them. One existing divergence
went with it: an unreadable `git/ignore` failed ferret's grep, and rg skips it
silently. The crawl is unchanged.

Question: should ferret read git's `core.excludesFile` setting, and if so in
uncovered grep's rg mode (D112 B), in the indexed crawl (D13), or both?

Evidence (rg 15.1 and 15.2, git 2.54, private HOME; full notes in the
research brief):

- **read-from-code:** the crawl's global layer is ferret's own
  `$XDG_CONFIG_HOME/ferret/ignore`; it reads no git config. Uncovered grep
  reads git's default global file (`$XDG_CONFIG_HOME/git/ignore`, else
  `~/.config/git/ignore`) and applies it only inside a work tree.
- **measured:** inside a work tree, rg and git both honour an absolute or `~/`
  `core.excludesFile` set in `~/.gitconfig` or `$XDG_CONFIG_HOME/git/config`
  (`~/.gitconfig` wins), and the setting replaces the default file. git also
  honours `[include]`d files and per-repo `.git/config`; rg honours neither.
- **measured:** on this machine the key is unset everywhere, so uncovered
  grep already agrees with rg and git here.
- **measured:** the rejected earlier attempt ran three `git` processes per work
  tree, 1.55 s across the 585 work trees under `~/w`. Reading the files
  directly costs 37 µs per uncovered grep, or 4.6 ms per crawl at 585 trees.

Options:

- **A — neither.** No new code. A future `core.excludesFile` setting would
  hide files from rg and git but not from ferret.
- **B — uncovered grep only.** Each uncovered grep reads the two global git
  config files (and `GIT_CONFIG_GLOBAL` when set, as rg 15.2 does), then the
  chosen excludes file: 37 µs, no watches. Matches rg exactly, including that
  rg ignores `[include]` and `.git/config`. The index keeps D13's rules.
- **C — both, matching git.** Read global, included and per-repo config in the
  crawl too (4.6 ms per crawl), and watch those files. The index would start
  applying git's global excludes, which it does not today, so what it hides
  changes; rg mode would then differ from rg on includes and local config.
- **D — crawl only.** The index half of C; uncovered grep still misses the
  setting.

Recommendation: **B**. It closes the only rg-compatibility gap at no
measurable cost and leaves D13's index rules alone.

What would change it: deciding that the index should hide what git's global
excludes hide. Then add one crawl-wide layer for the global setting (C
without per-repo config).

## D116 — How long should a spawned daemon stay up when idle?

**Status: answered C, 2026-10-23.** Fix the watcher's bookkeeping, then stay up.

> Dave: For D116, don't we want the daemon to stay running for indexing
> purposes? Perhaps the question should be what can the daemon drop from
> memory when not being searched?

Question: should the daemon's 15-minute idle exit change, now that grep,
search and find all spawn it and agents work in bursts?

Evidence (**read-from-code** unless marked):

- `FERRET_DAEMON_IDLE_MS` defaults to 900,000 (15 min); `0` disables exit.
  A burst after a longer pause pays a cold start: the command that spawns
  answers in-process from the last published catalog, and the new daemon
  then walks every root (the startup backstop) at nice 19.
- Resident memory is **unmeasured** for a daemon. The nearest **measured**
  figures are a query-only engine at 10.4M names: 410–800 MiB current RSS
  (ROADMAP S1b M1/M3). A daemon adds a writer, content and one inotify watch
  per directory.
- The hourly backstop walk keeps running while the daemon lives.
- Separately, being built now without a decision because it changes no
  policy: `ferret index` and `roots remove` spawn the daemon after they
  publish, as search, find and grep already do.

Options:

- **A — keep 15 minutes.** Memory and watches go away after a short pause.
  A burst after a longer gap starts cold, and its first command may answer
  from a stale catalog.
- **B — raise the default to 4 hours** (an estimate of one sitting). One
  loaded engine and fresh answers across a working session; holds the
  daemon's memory and watches through the tail, plus up to four hourly
  backstop walks.
- **C — have `ferret index` offer the user systemd unit**
  (`contrib/systemd/ferretd.service`, `--idle-ms 0`). The only setup where the
  first command always finds a ready daemon. Holds memory for the whole
  login, and the template's binary path and index do not match this machine
  yet.

Recommendation: **A**, plus the spawn from `index` that is already being
built.

What would change it: the daemon's steady RSS on the index you actually use.
If it is well under ~400 MiB, choose B.

### Answer to Dave's question (2026-10-22)

Yes: a daemon that exits stops indexing, and each respawn pays a full rewalk.
The measurements below say most of an idle daemon's memory is not the index at
all. It is the file watcher's bookkeeping, which is a data-structure fix rather
than a lifecycle choice. So the revised question is below; the original
options above stand as asked.

Evidence (`c9b17e8`, release builds, private XDG dirs and index):

- **measured, msa2, `~/w`:** 65,291 entries, 8,044 directories, 7,901
  watches. The daemon idles at **30 MB** RSS (22 MB heap) after an index, a
  find and two greps, unchanged over 10 minutes.
- **measured, geekom, `/nix/store`:** 5.38M entries, 1.39M directories. The
  watcher wants 3.26M watches; the cap is 458,752 (seven eighths of the
  kernel's per-user limit). The rest of the root is uncovered, so the daemon
  rewalks all of `/nix/store` every 5 minutes (`FERRET_POLL_MS`). Each rewalk
  outlasts the interval, so the writer never goes idle: one nice-19 thread at
  about 65% CPU, reading. RSS **2.0 GB** after 10 minutes, peak 2.36 GB.
- **measured, same index, polling and the hourly backstop pushed to a day:**
  the startup rewalk finishes in about 60 s (peak 2.0 GB), then the daemon
  sits idle at **1.69 GB** heap, flat for 10 minutes.
- **measured, heaptrack, same setup:** 1.27 GB live at exit against 1.82 GB
  RSS, so about 0.5 GB is memory glibc kept after the rewalk's peak. The live
  heap, by allocation site:

  ```text
  watcher: policy inputs (per watched dir, per ignore file)   634 MB
  watcher: policy failures (one full path per unwatched dir)   181 MB
  watcher: arming and reconcile                                114 MB
  catalog, resident sections                                    167 MB
  catalog, resident names                                        45 MB
  writer session tables                                          55 MB
  content index (segments, trigram inverter, doc names)          66 MB
  name index                                                      5 MB
  ```

  The watcher's 930 MB is about 1.6 KB per watched directory. Each record
  owns a copy of the root path and the file name, in nested `BTreeMap`s
  (`ferret-crawl/src/watch/policy.rs`). Past the cap, every failed directory
  also keeps a `(root, full path)` record, although the whole root is polled
  anyway.
- **measured, `/nix/store`:** find with no daemon (open the catalog, answer,
  exit) takes 0.41–0.52 s and 415–435 MB peak; through the daemon, 0.05 s.
- **estimated, not measured:** your home on msa2 is about 3.3M entries and
  185K directories once `target`, `node_modules` and `.git` are pruned, so it
  fits under the watch cap. Scaling the per-directory and per-entry figures
  above gives roughly 300 MB of watcher state and 200 MB of index state. I
  did not index it: reading every private file's content into a test index
  was refused by the session's permission check.
- **read-from-code:** the writer needs the catalog to apply changes
  (`WriterSession::lookup_base`), so the catalog cannot be dropped while the
  daemon indexes. Only query-side state (name postings, content segments, doc
  names, planner) is free to drop: about 130 MB at 5.4M entries.
- **measured, side finding:** a `ferretd` started before any catalog exists
  fails for good. `ferret index` then exits 3 with "daemon could not open the
  catalog" until the daemon is restarted. An always-on unit hits this on a
  fresh install.

Question: should the daemon stay up to keep indexing, and what should it drop
to make that cheap?

Options:

- **A — keep the 15-minute exit (today).** Frees everything between bursts.
  The index goes stale while no daemon runs (D117 B now says so). Each respawn
  rewalks every root: about 60 s of nice-19 I/O and a 2.0 GB peak on
  `/nix/store`.
- **B — stay up, as the code is today.** Always fresh. Costs about 1.7 GB on
  `/nix/store`, an estimated 0.5 GB on your home, and, past the watch cap, a
  rewalk that never stops.
- **C — fix the watcher's bookkeeping, then stay up.** Store a root as an id
  and the policy file names as a small enum instead of owned paths. Drop
  per-path failure records once a root is polled. Return freed memory after a
  rewalk with `malloc_trim` (an `unsafe` FFI call, per D11). Expected,
  unmeasured: about 1.27 GB live falls to about 0.4 GB on `/nix/store`, and
  home falls to about 0.25 GB. Also make a daemon with no catalog able to
  create one. A few days' work; it helps A as much as B, since the same 930 MB
  is held for each 15-minute window.
- **D — C, plus drop query-side state after N minutes without a query.**
  Saves about another 130 MB at 5.4M entries. The next query reloads it,
  bounded by the 0.4–0.5 s in-process figure above (unmeasured inside the
  daemon).

Recommendation: **C**, and make "stay up" the default once C's idle figure is
measured. The watcher fix is needed whichever lifecycle wins. Separately, the
poll interval past the watch cap should grow with the last rewalk's duration
so that it cannot run back to back. That is a bug fix, not a choice.

What would change it: if C measures above about 300 MB idle on a home-sized
tree, add D. If you want the daemon off when you are not using ferret at all,
keep A's exit after C, with a longer timeout.

> Dave: go with C for D116

Done, 2026-10-23 (7acaf31, 033ea6a, 07fc3dc, aeadcab, dd73ad9, 1c45275). The
watcher keeps failures exactly for the first 1,024 directories per root and
counts the rest. A daemon with no catalog creates one. A poll that outlasts its
interval is spaced to four times its duration, and a polled root is walked like
a backstop rather than as one whole-root selection. `malloc_trim` runs after
any refresh of a second or more and after each command. Staying up is the
default; `FERRET_DAEMON_IDLE_MS` still sets an exit.

Measured on geekom's `/nix/store` (5.4M entries, past the watch cap), release
build:

| | Before C | After C |
| --- | --- | --- |
| Startup rewalk | ~60 s | ~60 s (33 s CPU) |
| Peak RSS | 2.36 GB | 1.07 GB |
| Idle, anonymous | 1.69–2.0 GB | ~640 MB, flat |
| Polls past the cap | back to back | ~30 s CPU every ~5 min |

`malloc_trim` accounts for about 78 MB of the idle figure (715 → 637 MB). Two
defects turned up on the way and are fixed: polls re-walked the whole root as
a selection, so they never finished and `watch_failed` grew; and with an
overlay, `finish_observations` re-sorted a large directory's children for each
small group, which made one startup rewalk take 16 minutes.

D is not added. The 300 MB threshold was for a home-sized tree, which has not
been measured; scaling B's home estimate (0.5 GB) by `/nix/store`'s fall (1.7
GB → 0.64 GB) gives about 0.2 GB.

## D117 — Should an answer from a possibly stale index say so?

**Status: answered B, 60 s, on by default, 2026-10-22.**

> Dave: Agree with your D117 recommendation too.

Done, 2026-10-23 (7bc802a). Search, default find and indexed grep that answer
in-process print `ferret: warning: no daemon answered; …` with the index's age
when `current` is more than 60 s old. `FERRET_STALE_NOTICE=0` silences it.

Question: when no daemon is reachable and a command answers in-process from
the published catalog, should it print a one-line notice on stderr if that
catalog is old?

Evidence (**read-from-code**):

- `search --help` says the daemon keeps names and content within about a
  second of change. On the fallback path that is false, and nothing says so.
  A file an agent wrote minutes ago can be missing from `grep`, `find` and
  `search`.
- The manifest carries no clock, but every publication renames `current`
  into place, so its mtime is the publication time: one `stat`.
- `--quiet` already means "stop after a match" in grep, so a silencing switch
  would have to be new (for example `FERRET_STALE_NOTICE=0`, shaped like
  `FERRET_NO_DAEMON`).

Options:

- **A — stay silent.** Nothing changes; stale answers stay unannounced.
- **B — one stderr line when no ready daemon was reachable and the catalog is
  older than 60 s.** Exit status is unchanged. 60 s sits above the 1 s
  debounce and the 10 s startup deadline, so a fresh index stays quiet. A
  harness that treats any stderr as failure would flag a stale but successful
  grep.
- **C — warn on every local answer from an old catalog**, daemon or not.
  Covers paths that never try the socket (find actions, `-I`), but warns while
  an idle daemon is correctly keeping an unchanged tree.

Recommendation: **B**, 60 s, on by default.

What would change it: agent harnesses that fail a tool call when stderr is
non-empty. Then default the notice off for grep and keep it for search and
find.

## D118 — How agents' searches reach ferret (S4)

**Question:** Should agents reach ferret through a skill they may choose to
use, through a hook that refuses the built-in search tools, or through ferret
answering as `rg` itself?

Context:

- Claude Code's Grep tool runs its bundled rg, and its system prompt prefers
  Grep and Glob over Bash. Codex, grok and Bash calls run whichever `rg` is
  first on PATH.
- Measured on 2026-10-23 (ROADMAP § Timing against rg): through ferret,
  sparse queries are 2–4× faster than rg and dense output is at parity; through
  a pipe ferret is ahead. `--live`, for a path no index covers, is 1.5–2.7×
  slower than rg.
- The query log records only ferret calls, so it fills only as fast as
  routing does.

| Option | Costs | Buys |
| --- | --- | --- |
| A. Skill only: `contrib/claude/skills/ferret/SKILL.md`, symlinked into `~/.claude/skills` | Advisory. The model reaches for Grep and Glob first, so most searches never touch ferret and the query log stays thin. | No risk to any search. Agents learn `ferret search` for what rg cannot do: name, content and age in one query, and indexed find. |
| B. Skill, plus a PreToolUse hook that refuses Grep and Glob under an indexed root and names the ferret command | One refused call and a Bash retry for each search: an extra turn (several seconds, plus tokens) until the model adapts, again in every new session. Needs `Bash(ferret:*)` allowed. The model translates Glob to `ferret search` or find, and a wrong translation costs a further turn. | Every search under a root goes through ferret; a full query log. |
| C. Skill, plus an `rg` shim: a script named `rg` early on agents' PATH. It runs `ferret grep` for paths under a root and execs the real rg otherwise, or on a usage error (exit 2). Claude Code is pointed at PATH's rg with `USE_BUILTIN_RIPGREP=0`. | ferret grep must accept every flag the Grep tool passes, or fall back. A ferret bug silently degrades every agent's search until someone notices. One more file to install. Glob is not routed. The variable exists in the Claude Code 2.1.296 binary; its exact effect is unverified. | Transparent for every agent (Claude, codex, grok) with no extra turns. It tests the drop-in thesis on real traffic. The query log fills with real rg-shaped queries. Uncovered paths keep rg's speed. |

**Recommendation: C, in two steps.**

1. The shim only logs: it records each argv it sees and then execs the real
   rg. Run it for a day of normal agent work.
2. Replay the captured invocations through the grep differential against rg.
   Only when they agree does the shim switch to ferret.

The skill ships first under any option; it documents `ferret search` and
`ferret find`, which no rg route reaches.

**Fact that would change it:** if the captured invocations rely on flags
ferret grep rejects often enough that the shim mostly falls back, then B for
Grep alone, since a refusal at least tells the model what to do instead.

> Dave (2026-10-24, in chat): Agree with the recommendation. Asked whether
> the shim should also run the equivalent rg or find in the background to
> verify ferret's answers, rather than only recording queries for later.

Checked on 2026-10-24, before any logging day:

- `USE_BUILTIN_RIPGREP=0` works. With a logging `rg` first on PATH, a
  `claude -p` session (2.1.296) ran every Grep and Glob call through it.
- Glob goes through rg too, as
  `--files --null --glob PATTERN --sort=modified --no-ignore --hidden .`, so
  "Glob is not routed" above was wrong. Its `--no-ignore` asks for files the
  index leaves out, so Glob stays with the real rg either way.
- Every Grep call passes `--hidden`, six `--glob !.VCS` excludes and
  `--max-columns 500`, then `-l --null`, `--json -n`, or similar. Today
  ferret grep rejects `--max-columns` (exit 2), so the shim would fall back on
  every Grep call until ferret supports it.

Proposed in reply: step 1 becomes a shadow rather than a log. The shim serves the real rg's
answer, then runs `ferret grep` with the same argv in the background at idle
priority and records whether the two agree. The agent never waits for or sees
ferret's answer. A mismatch is re-run once after a pause; one that then
agrees is index lag, not a wrong answer. Commands with side effects (`--pre`
for rg; `-exec`, `-delete`, `-ok`, `-fprint` for find) are never shadowed.
After the switch the roles reverse, with rg shadowing ferret on a sample.

## D119 — One unreadable root blocks a first index of healthy roots

**Question:** When the first `ferret index A B` finds a coverage fault under
B that has no anchor, should the whole command publish nothing (today), or
should it publish A and report B as failed?

Context:

- Reproduced 2026-10-25 on main: `ferret index GOOD BAD`, where `BAD` holds a
  self-looping `.ferretignore` symlink, exits 3 with "nothing published" and
  ReadIgnore/ELOOP; `ferret search` then reports no index. `ferret index GOOD`
  alone succeeds.
- This is D26's S1+ amendment working as written: a fault with no anchor in a
  previous generation blocks publication (`ferret-crawl/src/index.rs` ~1021,
  ~701; `coverage.rs` ~425). Once an index exists, faults under an existing
  root are retained and the rest publishes; only the first index and newly
  added roots have no anchor.
- It bit in practice: p16s has no index tonight because stale worktrees under
  `~/w/tfa` faulted (fixed separately as ef5dced, which makes a missing gitdir
  a known state). B's rules here are genuinely unknown, so treating them as
  empty would be wrong; the question is only what happens to A.
- Raised by Sol's architecture review of 184cca3.

| Option | Costs | Buys |
| --- | --- | --- |
| A. Keep invocation-wide atomicity | A first index of many roots fails on any one bad root; the user must find and drop it, then rerun. Every nonzero `index` still guarantees nothing was published. | No change. Simple contract for scripts. |
| B. Publish the fully observed disjoint roots; report each failed root; exit nonzero (partial success) | A failed `index` can now have published something, and the configured roots become the healthy subset, so the failed roots must be named for the user to retry. Overlapping roots and global-policy failures stay atomic. About 4–8 h: group observations by root through coverage classification, build one checkpoint from the healthy groups. | `ferret index ~/w` style first runs work despite one bad corner. Matches what an existing index already does for faults under existing roots. |
| C. B, but only behind an explicit flag (`--partial`) | Same build as B plus a flag; the default stays surprising on first use. | Scripts keep A's guarantee; humans and agents opt in. |

**Recommendation: B**, scoped to explicitly supplied disjoint roots (first
index and newly added roots), with a distinct exit status for partial
success and an all-failed run still publishing nothing. On-disk format
unchanged.

**Fact that would change it:** a caller that relies on "nonzero `index`
means nothing was published" — then C.

> Dave (2026-10-25, in chat): B.

**Implemented (2026-10-25, wt/partial-publish).** A refreshed root is
withdrawn when the previous generation lacks it, it is disjoint from every
other configured root, and its own faults cannot be resolved alone: any fault
but directory EACCES on the initial checkpoint, or `coverage::resolve`
returning none on the log path and the full-rewalk fallback. Its batches and
faults leave the run and it leaves the plan's roots, so no generation records
it; the report lists it in `failed_roots`. Shared hard-linked content
publishes through the healthy name, since an alias records the stored
observation in its own batch; content only the withdrawn root names publishes
nowhere. Everything else stays atomic, as before: a run whose refreshed roots
were all withdrawn, a nested or enclosing root, an old generation that moved,
a policy or sniffer change with protection, and an unreadable global ignore
file (read before the walk).

Exit status 4 is new (`Exit::Partial`). 0–3 are taken (3 already means
"nothing published", which stays true), grep keeps rg's 0–2, find GNU's 0–1,
ferretd exits 3; nothing returns 4. stderr gives each withdrawn root as
`ferret: not indexed: ROOT: …; retry with ferret index ROOT` followed by its
faults; stdout adds a `failed  ROOT` line; the query log adds outcome
`partial` and a `failed_roots` count. `index` has no `--json`. The daemon
renders the same way and the client maps 4. The report's walk counts still
include the withdrawn roots' walks. A publication that becomes visible and
then fails to be made durable (`IndexError::Undurable`) keeps its report, so
a withdrawn root still exits 4 with its retry line, beside the durability
warning; with nothing withdrawn it exits 0 with the warning, as before.

Building the daemon test found a separate crash: a self-linked
`.ferretignore` in an added root sent the watch's policy-input registration
into unbounded recursion and aborted ferretd. It now follows at most 40 links
(MAXSYMLINKS) and records the input as a failure.

## D120 — A file edited through a hard link outside every root

**Question:** When a file inside a watched root gains a hard link outside every
root and is then rewritten in place through that outside name, should the
daemon catch the change within minutes, or is the hourly full backstop the
accepted bound?

Context:

- Reproduced by Sol's round-2 review (2026-10-25) on a debug build: a
  single-link file under a watched root, then `ln` to a name outside all
  roots, then an in-place rewrite through that name. Linux reports the write
  to the outside name's parent directory, which nothing watches. Several
  poll intervals passed with no refresh; status showed `polling_roots=[]`,
  `watch_uncovered=false`; the new term was not found, the old one was.
- The watcher already polls roots holding files *last seen* with more than
  one link (`watch/aliases.rs` ~25). A file seen with one link has no record,
  so a later outside link is invisible until the hourly full backstop
  (`writer.rs` ~297).
- Editors that save by writing a new file and renaming it break the link, so
  the rewrite lands on a new inode outside the root and the indexed file is
  unchanged. Only in-place writers (`>>`, `dd conv=notrunc`, some IDEs,
  databases) hit this.
- Not a publication or D73 snapshot violation: results are stale, not wrong
  about what the index holds.

| Option | Costs | Buys |
| --- | --- | --- |
| A. Accept the hourly backstop; document the bound; say that `watch_uncovered=false` cannot rule out a later outside link | Up to an hour stale in this rare case. A doc change only (~30 min). | No added background I/O. |
| B. Re-walk every fully watched root on the 5-minute poll, with the existing adaptive spacing | Background crawl duty rises for every root: ~/w is 1.8 s per walk (negligible), a `/nix/store`-sized root is ~60 s per walk, so spacing must stretch it. Changes the D116 resource tradeoff. 3–5 h plus measurement. | This case is bounded at ~5 min wherever the walk is cheap. |
| C. Scale the full backstop by each root's measured walk cost (e.g. keep background walking under ~1% of wall-clock) | Same build as B plus a per-root cost record and a cap; still an hour or more for huge roots. 4–6 h. | Cheap roots (most of `~/w`) refresh every few minutes; expensive ones keep today's cost. One knob instead of two intervals. |

**Recommendation: A** now, because the trigger is a rare combination (an
outside hard link *and* an in-place writer), and the fix costs background
I/O on every root to catch it. C is the shape to build if it ever matters.

**Fact that would change it:** a real workflow of yours that edits files
under `~/w` in place through links outside it — then C.

> Dave: A. Exceedingly rare; waiting for a full walk is fine. (2026-10-26)
> The interval of that full walk is D121.

## D121 — How often should the full backstop walk run?

**Question:** The daemon re-walks every root once an hour (`FERRET_BACKSTOP_MS`,
default 3,600,000). Should it stay hourly, become daily, or be chosen from a
measurement of what one walk costs?

Context:

- The walk stats every entry and compares `(size, mtime, ctime)` with the
  catalog; unchanged files are not opened. It runs at nice 19 through the
  paced writer.
- DESIGN's re-run figures: about 2 GiB peak at 10M entries (old generation
  plus new batches, ~120 B per entry). Nothing measures a backstop on a real
  machine: the daemon does not log its own refreshes.
- The same walk is the only bound for other changes inotify never reports:
  D120's outside hard link, permission changes without an event (LSM, group
  membership, FUSE), a mount appearing, EACCES-opaque directories. Lengthening
  it delays all of them.
- The interval is an environment variable only, not a config value.

| Option | Costs | Buys |
| --- | --- | --- |
| A. Keep 1 h | Up to 24 walks a day, each with its memory peak | Unreported changes appear within the hour |
| B. Default to daily, as a config value | Those changes wait up to a day | ~1/24 of the walk cost; tunable per machine |
| C. Measure first (wall-clock, peak RSS, bytes read on each machine), then pick | One measured walk per machine | The interval rests on a number |

**Recommendation: C, then B** unless the walk is cheap.

**Fact that would change it:** a walk on `~/w` of a few seconds and well under
100 MB makes A free; minutes or several hundred MB means B.

> Dave: agreed, C then B. Keep 1 h meanwhile; measure on the three machines.
> (2026-10-26)

Measured 2026-10-26 with the deployed build (184cca3) on a scratch index of
`~/w` (`FERRET_NO_DAEMON=1`, private `FERRET_INDEX`/`XDG_STATE_HOME`), warm
cache, GNU time for wall-clock and peak RSS:

| Host | Entries | First index | Re-walk | Re-walk at nice 19 on one CPU |
| --- | --- | --- | --- | --- |
| msa2 (32 CPUs) | 7,693 dirs, 56,941 files | 6.13 s, 112 MiB, 889 MiB read | 0.08 s, 18 MiB, 0 read | 0.25 s, 14 MiB |
| geekom (22 CPUs) | 6,377 dirs, 33,159 files | 3.44 s, 101 MiB, 531 MiB read | 0.05 s, 17 MiB, 0 read | 0.10 s, 12 MiB |
| p16s (16 CPUs) | 36,193 dirs, 189,730 files | 89.4 s, 250 MiB, 3.6 GiB read | 0.24 s, 74 MiB, 0 read | 1.16 s, 59 MiB |

p16s was measured later the same day with the build at 61411b0, once ef5dced
had unblocked its first index; its `~/w` is three times msa2's.

A re-walk of `~/w` costs at most a quarter of a second, and at most 74 MiB on
the largest tree; throttled to one CPU at nice 19 it is still under 1.2 s. By
the fact above the hourly walk is free here: **outcome A, keep 1 h.**
Cold-cache cost is not measured.

## D122 — When the index writer panics: exit, or contain it?

**Question:** When the daemon's index writer panics, should the daemon keep
exiting so the next client starts a fresh one (today, since 7fce850), or
contain the panic and keep serving?

Context, from a read of the code on 2026-10-26:

- The writer mutex is held through `refresh` (walk, hash, commit, `select`),
  `index_change`, `compact` and the content `attach`/`follow` paths, all in
  `engine.rs`. A panic there lands in the writer thread's `catch_unwind`, or
  is re-raised from a command thread, and the host drains and exits with
  status 3. The 2026-10-25 "dead name" panic (fixed in 61411b0) is the one
  seen in the field; before 7fce850 it left msa2's daemon serving a 10-hour
  stale view.
- What a user sees today: a client waiting on `index` or `roots remove` gets
  exit 3, "writer failed; retry". Queries already running finish. The next
  query answers in-process (tens of ms at `$HOME`, about 1 s at 10M) and
  spawns a fresh daemon, whose startup backstop re-walks everything (0.1-1.2 s
  for `~/w`, D121; minutes past the watch cap).
- A deterministic panic re-fires in each new daemon's startup backstop: a
  crash and respawn loop paced by queries, each paying an in-process load.
- The "writer stopped" log line drops the panic message; the payload is
  available (`panic_message`).

| Option | Costs | Buys |
| --- | --- | --- |
| A. Keep exit-on-panic; put the panic message in the "writer stopped" line | One restart per panic, as above; a deterministic panic loops. About 15 min. | Simplest: a fresh process reloads exactly what is on disk. The log names the cause. |
| B. Contain: `catch_unwind` in each writer method, rebuild the writer session from disk, clear the poison, re-select from the recovered view, retry with backoff, exit after a cap | 1-2 days plus review; a recovery path that runs only on bugs and is hard to test; while retrying, answers come from a frozen catalog, the state 7fce850 removed | No restart or reload cost; a transient panic is invisible to clients |
| C. A, plus a crash-loop guard: a daemon whose writer panics again soon after a respawn stops respawning; clients answer in-process and `status` reports the panic | About half a day | Bounds the deterministic loop; keeps A's simplicity |

**Recommendation: A.** One panic has been seen, it was deterministic, and
the restart cost on these machines is about a second.

**Fact that would change it:** a crash loop actually observed, or a
transient writer panic on a tree whose startup backstop takes minutes; then
C, and B only if transient panics recur.

> Dave: Agree. (2026-10-26)

## D123 — Keep the changed-during-read check on rg-mode scans?

**Question:** When `ferret grep` scans a directory no index covers, should
each file read keep the changed-during-read check (an fstat after the read
and a re-stat of the name, retrying on change), or skip it there to match
rg?

Context, measured on 2026-10-26 (msa2, warm cache, the private 112k-file
corpus) after ab49c3f:

- The uncovered-directory scan went from 3.6x slower than rg at 32 threads
  to level (185 ms against 176) and from 1.4x to 1.12x at one thread (889 ms
  against 794), with output identical to rg's on every row tested.
- The check is three of the four remaining syscalls per file that rg does not
  make, and most of the one-thread gap: an uncommitted build without it
  measured 837 ms against rg's 789 in the same run.
- rg has no such check: a file rewritten while it is read prints whatever
  was read. ferret retries and prints a consistent version.

| Option | Costs | Buys |
| --- | --- | --- |
| A. Keep it everywhere | About 85 ms per 112k files at one thread (~10%); nothing at 32 threads, where ferret is already level | A file rewritten mid-read is retried, never printed torn |
| B. Skip it for uncovered rg-mode scans only; `--live` and indexed reads keep it | Torn output, silently, for a file rewritten mid-read, as rg does | One thread within ~6% of rg |

**Recommendation: A.** The gap exists only at one thread, and agents run
with every core.

**Fact that would change it:** a decision that uncovered scans should match
rg's behaviour exactly rather than only its output; then B.

> Dave: what is the benefit of re-reading? If it's constantly changing,
> you'll end up re-reading forever. I think we should be erring to the truth
> when the command was called rather than when we send the response.
> (2026-10-26)

The retry is bounded: one re-read, then the file is reported as "changed
during read" and its matches are dropped. A file that is appended to without
pause, such as a live log, therefore prints an error instead of its matches,
which is further from the truth at call time than a single read. Taken as
B: uncovered rg-mode scans read each file once and print what was read.
Indexed reads keep the check, because there it decides whether the bytes
read are the catalogued version and may be cached against it; `--live` is
unchanged for now.

**Correction (Astra review of f1216a9..e384dab, 2026-10-26):** the
paragraph above, and the brief's "never printed torn", overstate the check.
Its closing fstat already accepted a file that grew or was overwritten at
the same size, so an appended-to log never failed it, and a same-size
overwrite could print torn before D123 too. What it rejected was a file
that shrank, or whose name was unlinked or replaced, during the read: those
were retried once, then reported as changed with their matches dropped.
The outcome stands. Built in 9010608; `-L` walks were missed there and are
covered in the follow-up.

## D124 — Committed `vendor/` trees: rg searches them, ferret's walks skip them

**Question:** When a repository commits a directory that ferret's seeded
global ignore excludes (`vendor/`, `build/`, `dist/`, …), should
`ferret grep` still answer as rg does and search it?

Context, from the S4 shadow log on 2026-10-26 (msa2):

- All three `differ` verdicts so far are one case: a private app
  repository commits `vendor/` (5,393 tracked files, 56 MiB), which
  `~/.config/ferret/ignore` excludes. rg searches 2,774 of those files
  (14 MiB; its hidden and nested-ignore rules drop the rest) from the repo
  root; ferret treats the directory as absent. Starting the search at `vendor/` itself works in
  both, because an explicit start bypasses ignore rules.
- Under `~/w`, that repository and its two worktrees are the only git repos
  (3 of about 45) with tracked files under any seeded name; 99.9% of those
  files are under `vendor/`. Elsewhere the names are already gitignored, so
  rg skips them too.
- A sweep later the same evening, counting the files `rg --files --hidden`
  lists under any seeded name in each git repo under `~/w`, found one more
  case of size: a second repository that does not gitignore its Rust
  `target/` (7,913 files of build output) or `.direnv/` (2). The rest are
  tiny: 31 files of another untracked `target/`, 16 under a nested
  `vendor/`, one `__pycache__/`. Only the first repository's `vendor/` is
  tracked. The shadow now logs a listing (`--files`, `-l`) that differs
  only by such names as `known:policy` rather than `differ`; lines output
  still reads `differ`.
- GREP.md already lists "Git's user-global ignores differ from ferret's
  global file" as a known difference, so this is policy, not a bug. Once
  S4 step 2 switches agents to ferret, an agent searching that repository
  gets no vendor hits and no hint that anything was skipped.

| Option | Costs | Buys |
| --- | --- | --- |
| A. Keep the policy; the shadow labels these `known:policy` | Agents silently miss committed vendor code in repos like that one | Nothing to build; a smaller index |
| B. In rg mode, scan live any directory excluded only by the global ignore file (not by git or `.ferretignore`) | A second visibility rule in the walker and index planner; every search from such a repo root rescans the tree (here 2.8k files, 14 MiB, tens of ms) | rg's answer for these trees, with the index still small; other documented differences remain |
| C. Drop `vendor/`, `build/` and `dist/` from the seed, and from your own file | Measured by indexing one checkout both ways: 1,061 → 6,160 indexed documents, 1.8 → 9.6 MB of index on disk. Three checkouts on msa2: about +15k documents and +24 MB. New setups only, unless you edit your file | rg's answer for these names with no new mechanism; gitignored build output stays excluded by `.gitignore` |

**Recommendation: C.** Where these directories are build output they are
nearly always gitignored, so the seed adds little; where they are committed
they are source, which a search should see. B is more faithful for the
names left in the seed (`node_modules/`, `target/`) but no repo here
commits those. The untracked `target/` above stays a `known:policy`
difference under C: there rg's answer is build output an agent would
rather not see.

**Fact that would change it:** a repo where a committed tree under one of
these names is large enough to matter for index size (say over 1 GB); then
B, or A with a per-repo `.ferretignore`.

> Dave: C (2026-10-27). The seed drops `vendor/`, `dist/` and `build/`, and
> msa2's ignore file loses the same three lines.

## D125 — When the agents' `rg` starts answering from ferret

**Question:** When should the stand-in `rg` switch to `FERRET_ANSWER=ferret`,
so agents get ferret's answers with real rg taking the commands ferret
refuses?

Context: S4 step 2 is built and off (ROADMAP § S4). The shadow log's only
`differ` verdicts so far are D124's committed `vendor/` tree; every
`rejected` call is either fixed or a refusal that falls back to rg.

Evidence (2026-10-27): with 2323dac deployed on msa2, which lists
`--files --sort=modified` (the Glob tool's shape) instead of refusing it,
three fresh agent runs logged 127 calls: 117 `agree`, 10 skipped (stdin,
`--version`, special paths), no `differ` and no `rejected`. Each earlier run
had 13 to 16 sort refusals.

| Option | Costs | Buys |
| --- | --- | --- |
| A. Flip after D124 is settled and a fresh day's log has no unexplained `differ` | Waits on D124 and a day of shadow traffic | Agents meet no known wrong answer on the first day |
| B. Flip now and accept the D124 gap | Agents miss committed `vendor/` code in one repository | Answer-mode traffic a day sooner |

**Recommendation: A.**

**Fact that would change it:** the repository that commits `vendor/` is one
you rarely search, which makes B's cost close to nothing.

> Dave: A (2026-10-27). Flip after C is deployed and a day's shadow log shows
> no unexplained `differ`.

## D126 — A committed `vendor/` tree's `.ignore` file

**Question:** Now that D124 indexes committed `vendor/` trees, should the
index honour `.ignore` and `.rgignore`, which rg reads and ferret does not?

Context (2026-10-27): in the one repository that commits `vendor/`, a
vendored project's `.ignore` excludes a 2,526-file source checkout. rg skips
it; ferret's indexed answer lists it, so the shadow logs `differ` there and
D125's clean day cannot happen. Under `~/w` this is the only `.ignore` or
`.rgignore` outside test fixtures. The same tree also exposed a separate
bug, fixed regardless of this answer: a gitignore `!.env.example` reveals
that hidden file to rg and to ferret's live walk, but not to the indexed
answer.

| Option | Costs | Buys |
| --- | --- | --- |
| A. One line in that repository's `.ferretignore` | Per-repo, by hand; the next such file differs again | No code; the shadow is clean tomorrow |
| B. Crawl honours `.ignore`/`.rgignore` like `.gitignore` | A policy input, a full refresh, and a design line; `--no-ignore-dot` must then bypass the index | rg's answer for every such file |
| C. Leave it; extra results are harmless | The shadow keeps logging `differ` there, so D125 needs a manual exception | Nothing to do |

**Recommendation: A** now, B if a second repository needs it.

**Fact that would change it:** agents working in repositories you don't
own, where `.ignore` files are common (B).

> Dave: B (2026-10-27). "This tool isn't just for me."

## D127 — A hidden file revealed by a `!` rule, in indexed answers

**Question:** How should indexed grep learn that an ignore file's `!` line
reveals a hidden name, as it does for rg and for ferret's live walk?

Context (2026-10-27): rg lists a hidden file or directory when the last
matching ignore rule whitelists it (`!.env.example`, `!/.claude/skills/`).
The live walk asks `DirRules` and agrees. The indexed path has no rules at
query time and the catalog keeps no flag, so it hides them. Under `~/w`, 7 of
54,932 files rg lists are revealed this way: four under `.claude/`, three
`.env.example`. Agents do Glob for `.claude/` skill files. A failing rg differential
(`-l` and `--files`) is written and waits on this answer.

| Option | Costs | Buys |
| --- | --- | --- |
| A. The crawler records a revealed-name set in the catalog | A catalog section kept through incremental refreshes, and a rebuild; the crawl asks the rules once per hidden name | Exact answers with no query cost; the set is tiny (7 entries here) |
| B. At query time, load the rules for each hidden candidate's directory | Reads ignore files on every query that meets a hidden name (most `--files` listings), so today's 2-4 ms answers slow down | No format change |
| C. Document it; the shadow labels it `known:policy` | Agents miss whitelisted dotfiles, `.claude/skills/` included | Nothing to build |

**Recommendation: A.** The crawler already decides each name with the
rules in hand, so asking whether a hidden name's deciding rule was a `!`
line costs one list lookup per hidden name at crawl time.

**Fact that would change it:** D65's catalog rework landing soon, which
would make A's format change cheaper to fold in there (A after D65).

> Dave: A (2026-10-27). Needed only under D128's `--rg`.

**Implemented (2026-10-27, d127-revealed).** Catalog format 5 adds a revealed
section and a NamePut flag, kept through refresh and compaction. Only the
repository bands count: a `!` line in `.ferretignore` or in ferret's global
ignore file reveals nothing, because rg reads neither. Git's global excludes
are still not read by the crawl, so a `!` line there reveals nothing indexed
(GREP.md § Ignore rules and visibility).

## D128 — Whose visibility rules `ferret grep` follows by default

**Question:** Should `ferret grep` follow ferret's own visibility rules by
default, with rg's exact rules only behind a flag, and which should the
agents' `rg` stand-in use?

Context (2026-10-27): Dave, on D127: "I don't think we should be trying so
hard to match rg's inclusion rules unless --live is passed." Today indexed
grep applies rg's hidden-file rule over the index: the index holds dot
files, and grep hides them unless `--hidden`, a positive glob or an explicit
start reveals them; D127 is the last gap in that. `--live` already exists
and means something else: a live walk with ferret's policy (D73). Under
`~/w`, ferret's own rules would show 938 more files than rg's (51,280 to
52,218): mostly `.ai/` notes (396), `.gitignore` files (134), `.github/`
(100) and `.claude/` (53).

| Option | Costs | Buys |
| --- | --- | --- |
| A. Ferret's rules by default; a new `--rg` flag for exact rg visibility, which the stand-in passes | Two visibility modes stay; D127 is still needed for the stand-in | People get ferret's model; agents keep rg's answers and the shadow stays a parity test |
| B. Ferret's rules everywhere, the stand-in included | Agents see dot files rg hides (about 1.8% more files here); the shadow must classify hidden-path differences as policy | One visibility mode; the held-hidden and whitelist machinery goes; D127 is moot |
| C. Keep rg's rules as the default (today) and do D127 A | The catalog change in D127 | No behaviour change |

**Recommendation: A.** Agents are the main client of the stand-in, and they
are written against rg; the flag keeps the hard parity work where it pays.

**Fact that would change it:** you're content for agents to see dot files
that rg hides. Then B is simpler and deletes code.

> Dave: A (2026-10-27).

## D129 — What opening the index costs a grep without the daemon

**Question:** Which of the index-open costs should a one-shot `ferret grep`
stop paying, and in what order?

Context (2026-10-27, measured at 4324ce4; ROADMAP S3 "In-process setup"):
on the 112k-document corpus, `grep -q -F the` without the daemon takes
67-70 ms at one thread, 35.5 ms at the default 16. `parse_index` is 34.2 ms
of it (term half 17.0, trigram half 16.3) and 49 of the 66 MB heap peak;
`ResidentNames` 6.2 ms; BLAKE3 4.3 ms; section reads 3.1 ms. Grep looks up
only trigram segments (20 lookups in a logged run, none in term segments),
but `View::open` reads, hashes and parses both halves of every pair. Every
query without the daemon pays this, and so does every daemon start.

| Option | Costs | Buys |
| --- | --- | --- |
| A. Keep as is | Nothing | Nothing; 70 ms per one-shot grep on a 112k-document index |
| B. Open each segment kind (term, trigram) the first time a query needs it | A lazy cell per kind in `View`; no format change | Measured with a temporary switch, output byte-identical: 70 → 45 ms at one thread, 35.8 → 30.5 ms at 16; heap 63.0 → 38.7 MiB |
| C. B, then a lighter resident index: keep the verified Index bytes and one sample per 16 blocks instead of parsed `Block`s | A lookup decodes a block on demand; more code in `ferret-index` | Building it measured 12.0 against 34.2 ms; heap estimated 15 against 49 MB (not measured end to end). The only option that helps the trigram half grep uses |

Separately, and with no tradeoff: drop the per-section re-hash of Sums
(0.56 ms), which the whole-Sums and head checksums already cover, and have
one-shot CLI opens skip the `ResidentNames` build as `find` already does
(estimated −6 ms, needs a raw-heap path in `NameIndex`). Verifying only
once, or only in the daemon, would save about 4 ms more and stop detecting
silent media corruption of the Index or catalog until the next rebuild;
not recommended.

**Recommendation: C, built as B first.** B is small and measured; C is where
the remaining parse time is, and D65's static dictionary (decided, not
built) only replaces the term half.

**Fact that would change it:** query logs showing most no-daemon queries are
`text:` searches, which need both halves. Then B buys nothing and C goes
first.

Built (2026-10-27). The two no-tradeoff items are on main: no Sums
re-hash, and `Engine::open_one_shot`, which grep uses so that it skips the
`ResidentNames` build and the name planner. On the corpus at one thread
`grep -q` went from 605M to 394M instructions, and from about 68 to 65 ms.
B is built on branch `perf-d129-open` (9c38677) and waits on this decision:
65 → 42 ms at one thread, 34 → 28 ms at 16, heap 61.5 → 37.2 MiB, output
identical. The behaviour B changes, which Astra's review made wider than
first stated: a damaged section of a segment kind the query never opens
is no longer reported. That means the term half for every grep, and the
trigram half for `grep -v` and for patterns with no usable trigram; those
answer correctly from the files and exit 0 instead of 2. A query that
reads a damaged half still fails with the same message. If that matters,
B can open the trigram half eagerly in indexed grep setup: that keeps
grep's diagnostics, and saves only the term half (about half of B's gain).
At 9c38677 the trigram half's `parse_index` is 38% of what remains, which
is C's target.

C built (2026-10-27, branch `perf-d129c`, from main without B, so both
halves still open; no format change). Two steps, measured on the corpus
with `grep -q -F the`:

| | instructions, 1 thread | ms, 1 thread | ms, 16 | max RSS |
| --- | --- | --- | --- | --- |
| main | 394M | 64 | 34 | 70 MB |
| C′: first terms read in place, inline order check, one- and two-byte varint fast paths | 309M | 51 | 29 | 65 MB |
| C: the Index bytes and every 16th block's place stay resident, the rest decoded on demand | 295M | 42 | 26 | 38 MB |

The block offsets could not shrink to `u32`: the writer caps only the
fingerprint section, so blocks, postings, index and term lengths are
unbounded, and `Block` was not the instruction cost in any case. That was
per-block call overhead: a `memcpy` of each first term into a growing buffer
and a `memcmp` against the last. A lookup now decodes up to 15 index entries;
on a 40-trigram literal that cost under 1M of 532M instructions. Output is
identical on 12 queries at 1 and 16 threads.

**Question (C follow-up):** Should a segment still check its block index's
term order when it opens, now that most of what remains of `parse_index` is
that check plus the walk the samples need?

| Option | Costs | Buys |
| --- | --- | --- |
| A. Check at open, as now | 33M instructions per corpus open (10% of grep `-q`) | A checksum-valid but misordered Index (writer bug, crafted file) fails at open with `Corrupt(Index)` |
| B. Check samples at open and each run as a lookup walks it | That Index can make a lookup miss a present term and return no rows instead of an error, because the walk stops at the first greater term | About 33M instructions (342 → 309M under callgrind); the walk itself, 76M, stays |

**Recommendation: A.** Only a writer bug can produce the case B stops
catching, but B turns it into silently wrong results, and 10% does not buy
that. The remaining walk could go only with a format change that stores the
samples, which D65's static dictionary will revisit for the term half.

**Fact that would change it:** a decision that checksum-valid crafted files
are outside the read contract, as `segment/mod.rs` already says for a list
that panics inside intpack. The writer already refuses out-of-order terms
(`WriteError::TermOrder`), so B's only remaining exposure would be a bug in
how the writer encodes the index.

## D130 — Whether `ferret grep --files` walks in parallel

**Question:** Should an uncovered `--files` walk run on several threads, as
content search already does, at the cost of a fixed output order?

Context (2026-10-27): after 90cb8bc the default uncovered walk matches
`--rg`'s speed at one thread, but `--files` is still a serial walk. On deep
trees it is about 6× behind `rg -j32` (154 against ~25 ms), and ahead of
`rg -j1`.

| Option | Costs | Buys |
| --- | --- | --- |
| A. Keep it serial | 6× behind rg on deep trees | Deterministic order at any thread count |
| B. Parallel when threads > 1 | Order becomes nondeterministic, as rg's `--files` and ferret's own content output already are; a second walker integration in `names.rs` | Probably close to rg (unmeasured) |
| C. Parallel, reordered to serial order before printing | Holds every name (2.6 MB for 112k names) or needs an ordered merge; first output waits for the walk | Today's output, faster |

**Recommendation: B**, matching content search and rg; `--sort path` stays
the way to get a fixed order.

**Fact that would change it:** any consumer (test, skill, script) relying on
`--files` order without `--sort`.

Measured (2026-10-27, branch `d130-b`, built as B): cargo registry, 8,533
directories. Changes that do not depend on this decision are on main: an
idle-offer backoff in the scheduler, a depth-indexed live policy, and
reserving the descriptor table at process start, which removed a 20 ms RCU
wait. With all of those:

| `--files` | serial (main) | B | rg |
| --- | --- | --- | --- |
| 1 thread | 106 ms | 107 ms | 182 ms |
| 8 threads | 106 ms | 22 ms | 33 ms |
| default threads | 106 ms | 19 ms | 30 ms |

One test read walk order from `--files`; it now pins one thread. The
shadow and differential tests already sort. Astra reviewed B and its two
findings are fixed: a missed operand-boundary flush, and a scheduler
backoff that could delay a loan.

Re-measured (2026-10-27, branch `d130-b2`: B rebased onto D132 and the
cg-futex merge, gates green). The same names at 1, 8 and 32 threads:

| `--files` | serial (main) | B | rg |
| --- | --- | --- | --- |
| 1 thread | 77.9 ms | 77.8 ms | 169.3 ms |
| 8 threads | 77.7 ms | 20.1 ms | 28.7 ms |
| 32 threads | 79.3 ms | 25.5 ms | 20.8 ms |
| default threads | 77.8 ms | 17.8 ms | 24.7 ms |

## D131 — Whether a crawl hands file bytes to the content index

**Question:** Should the crawl hand the bytes it read for hashing straight
to the content indexer, instead of the follow pass reading each document
again?

Context (2026-10-27, branch perf-t2-crawl): the crawl reads every file to
hash it (53,816 under `~/w`), and the content follow then reopens and
rereads the 11,912 documents. It is the largest remaining full-crawl cost
after t2 cut the syscalls per directory from 43.2 to 34.2.

| Option | Costs | Buys |
| --- | --- | --- |
| A. Keep the second read | 11,912 extra opens and reads per full crawl, mostly from page cache | The crawl and the content index stay independent; memory stays bounded by one file |
| B. Hand bytes over for documents under a size bound (say 256 KiB), reread above it | Buffered bytes in flight between two stages; the handoff joins `ferret-crawl` to the indexer's pipeline | Most of the second read on a cold cache, where it is a disk read |
| C. Index content inside the crawl's read | One pass; the crawl crate learns about tokens, against the crate graph | The whole second read |

**Recommendation: A for now.** On a warm cache the reread is cheap and the
wall-clock gain was inconclusive; B is worth it only if cold-cache crawls
matter.

**Fact that would change it:** a cold-cache full crawl (first index, or after
reboot) where the reread shows as a measurable share of wall time.

## D132 — A walked directory's rules load at its first entry

**Decided (2026-10-27, branch perf-policy-listing):** grep's live walk loads a
directory's ignore rules when the walk yields that directory's first entry,
not when its parent accepts it. By then the walk has listed the directory and
holds it open. Each ignore input is read through that handle, and only the
inputs its listing held are opened. Before this, every walked directory cost
an open of its path plus a probe per input name: four `newfstatat` calls, and
five inside a work tree.

**Measured** on `~/.cargo/registry/src` (8,533 directories), hyperfine medians
in ms:

| Row | Before | After |
| --- | --- | --- |
| `--files`, one thread | 107.3 | 81.8 |
| `-c xylophone`, one thread | 427.1 | 399.2 |
| `-c xylophone`, 8 threads | 83.6 | 76.8 |
| `-c xylophone`, default threads | 63.6 | 58.6 |

On content grep, syscalls fall from 283k to 232k, and the 34k `newfstatat`
probes go entirely. `--files` now retains parent handles while a live policy
is in force.

**What stays as it was:**
- A fault loading a directory's rules still names that directory and skips
  its entries.
- A bad pattern still warns once, even when no entry of the directory is
  selected.
- The load is guarded so workers that split a directory's listing would share
  it. A review found no split today: a directory's first entry always reaches
  the policy in the step that lists it, before any loan.

Start operands and their ancestors are still probed by name, because grep may
search an ancestor it cannot list.

**What changed:** a directory the walk cannot list (mode `0311`, say) yields
no entries, so its rules never load. A bad ignore pattern in it used to warn
beside the listing fault; now only the fault is reported, as rg reports it.

## D133 — Grep's collector keeps its wakeups

**Decided (2026-10-27):** content grep's completion protocol is unchanged.
A study (branch `perf-cg-futex`) asked whether collector wakeups explain why
`grep -c` trails `rg -j32` (about 70 against 44 ms on the cargo registry).

They explain the futex calls: 57,372 per run at eight threads, almost all
from the publisher's channel waits and the workers' wakes. Dropping empty
plain completions and futile walk loans cut that to 892, and voluntary
context switches from about 11,000 to 560. Wall time at 32 threads did not
move, and system time rose, so none of the runtime candidates was kept.
Removing the per-file `fstat` made no consistent difference either.

**What was kept:** a test through the real group engine that a completed
count, whose capture weight is zero, still publishes while its worker reads
a slow file. Gating the idle handoff on captured bytes strands it.

**Where the gap is:** at 32 threads ferret spends about 1,070 ms of system
time to rg's 820, with fewer syscalls (294k against 274k under strace,
mostly the same opens, reads and closes). User time and wakeups are not
it: a reused probe buffer, which removed the allocator's consolidation pass
from the profile, changed wall time by under 2%. Kernel profiling needs
`perf_event_paranoid` at 1 or lower; at the default thread count the gap is
58.6 against 53 ms.

## D134 — Live lookups resolve beneath retained handles, never through /proc

**Decided (2026-10-27):** every per-entry lookup in the live walk is an
`*at` call on the retained parent directory (or the cwd handle): `openat`
to descend, `statx` for metadata, `readlinkat`, `faccessat`, and `openat`
plus a listing for `-empty`. Before, each one rebuilt a
`/proc/self/fd/N/name` path for std's path APIs, which costs a full path
resolution through procfs per call.

| `find -I` on the cargo registry (48k files) | before | after | GNU |
| ------------------------------------------- | -----: | ----: | --: |
| `-type f -size +100k`                       |  100.9 |  16.5 | 115.3 |
| `-empty`                                    |  115.7 |  24.1 | 145.9 |
| `-readable`                                 |   90.3 |  17.9 |   — |

Times are ms. `grep --files` at one thread went from 77.0 to 67.3 ms.

`walk::Metadata` is now ferret's own struct, filled from `statx` or, where
the kernel or a seccomp filter refuses `statx` with ENOSYS, from `fstatat`,
as std did. `statx` carries `AT_NO_AUTOMOUNT`, matching GNU's `fstatat`.
The procfs hop also used up one of Linux's 40 symlink expansions; a start
at that limit now resolves as GNU's does. Astra's two review rounds found
the missing fallback and a listing that needed search permission; both are
fixed and tested.

**Still through /proc:** `-printf %F` (statfs, per printed entry) and the
`-newer` family's reference operands (once per operand).

