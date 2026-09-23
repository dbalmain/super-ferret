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

| Id  | Question                                             | Status         | Answer                                                                                                         |
| --- | ---------------------------------------------------- | -------------- | -------------------------------------------------------------------------------------------------------------- |
| D1  | Repository shape                                     | answered       | A: one repo, workspace under `crates/`; crate boundaries get the most design thought                           |
| D2  | Format of the living documents                       | answered       | A: Markdown living docs; research HTML under `docs/research/`; intpack pages copied to `docs/intpack/`         |
| D3  | Build order: index first, or the no-index tool first | answered       | B: usable tool first, to start collecting data                                                                 |
| D4  | What identifies a document                           | answered       | ordinal doc ids in add order; doc → hash → inodes → names (restatement confirmed)                              |
| D5  | Where the mutable state (paths, inodes) lives        | answered       | A: own catalog; memory budget configurable, set by experiment                                                  |
| D6  | What the index holds: postings, filters, positions   | answered       | every structure is a candidate filter, verified by scan; trade-offs by experiment                              |
| D7  | Positions                                            | merged into D6 |                                                                                                                |
| D8  | Regex at first ship                                  | answered       | not a bare scan: trigram filters (B) or postings (C), by experiment                                            |
| D9  | What a term is                                       | answered       | B: identifier splitting, filenames especially                                                                  |
| D10 | Which roots                                          | answered       | A: configured roots; `.gitignore` respected, `.ferretignore` and global overrides                              |
| D11 | `unsafe` posture and the intpack dependency          | answered       | flexible: no blanket `forbid`; SIMD where it pays; intpack may be vendored                                     |
| D12 | Licence                                              | answered       | A: `MIT OR Apache-2.0`                                                                                         |
| D13 | Ignore rules: precedence, and whose matcher          | answered       | A: `ignore` crate behind `DirRules::decide`; `!` un-ignores over an ancestor `.ferretignore` or a `.gitignore` |
| D14 | Filename search: scan the names, or index them       | answered       | C, scan first; an optional resident daemon keeps names warm                                                    |
| D15 | Result unit: per path or per document                | answered       | per path; a view may group (e.g. image search, once per content)                                               |

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

| Option                                                                                                                                                                        | Costs                                                                                                                                                                                                                                                                                                                                                                  | Buys                                                                                                                                                                                                                                                                                                                              |
| ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| A. Content-addressed: doc id ← (content hash, extractor version, tokenizer version). `(dev, ino) → doc`. `(parent ino, name) → child ino`. Paths resolved by walking parents. | Every indexed file is hashed in full (it is being read in full to tokenize, so the marginal cost is the hash). A result is a (doc, path) pair, not a path: a doc with three names is three results. Metadata predicates (`ext:`, `path:`, `mtime:`) live on the inode/name, not the doc, so composing them with term postings needs a doc↔inode mapping at query time. | Rename or move of a file or a whole directory is one row update; a cross-filesystem move (copy + delete) reuses the doc. Duplicate content is indexed once. A changed tokenizer is a reindex keyed by version, not a migration. Inode reuse (a new file landing on a recycled number) is detected by hash mismatch, not by trust. |
| B. Inode-addressed: doc id ← `(dev, ino)`; hash kept only to skip re-tokenizing unchanged content                                                                             | A duplicate file is indexed twice. Cross-filesystem moves reindex. Inode reuse after delete is a correctness hazard: `(dev, ino)` alone is not an identity on Linux — it needs `ctime` or a generation number alongside.                                                                                                                                               | Simpler results: one doc, one path. Simpler query-time composition.                                                                                                                                                                                                                                                               |
| C. Path-addressed                                                                                                                                                             | Every rename is a reindex — the thing the chain exists to avoid.                                                                                                                                                                                                                                                                                                       | Nothing.                                                                                                                                                                                                                                                                                                                          |

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

## Settled without a brief (object if wrong)

- The CLI emits JSON lines behind a flag from the first slice, with stable exit
  codes and byte offsets in hits, because the agent skill is the second consumer
  and the first that cannot read prose.
- A local query and timing log exists from the first slice; the opt-in upload is
  a later slice over the same records.
- One bench run at a time on msa2; measurement hygiene as in
  `intpack-bench/README.md`.
- No `git push` and no `Cargo.toml` edits by offloaded agents.
