# R5 — Prior-art index formats: what they chose, what it cost

**Reframe note:** Dave has decided to build the index layer himself — this is no
longer a buy-vs-build comparison. He wrote Ferret (the Ruby port of Lucene,
github.com/dbalmain/ferret) and already knows Lucene's architecture — segments,
BM25, merge policy — from the inside, so none of that is repeated here. This
file's job is the design-decision ledger: for each engine, what format did it
pick, what did that choice cost in bytes and latency, and what would he do
differently. Embeddability is demoted to a one-line footnote per engine, licence
is recorded because permissive-vs-GPL determines whether a design is "readable
intel" or "vendorable code," and everything is ranked first on **bytes per
indexed token** (his stated preference: "I'd go for slightly slower search
performance for a more efficient denser index"), not on QPS.

Scale: ~1M files, tens to a few hundred GB, single Linux workstation.
Requirements: boolean, phrase, **true regex over content**, near-real-time
incremental updates, low idle resource use.

**Central distinction used throughout this doc, because most of the report
depends on it:**

- **Regex over the term dictionary** — the query engine walks the _vocabulary_
  (the sorted/FST-encoded list of distinct terms) with a regex/automaton and
  turns matching terms into an OR of exact-term postings lookups. This is what
  Tantivy's `RegexQuery`, Lucene's `RegexpQuery`, and Xapian's wildcard support
  do. It is **fast** (proportional to vocabulary size, not corpus size) but it
  only ever matches whole _tokens_ as the tokenizer produced them — no
  cross-token spans, no matching inside a word if the tokenizer split it
  differently, no case/punctuation the analyzer stripped, and it plain doesn't
  work if the pattern crosses what became separate terms.
- **Regex over document content** — the query actually inspects the original (or
  near-original) byte/character stream of each candidate document, as
  `grep`/`ripgrep` does. This is what a trigram index (SQLite FTS5 `trigram`
  tokenizer, or a custom n-gram index) makes _tractable_ by using the index only
  to shortlist candidate documents, then running the real regex engine over
  their content. None of the classic inverted-index engines below do this
  natively; they'd need this bolted on (trigram tokenizer, or a codec-level
  content store you regex separately).

A tool advertising "regex search" that only does the first kind will silently
fail on patterns spanning tokenizer boundaries, case folding, or stemmed forms —
worth stating explicitly in any spec Dave writes.

## Comparison table

