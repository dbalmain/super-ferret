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

| Id  | Question                                                 | Status         | Answer                                                                                                         |
| --- | -------------------------------------------------------- | -------------- | -------------------------------------------------------------------------------------------------------------- |
| D1  | Repository shape                                         | answered       | A: one repo, workspace under `crates/`; crate boundaries get the most design thought                           |
| D2  | Format of the living documents                           | answered       | A: Markdown living docs; research HTML under `docs/research/`; intpack pages copied to `docs/intpack/`         |
| D3  | Build order: index first, or the no-index tool first     | answered       | B: usable tool first, to start collecting data                                                                 |
| D4  | What identifies a document                               | answered       | ordinal doc ids in add order; doc → hash → inodes → names (restatement confirmed)                              |
| D5  | Where the mutable state (paths, inodes) lives            | answered       | A: own catalog; memory budget configurable, set by experiment                                                  |
| D6  | What the index holds: postings, filters, positions       | answered       | every structure is a candidate filter, verified by scan; trade-offs by experiment                              |
| D7  | Positions                                                | merged into D6 |                                                                                                                |
| D8  | Regex at first ship                                      | answered       | not a bare scan: trigram filters (B) or postings (C), by experiment                                            |
| D9  | What a term is                                           | answered       | B: identifier splitting, filenames especially                                                                  |
| D10 | Which roots                                              | answered       | A: configured roots; `.gitignore` respected, `.ferretignore` and global overrides                              |
| D11 | `unsafe` posture and the intpack dependency              | answered       | flexible: no blanket `forbid`; SIMD where it pays; intpack may be vendored                                     |
| D12 | Licence                                                  | answered       | A: `MIT OR Apache-2.0`                                                                                         |
| D13 | Ignore rules: precedence, and whose matcher              | answered       | A: `ignore` crate behind `DirRules::decide`; `!` un-ignores over an ancestor `.ferretignore` or a `.gitignore` |
| D14 | Filename search: scan the names, or index them           | answered       | C, scan first; an optional resident daemon keeps names warm                                                    |
| D15 | Result unit: per path or per document                    | answered       | per path; a view may group (e.g. image search, once per content)                                               |
| D16 | Replace `ignore` with our own gitignore matcher          | adopted        | A, adopted: own matcher in `ferret-policy`, no dependencies; at or below `ignore` on every measured rule set   |
| D17 | Whose regex engine, and when                             | answered       | A: `regex` executes behind a narrow trait; choose A/B/C at S3 on verification share of latency                 |
| D18 | Symlinks: catalogue as links, and what they match        | deferred       | links catalogued as links now (target text stored); reverse map and content matches later; no pull-in          |
| D19 | Ignore matching: whole paths, or per-directory rule sets | answered       | B-flat-indexed: one shared, indexed rule list per directory; last match wins                                   |
| D20 | Walk across mount points, or stay on the root's device   | answered       | A: cross mount points below a root, as now                                                                     |
| D21 | Walk by path, or by directory handle                     | answered       | B: `rustix` handles for every operation below the root, now                                                    |
| D22 | A root inside a git work tree                            | answered       | B: the enclosing work tree's `.gitignore` and exclude apply; a `.ferretignore` above the root does not         |
| D23 | Recording work trees, so duplicate results can be hidden | answered       | A: a `worktrees` table; collapse identical copies by `DocId` by default                                        |
| D24 | How many walk workers by default                         | answered       | B: `min(16, cores)`, fastest cold (2.50 s vs 3.08 s at 8); `default_workers()`                                 |
| D25 | A configured root that git ignores                       | answered       | B: a configured root is always walked; rules apply below it                                                    |
| D26 | A re-run: rebuild the snapshot, or mutate through a log  | open           |                                                                                                                |
| D27 | `InoId` and `NameId`: stable, or renumbered              | open           |                                                                                                                |
| D28 | Name layout: raw sorted by parent, or front-coded        | open           |                                                                                                                |
| D29 | How a parallel walk tells the catalog each parent        | open           |                                                                                                                |
| D30 | What `ferret find` builds when it opens the catalog      | open           |                                                                                                                |
| D31 | One inode, several names                                 | open           |                                                                                                                |
| D32 | A reader while `ferret index` runs                       | open           |                                                                                                                |
| D33 | What the walker must also hand the catalog               | open           |                                                                                                                |

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
on this machine catalogues **434,846 entries** (76,789 directories, 354,383
content-indexable files, 3,674 catalogue-only), with **10.2 MB of name bytes,
23.4 B per name**, and no I/O faults and no traversed directories. Full relative
paths would be 43 MB. Everything else below marked an estimate is one.

