# Decisions

The living decision record for Super Ferret. Every open question is written in
one shape — question, named options, the tradeoff per option, a recommendation
and the one fact that would change it — and answered questions stay here with
their answer, so the record survives the conversation that produced it.

Predecessors, carried forward where still open:

- `claude/architecture.html` § Open questions (Q1–Q7, 2026-09-05)
- `grok/decisions.html` (survey scoping, 2026-09-04)

## Status

| Id  | Question                                             | Status | Answer |
| --- | ---------------------------------------------------- | ------ | ------ |
| D1  | Repository shape                                     | open   |        |
| D2  | Format of the living documents                       | open   |        |
| D3  | Build order: index first, or the no-index tool first | open   |        |
| D4  | What identifies a document                           | open   |        |
| D5  | Where the mutable state (paths, inodes) lives        | open   |        |
| D6  | Postings versus filters, and how that gets decided   | open   |        |
| D7  | Positions                                            | open   |        |
| D8  | Regex at first ship                                  | open   |        |
| D9  | What a term is                                       | open   |        |
| D10 | Which roots                                          | open   |        |
| D11 | `unsafe` posture and the intpack dependency          | open   |        |
| D12 | Licence                                              | open   |        |

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

## D6 — Postings versus filters, and how that gets decided

**Question:** For the term → candidate-documents structure over tier-1 text,
what is compared, where, and when?

The candidates: (i) term → doc-id postings (intpack `pfor128skip` or Elias-Fano,
both in intpack); (ii) a per-document filter over its terms (bloom or binary
fuse, ~9 bits/key) with a scan of the survivors; (iii) per-block filters over
the concatenated compressed text, VictoriaLogs-style. The research's
`grok/decisions.html` last question and `claude/architecture.html` § What was
rejected both argued (i) with a filter gate in front — Splunk's layering — but
argued it from vendor figures; you want it measured.

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

## D8 — Regex at first ship

**Question:** Regex over content is a hard requirement. At first ship, is it
answered by a scan, by a trigram tier, or by per-file filters (architecture Q1)?

| Option                                                                                                                                                                           | Costs                                                                                                                         | Buys                                                                                                          |
| -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ----------------------------------------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------- |
| A. Scan path: walk the catalog (already exclusion-filtered, no directory traversal), run the regex over file bytes in parallel; ripgrep-class, 50 ms warm on the measured corpus | Cold cache is unmeasured. No index-side narrowing.                                                                            | The requirement is met in slice 1 with no new structure. The verifier is needed by every other option anyway. |
| B. Per-file binary fuse filter over trigrams as a gate in front of A                                                                                                             | ~31 MB on the measured corpus; the Cox regex→trigram derivation has to be written (the `grok/cox-trigrams` course covers it). | Skips most files without opening them — the cold-cache win at 12% of the trigram tier's cost.                 |
| C. Full trigram postings (T2)                                                                                                                                                    | ~252 MB estimate, 3.1× the entire term index; a selectivity estimator and cost model.                                         | Candidate narrowing inside surviving files.                                                                   |

**Recommendation:** A in slice 1, with B as the first experiment row after the
stage-0 cold measurement. The fact that would change it: a cold scan of the 1.6
GB text tier above ~5 s makes B a slice-1 item; under ~1 s, B is dropped too.

## D9 — What a term is

**Question:** For tier-1 text (code, config, prose), what does the tokenizer
emit?

| Option                                                      | Costs                                                                 | Buys                                                                            |
| ----------------------------------------------------------- | --------------------------------------------------------------------- | ------------------------------------------------------------------------------- | ---------------------------------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------- |
| A. Maximal runs of `char::is_alphanumeric()                 |                                                                       | '\_'`, lowercased; no stemming, no splitting; ASCII-only fast path. `std` only. | `fooBar` and `foo_bar` are one term each; `TODO` and `todo` are one term. Prose stemming absent (`index` ≠ `indexes`). | Zero dependencies; deterministic; the term dictionary stays small on code, where the identifier is the query. |
| B. A plus identifier splitting: emit `fooBar`, `foo`, `bar` | Roughly 1.5–2× the postings on code (estimate — a bench row).         | Sub-word search on identifiers.                                                 |
| C. Language-aware (tree-sitter)                             | A large dependency with per-language grammars; a plugin-tier concern. | Symbols versus comments versus strings.                                         |

**Recommendation:** A, with the tokenizer version in the doc identity (D4) so B
is a reindex of tier 1, not a format change. The fact that would change it: the
query log, once it exists — if sub-identifier queries are common, B.

## D10 — Which roots

**Question:** Configured roots, `$HOME`, or `~/w`? (Architecture Q2.)

| Option                                                                           | Costs                                                                                                 | Buys                                             |
| -------------------------------------------------------------------------------- | ----------------------------------------------------------------------------------------------------- | ------------------------------------------------ |
| A. Configured roots, default `$HOME` minus a built-in denylist plus `.gitignore` | One config file. `$HOME` is unmeasured: R8 counted ~1.55M directory entries, 2.7× `~/w`'s file count. | Right on both a dev tree and a document archive. |
| B. `~/w` only                                                                    | Every number in the research applies, and the answer is that grep already wins.                       | Certainty.                                       |

**Recommendation:** A, and run M1's census over `$HOME` in the stage-0 day
(D3/C). The fact that would change it: nothing — but the census decides whether
the extracted-document tier is empty or the product.

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

## D12 — Licence

**Question:** "As open a licence as possible" — which?

| Option                   | Costs                                                                              | Buys                                                         |
| ------------------------ | ---------------------------------------------------------------------------------- | ------------------------------------------------------------ |
| A. `MIT OR Apache-2.0`   | Attribution required.                                                              | The Rust convention; matches intpack; Apache's patent grant. |
| B. `0BSD` or `Unlicense` | No patent grant; some corporate policies reject public-domain-equivalent licences. | No attribution, nothing to comply with.                      |

**Recommendation:** A, for consistency with intpack and intpack-bench. The fact
that would change it: a stated wish for public-domain-equivalent terms.

---

## Settled without a brief (object if wrong)

- The CLI emits JSON lines behind a flag from the first slice, with stable exit
  codes and byte offsets in hits, because the agent skill is the second consumer
  and the first that cannot read prose.
- A local query and timing log exists from the first slice; the opt-in upload is
  a later slice over the same records.
- One bench run at a time on msa2; measurement hygiene as in
  `intpack-bench/README.md`.
- No `git push` and no `Cargo.toml` edits by offloaded agents.