| Engine                       | Lang                 | Licence                                        | Embeddable?                                                                                             | Last release checked                                                                                                | Term dict                                                                    | Postings codec                                                                                               | Regex: dict or content?                                                                | Size ratio                                                                                                                         | Verdict                                                                                                                                                                                                |
| ---------------------------- | -------------------- | ---------------------------------------------- | ------------------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------- | ---------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------ | -------------------------------------------------------------------------------------- | ---------------------------------------------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ |
| **Tantivy**                  | Rust                 | MIT                                            | Library (also has server wrappers)                                                                      | v0.24 area, active, 2025 [community-anecdote — see below]                                                           | FST (`fst`/`tantivy-fst`)                                                    | Blocked bitpacking, 128-doc blocks, delta+bitpack docids, bitpack freqs                                      | Dictionary (via `tantivy-fst`+`regex`)                                                 | Not officially published; community reports ~1.1–1.4x of Lucene's ratio [community-anecdote]                                       | **Strong candidate**                                                                                                                                                                                   |
| **fst (BurntSushi)**         | Rust                 | MIT/Unlicense                                  | Library (pure data structure, not a search engine)                                                      | active                                                                                                              | FST itself                                                                   | N/A (no postings; it's the dict layer)                                                                       | Automaton traversal (regex-automata, Levenshtein) over the FST directly                | N/A                                                                                                                                | Building block, not an engine                                                                                                                                                                          |
| **Quickwit**                 | Rust (on Tantivy)    | AGPL/Business Source-ish (check per version)   | Server-first, split/object-store oriented                                                               | active                                                                                                              | Tantivy's FST                                                                | Tantivy's codec, "split" files on S3/object store                                                            | Dictionary                                                                             | designed for cold storage, not tiny local index                                                                                    | Wrong shape (distributed, not embedded)                                                                                                                                                                |
| **Toshi**                    | Rust (on Tantivy)    | MIT                                            | Server (HTTP wrapper over Tantivy)                                                                      | effectively unmaintained, last meaningful activity ~2021 [community-anecdote]                                       | Tantivy's                                                                    | Tantivy's                                                                                                    | Dictionary                                                                             | same as Tantivy                                                                                                                    | Dead — don't use                                                                                                                                                                                       |
| **Sonic**                    | Rust                 | MPL-2.0                                        | Server (own protocol), tiny                                                                             | last tagged release 2021-ish, low activity since [community-anecdote]                                               | custom trie-ish, no phrase/BM25, more like autocomplete/tag store            | N/A — not classic postings                                                                                   | none (no regex)                                                                        | intentionally minimal                                                                                                              | Wrong tool (not full-text search, more a fuzzy autocomplete index)                                                                                                                                     |
| **Lnx**                      | Rust (on Tantivy)    | AGPL                                           | Server (HTTP)                                                                                           | intermittent maintenance, sparse commits in recent years [community-anecdote]                                       | Tantivy's                                                                    | Tantivy's                                                                                                    | Dictionary                                                                             | same as Tantivy                                                                                                                    | Not needed — wraps Tantivy with extra ops weight you don't want in a desktop binary                                                                                                                    |
| **Pagefind**                 | Rust→WASM            | MIT                                            | Library-ish (JS/WASM consumer, not a Rust embed)                                                        | active, used widely for static sites                                                                                | custom, index chunked alphabetically, per-page "fragment" files              | custom compact binary, gzip'd chunks                                                                         | none (prefix/substring, not regex)                                                     | fragments ~1–10KB, chunks ~40KB [community-anecdote]                                                                               | Wrong shape — browser/static-site design, not a desktop file-search backend, but its **partition-by-first-letters-and-lazy-fetch** idea is worth stealing for a "load only what you need" index layout |
| **Tinysearch**               | Rust→WASM            | MIT/Apache-2.0                                 | Library-ish (WASM)                                                                                      | low activity since ~2020 [community-anecdote]                                                                       | small custom, optimized for byte-size not scale                              | simple                                                                                                       | none                                                                                   | tuned for tiny blog indexes, not 1M files                                                                                          | Wrong scale entirely                                                                                                                                                                                   |
| **SurrealDB full-text**      | Rust                 | BUSL (source-available)                        | Embeddable in-process mode exists                                                                       | active, full-text search added ~2023–2024                                                                           | BM25 index built on its own storage (RocksDB/others)                         | not independently documented in depth                                                                        | Dictionary-ish (LIKE/full-text, no true regex claimed)                                 | not published                                                                                                                      | Immature for this use, licence also a concern                                                                                                                                                          |
| **Qdrant full-text filter**  | Rust                 | Apache-2.0                                     | Library/server, but full-text is a _filter_ on a vector DB, not a primary index                         | active                                                                                                              | tokenized inverted filter, not a ranked FTS engine                           | simple                                                                                                       | none                                                                                   | N/A                                                                                                                                | Wrong tool — it's a keyword filter bolted onto a vector index, not a search engine                                                                                                                     |
| **Nucliadb** (Rust bits)     | Rust+Python          | AGPL                                           | Server, hybrid vector+text                                                                              | active but oriented at their SaaS                                                                                   | Tantivy under the hood for text                                              | Tantivy's                                                                                                    | Dictionary                                                                             | N/A                                                                                                                                | Not embeddable in the relevant sense — same as Quickwit/Lnx: it's Tantivy wrapped in ops machinery                                                                                                     |
| **Apache Lucene**            | Java                 | Apache-2.0                                     | Library (JVM)                                                                                           | 10.x, active, e.g. 10.1.0 released 2025 [community-anecdote — check exact date before quoting]                      | FST (`.tip`) + block terms (`.tim`)                                          | FOR/PFOR-style 128-doc packed blocks + VInt tail (`.doc`/`.pos`/`.pay`), skip data carries block-max impacts | Dictionary (`RegexpQuery`)                                                             | best-in-class reference numbers exist (see below)                                                                                  | Wrong language for a Rust `unsafe_code=forbid` workspace, but the **format is the one to copy**                                                                                                        |
| **Xapian**                   | C++                  | GPLv2+                                         | Library (C++ w/ many bindings)                                                                          | 1.4.x LTS + 1.5 dev; 1.4.27 released 2024-12-06 [upstream-documented]                                               | Sorted B-tree (glass backend), not FST                                       | Compressed posting lists in glass's own varint-ish scheme, 8 KB blocks                                       | Dictionary (wildcard `*`, no general regex in query language)                          | not centrally published, "compact" tool shrinks                                                                                    | Real candidate, esp. as Recoll's backend, but C++/GPL and no FST/regex                                                                                                                                 |
| **PISA**                     | C++                  | Apache-2.0                                     | Library, but really a research index — no incremental update at all                                     | active research project, commits through 2025                                                                       | Not a general term dict — built for batch, sorted docids                     | Partitioned Elias-Fano (best-known-in-literature)                                                            | Not a general query engine — no content regex, minimal dict ops                        | Publishes real bpi/size numbers (below)                                                                                            | Format numbers worth mining, engine itself unusable as-is (batch, no updates, no phrase-in-the-conventional-sense API)                                                                                 |
| **SQLite FTS5**              | C                    | Public domain                                  | Library (embedded, ships with SQLite)                                                                   | shipped continuously with SQLite, e.g. 3.47+ in 2024-2025 [upstream-documented]                                     | LSM-style segment B-trees in `%_data`/`%_idx`                                | Doclist = delta rowids + position lists, varint-encoded                                                      | **Content regex possible** via `trigram` tokenizer + LIKE/GLOB, or dict-only otherwise | measured: 743MiB (detail=full) / 340MiB (detail=column) / 134MiB (detail=none) on one email corpus [community-anecdote, see below] | **Serious candidate**, esp. paired with trigram tokenizer for grep-like queries                                                                                                                        |
| **DuckDB FTS**               | C++                  | MIT                                            | Library (embedded, extension)                                                                           | active, ships with DuckDB releases                                                                                  | implemented as SQL macros over internal tables, not a bespoke inverted index | not a custom postings codec — leverages DuckDB's columnar storage                                            | Dictionary only, weak                                                                  | N/A                                                                                                                                | Not built for this — it's a convenience layer, not a tuned engine                                                                                                                                      |
| **Bleve**                    | Go                   | Apache-2.0                                     | Library (Go only)                                                                                       | active though slower cadence                                                                                        | uses "scorch" segments, similar family to Lucene (FST-based)                 | zap/scorch codec, roaring bitmaps for doc sets                                                               | Dictionary (`RegexpQuery`)                                                             | not centrally published                                                                                                            | Wrong language (Go, not embeddable in a Rust binary without FFI/cgo-style overhead and a second runtime)                                                                                               |
| **Manticore Search**         | C++ (Sphinx-derived) | GPLv3 (+ paid tiers)                           | Server-first, embeddable "manticore columnar library" exists separately                                 | active                                                                                                              | inherited from Sphinx's inverted index design                                | own postings, has columnar storage add-on                                                                    | Dictionary (has regex query support in recent versions) [community-anecdote]           | not published                                                                                                                      | Server-shaped, GPLv3 entanglement risk for embedding                                                                                                                                                   |
| **Meilisearch**              | Rust                 | MIT                                            | Server-first (not designed to be embedded as a library in another Rust binary; it's a database process) | active                                                                                                              | own structures on top of LMDB (heed crate)                                   | roaring bitmaps for docid sets per token [upstream-documented via engineering blog]                          | none (typo-tolerant prefix search, not regex)                                          | not published                                                                                                                      | Wrong shape for embedding — it is itself an LMDB-backed process/binary, and it targets typo-tolerant UX search, not boolean/regex power search                                                         |
| **Typesense**                | C++                  | GPLv3                                          | Server-first, fully in-RAM                                                                              | active                                                                                                              | in-memory, not disk-format relevant here                                     | in-memory, RAM-resident                                                                                      | none                                                                                   | entire index must fit in RAM                                                                                                       | Wrong resource model for 200GB of files: it wants everything resident                                                                                                                                  |
| **Vespa**                    | Java/C++             | Apache-2.0                                     | Server, heavyweight distributed system                                                                  | active, backed by Yahoo/Verizon                                                                                     | own C++ backend (proton), inverted + tensor indexes                          | own custom codecs                                                                                            | Dictionary + some regex support                                                        | not comparable                                                                                                                     | Enormous overkill for a desktop tool                                                                                                                                                                   |
| **Elasticsearch/OpenSearch** | Java (on Lucene)     | SSPL/Elastic License / Apache-2.0 (OpenSearch) | Server, JVM, heavy                                                                                      | active                                                                                                              | Lucene's                                                                     | Lucene's                                                                                                     | Dictionary (`regexp` query type, same caveat)                                          | Lucene's ratios apply plus JVM/cluster overhead                                                                                    | Wrong shape — this is a distributed cluster product, not embeddable                                                                                                                                    |
| **Groonga**                  | C                    | LGPLv2.1                                       | Library (C, embeddable, backs Mroonga/PGroonga)                                                         | active                                                                                                              | its own "patricia trie"/hash-based lexicon options                           | own compressed postings ("chunk" storage)                                                                    | Dictionary; has a `TokenNgram` mode enabling substring-ish/near-regex matches          | not centrally published                                                                                                            | Plausible dark horse, but FFI-from-Rust + fewer Rust-native guarantees                                                                                                                                 |
| **Whoosh/Woosh**             | Python               | BSD-2                                          | Library (Python only)                                                                                   | Whoosh is effectively **unmaintained** since ~2016-2017; **Woosh is its 2023+ community fork** [community-anecdote] | on-disk B-tree-ish                                                           | own block postings                                                                                           | Dictionary                                                                             | not published, generally poor at scale                                                                                             | Wrong language and known slow at scale — a trap if picked for "it's pure Python, easy" reasons                                                                                                         |
| **Zincsearch**               | Go                   | Apache-2.0                                     | Server (uses Bluge, a Bleve fork, underneath)                                                           | active but small team, momentum recently shifted to its successor OpenObserve                                       | Bluge/Lucene-family FST                                                      | Bluge codec                                                                                                  | Dictionary                                                                             | not published                                                                                                                      | Wrong language, small ecosystem, project's own team has moved on to OpenObserve                                                                                                                        |

Notes on cells marked `[community-anecdote]` for version/date: GitHub release
pages and changelogs should be checked directly before this table is quoted
verbatim in the final report — I was not able to pin exact release dates for
every project in the time available (see Done-note).

---

## Tantivy

- **Language/licence**: Rust, MIT. Library only (no official server binary
  shipped in core, though `tantivy-cli` and quickwit/lnx/toshi/nucliadb wrap it
  as one).
- **Architecture doc**:
  [ARCHITECTURE.md](https://github.com/quickwit-oss/tantivy/blob/main/ARCHITECTURE.md)
  — segments, each self-contained with its own term dictionary, inverted index,
  stored fields (row-oriented, compressed), and "fast fields" (columnar, for
  sorting/faceting/numeric filters).
- **Term dictionary**: an **FST** (via the `fst`/`tantivy-fst` crates) maps
  `Term → TermOrdinal`; a separate "term info store" maps
  `TermOrdinal → TermInfo` (file pointers into the postings). This is the same
  design family as Lucene's `.tip` FST + `.tim` blocks, but tantivy uses the FST
  as the primary dictionary rather than a block-tree.
  [ARCHITECTURE.md](https://github.com/quickwit-oss/tantivy/blob/main/ARCHITECTURE.md),
  [DeepWiki term-dictionary summary](https://deepwiki.com/quickwit-oss/tantivy/8.3-term-dictionary-and-posting-lists)
  (secondary source, cross-check against ARCHITECTURE.md).
- **Postings codec**: blocks of 128 docs; doc IDs are delta-encoded then
  bitpacked (uniform bit-width per block, chosen by the largest delta in that
  block); term frequencies bitpacked similarly. This is the same family as
  Lucene's packed-int/FOR blocks (see Lucene section) — tantivy's own design was
  explicitly modeled on Lucene's.
  [fulmicoton "Of tantivy, a search engine in Rust"](https://fulmicoton.com/posts/behold-tantivy/)
  (author's own blog, primary — Paul Masurel wrote tantivy).
- **Positions**: stored separately per term for phrase queries; tantivy supports
  `PhraseQuery` (exact and, in newer versions, slop-tolerant).
- **Fast fields**: columnar u64/i64/f64/bytes storage, bitpacked, used for
  sort/range/facet without touching the inverted index — analogous to Lucene's
  DocValues (`.dvd`/`.dvm`).
- **Directory abstraction**: `tantivy::Directory` trait abstracts file access;
  ships `MmapDirectory` (default, mmaps segment files) and `RamDirectory`. This
  is exactly the seam a desktop app would use to control residency (e.g. advise
  `madvise`/readahead, or swap in encryption).
- **Regex/fuzzy/phrase**: `RegexQuery` walks the FST term dictionary using
  `regex`/`regex-automata`-compiled automata via the `tantivy-fst` Automaton
  trait — **dictionary regex, not content regex**
  ([docs.rs RegexQuery](https://docs.rs/tantivy/latest/tantivy/query/struct.RegexQuery.html)).
  `FuzzyTermQuery` does Levenshtein-automaton term matching the same way.
  `PhraseQuery` and `PhrasePrefixQuery` exist for exact/prefix phrase matching
  using position postings.
- **`unsafe` usage**: Tantivy uses `unsafe` internally (mmap'd byte slices read
  as ints, bitpacking tricks, some SIMD). I could not get an exact count via web
  search in the time budget (see Done-note) — **this must be verified directly
  against the vendored source** (`grep -rn unsafe` in the crate) before any
  claim that it's "mostly safe." Given the workspace's `unsafe_code = "forbid"`
  lint, embedding tantivy means either (a) accepting an exception/allow for a
  vendored dependency (lint applies to your own crate's code, not transitively
  to deps, so this is not actually a blocker the way it first appears — worth
  confirming this is Dave's understanding too), or (b) it's a non-issue because
  `forbid` only governs code in the workspace's own crates, not third-party
  dependencies. **This is the load-bearing fact to nail down before treating
  "tantivy has unsafe" as disqualifying.**
- **Indexing throughput**: no single upstream-published MB/s figure found.
  Community numbers: ~300 GB/hour indexing English Wikipedia (5M docs / 8GB)
  with 4 threads on "an outdated desktop" [community-anecdote, via search-engine
  survey blog], and "multithreaded indexing... Wikipedia in less than 3 minutes
  on a desktop" [community-anecdote]. Tantivy 0.22 release notes claim ~40%
  indexing throughput improvement on a GitHub-issues dataset benchmark vs prior
  version [Quickwit blog, third-party-ish but from the maintainers
  — https://quickwit.io/blog/tantivy-0.22]. No independently reproduced number
  found; treat all of the above as directional, not a spec.
- **Index size ratio**: no upstream-published ratio found. Not fabricating a
  number here — flagged as a gap.
- **Incremental update/delete**: segment-based, LSM-like — new docs go into new
  segments, deletes are a bitset (tombstone) applied at merge time, merges are
  triggered by policy (default `LogMergePolicy`). A single document "update" is
  delete-by-term + re-add, not in-place mutation — same model as Lucene.
- **Memory footprint / mmap**: `MmapDirectory` is the default; segments are
  memory-mapped, so working-set RAM is governed by the OS page cache, not a
  fixed heap. Good fit for "1M files, memory-lean" claims, but subject to the
  same page-cache pressure any mmap'd multi-GB index has under memory pressure.
- **Embeddability verdict**: **the leading Rust option.** Library-only, MIT,
  Lucene-grade codec design, real Directory abstraction for mmap control,
  dictionary regex + fuzzy + phrase all present. Its main open questions for
  this project are (1) no independently-verified index-size/throughput numbers —
  budget your own benchmark rather than trusting blog posts, and (2) it does
  **not** give you content-regex/grep-style matching out of the box — you'd
  still need a trigram layer or a raw-content grep fallback for true
  regex-over-content, same gap as everything else in this table except SQLite
  FTS5 with trigram tokenizer.

## fst (BurntSushi) + regex-automata

- Not a search engine — a **building block**: `fst` represents sorted string
  sets/maps as a minimal acyclic FST, memory-mappable, with
  automaton-constrained traversal (union/intersect/regex/Levenshtein) baked into
  the iteration API.
  [github.com/BurntSushi/fst](https://github.com/BurntSushi/fst)
- `regex-automata` implements the `fst::Automaton` trait behind a `transducer`
  feature flag, meaning a compiled DFA from `regex-automata` can drive traversal
  of an `fst::Set`/`fst::Map` directly — this is precisely the mechanism
  Tantivy's `RegexQuery` and `FuzzyTermQuery` are built on.
  [docs.rs/regex-automata](https://docs.rs/regex-automata)
- **Why it matters for a build-it-yourself decision**: if Dave builds his own
  term dictionary, `fst` + `regex-automata` gives him Tantivy's dictionary-regex
  capability "for free" as a well-tested, MIT-licensed, pure-Rust dependency —
  he would not need to reimplement automaton-vs-FST traversal himself even in a
  from-scratch index. This narrows the real "build" gap to postings codec +
  segment management + query planning, not the regex machinery.
- Levenshtein automaton fuzzy search ships directly in `fst` behind a
  `levenshtein` feature — `fst::automaton::Levenshtein::new("foo", 1)`.
  [fst README](https://github.com/BurntSushi/fst/blob/master/README.md)

## Quickwit / Lnx / Toshi / Nucliadb — "Tantivy wrapped in a server"

All four wrap Tantivy's actual index format and add: distributed/object-store
splits (Quickwit — designed for S3-backed log search, not local desktop files),
a REST/GraphQL server (Lnx), or a hybrid vector+text SaaS layer (Nucliadb). None
change the underlying codec facts above. For a _single local binary_, all four
add operational weight (HTTP servers, cluster metadata, split management) that a
desktop tool doesn't want — if Tantivy is the choice, use the crate directly,
not one of these wrappers. Toshi in particular looks abandoned (little activity
in recent years, [community-anecdote] — verify against its GitHub commit history
before citing as a live option).

## Sonic

Rust, MPL-2.0, tiny (its pitch is "small footprint, not full IR"). It's closer
to a fuzzy-match/tag/autocomplete store than a boolean+phrase+regex full-text
engine — no BM25 ranking, no phrase queries in the Lucene sense. Not a fit for
this project's requirements; mentioned only to rule it out explicitly.

## Pagefind

Rust compiled to WASM, MIT. The interesting idea, independent of the
implementation language: **the index is partitioned into small chunks keyed
alphabetically, plus one "fragment" file per source page**, and the client only
fetches the chunks/fragments a given query actually touches [community-anecdote
survey of Pagefind's architecture — I could not reach Pagefind's
own architecture doc directly in this pass, flag for re-verification]. Fragment
files run roughly 1–10 KB, index chunks around 40 KB, all gzip-compressed at
build time. For a desktop tool this specific implementation (WASM, browser fetch
semantics) is the wrong shape, but the **lazy-load-by-partition** principle maps
directly onto "don't mmap all 600MB of a 200GB corpus's index at startup — mmap
the postings and dict shard a query actually needs." Worth stealing as a design
pattern, not as a dependency.

## SurrealDB / Qdrant full-text — ruled out

SurrealDB's BM25 full-text index (added ~2023) runs on top of its own storage
engines (RocksDB etc.) and is one feature of a general multi-model database —
licensed under the Business Source License (source-available, not OSI-open), and
its full-text internals aren't independently documented in the depth this report
needs. Qdrant's "full-text" capability is a token filter used for pre-filtering
vector search, not a ranked/phrase-capable FTS engine. Neither is a serious
candidate for this project's core index layer; both ruled out on
architecture-fit rather than benchmarked and rejected.

## Apache Lucene (reference format — JVM, not embeddable here, but copy the design)

- **Codec family**: current default is the `Lucene99`/`Lucene90`-lineage codec
  set (naming tracks the Lucene major version the codec was introduced in).
- **Term dictionary**: `.tip` holds a **per-field FST** mapping term prefixes to
  on-disk blocks in `.tim`; `.tim` itself is arranged in **blocks of 25–48
  terms**, each entry either a term or a pointer to a sub-block — a block tree,
  not a flat FST-to-leaf mapping.
  [Lucene90BlockTreeTermsWriter javadoc](https://lucene.apache.org/core/9_0_0/core/org/apache/lucene/codecs/lucene90/blocktree/Lucene90BlockTreeTermsWriter.html)
- **Postings**: `.doc` holds doc IDs + freqs + skip data; `.pos` holds positions
  (and offsets/payloads when requested); `.pay` holds payloads and offsets when
  stored separately. Encoding is **128-integer packed blocks** (bit-width chosen
  per block from its max value — FOR-style) followed by a **VInt tail** for the
  remainder that doesn't fill a full block.
  [Lucene99PostingsFormat javadoc](https://lucene.apache.org/core/9_11_0/core/org/apache/lucene/codecs/lucene99/Lucene99PostingsFormat.html)
- **Skip lists / block-max WAND**: skip entries align to block boundaries (every
  128 docs), and each skip entry additionally carries **"competitive" impact
  deltas** (freq/norm) that let the block-max WAND scorer skip whole blocks that
  can't beat the current top-k threshold without decoding them — this is the
  mechanism behind Lucene's impact-ordered/`IndexSearcher` two-phase scoring for
  `TOP_SCORES` mode. [same javadoc as above]
- **DocValues**: `.dvd`/`.dvm` (not fetched in depth this pass — flagged).
- **Query capabilities**: boolean (`BooleanQuery`), phrase (`PhraseQuery`,
  positional), `RegexpQuery` (**dictionary regex**, same caveat as Tantivy —
  built by compiling the pattern to an automaton and intersecting with the
  FST/block-tree), `FuzzyQuery` (Levenshtein automaton), range/numeric via
  `PointValues`/BKD trees, faceting via a separate module.
- **Embeddability verdict**: not usable directly — JVM, and this is a Rust
  project. Included because **its format is the one Tantivy already copied**,
  and it is the best-documented reference for "what a mature FOR/PFOR + FST +
  block-max codec looks like in production," useful as the design target if Dave
  builds his own.

## Xapian

- C++, GPLv2+ (note: **copyleft** — matters if the desktop tool is ever
  distributed under a different licence or commercially).
- Current backend is **"glass"**: each database is one or more B-tree-based
  tables (postlist, termlist, position, docdata, etc.), stored as fixed (default
  8KB) blocks.
  [Xapian glossary](https://getting-started-with-xapian.readthedocs.io/en/latest/glossary.html),
  [GlassDatabase source docs](https://xapian.org/docs/sourcedoc/html/classGlassDatabase.html)
- **Postings**: compressed posting lists inside the postlist B-tree — Xapian
  doesn't publish a from-scratch encoding spec as legible as Lucene's javadocs;
  the practical source of truth is `backends/glass/*.cc` in the Xapian source
  tree (not deep-read in this pass — flagged).
- **Term dictionary**: sorted B-tree keyed by term, not an FST — lookups are
  B-tree seeks, not automaton traversal. This is _why_ Xapian's query language
  supports `*` wildcard-prefix matching but not general regex: a B-tree gives
  you efficient prefix range scans cheaply, but not efficient
  arbitrary-automaton intersection the way an FST does.
- **Why it backs Recoll**: Recoll (a well-known Linux desktop full-text search
  tool) uses Xapian specifically for its strong boolean-query semantics and
  stable on-disk format across versions; BM25 (the standard "Okapi BM25"
  weighting) is supported natively.
  [Xapian overview](https://xapian.org/docs/overview.html)
- **Maintenance**: actively maintained, 1.4.x is the LTS line — **1.4.27
  released 2024-12-06** [upstream-documented,
  https://xapian.org/docs/xapian-core-1.4.27/NEWS]; a 1.5 development series also
  exists.
- **Embeddability verdict**: real candidate on maturity and battle-testing
  (powers Recoll on exactly this use case — desktop file search), but: C++ (FFI
  cost/complexity from a Rust `unsafe_code=forbid` binary — you'd be linking a
  large C++ library, which is a bigger unsafe-boundary concern in practice than
  Tantivy's internal `unsafe` blocks), GPLv2+ licensing, no
  FST/regex-over-dictionary support, and no publicly indexed size/throughput
  numbers I could locate this pass to compare against Tantivy/Lucene.

## PISA

- C++, Apache-2.0, from the University of Pisa research group (successor to the
  older `ds2i` codebase). This is a **research index**, not a production
  embeddable library: batch-built, no incremental update support at all —
  building a new index is the only way to add documents.
  [pisa.readthedocs.io](https://pisa.readthedocs.io/)
- **Postings codec**: implements and compares many codecs, but its signature
  contribution is **Partitioned Elias-Fano**: split each postings list into
  variable-length chunks, Elias-Fano-encode each chunk plus a second Elias-Fano
  layer over the chunk endpoints — a two-level EF structure that captures local
  clustering that plain (single-partition) Elias-Fano misses.
  [Ottaviano & Venturini, "Partitioned Elias-Fano Indexes", SIGIR 2014, PDF](http://groups.di.unipi.it/~ottavian/files/elias_fano_sigir14.pdf)
- **Query processing**: supports `AND`/`OR`/`MaxScore`/`WAND`/`BlockMax WAND`/
  `BlockMax MaxScore`/`Variable BlockMax WAND` — this is the reference
  implementation many production block-max WAND implementations (including
  Lucene's) are benchmarked against in IR literature.
  [PISA docs](https://pisa.readthedocs.io/)
- **Document reordering**: recursive graph bisection — reorders document IDs to
  cluster documents that share vocabulary, which materially improves
  delta/Elias-Fano compressibility because postings lists become more tightly
  clustered.
  [Compressing Graphs and Indexes with Recursive Graph Bisection](https://www.researchgate.net/publication/305998111_Compressing_Graphs_and_Indexes_with_Recursive_Graph_Bisection)
- **Published numbers**: I could not directly fetch the SIGIR'14 paper's PDF in
  this session (SSL error on the direct fetch) and could not independently pull
  exact bits-per-posting or GB figures for Gov2/ClueWeb09 in the time available
  — search results surfaced only relative comparisons from a _follow-on_ paper:
  **clustered Elias-Fano indexes retain 24% less space than OptPFD on Gov2 and
  14.5% less on ClueWeb09**, and are **up to 11% smaller than partitioned
  Elias-Fano on Gov2, 6.25% on ClueWeb09** [paper-reported, via search
  snippet of a secondary paper — treat as needing direct-PDF verification before
  quoting absolute numbers in the final report; ClueWeb09 is documented
  elsewhere as ~50M English web pages]. This is a **gap**: I was asked to get
  PISA's real numbers and could not fully deliver them this pass — flagged
  prominently in the Done-note.
- **Embeddability verdict**: **not usable as a library for this project.** No
  incremental update at all (rules it out for "fast incremental updates," a hard
  requirement), no phrase/regex query surface built for general full-text apps —
  it's built to benchmark ranking/compression research, not to serve a live
  boolean+phrase+regex desktop query language. Its value here is entirely as a
  **codec reference**: if Dave wants a smaller index than FOR/bitpacking gives,
  Partitioned Elias-Fano + recursive graph bisection is the literature's
  best-documented answer for _how much_ smaller, even though the numbers above
  need re-verification.

## SQLite FTS5

- C, public domain, ships embedded in SQLite itself — genuinely the easiest
  "buy" option to actually ship inside a single Rust binary (via
  `rusqlite`/`libsqlite3-sys`, statically linked, no separate server process, no
  separate file format to invent).
- **Schema**: `%_data` and `%_idx` together implement a **log-structured merge
  tree stored inside ordinary SQLite tables** — `%_data` holds fixed b-tree
  pages per segment, `%_idx` holds (segment id, term-prefix, page number)
  triples used to seek into `%_data` without scanning; `%_docsize` tracks
  per-column token counts (droppable via `columnsize=0`); `%_content` holds the
  indexed text unless the table is "contentless" (`content=''`, which saves the
  raw-text duplication cost by pointing back at the application's own storage).
  [sqlite.org/fts5.html, §4 "The FTS Index"](https://sqlite.org/fts5.html)
- **Doclist format**: for each term, a **doclist** = delta-encoded rowids
  (varint), each followed (when `detail=full`, the default) by a **position
  list**: column number + within-column offsets, also varint-delta-encoded.
  [sqlite.org/fts5.html, §5 "Key/Doclist Format"](https://sqlite.org/fts5.html)
- **`detail=` size/capability tradeoff — real measured numbers**: `detail=full`
  (positions + column, default) vs `detail=column` (column only, no phrase/NEAR)
  vs `detail=none` (existence only, no column filter, no phrase, no NEAR). One
  documented email-corpus test: **743 MiB (full) → 340 MiB (column) → 134 MiB
  (none)** [community-anecdote quoting a real measurement — attributed
  to SQLite's own documentation/mailing-list discussion around
  fts5.html's guidance; re-verify the exact corpus and source location before
  quoting as upstream-documented in the final report, since my fetch
  summarized rather than quoted the primary line verbatim]. This is the single
  most concrete "index size ratio vs feature tradeoff" number found across all
  engines in this survey — and it directly quantifies the **cost of phrase
  support**: ~5.5x size blowup from `none` to `full` on that corpus. Since
  phrase search is a hard requirement here, Dave is priced into at least
  `detail=full` (or `column` if NEAR/phrase can be relaxed to same-column-only).
- **Trigram tokenizer** (`fts5vocab`'s sibling feature, added in SQLite
  **3.34.0**, released 2020-12-01 per SQLite's own changelog conventions —
  version confirmed by feature name, exact date not re-verified this pass):
  indexes every overlapping 3-character sequence, enabling **indexed
  `LIKE`/`GLOB`** and near-regex substring queries — this is the one engine in
  this table that gets you closer to **content-level** pattern matching rather
  than pure dictionary-term matching, because a trigram index doesn't care about
  tokenizer boundaries the way a term-based FST/B-tree dictionary does. Caveat
  directly from the docs: substrings under 3 characters can't be found via a
  trigram full-text query (though `LIKE`/`GLOB` fallback still works, just
  unindexed for short patterns).
  [sqlite.org/fts5.html](https://sqlite.org/fts5.html)
- **`bm25()`**: built-in ranking function, lower score = better match (negated
  for intuitive `ORDER BY`), with optional per-column weight arguments.
- **Incremental update**: standard SQLite `INSERT`/`UPDATE`/`DELETE` on the
  virtual table; internally this appends new segments and periodically merges
  (LSM-style, same conceptual shape as Tantivy/Lucene segment merging, just
  implemented inside SQLite's own b-tree machinery) — no special API needed,
  which is a real ergonomic win for a desktop tool that's already going to want
  SQLite for metadata anyway.
- **Query capabilities**: boolean (`AND`/`OR`/`NOT` in FTS5 query syntax),
  phrase (`"exact phrase"`), NEAR (`NEAR(a b, N)`), prefix (`term*`), column
  filters, `bm25()` ranking. **No native regex query operator** — you get
  regex-_like_ power only via the trigram tokenizer + external LIKE/GLOB pattern
  translation, which is still dictionary/pattern matching over indexed trigrams,
  not a true regex engine (translating an arbitrary regex into a
  trigram-AND-of-substrings filter is a well-known but non-trivial technique —
  e.g. what codesearch/Zoekt/livegrep do — and FTS5 itself doesn't do this
  translation for you).
- **Memory/mmap**: standard SQLite page cache; can be configured with
  `PRAGMA mmap_size` for mmap'd reads. Well-understood operational
  characteristics — this is SQLite, one of the most battle-tested storage
  engines in existence.
- **Embeddability verdict**: **the other serious candidate**, on different
  grounds than Tantivy. Trivial to embed (already-linked SQLite, zero extra
  process/format), free incremental update model, documented and measurable
  size/feature tradeoffs, and the trigram tokenizer is the closest thing in this
  whole survey to actually getting **content-level** substring/near-regex
  behaviour rather than pure dictionary-term regex. Downsides: no FST, no
  automaton-driven dictionary regex the way Tantivy/Lucene have it (so
  prefix-of-terms searches are less elegant), and a full custom regex engine
  still has to be built on top (trigram-set intersection to shortlist + real
  regex execution on shortlisted rows) — FTS5 gets you the shortlist mechanism,
  not the regex itself.

## DuckDB FTS, Bleve, Manticore, Meilisearch, Typesense, Vespa, ES/OpenSearch, Groonga, Whoosh/Woosh, Zincsearch — brief verdicts

- **DuckDB FTS**: implemented as SQL macros over DuckDB's own columnar tables
  rather than a bespoke inverted-index codec — convenient inside a DuckDB-based
  analytics tool, but not a tuned full-text engine and not independently
  benchmarked for this use case. Skip.
- **Bleve** (Go): Lucene-family design (FST-based "scorch" segments,
  roaring-bitmap doc sets), `RegexpQuery` = dictionary regex, same caveat as
  everywhere else. Wrong language for a Rust binary — embedding means
  cgo/FFI-equivalent complexity or a second runtime; only worth it if Dave were
  building in Go, which he isn't.
- **Manticore Search**: Sphinx-derived C++ engine, server-first, GPLv3, has a
  separately distributable columnar storage library. Real production engine but
  the wrong shape (server process) and licence for embedding in a single Rust
  binary.
- **Meilisearch** (Rust!): despite being Rust, it is architecturally a
  **standalone LMDB-backed server/database process**, not a library meant to be
  `cargo add`ed into another binary's process space — its whole design (own HTTP
  API, own process lifecycle, own on-disk LMDB env) fights being embedded. Uses
  roaring bitmaps over LMDB (`heed` crate) for per-token doc-id sets, tuned for
  typo-tolerant UX search rather than boolean/regex power search. Not the right
  fit despite the language match.
- **Typesense**: fully in-RAM index — wrong resource model for 10–200GB of file
  content on a desktop machine that shouldn't need to hold the whole index
  resident.
- **Vespa**: distributed serving system, enormous operational surface for a
  desktop tool. Skip.
- **Elasticsearch/OpenSearch**: Lucene under the hood (so the format section
  above applies), but wrapped in a JVM cluster product. Skip for embedding;
  useful only as "what Lucene's format buys you at scale."
- **Groonga** (C, LGPL): a genuine dark horse — embeddable C library, own
  lexicon/postings design, has an n-gram tokenizer mode that gets partway to
  content-level matching similar in spirit to FTS5's trigram tokenizer.
  Underexplored in mainstream Rust-search discourse; worth a closer look if
  Xapian's GPL and Tantivy's unresolved unsafe/benchmark questions both become
  blockers, but FFI-from-Rust cost is the same category of concern as Xapian.
- **Whoosh/Woosh** (Python): Whoosh is unmaintained since roughly 2016-2017;
  Woosh is a newer community fork picking it back up (~2023+). Wrong language
  regardless of fork status, and Whoosh has a long-standing reputation for poor
  performance at scale — a trap if picked for "pure-Python, simple to read"
  reasons rather than performance fit.
- **Zincsearch** (Go): built on Bluge (a Bleve fork). The Zincsearch team's own
  momentum has shifted to a successor project (OpenObserve) in recent years — a
  maintenance-trajectory yellow flag independent of the language mismatch.

---

## Density ranking — bytes per indexed token, and what buys it

Ranked by how small a fully-featured (phrase-capable) index gets, since that is
the stated priority ("slightly slower search for a denser index"). Numbers mix
labelled classes — do not average across rows.

| Engine / knob                                                            | Density-relevant choice                                                                                                                                                                                                | What it costs when turned on                                                                                                                                                                                                                                                                                                                                                 | Label                                                                                                         |
| ------------------------------------------------------------------------ | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------- |
| **SQLite FTS5, `detail=none`**                                           | drop positions and column info entirely, doclist = rowid list only                                                                                                                                                     | 134 MiB on the reference corpus vs 743 MiB at `detail=full` — **~5.5x smaller**, but loses phrase/NEAR/column filters entirely                                                                                                                                                                                                                                               | [community-anecdote, one corpus — needs re-measurement on Dave's own data]                                    |
| **SQLite FTS5, `detail=column`**                                         | keep column, drop offsets                                                                                                                                                                                              | 340 MiB vs 743 MiB — **~2.2x smaller than full**, loses phrase/NEAR (same-column existence only)                                                                                                                                                                                                                                                                             | [community-anecdote]                                                                                          |
| **SQLite FTS5, `detail=full`**                                           | full position lists per occurrence                                                                                                                                                                                     | baseline (743 MiB) — this is the tier phrase search actually requires                                                                                                                                                                                                                                                                                                        | [community-anecdote]                                                                                          |
| **Lucene/Tantivy: positions on vs off**                                  | Lucene lets a field be indexed `DOCS_AND_FREQS` (no positions) instead of `DOCS_AND_FREQS_AND_POSITIONS`; Tantivy has the equivalent `IndexRecordOption` choice per field                                              | no independently published bytes-per-token delta found this pass — same shape of tradeoff as FTS5's detail= knob, but nobody has published the number the way SQLite's docs incidentally did                                                                                                                                                                                 | [gap — flagged]                                                                                               |
| **Lucene `match_only_text` (Elasticsearch field type, on Lucene 8.5+)**  | stores _no_ term frequency or position data at all — only whether the term occurs at least once per doc; sacrifices phrase/proximity and BM25-quality scoring for a smaller postings list than even freq-only encoding | ES's own docs frame it purely as "smaller index, phrase queries execute a slower fallback (positions reconstructed from `_source` or disabled)" rather than a quantified ratio — worth reading `PostingsEnum` behaviour under this field type directly in Lucene source before adopting the idea, since Dave needs phrase support and this trades exactly the thing he needs | [upstream-documented existence, no size number located]                                                       |
| **PISA: Partitioned Elias-Fano vs plain Elias-Fano vs OptPFD**           | encode postings as two-level Elias-Fano rather than single-partition EF or FOR-family (OptPFD)                                                                                                                         | secondary source: clustered EF (a further refinement) is **24% smaller than OptPFD on Gov2, 14.5% smaller on ClueWeb09**; and **up to 11%/6.25% smaller than plain partitioned EF** on the same two collections respectively                                                                                                                                                 | [paper-reported, via a follow-on paper's abstract, not the primary SIGIR'14 table — re-verify, see Done-note] |
| **Doc-values-only fields (Lucene `.dvd`/`.dvm`, Tantivy "fast fields")** | move a field out of the inverted index entirely into columnar storage — no postings list at all, just a dense/sparse array keyed by internal doc id                                                                    | eliminates that field's postings cost completely (not a ratio — a removal), at the cost of no term-level search on that field, only exact/range lookup via the columnar array. Directly applicable to metadata-shaped fields (path, mtime, size, extension) that don't need free-text matching                                                                               | [architectural fact, no single benchmark cited]                                                               |
| **Recursive graph bisection (PISA)**                                     | reorder document IDs so documents sharing vocabulary get adjacent IDs, before delta-encoding postings                                                                                                                  | improves delta/EF compressibility because gaps between consecutive matching doc IDs shrink; PISA's own docs list it as a standard pre-compression pass, magnitude folded into the OptPFD/EF numbers above rather than isolated                                                                                                                                               | [paper-reported, not isolated]                                                                                |

**Reading these together**: the single most useful transferable idea is that
_every_ format in this survey that documents a density knob does it the same way
— **drop positions, keep just doc-membership (or freq)** — and the price is
always phrase support. Since phrase is a hard requirement here, the practical
design question isn't "positions or not" globally, it's **which fields need
positions**: file path/metadata almost certainly don't (doc-values shaped), file
body text does. A from-scratch design gets to make that choice per-field the way
Lucene does, rather than per-index the way FTS5's `detail=` does — worth taking
as a concrete improvement-over-prior-art point in the design doc, if Dave lands
on his own per-field postings-vs-doc-values split as SQLite FTS5 doesn't offer
this modularity in-table (it's one `detail=` setting for the whole FTS5 table,
not adjustable per column, since `detail=full` applies to all indexed columns of
a virtual table).

## What each format got right, and what it cost

A ledger of specific choices worth stealing or deliberately not repeating,
independent of whether the engine itself is embeddable.

- **Lucene's block-tree term dictionary (FST over term prefixes → block
  pointers, not FST-to-leaf-postings)**: the FST only carries you to a
  ~25-48-term block in `.tim`, and the block itself is scanned/binary-searched
  linearly. This is a deliberate density-vs-lookup-speed tradeoff over a "pure"
  FST-to-postings mapping (which is closer to what Tantivy does) — Lucene trades
  a small amount of per-lookup work for a smaller FST, since the FST only needs
  enough states to disambiguate down to block granularity, not down to
  individual terms. Worth studying directly (not summarized further here, since
  Dave already knows Ferret/Lucene's block-tree from the inside) at
  [Lucene90BlockTreeTermsWriter javadoc](https://lucene.apache.org/core/9_0_0/core/org/apache/lucene/codecs/lucene90/blocktree/Lucene90BlockTreeTermsWriter.html).
- **Lucene's impact/competitive-score skip data (block-max WAND lineage,
  Lucene90+ codecs)**: every 128-doc skip entry additionally carries the block's
  max competitive freq/norm, letting `TOP_SCORES`-mode search skip decoding
  whole blocks that provably can't enter the current top-k. This is a pure
  query-time win bought with a few extra bytes per skip entry — cheap relative
  to the postings themselves, and orthogonal to whichever postings codec is
  chosen underneath. If Dave wants "slightly slower search, denser index," this
  specific piece stays worth keeping regardless — its density cost is negligible
  and its latency win is real, so it isn't actually on the size/speed tradeoff
  curve the rest of this table is.
  [Lucene99PostingsFormat javadoc](https://lucene.apache.org/core/9_11_0/core/org/apache/lucene/codecs/lucene99/Lucene99PostingsFormat.html)
- **Tantivy's fast fields (columnar bitpacked side-store)**: same idea as Lucene
  doc-values, cleaner API surface, and it's the mechanism that would let a
  from-scratch design keep file metadata (path, size, mtime) entirely out of the
  postings/positions machinery — no term dictionary entry, no postings list,
  just a dense array indexed by internal doc id. Confirms this is a settled,
  worth-copying pattern rather than a Tantivy-specific quirk.
- **PISA's Partitioned Elias-Fano**: the clearest published evidence that
  _global_ single-partition compression schemes (plain Elias-Fano, or a single
  bit-width choice as in naive FOR) leave real bytes on the table versus schemes
  that let compression parameters vary _within_ a single postings list based on
  local clustering. This generalizes past PISA specifically: any codec Dave
  builds should default to per-block (not per-list, and certainly not global)
  parameter selection, which Lucene/ Tantivy's per-128-doc-block bit-width
  already does at the FOR level — Partitioned Elias-Fano is the same idea taken
  further, varying the _chunk size itself_, not just the bit-width within a
  fixed chunk size.
- **SQLite FTS5's `detail=` as a table-wide, not per-column, knob**: the
  clearest thing to do _differently_. FTS5 forces one detail level across an
  entire virtual table's columns; a from-scratch design can make this a
  per-field decision (positions for body text, existence-only for metadata
  fields, doc-values for anything needing range/sort rather than term match)
  without giving up anything FTS5 users don't already give up per-table today.
- **SQLite FTS5's trigram tokenizer as a shortlist mechanism**: the one format
  choice in this whole survey that gets partway to **content regex** rather than
  dictionary regex — index every overlapping 3-gram, use set intersection over
  the trigram postings to shortlist candidate documents, then run a real regex
  engine against shortlisted content. The generalizable lesson: **true content
  regex is not a term-dictionary feature at all**, it's a separate
  n-gram-indexed shortlist stage bolted in front of a real regex engine — no
  engine surveyed here (including Lucene) gets this from its primary inverted
  index. This is the piece of prior art most directly worth designing in from
  day one, since it's a stated hard requirement and every mainstream engine
  treats it as an afterthought or omits it.
- **Xapian's B-tree-not-FST dictionary**: readable design intel (why does a
  B-tree dictionary support prefix/wildcard cheaply but not automaton
  intersection?) but **not vendorable** — GPLv2+ means the code itself can't be
  pulled into a permissively-licensed project; only the ideas can be studied and
  reimplemented independently. Record this explicitly wherever Xapian's design
  is cited as a source of technique rather than of code.
- **Groonga (LGPLv2.1)**: sits in between — LGPL permits dynamic linking without
  relicensing the linking project, but static-linking a Rust binary against an
  LGPL C library still carries obligations (relinkability) that most
  permissive-licensed Rust projects avoid by policy; treat its design (n-gram
  lexicon mode) as intel, same caution as Xapian, lower severity.
- **Recoll (via Xapian)**: the closest existing product to this project's shape
  — desktop, boolean-strong, BM25 — inherits Xapian's density profile (B-tree
  dictionary, 8KB blocks) and its licence constraint. Worth using as a
  UX/feature-set reference, not a format reference beyond what's captured above.

**Licence summary for quick reference** (permissive preferred, per Dave's stated
preference):

| Licence class                                           | Engines                                                                                                                                                                                                            |
| ------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ |
| Permissive (MIT/Apache/BSD/public-domain) — vendorable  | Tantivy, fst, Pagefind, Tinysearch, Lucene, Bleve, PISA, SQLite/FTS5, DuckDB FTS, Qdrant, Nucliadb-adjacent Tantivy bits, Sonic (MPL-2.0 is copyleft-lite, file-level, effectively fine to study/link), Zincsearch |
| LGPL — linkable with obligations, not freely vendorable | Groonga                                                                                                                                                                                                            |
| GPL/AGPL/BUSL/SSPL — design-only, not vendorable        | Xapian (GPLv2+), Quickwit/Lnx/Nucliadb (AGPL), Manticore (GPLv3), Typesense (GPLv3), SurrealDB (BUSL), Elasticsearch (SSPL/Elastic License)                                                                        |

**Traps** (each looks tempting for a specific wrong reason, restated with the
build-it-yourself framing):

- **PISA's numbers look best in the literature** — true, but they come from a
  batch-built research index with **zero incremental-update support**; mine the
  Partitioned-EF and recursive-graph-bisection ideas, don't expect a
  from-scratch incremental design to hit the same ratios without its own
  batch-vs-incremental tradeoff analysis.
- **`match_only_text`-style zero-position fields look like free density** — they
  are, but the cost is phrase support specifically, which is a named hard
  requirement; only apply this per-field to things that never need phrase
  (metadata), never to body text.
- **Any engine's "regex" query type** (Lucene `RegexpQuery`, Tantivy
  `RegexQuery`, Bleve `RegexpQuery`) — dictionary-term automaton matching only,
  not content regex. The one documented format-level answer to true content
  regex in this whole survey is the trigram-shortlist pattern (SQLite FTS5's
  tokenizer, or a hand-rolled equivalent) — build that in as a first-class piece
  of the format, not an afterthought.
- **Xapian/Groonga's designs are worth reading, not worth linking against**
  given the licence table above — read the source for technique, reimplement
  independently.

---

## Done-note

**Verified with primary/upstream sources:**

- Tantivy's FST term dictionary + 128-doc bitpacked postings, via its own
  ARCHITECTURE.md and the author's own blog post.
- Tantivy `RegexQuery`'s dictionary-automaton mechanism, via docs.rs.
- `fst`'s automaton-trait integration with `regex-automata` and its Levenshtein
  feature, via BurntSushi's own README/source.
- Lucene's `.tim`/`.tip` block-tree+FST term dictionary and `.doc`/`.pos`/
  `.pay` 128-int packed-block postings with block-max-WAND impact skip data, via
  Lucene's own javadoc pages (9.0.0 BlockTree, 9.11.0 PostingsFormat).
- SQLite FTS5's `%_data`/`%_idx` LSM-b-tree structure, doclist/position format,
  `detail=` levels, trigram tokenizer, and `bm25()`, via sqlite.org/fts5.html
  directly.
- Xapian's glass B-tree backend shape and 1.4.27 release date (2024-12-06), via
  xapian.org's own docs/NEWS.
- PISA's Partitioned Elias-Fano design and recursive-graph-bisection reordering,
  via the Ottaviano/Venturini SIGIR'14 abstract and a follow-on paper's
  description (not the full PDFs).

**Could not verify / gaps to close before this goes in the final report:**

1. **PISA's actual published bits-per-posting / GB numbers on Gov2/ ClueWeb09**
   — the direct PDF fetch failed (SSL error on `groups.di.unipi.it`), and I only
   recovered _relative_ percentages from a secondary paper's abstract via search
   snippets, not the primary size/latency table the task explicitly asked me to
   get. **This is the most important unfilled gap in this report** — re-fetch
   `http://groups.di.unipi.it/~ottavian/files/elias_fano_sigir14.pdf` (retry;
   the SSL error looked transient/tool-side rather than the server being down)
   or pull the numbers from `pisa.readthedocs.io`'s own benchmark pages before
   quoting absolute PISA numbers anywhere downstream.
2. **Tantivy's exact `unsafe` block count/locations** — flagged as load-bearing
   for the `unsafe_code = "forbid"` question but not counted; needs a local
   `grep -rn unsafe` on a cloned copy of the crate, which I could not do without
   cloning the repo (out of scope for a web-research pass).
3. **Whether `unsafe_code = "forbid"` at the workspace level actually constrains
   dependency code at all** — I asserted the common understanding (it governs
   the crate it's set in, not transitively), but did not verify this against the
   specific Cargo/clippy semantics Dave's workspace uses. This should be
   confirmed against the actual `Cargo.toml`/`clippy.toml` before treating
   Tantivy's internal unsafe as a non-issue.
4. **Tantivy's index-size ratio vs raw text** — no upstream number found; every
   figure I found was anecdotal or absent. Flagged rather than estimated.
5. **Exact last-release dates** for Tantivy, Toshi, Sonic, Lnx, Zincsearch,
   Nucliadb, Manticore's regex-support version — marked `[community-anecdote]`
   throughout and should be spot-checked against each project's GitHub releases
   page before publication; I prioritized breadth across ~25 engines over
   exhaustively pinning every date given the time budget.
6. **No contradictions found** between sources on the core architectural claims
   (FST vs B-tree dictionaries, block-based postings, dictionary-vs- content
   regex) — the "regex over dictionary, not content" distinction was consistent
   across every engine I checked it for (Tantivy, Lucene, implied for
   Bleve/Manticore via their shared Lucene-family lineage).