Reviewed by Sol (gpt-6-sol) before reaching you; its findings are folded in.

Settled without a brief (object if wrong):

- **Hashing uses the `blake3` crate** (`CC0-1.0 OR Apache-2.0`), in
  `ferret-crawl`, as D4 recommended and its restatement confirmed. That adds a
  `blake3` edge to DESIGN § Crates in the same commit. Only files the policy
  sends to the index are hashed; catalogue-only files (binary, over the size
  cap) have no `DocId`.
- **`DocId` is never renumbered by the catalog.** Postings are keyed by it (D4);
  only an index merge may reclaim a dead doc, and that is S2's business.
- **New `DocId`s are assigned in the order hashing finishes.** D4 expected the
  first crawl to be close to path-sorted; `walk_parallel` gives no cross-worker
  order, so it is not. Revisit if an S2 postings measurement shows the lost
  locality costs bytes.

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

| Option                                                                                                                                                                                                                          | Costs                                                                                                                                                                                                                                                                                            | Buys                                                                                                                                                                             |
| ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| A. Rebuild and swap: the walk writes a new snapshot. A file's hash and `DocId` carry over when `(dev, ino, size, mtime, ctime)` match the old row. A directory whose listing faulted keeps its old children and their subtrees. | Writes the whole snapshot every run: at least 36 MB for `$HOME` and 83 MB at 1M entries (estimate, a floor: 48 B per inode plus 12 B per name row plus name bytes). Transient maps over the old snapshot for `(dev, ino)` and hash lookups, whose peak RSS must be measured against D5's budget. | No log format, no replay, no sweep. Deletion is absence, except under a faulted directory. The log arrives with the daemon, over a snapshot format already settled and measured. |
| A′. As A, but a crawl with any I/O fault publishes nothing and keeps the old snapshot                                                                                                                                           | A root with one permanently unreadable directory (common under `/`, zero under `$HOME` today) never updates.                                                                                                                                                                                     | The simplest correct rule.                                                                                                                                                       |
| B. Mutate: diff the walk against the catalog, append a record per change, compact when the log grows                                                                                                                            | The log format, replay, torn-tail recovery, compaction, and a sweep for entries the walk did not visit, which needs the same faulted-directory rule as A. All written in S1 and exercised only by full re-crawls until S5.                                                                       | One mechanism from the start. A re-run that changed little writes little.                                                                                                        |

**Recommendation:** A′ for S1, moving to A once measured fault frequency
justifies typed faults and subtree reconciliation. The log is the daemon's
requirement, and designing it before the daemon means designing it without the
workload that shapes it (small inotify bursts, not whole-tree diffs). A
forecloses nothing: B's log is an addition over A's snapshot. A′ is the only
variant that is correct with today's untyped faults; A's carry-forward needs
each fault to say which operation failed on which entry. The facts that would
change it: roots with permanent faults (anything under `/`), which make A′ never
publish and so make A necessary; or a measured snapshot write that a person
would notice on a re-run, over about a second at 1M entries, which favours B.

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

**Recommendation:** A, and measure B on `$HOME` before the format is frozen: net
snapshot bytes and scan latency together, since density comes first in this
project. The fact that would change it: B saving enough of the whole snapshot to
matter at a scan cost that stays within the name-query target.

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

## D30 — What `ferret find` builds when it opens the catalog

**Question:** Are the lookup structures DESIGN says are "derived at load" —
`hash → DocId`, `DocId → [InoId]`, `InoId → [NameId]`, and each directory's work
tree — rebuilt by every reader, or stored in the snapshot?

The name scan's target is single-digit milliseconds warm (estimate), and every
`ferret find` without the daemon pays whatever opening costs.

| Option                                                                                                                                                                                                            | Costs                                                                                                                                                                                                                  | Buys                                                                                                                        |
| ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | --------------------------------------------------------------------------------------------------------------------------- |
| A. Rebuild all of them on open (DESIGN now)                                                                                                                                                                       | A hash map of up to 354k entries (one per distinct content; the file count is the upper bound) and three inverse arrays on every invocation: tens of milliseconds at 1M (estimate), which would dominate a name query. | The smallest file. Nothing to keep consistent.                                                                              |
| B. Store the query-path structures as arrays in the snapshot (`DocId → [InoId]` and `InoId → [NameId]` as offset-plus-list arrays, the work tree per directory); build the `hash → DocId` map only in the indexer | About 8 B per entry more on disk (estimate), all sequential and mapped rather than read.                                                                                                                               | Opening is an `mmap`; a name-only query touches the heap and the rows it hits. The hash map is paid only by `ferret index`. |
| C. Build each lazily, on the first query that needs it                                                                                                                                                            | The first content query pays A's cost for its structure.                                                                                                                                                               | A name-only query pays nothing, without growing the file.                                                                   |

**Recommendation:** C for S1. With D31 C, a name-scan hit already identifies its
`NameId`, and its row gives the inode and the one parent chain, so a name-only
`find` needs none of these structures. `DocId → [InoId]` and the full inverse
serve `stats` and, from S2, content results; they are built when first needed,
and one is persisted when a measured query shows its build dominating. The fact
that would change it: a measured S1 query whose latency is mostly that build,
which moves that structure to B.

## D31 — One inode, several names

**Question:** When hard links or a bind mount give one `(dev, ino)` several
names, is that one `inodes` row or one per name?

DESIGN already implies one row with several `names` edges, but also says a
directory has exactly one name, which a bind mount below a root breaks (D20: the
walk crosses mounts). The prevalence of either on `$HOME` is not measured.

| Option                                                                 | Costs                                                                                                                                                                                                                   | Buys                                                                                                                             |
| ---------------------------------------------------------------------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | -------------------------------------------------------------------------------------------------------------------------------- |
| A. One row per `(dev, ino)`, several `names` edges                     | The merge (D29) must unify rows by `(dev, ino)` across workers. A directory with two names has two parent chains, so a path is no longer unique per directory row; a path walk must start from the name, not the inode. | Hashed once; a doc's liveness counts distinct inodes. DESIGN's model as written for files.                                       |
| B. One row per observed name                                           | Several rows for one inode, each hashed unless the carry-over catches it, and doc liveness counted per name.                                                                                                            | Every row has exactly one path, so directories keep the one-name rule.                                                           |
| C. A for files; B for directories (a directory seen twice is two rows) | Two rules instead of one.                                                                                                                                                                                               | Hard-linked files share one row and one hash, and a directory row keeps one path, so work-tree membership stays one parent walk. |

**Recommendation:** C. Hard-linked files are what dedup is for; aliased
directories are rare and mostly bind mounts, where two independent subtrees are
the honest result (D15 is per path). This identity rule governs D26 and D29: the
merge deduplicates **files** by `(dev, ino)`, which is also the hash carry-over
key; directory rows are not unique by `(dev, ino)`, and an old directory row is
found by its path edge (parent row and name) plus its identity. The fact that
would change it: a census showing many aliased directories **and** a way to give
each alias its own path context under one row; without the second, A cannot
represent them.

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
`fstat`s of the open file: a changed `(size, mtime, ctime)` retries once, then
reports a fault and leaves the file unhashed for this run, rather than recording
a hash that matches neither version.

**Recommendation:** A, with the stat-bracketed hash. It keeps D21's guarantee
end to end, and the walker's `.git` probe is the natural place to classify work
trees. The fact that would change it: hashing inside a walker callback
measurably defeating crawl throughput, which would change how handles are passed
(a queue of opened descriptors for hashing workers) while keeping
handle-relative opens.
