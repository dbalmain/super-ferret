# R7 — Substring, n-gram, suffix and filter structures (the regex-capable half)

Scope: everything that answers "does this arbitrary substring or regex occur,
and where" — as opposed to R6's classical term-postings/BM25 inverted index.
Written for a Rust implementer who has a hard requirement to support regex
queries over a personal-workstation-scale corpus: ~1M files (bound ~5M), tens to
a few hundred GB, mostly source code and text/config, then PDF/Office, then
media metadata, with a nice/ionice'd background indexer doing near-real-time
incremental updates.

**Standing design constraint for this reader**: he has built a classical
inverted index before (Ferret, the Ruby Lucene port) and is comfortable with
that half of the problem — this slice exists for the succinct/suffix half he
hasn't. He is implementing everything himself in Rust, not linking a library, so
pseudocode here is the deliverable, not decoration. And he has stated an
explicit preference: **index density over query speed** — "I'd go for slightly
slower search performance for a more efficient denser index." Every structure
below is therefore compared **bytes-of-index-per-byte-of- text first, latency
second**, and every comparison also states its **incremental-update cost**,
because a structure that is small but static (rebuild-only) is a first-order
problem for a near-real-time indexer, not a footnote.

---

## 1. n-gram indexes

### 1.1 Trigram indexes: the basic idea

Build a postings list keyed by every contiguous 3-character (or 3-byte)
substring ("trigram") in the corpus: `trigram -> {doc_id, ...}` (or
`trigram -> {(doc_id, offset), ...}` if position-carrying). A literal substring
of length ≥3 is looked up by ANDing the postings lists of its constituent
trigrams; a regex is translated into a boolean expression over trigram existence
(see §2) and evaluated the same way, then every candidate document is
**verified** with a real regex engine because the trigram query is a necessary,
not sufficient, condition.

This is exactly Russ Cox's "Google Code Search" design
[swtch.com/~rsc/regexp/regexp4.html](https://swtch.com/~rsc/regexp/regexp4.html)
(2012) — read this first, it is the primary source for §2 as well.

### 1.2 Index-size multiplier

Doc-level (non-positional) trigram index: one posting per (trigram, doc) pair.
For source code, the number of _distinct_ trigrams per file saturates quickly
(English/code alphabets have on the order of 100–200 trigrams per KB of distinct
content before repeats), so a doc-level index over many small files is compact,
typically well under 1× source size **[estimated]** — this is why Google Code
Search and early `grep`-index tools chose doc-level trigrams. However doc-only
trigrams only narrow the _candidate file set_; they give no in-file offset, so
verification must re-scan the whole file.

Position-carrying (offset-level) trigram index, as in zoekt, is much larger:
zoekt's own design doc states that **positional trigram search requires roughly
1.2× the corpus size in RAM**
[github.com/sourcegraph/zoekt/blob/main/doc/design.md](https://github.com/sourcegraph/zoekt/blob/main/doc/design.md)
[upstream-documented]. That number already reflects zoekt's compact
run-length/delta-coded posting representation — a naive "list of (docid, offset)
pairs" implementation would be several times larger before compression, because
a 32-bit offset per trigram occurrence dominates a 1-byte-per-position corpus.
Positional trigrams pay for themselves by letting verification jump straight to
candidate offsets instead of rescanning whole files.

### 1.3 The common-trigram pathology

Natural-language and code text have a extremely skewed trigram frequency
distribution: trigrams like `" th"`, `"the"`, `"ing"`, `" = "`, `"()."` in code
occur in nearly every document, so their postings lists are nearly as large as
the total document count and contribute no selectivity. Two mitigations, both
used in practice:

- **Stop-trigram elision / capping**: drop or cap postings lists for trigrams
  whose document frequency exceeds a threshold (analogous to stopwords in
  classical IR); AND-ing a query still works because a dropped trigram is
  treated as "matches everything" for planning purposes, it just contributes
  nothing to selectivity. Cox's article explicitly discusses degrading to "match
  all documents" when no useful trigram constraint can be derived — the same
  escape valve.
- **Query planning picks the rarest trigram(s)** rather than ANDing all of them
  exhaustively — see §1.4.

Case folding and alphabet size directly control this pathology: folding to
lowercase collapses the effective alphabet (e.g. ASCII code ~ 96 useful chars →
~63 after folding letters), which _increases_ trigram collision rate and average
posting-list length, trading a smaller index (half as many distinct trigrams to
track, since `The`/`the`/`THE` collapse) for less selectivity per trigram and
for the loss of case-sensitive matching unless the query engine also case-folds
the pattern (which breaks exact-case regex requirements, e.g. matching `\bFoo\b`
only when capitalized). Most production code-search tools (zoekt,
`grep -i`-style tools) keep **both** a case-sensitive trigram index and fold for
case-insensitive queries by trying all case variants of each trigram in the
query — this is exactly what Cox's article and zoekt do for `(?i)` patterns; it
multiplies the boolean query by up to 2^3 case variants per trigram (worse for
non-ASCII).

### 1.4 Variable-length n-grams and n-gram selection

Pure trigrams both over- and under-index: short literals (length 1–2) can't be
looked up at all (must fall back to "match all" or a linear scan), and long
literals are only as selective as their _rarest_ trigram. Two refinements:

- **Rarest-k-gram selection**: rather than ANDing every trigram of a long
  literal, an implementation can query only the trigram(s) estimated to be
  rarest (from global trigram-frequency statistics gathered at index build time)
  and skip near-universal ones — this bounds query cost without losing
  correctness, since dropped trigrams only make the _candidate set_ larger,
  never wrong (they can never eliminate a true positive because they are
  redundant AND terms). This is standard practice in n-gram query planning; see
  the general discussion in Cox's article and its treatment of simplifying
  AND/OR trees.
- **Variable-length n-grams (bigrams for CJK/short-alphabet text, 4-grams or
  5-grams for very low-entropy corpora)**: bigram indexes are common for CJK
  text because trigram-based indexing under-indexes single- and double-character
  words common in Chinese/Japanese search (a well-known problem discussed
  extensively in the CJK-search literature, e.g. PostgreSQL's `pg_bigm` project
  exists specifically because `pg_trgm` performs poorly on CJK). For source code
  (ASCII-dominated), trigrams are the standard choice; going to 4-grams roughly
  quarters posting-list length at the cost of proportionally more distinct index
  keys and worse recall for 3-character search terms.

### 1.5 Trigrams with successor lists (zoekt's design) — why it beats plain trigrams

zoekt does not store a flat trigram → doclist map; instead it augments each
trigram occurrence in the _index build_ pipeline with **successor information**
so that, when checking a long literal, the search does not need to independently
AND N separate trigram posting lists and then verify byte- by-byte over the
whole candidate set — it can instead check that consecutive trigrams occur at
_consecutive offsets_ using the position information cheaply during the boolean
evaluation itself, collapsing what looks like an N-way AND into a much cheaper
"walk the shortest posting list, check neighbours" operation. Practically:
because zoekt already stores per-occurrence offsets (§1.2), a literal of length
L decomposes into L-2 overlapping trigrams whose positions must be consecutive
(offset, offset+1, offset+2, …); the query executor picks the _rarest_ trigram's
posting list as the driver and, for each candidate position, checks the
neighbouring trigrams directly at `offset±1` without a second index lookup. This
is why positional trigrams "beat" doc-only trigrams for longer literals:
verification cost becomes O(occurrences of rarest trigram) instead of O(file
size) per candidate document. Source:
[zoekt design doc](https://github.com/sourcegraph/zoekt/blob/main/doc/design.md)
[upstream-documented].

### 1.6 Position-carrying vs doc-only: false-positive/verify tradeoff

|                           | Doc-only trigrams                                                                                                   | Position-carrying trigrams                                                        |
| ------------------------- | ------------------------------------------------------------------------------------------------------------------- | --------------------------------------------------------------------------------- |
| Index size                | Small (∝ distinct trigrams × docs containing them)                                                                  | Larger — zoekt: ~1.2× corpus [upstream-documented]                                |
| False positives           | Any doc containing all required trigrams, even in unrelated contexts (e.g. trigrams "distributed but not adjacent") | Adjacency check at query time removes most of these before the regex verify stage |
| Verify cost per candidate | Full-document regex scan                                                                                            | Regex scan can seed from candidate offsets, often near-O(1) per candidate         |
| Best for                  | Filename-scale or many-small-file corpora, index-size-constrained                                                   | Single large repo, latency-constrained, code-search UX (jump-to-match)            |

### 1.7 Character-level vs byte-level n-grams; UTF-8/CJK handling

Byte-level trigrams (operate on raw UTF-8 bytes) are simplest to implement,
require no decoding, and work uniformly for binary-ish text, but a single
multi-byte UTF-8 codepoint (2–4 bytes) gets shredded into fragments that
straddle codepoint boundaries, which is harmless for ASCII-dominated text (each
ASCII byte-trigram = a 3-character trigram) but produces meaningless n-grams for
non-ASCII scripts, hurting both selectivity and query correctness for those
regions. Character-level (rune) trigrams decode UTF-8 first and n-gram over
codepoints; zoekt does exactly this — it indexes rune offsets and separately
maintains a **rune-to-byte offset lookup table sampled every 100 runes** to
translate between rune-space (used for n-gramming) and byte-space (used for
actual file slicing) [upstream-documented, zoekt design doc]. For CJK text specifically,
prefer bigrams over trigrams (see §1.4) since single/double-character tokens are
common and a full trigram may span multiple semantic units or fail to exist for short
strings at all.

Implementation recommendation for a Rust build: decode to `&str`/`char`
iteration (already gives you rune boundaries for free vs. manual UTF-8
handling), n-gram over `char`, and store a periodic rune→byte offset table
(zoekt's "every 100 runes" figure is a reasonable starting sampling rate — it
trades a small lookup-table size for O(sample-interval) byte-offset
reconstruction cost) [upstream-documented pattern; the specific interval
is zoekt's choice, not a universal constant].

### 1.8 Real index-size numbers, labelled

- zoekt positional trigram index: **~1.2× source corpus size in RAM**
  [upstream-documented] (github.com/sourcegraph/zoekt/blob/main/doc/design.md).
- Google Code Search era doc-only trigram index: Cox's article does not give a
  single index/corpus ratio figure; treat any specific ratio for that system as
  **not verifiable from primary source** — do not cite a number for it.
- `pg_trgm` (PostgreSQL trigram GIN index) is commonly reported in the Postgres
  community as **2–4× the indexed text size** for a GIN trigram index, but this
  figure is anecdotal/community-reported rather than from a controlled paper —
  label any use of it **[third-party-benchmark, weak provenance]** and
  re-measure before relying on it.

---

## 2. Regex → index query planning (the centrepiece)

### 2.1 Cox's algorithm

Primary source:
[Regular Expression Matching with a Trigram Index](https://swtch.com/~rsc/regexp/regexp4.html)
(Russ Cox, 2012) [upstream-documented / primary source — cite this for every
claim in this section unless noted].

The algorithm computes, bottom-up over the regex's parse tree, four sets per
node:

- **`match`** — the (possibly infinite) set of strings the subexpression can
  match. Represented abstractly/symbolically, never enumerated in full.
- **`exact`** — a _finite_ set of strings such that `match == exact` exactly,
  when the node's language is small and enumerable (e.g. literal `"foo"`, or
  small alternation `(cat|dog)`); `None`/absent otherwise.
- **`prefix`** — a set of strings, each a required prefix of every match, used
  when `exact` cannot be computed.
- **`suffix`** — dually, required suffixes of every match.

From whichever of these sets is available, the algorithm derives a trigram
**query**: a boolean AND/OR tree over "trigram T must appear in the document".
Composition rules (informally, per Cox):

- **Concatenation** `AB`: if both children have `exact` sets, the parent's
  `exact` set is the cross product `exact(A) × exact(B)` (capped in size — Cox
  caps the exact set at a small constant, beyond which it degrades to
  prefix/suffix tracking only, to bound blow-up from things like
  `(a|b|c|d)(a|b|c|d)(a|b|c|d)...`). If not exact, the parent's query is
  `query(A) AND query(B)`, plus a cross term combining `suffix(A)` and
  `prefix(B)` to catch trigrams that straddle the boundary between A and B's
  matched text (this is the subtle bit: a trigram index only cares that a
  literal 3-byte run occurs somewhere contiguous, and that run can span a
  concatenation boundary that neither child alone "owns").
- **Alternation** `A|B`: `exact` is the union if both are exact and small
  enough; otherwise the query is `query(A) OR query(B)` — and this is the single
  biggest source of query degradation, since OR-ing two big/uncertain subqueries
  usually yields something not much more selective than "match all" (see §2.2).
- **Star/plus/repetition** `A*`, `A+`, `A{n,}`: unbounded repetition destroys
  `exact` (infinite language) and generally destroys usable `prefix`/`suffix`
  beyond what a single iteration of A contributes, because the matched text can
  repeat A an arbitrary number of times with arbitrary content in between if A
  itself isn't a fixed literal — this is why `.*` inside a pattern is
  catastrophic (see §2.2).
- **Character classes** `[abc]`, `.`: `exact` is the (small) enumerated set for
  a small class; for `.` or a large class, degrades immediately to "match all"
  for that position, which usually propagates outward and poisons any
  surrounding trigram containing that position.

**Simplification**: the raw derived query is a boolean tree that can be
arbitrarily large (from combinatorial `exact`-set cross products); Cox's
implementation simplifies it — folding duplicate leaves, distributing
AND-over-OR to detect always-true subtrees, and capping exact-set sizes — down
to a bounded-size boolean query over trigrams before it is ever sent to the
posting-list executor.

### 2.2 What makes a regex unindexable

| Pattern feature                                                                      | Why it degrades                                                                                                            | Result                                                                                                                                                                                                                                                                                                 |
| ------------------------------------------------------------------------------------ | -------------------------------------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ | --- | ---- | ---------------------------------------------------------- | -------------------------------------------------------------------------------------- |
| Leading `.*` (`.*foo`)                                                               | No required prefix; `.*` matches from any position                                                                         | Query degenerates toward `query(foo)` alone — actually fine if the rest is a literal, since `.*` just means "foo can occur anywhere", but if `.*` appears **between** two required literals inside a longer chain it still forces the AND-of-suffix/prefix cross term to fall back to disjoint queries |
| `.*` immediately surrounded by nothing indexable (`.*`, `.+`, whole pattern is `.*`) | `match` = everything                                                                                                       | Query = "match all" — full corpus scan required                                                                                                                                                                                                                                                        |
| Alternation of many short literals (`(a                                              | b                                                                                                                          | c                                                                                                                                                                                                                                                                                                      | ... | z)`) | Each branch under ~3 chars has no derivable trigram at all | OR of "match all"s ⇒ degrades to match-all as soon as any branch is under 3 characters |
| Character classes / `.` spanning a required position                                 | Enumerable set too large to keep as `exact`                                                                                | Falls back, poisoning any trigram straddling that position                                                                                                                                                                                                                                             |
| Unbounded repetition (`a{2,}`, `x*`)                                                 | Infinite/variable-length match, prefix/suffix don't compose cleanly across iterations                                      | Only the fixed literal parts (if any) survive as constraints                                                                                                                                                                                                                                           |
| Case-insensitivity `(?i)`                                                            | Every literal becomes an alternation over case variants per character                                                      | Multiplies the derived query size, up to intractable for long literals — must cap and fall back                                                                                                                                                                                                        |
| Word boundaries `\b`, lookaround, backreferences                                     | Not a property of the matched _text_ at all — they're zero-width assertions about context, or (backreferences) not regular | Cannot be encoded in a trigram existence query at all; must be dropped from query derivation and left entirely to the verify pass                                                                                                                                                                      |
| Anchors `^`/`$` inside multiline text                                                | Similarly a position constraint, not a substring constraint                                                                | Same — drop from index query, verify-only                                                                                                                                                                                                                                                              |

The practical rule for an implementer: **derive the query, and if it degenerates
to "match all" (or if simplification produces a query whose estimated
selectivity from the trigram frequency stats — §1.3/1.8 — is worse than some
threshold, e.g. matches >X% of documents), skip the index step entirely and fall
through to a brute-force scan (§5)**. This fallback path is not an edge case to
special-case defensively; for a general-purpose regex tool it is a _frequent,
expected_ branch (leading `.*`, `\b`-only patterns, short alternations are all
common in real queries), so budget for it as a first-class code path, not an
escape hatch.

### 2.3 Deriving the query from `regex-syntax`'s HIR instead of hand-parsing

Rust's `regex` crate splits into `regex-syntax` (parser → AST → HIR),
`regex-automata` (NFA/DFA construction, literal extraction, prefilters, lazy
DFA, meta engine), and `regex` (thin API wrapper) — see
[Andrew Gallant's "Regex engine internals as a library"](https://burntsushi.net/regex-internals/)
[upstream-documented] and the
[rust-lang/regex repo](https://github.com/rust-lang/regex). A from-scratch
Cox-style planner does **not** need its own regex parser: walk
`regex_syntax::hir::Hir` (already gives you a clean recursive tree of `Literal`,
`Class`, `Repetition`, `Concat`, `Alternation`, `Look` nodes with Unicode
handled) and implement the `match`/`exact`/`prefix`/`suffix` computation as a
fold over `Hir`, mapping `Hir::Look` (word boundary, anchors) straight to "no
constraint, drop" per §2.2. This reuses a well-tested, actively maintained
Unicode-correct parser and HIR instead of reinventing regex parsing, and keeps
the trigram-planner logic focused purely on the lattice computation.

Separately, and worth understanding because it is _the same technique at a
different level_ of the stack: `regex-automata`'s own literal-extraction
machinery (used to build **prefilters** — Memchr/Memchr2/Memchr3/Memmem (Two-Way
algorithm)/Teddy/ByteSet/Aho-Corasick, selected automatically based on the
extracted literal set) [upstream-documented, chiark mirror
of regex_automata::util::prefilter docs; github.com/rust-lang/regex] already
computes something close to a `prefix` set for the whole pattern, used to skip
past non-matching regions quickly during actual scanning rather than to build an
index query. Teddy in particular is a SIMD algorithm (now living in the
`aho-corasick` crate) for matching several short literals simultaneously — it is
the mechanism the `regex` crate's meta engine falls back on for multi-literal
prefiltering when a pattern reduces to "one of these N short strings, then
verify" — and is directly relevant to your **verify pass**: after the trigram
index narrows candidates to a file set, running the compiled regex (with its own
Teddy/memchr prefilter already active) over each candidate file is the correct
verify strategy, not a naive DFA byte scan.

### 2.4 Verify-pass engines compared

| Engine                                                                             | Model                                                                                                                                    | Multi-pattern                                                                                                                          | SIMD prefilter                                                                  | Streaming                                                                                                                                        | Typical throughput                                                                                                                                                                                                                                                                                                                                                               | Notes                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                               |
| ---------------------------------------------------------------------------------- | ---------------------------------------------------------------------------------------------------------------------------------------- | -------------------------------------------------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------ | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Rust `regex` (regex-automata, "meta" engine)                                       | Hybrid: literal prefilter (memchr/Teddy) + lazy DFA, falls back to PikeVM for exotic features                                            | No native multi-pattern (single compiled `Regex`; `RegexSet` gives multi-pattern _membership_ only, not per-match capture efficiently) | Yes — Teddy/memchr/memmem, auto-selected from extracted literals                | No native streaming API on the whole match, but works fine incrementally over mmap'd bytes                                                       | Simple literal scans: multiple GB/s (see §5 ripgrep numbers, since ripgrep _is_ this engine); complex patterns markedly slower                                                                                                                                                                                                                                                   | Guarantees linear time (no catastrophic backtracking) — a hard requirement for an index-adjacent verify stage that must not be DoS'd by pathological input                                                                                                                                                                                                                                                                                                                                                                                          |
| RE2 (Google, C++)                                                                  | Automaton-based, also backtracking-free/linear-time                                                                                      | No                                                                                                                                     | Some literal prefiltering, less aggressive than Rust regex's Teddy integration  | Yes, works over streams                                                                                                                          | Comparable order of magnitude to Rust `regex` for typical patterns; both are "safe" linear engines with similar DFA-based architecture                                                                                                                                                                                                                                           | Reference safe engine; Rust `regex` is heavily inspired by/comparable to RE2's design (same original author lineage)                                                                                                                                                                                                                                                                                                                                                                                                                                |
| Hyperscan / Vectorscan (Intel; Vectorscan is the ARM/non-Intel-SIMD portable fork) | Compiles a **database of many patterns at once** into a hybrid of DFA/NFA/SIMD literal-matching (Hyperflex is the newest SIMD DFA model) | **Yes — this is its core value proposition**: designed from the ground up for thousands of simultaneous patterns                       | Extensive: SSE/AVX throughout                                                   | **Yes, native streaming mode** — matches across block boundaries without re-scanning, essential for scanning very large files or continuous data | Historically quoted per-database in Gbps (a "3.0 Gbps" database processes a 3000-bit block in 1 µs single-threaded) [upstream-documented, Intel hyperscan performance docs]; the newer Hyperflex DFA model reaches **8.89 Gbit/s**, up to **2.27×** faster than Hyperscan's default McClellan DFA model on the benchmarked workload [paper-reported, Hyperflex arXiv 2512.07123] | Best fit for your verify stage specifically _if_ you need to test a candidate file against many distinct regex queries at once (e.g. batch/saved-search scenarios) — its multi-pattern-simultaneous design amortizes scanning cost across patterns in a way a loop over N separate `regex::Regex` objects cannot. Rust bindings exist (`hyperscan` / `vectorscan-rs` crates) but are a C FFI wrapper, not a native Rust engine — factor in build complexity (needs Ragel/Boost historically for Hyperscan itself, though Vectorscan has eased this) |
| PCRE2 (with JIT)                                                                   | Backtracking engine, JIT-compiled to native code per pattern                                                                             | No                                                                                                                                     | No dedicated literal-SIMD layer beyond PCRE2's own start-of-match optimizations | No                                                                                                                                               | JIT gives large speedups over interpreted PCRE2 for typical patterns, but as a backtracking engine it retains **worst-case exponential blowup** on pathological patterns (nested quantifiers, catastrophic backtracking)                                                                                                                                                         | `ripgrep` exposes `-P`/`--pcre2` specifically to get lookaround/backreferences unavailable in its default linear engine — but explicitly trades away the linear-time guarantee. Only use PCRE2 in a verify stage if you also impose a match-step/time budget, since an attacker-controlled or just unlucky file can pathologically stall it                                                                                                                                                                                                         |

Bottom line for the build: use **Rust `regex`** (specifically drop to
`regex-automata` directly if you want manual control over which prefilter gets
selected, or want to reuse its literal extractor for §2.3) as the default verify
engine — it is linear-time-safe, already SIMD-prefiltered, in-process (no FFI),
and is the same engine `ripgrep` uses, so its throughput characteristics are
exactly the brute-force numbers in §5. Reserve Hyperscan/Vectorscan for a
specific scenario: verifying **many saved/simultaneous regex queries against the
same candidate set** (batch search, continuous indexing of a mail/log stream)
where its multi-pattern compilation pays for itself; the FFI/build cost is not
worth it for a single-query desktop search tool's hot path. Only reach for PCRE2
if a user query needs backreferences/lookaround the linear engine can't express,
and gate it with an explicit step budget.

---

## 3. Suffix structures

### 3.1 Suffix arrays (+ LCP)

A suffix array (SA) is a permutation of `0..n` giving the starting offsets of
all suffixes of the text in sorted order; combined with the LCP (longest common
prefix) array it supports binary-search substring lookup in `O(m log n)` (or
`O(m + log n)` with extra structures) for a pattern of length `m`, and
**enumerates every occurrence directly as a contiguous SA range** — ideal for
substring/literal search, weak on its own for regex (regex needs either
automaton-driven backward search, §3.3, or brute-forcing multiple literal
fragments through the SA).

- **Construction**: SA-IS (Nong, Zhang, Chen) is the standard linear-time `O(n)`
  construction algorithm and the baseline most libraries implement or benchmark
  against; DivSufSort is a widely used, very fast (though not asymptotically
  linear in the worst case for all variants) practical implementation,
  historically the fastest "in practice" SA builder for years and the one SDSL
  and many suffix-array Rust crates wrap or port. Parallel SA construction (e.g.
  parallel variants of SA-IS, DC3/skew, or prefix-doubling with parallel sort)
  trades single-core optimality for wall-clock time on multi-core build machines
  — relevant since this is purely an index-build-time cost, not query-time, so
  throwing cores at it is free.
- **Memory cost**: the naive SA is one `u32`/`u64` per input byte — for n < 4
  GiB, 4 bytes/byte of text = **4× the text size**; add the LCP array (another
  ~1 byte/char with the standard compact encoding, or a full 4 bytes/char
  naively) to get the commonly cited **"4–5n bytes"** figure for an SA+LCP index
  over n bytes of text [estimated / standard textbook figure — this is the
  well-known rule of thumb from the suffix-array literature
  (e.g. Manber-Myers-era analysis, repeated in most SA construction
  papers' motivation sections), not a single specific benchmark citation]. This is
  why a raw SA over gigabytes of source code is expensive: 100 GB of text → **400–500
  GB of SA+LCP**, which is why practical tools either (a) only build SAs over much
  smaller working sets per query (livegrep, §3.2) or (b) move to compressed/succinct
  alternatives (§3.4 onward) for anything corpus-scale.
- **Query cost**: `O(m log n)` for a plain binary search over the SA (two binary
  searches to find the range boundaries), improvable to `O(m + log n)` with an
  LCP-accelerated search, and to close to `O(m)` with additional structures
  (e.g. an FM-index, which is a compressed SA anyway — §3.3).

**livegrep** builds an in-memory suffix array over each indexed repository and
serves substring queries with SA binary search;
[livegrep's design](https://github.com/livegrep/livegrep) accepts the 4–5n
memory cost as the price for very fast substring/regex-candidate lookup over a
repo-scale (not full-disk-scale) corpus held resident in RAM — it is explicitly
a "keep the whole index in RAM, shard across machines" architecture, not
intended for a single-machine desktop-scale corpus without significant RAM.
**Rust crates**: `suffix` (pure-Rust SA construction, uses SA-IS or similar,
unmaintained-ish but functional for smaller corpora), `libdivsufsort`-wrapping
crates / `cdivsufsort`, `divsufsort` bindings (FFI to the C library, actively
the fastest option available in the Rust ecosystem as of this writing) — check
current maintenance status before depending on any of these long-term, as this
niche has had several half-maintained crates over the years [note:
verify current crates.io state at implementation time rather than trusting
this list — it is a snapshot; `libdivsufsort` itself is
MIT-licensed, permissive].

**Given the stated density preference, disqualify the plain SA up front**: at
4–5 bytes of index per byte of text (above), a 100 GB corpus costs 400–500 GB of
SA+LCP — larger than the corpus itself, and well outside a "denser index" budget
for a personal workstation. Treat the plain suffix array as a **stepping stone
to the FM-index/BWT** (§3.3, whose construction typically goes _through_ a
suffix array at build time) rather than as a candidate on-disk structure in its
own right. Its incremental-update cost is also poor: an SA is a global sort
order over the whole text, so changing one file's content in general perturbs
the sort order non-locally; livegrep's answer is to rebuild per-repository
rather than patch in place, a reasonable strategy at repo scale but a poor fit
for a single always-on personal-workstation index that should reflect edits
within seconds.

### 3.2 Suffix trees / suffix automata (CDAWG) — and why nobody ships them

A suffix tree (compressed trie of all suffixes) gives `O(m)` query time (better
than an SA's `O(m log n)`) and the suffix automaton / CDAWG (compact directed
acyclic word graph) gives the minimal automaton recognizing all substrings of
the text, but both cost **considerably more than 4–5n bytes** in any
straightforward pointer-based implementation — classic estimates put a naive
suffix tree at 10–20+ bytes/character due to per-node pointers, child-edge
structures, and suffix links, i.e. **2–5× worse than a plain suffix array** for
the same query power **[estimated / standard textbook comparison]**. This is
precisely why the field moved to suffix arrays (Manber & Myers's original
motivation) and then to succinct BWT-based structures (§3.3): they deliver
equivalent or better asymptotic query bounds at a fraction of the space. No
production text-search-at-scale tool in the literature reviewed here ships a raw
suffix tree/CDAWG as its primary on-disk index for exactly this reason; where
they appear it is in-memory, per-query, small-scope (e.g. building a suffix
automaton over a single small string for an online-algorithms use case), not
corpus-scale.

### 3.3 FM-index / BWT

**Mechanism.** The Burrows-Wheeler Transform (BWT) of the text is (in effect)
the sequence of characters preceding each suffix in suffix-sorted order — it is
computable from a suffix array (`BWT[i] = text[SA[i]-1]`) and is invertible. The
FM-index layers three structures on top of the BWT string L:

1. A **rank/select** structure over L (typically a **wavelet tree** over the
   alphabet, or per-character bitvectors with rank support for small alphabets)
   answering `rank_c(L, i)` — "how many occurrences of character c are in
   L[0..i]" — in O(1) or O(log σ) for alphabet size σ.
2. The **C-array**: for each character c, the count of characters in the text
   lexicographically smaller than c (a tiny, O(σ)-size table).
3. A **sampled suffix array** (every k-th SA value stored explicitly) to recover
   actual text offsets from BWT-space positions during `locate`.

**Backward search** (the FM-index's core operation): to find the SA range
matching a pattern P, process P's characters right-to-left, maintaining a range
`[lo, hi)` in BWT-order that corresponds to all suffixes currently matching the
processed suffix of P:

```
# Backward search for pattern P[0..m) over FM-index (C array, rank support over BWT L)
lo, hi = 0, n
for i in reversed(range(m)):
    c = P[i]
    lo = C[c] + rank(L, c, lo)
    hi = C[c] + rank(L, c, hi)
    if lo >= hi:
        return NO_MATCH
return (lo, hi)   # [lo, hi) is the SA range of all suffixes starting with P; hi-lo = occurrence count
```

`count(P)` (how many occurrences) falls straight out as `hi - lo` — O(m) rank
operations, no locate needed. `locate(P)` (where) additionally walks each
position in `[lo, hi)` back to a sampled SA entry using the **LF-mapping**
(`LF(i) = C[L[i]] + rank(L, L[i], i)`, i.e. "step backward through the text one
character at a time until you hit a sampled position"), costing up to
`O(sample_interval)` extra backward steps per occurrence — this is the classic
**count is cheap, locate is proportional to sampling density** tradeoff: a
denser SA sample (smaller interval) speeds locate but grows the index; a sparser
sample shrinks the index but slows locate linearly in the interval.

**Space.** SDSL-lite (the canonical reference C++ succinct-data-structure
library, [github.com/simongog/sdsl-lite](https://github.com/simongog/sdsl-lite))
ships the standard benchmark suite for exactly this. Two labelled figures from
recent (2026) succinct-structure literature building on/comparing against the
sdsl family:

- A modern high-throughput rank structure ("QuadRank") is reported achieving
  **14.4% space overhead at 2.29 bits per base-pair** for a 4-letter-alphabet
  (DNA) FM-index variant, markedly better than classical SDSL rank structures on
  the same workload [paper-reported, DROPS SEA 2026, QuadRank]. This is a
  DNA/4-symbol-alphabet number — for text-scale alphabets (ASCII/UTF-8, σ up to
  256+) the bits-per-character cost of the wavelet tree layer is higher; do not
  transplant the 2.29 bits/bp figure to source-code text without re-measuring,
  it is alphabet-size-sensitive.
- SDSL's own `rank_support_v5` is documented at **6.25% space overhead** over
  the raw bitvector it augments [upstream-documented, SDSL documentation/README
  lineage] — this is the overhead of _one_ rank layer, not the whole FM-index; a
  full FM-index over general text also needs the wavelet tree's own bit-per-character
  baseline (roughly `log2(σ)` bits/char as a floor before any rank overhead, so ~8
  bits/char baseline for a byte-alphabet FM-index before compression tricks) plus
  the sampled-SA overhead for locate.
- General rule of thumb repeated across the succinct-index literature: an
  FM-index over general (non-highly-compressible) text lands **around the size
  of the compressed text itself** (roughly the zeroth/higher-order empirical
  entropy of the source, often cited loosely as "close to gzip-sized" for
  natural-language-like text) **plus** a modest (typically single-digit-percent
  to ~25%, sampling-rate-dependent) overhead for the SA-sampling/locate
  structures [estimated / standard characterization from the FM-index
  literature (Ferragina & Manzini's original FM-index papers and the SDSL
  benchmark writeups); no single number applies universally, treat this as
  an order-of-magnitude claim only, and benchmark with `sdsl-lite`'s
  own benchmark harness against your actual corpus before committing to
  a budget].

**Incremental-update cost.** An FM-index is built from a global BWT of the text,
so a classical FM-index is **effectively static**: inserting or changing bytes
anywhere in the text changes the suffix order of everything lexicographically
near the change, which in general means recomputing the BWT (and its wavelet
tree, and its SA samples) over the whole affected partition. This is the
first-order objection to using an FM-index as the _live_ structure for a
near-real-time indexer: it is excellent for space, poor for update latency. The
standard mitigation, and the one to plan around for this build, is
**partitioning**: build one FM-index per segment (e.g. per directory subtree, or
per batch of N newly-seen/changed files, analogous to Lucene/Ferret-style
segment merging that the reader already knows from R6's territory), keep small
segments as a simple uncompressed posting/trigram structure that accepts cheap
incremental writes, and periodically **merge-and-rebuild** cold segments into a
dense FM-index in the background (this is exactly the job description of the
nice/ionice'd background indexer already required). A single changed file then
costs an update to its small live segment (cheap) rather than a global BWT
rebuild (expensive), at the price of having to query N segments instead of one —
the same segment-merge tradeoff as a classical inverted index, just with denser
leaf structures.

### 3.4 r-index and run-length compressed indexes for repetitive collections

This is the section most relevant to a **source-code corpus**, because a real
desktop/dev corpus (many files with shared boilerplate, vendored dependencies,
multiple versions of similar files, generated code) is _highly repetitive_ in
the technical sense the succinct-index literature means: the number of BWT runs
`r` (maximal runs of identical characters in the BWT string) can be orders of
magnitude smaller than the text length `n` for repetitive text, even though `n`
itself is large.

- **r-index** (Gagie, Navarro, Prezza — SODA 2018 paper
  ["Optimal-Time Text Indexing in BWT-runs Bounded Space"](https://arxiv.org/abs/1705.10382),
  journal version J. ACM 67(1):2, 2020) is the first full-text index whose size
  is **O(r)** rather than O(n), while still supporting `count` in near-optimal
  time and `locate` in `O(log(n/r))` per occurrence after an O(r)-size
  suffix-array sampling scheme (their key technical contribution — a sampling of
  size **2r**, versus the `O(n/r)`-per-occurrence cost a naive application of
  classical FM-index sampling would give at the same index size)
  [paper-reported]. On very repetitive datasets the r-index's reference
  implementation is reported to **locate orders of magnitude faster than RLCSA**
  (a prior run-length-compressed SA) at matched index size [paper-reported,
  per the r-index README/paper abstract, github.com/nicolaprezza/r-index].
  Reference implementation:
  [github.com/nicolaprezza/r-index](https://github.com/nicolaprezza/r-index)
  (C++, research-grade — not a drop-in library, expect to port the core ideas
  rather than FFI-wrap it for a production Rust tool).
- **move-r** (Bertram et al., SEA 2024,
  [drops.dagstuhl.de/entities/document/10.4230/LIPIcs.SEA.2024.1](https://drops.dagstuhl.de/entities/document/10.4230/LIPIcs.SEA.2024.1))
  is the current (2024) state of the art built on the **move structure** (a data
  structure achieving O(r) space _and_ O(1)-time LF-steps, which the classical
  r-index's sampling scheme does not achieve directly). Reported figures versus
  prior r-index variants: move-r answers count/locate queries **2–35× faster
  (typically ~15×)** than other locate-supporting r-indexes, at **0.8–2.5× the
  size (typically ~2×)**, and can be **constructed 0.9–2× as fast (typically
  ~2×)** while using **1/3–1× (typically 1/2×)** the construction memory of
  prior approaches [paper-reported, move-r SEA 2024 abstract]. This is the
  single most relevant recent advance for a repetitive source-code corpus: it is
  specifically optimized for the O(r)-space regime your workload sits in, and
  its 2024 vintage means it already supersedes plain r-index on both size and
  speed in the reported benchmarks. Follow-on 2024–2026 work (**b-move**, adding
  fast bidirectional extension for automaton-driven search — directly relevant
  to §3.5's regex-over-FM-index question — and further move-structure
  refinements in 2026 SEA/arXiv papers on "bounding the average move structure
  query" and "optimal-time move structure construction") indicates this is an
  active, still-moving research area, not a settled one; do not assume the 2024
  move-r numbers are the ceiling.
- **Grammar-compressed indexes** (e.g. indexes built over a
  straight-line-program/CFG grammar compression of the text, exploiting the same
  repetitiveness from a different angle than BWT-runs) are the other major
  branch of repetitive-text indexing; they are more attractive when repetition
  is _structural_ (e.g. many exact-duplicate large blocks, which is common in
  versioned document collections and vendored dependency trees) versus
  r-index/move-r's strength on _local_ repeat structure. No single
  implementation of this branch is as mature/portable as move-r's C++ reference
  at present; treat grammar-compressed indexing as a research direction to
  monitor rather than something to build against today for a first version.

**Incremental-update cost for r-index/move-r**: no better than the plain
FM-index in principle, and arguably worse in practice — these structures' entire
value proposition is exploiting _global_ run-structure in the BWT (`r`, the run
count, is a property of the whole text), so a local edit can in the worst case
split existing runs and increase `r` in ways that are not confined to the edited
region. All published r-index/move-r work targets **static, build-once
collections** (pangenomes, versioned reference corpora) — none of the papers
reviewed here describe an incremental/dynamic construction or update algorithm.
Treat r-index/move-r as **cold-segment-only** structures in the same partitioned
architecture described above for the plain FM-index: the background indexer
periodically folds a batch of now-stable, rarely-changing segments (vendored
dependencies, old commits, anything below a "hasn't changed in N days"
watermark) into an r-index/move-r segment, while recently-touched files stay in
a small, cheaply-mutable segment (even a plain positional trigram postings list,
§1) until they age into the next merge. This is the density payoff for accepting
query-time complexity (querying across a live segment plus several cold merged
segments), which is exactly the tradeoff the reader has already accepted by
stating a density-over-speed preference.

### 3.5 Can any of these do true regex, not just substring?

Yes, via **automaton search over the FM-index**, sometimes called "backward
search with a DFA" or, in the bidirectional variant, letting the NFA/DFA states
be intersected with the current BWT range as you extend the match character by
character in _either_ direction (bidirectional FM-index, which is what b-move
above targets). The mechanism generalizes backward search (§3.3) from "one fixed
pattern" to "the current SA range for every active NFA/DFA state":

```
# Regex-over-FM-index via bidirectional backward search (conceptual)
# dfa: regex compiled to a DFA (or NFA with subset-construction on the fly)
# state: set of (dfa_state, [lo,hi) SA-range) pairs, extended one char at a time
frontier = { (dfa.start_state, full_range) }
for position in corpus_positions_to_try:   # or, extend both directions from a seed
    next_frontier = {}
    for (dfa_state, [lo,hi)) in frontier:
        for c in alphabet:
            new_dfa_state = dfa.transition(dfa_state, c)
            if new_dfa_state is dead: continue
            new_lo, new_hi = extend_range([lo,hi), c)   # LF-mapping / rank step
            if new_lo < new_hi:
                merge_into(next_frontier, (new_dfa_state, [new_lo,new_hi)))
    frontier = next_frontier
    if any(dfa.is_accepting(s) for (s, _) in frontier): record_matches(...)
```

Cost is bounded by the **product of DFA states × active SA-range splits**, which
is exactly why this only pays off well for patterns whose DFA stays small
(bounded character classes, no catastrophic state blowup from large `{n,m}`
repetition or wide Unicode classes) — a regex that degrades badly under Cox's
trigram-query derivation (§2.2: leading `.*`, huge alternations) tends to _also_
degrade badly here, because both approaches are ultimately bottlenecked by how
much the pattern actually constrains the search space. Practical takeaway:
FM-index/automaton regex search is a genuine alternative to "trigram-filter then
verify" — its selling point is that it never needs a separate verify pass (the
match is exact by construction, no false positives) — but it is a heavier
per-query cost (interactive automaton simulation over compressed rank
structures, vs. a handful of cheap posting- list ANDs) and a substantially
heavier engineering lift than reusing Rust's `regex` crate for verification.
**Recommendation**: build the trigram-index + verify architecture first (§2);
treat FM-index/automaton regex search as a second-phase enhancement specifically
for the repetitive-corpus win (§3.4), not as the initial regex engine.

### 3.6 Rust availability

Since the reader is implementing his own structures rather than depending on
these, treat this table as **reference implementations to read, not crates to
`cargo add`** — but licence still matters if you port code or algorithms from
one, so it's stated per row. **Verify licence and last-release date yourself at
implementation time** — the values below are from general knowledge of these
projects, not a live crates.io/GitHub check performed this session, so treat
them as a starting point, not the final word.

| Crate                                        | Provides                                                                                                             | Licence (verify before reuse)                                                                                                                         | Maintenance signal (check at implementation time)                                                                                                                                               |
| -------------------------------------------- | -------------------------------------------------------------------------------------------------------------------- | ----------------------------------------------------------------------------------------------------------------------------------------------------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `sucds`                                      | Succinct data structures (bit vectors with rank/select, Elias-Fano, wavelet-matrix-adjacent structures) in pure Rust | MIT — permissive                                                                                                                                      | Active-ish, from a Japanese succinct-structures research group lineage; check recent crates.io publish dates before depending                                                                   |
| `succinct`                                   | Older pure-Rust succinct structures crate (rank/select, bitvectors)                                                  | MIT/Apache-2.0 dual — permissive                                                                                                                      | Historically less actively maintained than `sucds` as of last broad ecosystem review — verify current status                                                                                    |
| `fm-index` (crates.io)                       | A pure-Rust FM-index implementation (count/locate over a wavelet-tree-backed BWT)                                    | MIT — permissive                                                                                                                                      | Smaller/research-adjacent project; verify it covers your alphabet size and locate-sampling needs before committing, and benchmark against `sdsl-lite`'s C++ numbers rather than assuming parity |
| `bio` (rust-bio)                             | General bioinformatics toolkit including suffix arrays, BWT, FM-index primitives (DNA/protein alphabet oriented)     | MIT — permissive                                                                                                                                      | Actively used in the bioinformatics Rust ecosystem, but tuned for small (4–20 symbol) alphabets — re-benchmark for byte/UTF-8-scale alphabets before reuse                                      |
| `divsufsort` / `cdivsufsort` (FFI wrappers)  | Fast SA construction via the C `libdivsufsort`                                                                       | MIT (both the Rust wrapper and upstream `libdivsufsort`) — permissive                                                                                 | FFI dependency, but the underlying algorithm is the field's long-standing fast-in-practice choice                                                                                               |
| `xorf` (§4)                                  | `Xor8`/`Xor16`, `BinaryFuse8`/`BinaryFuse16` filters                                                                 | MIT/Apache-2.0 dual — permissive                                                                                                                      | Actively maintained, Lemire-lineage-adjacent; safe to depend on directly rather than reimplement, per §4.1                                                                                      |
| `sdsl-lite` (C++, reference only)            | The canonical succinct-structure benchmark/reference library cited throughout §3.3                                   | GPL-3.0 — **flag: copyleft, do not link into a permissively-licensed build**; read/port algorithms and consult its benchmarks, do not vendor its code | Read-only reference for algorithms and benchmark methodology, not a dependency candidate given the licence                                                                                      |
| `nicolaprezza/r-index` (C++, reference only) | Reference r-index implementation (§3.4)                                                                              | GPL-3.0 — **flag: copyleft**                                                                                                                          | Research-grade reference; port the _algorithm_ from the paper, do not vendor the code, under a GPL constraint                                                                                   |
| `suffix`                                     | Pure-Rust suffix array crate                                                                                         | Check last-publish date; this niche has churned                                                                                                       |

None of these give you a maintained, production-grade, Rust-native r-index or
move-r out of the box as of this research pass — the reference implementations
for the state-of-the-art repetitive-collection indexes (r-index, move-r, b-move)
are C/C++ research code. Budget for a **port**, not a `cargo add`, if you want
move-r-class space/speed for the repetitive-corpus case; this is a multi-week
undertaking in its own right and should be scoped as a distinct, later milestone
rather than bundled into a first-version regex-search build.

---

## 4. Approximate filters (candidate pruning)

### 4.1 Comparison table

| Filter                                       | Bits/element (typical, ~1% FPR)                                                                                                                                             | Query cost                                                                                                   | Supports deletion                               | Notes                                                                                                                                                                                                                                                   |
| -------------------------------------------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------ | ----------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | --------------------------------------------------------------------------------------------------------------------------------------- |
| Bloom (standard)                             | ~9.6 bits for 1% FPR (`-log2(0.01)/ln2 ≈ 9.6`, standard formula) [estimated from the standard Bloom-filter bits-per-element formula, not a specific benchmark]              | k hash + memory probes (k ≈ 7 at 1% FPR), can be cache-unfriendly (k random probes across a large bit array) | No (counting Bloom variant needed)              | Simplest to implement; blocked-Bloom variants trade a small FPR increase for confining probes to one cache line                                                                                                                                         |
| Blocked Bloom                                | Slightly higher bits/element than standard Bloom at equal FPR                                                                                                               | 1 cache-line fetch + k probes within it                                                                      | No                                              | The standard practical fix for Bloom's cache-unfriendliness; used internally by many modern filter libraries (e.g. is the shape RocksDB's "full filter" and Apache-ecosystem Bloom filters use)                                                         |
| Cuckoo filter                                | ~1 byte (8 bits)/element at ~3% FPR in the original paper's headline configuration, tunable by fingerprint size                                                             | 2 candidate bucket lookups + linear probing within bucket                                                    | **Yes** — its main advantage over Bloom/XOR     | Better than Bloom at same FPR in the original paper's benchmarks, but construction/insertion involves a randomized kick-out process that can (rarely) fail at high load factors                                                                         |
| XOR filter                                   | ~8 bits/element (`Xor8`) for ~0.4% FPR                                                                                                                                      | 3 fixed memory probes (no loop), simple fixed formula, no branching                                          | No (immutable, built once from a known key set) | Faster to query and smaller than Cuckoo/Bloom at matched FPR _for static/immutable sets_ — the tradeoff is it must be fully rebuilt on any change                                                                                                       |
| **Binary Fuse filter** (Lemire et al., 2022) | **~9 bits/element for `BinaryFuse8`** (≈2^-8 FPR, <0.4%); **~18 bits/element for `BinaryFuse16`** (~0.0015% FPR) [upstream-documented, xorf crate docs / Lemire's writeups] | 3 fixed memory probes (like XOR filter)                                                                      | No (immutable)                                  | **Current best-in-class** for static sets: smaller and faster to _construct_ than XOR filters at equal FPR, with a higher construction success probability; the direct answer to "which filter should a 2026 build use" for any immutable candidate set | Paper: [Binary Fuse Filters: Fast and Smaller Than Xor Filters](https://arxiv.org/pdf/2201.01174), Graf & Lemire, 2022 [paper-reported] |

Rust crates: the **`xorf`** crate (`docs.rs/xorf`) implements `Xor8`/`Xor16` and
`BinaryFuse8`/`BinaryFuse16` [docs.rs/xorf/latest/xorf/struct.BinaryFuse8.html]
— this is the crate to reach for directly; it is Lemire-lineage-adjacent (same
author/community as the reference C implementations) and actively maintained as
of the last broad review. A `bloomfilter` or `fastbloom` crate covers the
mutable/Bloom case when you need incremental inserts (binary fuse filters
require the full key set up front and are immutable once built, which is fine
for a per-shard/per-segment index rebuilt on a schedule, less fine for a live
single-document insert path).

### 4.2 Binary fuse filter probe pseudocode

Construction requires the full key set up front (it solves a peelable-hypergraph
construction problem across three hash-derived positions per key, which is why
it is immutable and needs all keys at once); query is three fixed lookups XORed
together and compared to a stored fingerprint:

```
# BinaryFuse8 membership probe (conceptual; xorf crate does the real construction/hashing)
fn contains(filter, key) -> bool:
    h = hash(key)                       # one strong hash of the key
    f = fingerprint8(h)                 # 8-bit fingerprint derived from h
    (h0, h1, h2) = filter.three_positions(h)   # three array positions, xorf's "binary-partitioned" segments
    return filter.fingerprints[h0] ^ filter.fingerprints[h1] ^ filter.fingerprints[h2] == f
```

For this project: a **binary fuse filter per file (or per fixed-size block) over
that file's trigram set** is a cheap admission test _before_ touching the real
trigram posting lists — a query trigram set that fails all per-file filters for
a shard can skip the shard's posting-list lookups entirely. At ~9 bits/element
for the trigram alphabet size involved, this is a very small structure relative
to the postings themselves and pays for itself whenever most shards/files don't
contain the query's trigrams (true for anything beyond a small corpus).

### 4.3 ugrep-indexer's actual design (per-file n-gram Bloom pre-index)

[github.com/Genivia/ugrep-indexer](https://github.com/Genivia/ugrep-indexer)
(now folded into `ugrep` ≥6.0 as `ug --index`) builds, per indexed file, a
Bloom-filter-like structure over the file's n-grams (bigrams and trigrams, in a
shared hash table but with **separate "bit tiers" per n-gram length so 2-grams
and 3-grams never share bits and cannot cross-contaminate each other's
false-positive rate**) [upstream-documented, ugrep-indexer README]. Its stated
goal is a **>10× search speedup** by letting `ug --index` skip files whose
filter proves they cannot contain the query pattern, which matters most on
**large, cold (not page-cache-warm) filesystems** [upstream-documented] — i.e.
it is explicitly targeting the "avoid a disk read/decompression at all" case,
not just avoiding a CPU-bound regex scan, which is the right framing for a
desktop tool where cold-cache disk I/O usually dominates over in-memory scan
cost anyway (see §5). A notable correctness-relevant design choice: it uses **N²
hash functions rather than N** for its per-n-gram Bloom filter specifically
because short patterns have too few distinct n-grams to get a low false-positive
rate from a standard single-hash-per-position Bloom scheme — using more hash
functions per n-gram compensates [upstream-documented]. Its explicitly
documented **limitation**: indexed search is incompatible with
`-v`/`--invert-match`, `--filter`, `-P`/`--perl-regexp`, and `-Z`/`--fuzzy`
[upstream-documented] — the general lesson being that a pre-index filter only
helps for queries where "does the target substring/pattern's n-grams occur in
this file" is a valid _necessary_ condition; inverted matches and fuzzy matches
don't have that property (a fuzzy match can occur even when no exact n-gram from
the pattern is present), so any filter-based pre-index in your design needs the
same explicit escape hatch for those query classes — treat "can this query even
be filter-accelerated" as a first-class check on the query, not an assumption.

### 4.4 Hierarchical / block-level composition

The general composition pattern across all of §3–4: **coarse filter → finer
filter → exact verify**, each layer eliminating candidates cheaply before the
more expensive layer runs on what's left:

1. **Corpus/shard-level filter** (e.g. one binary fuse filter per shard over all
   trigrams present anywhere in the shard) — eliminates whole shards.
2. **Per-file filter** (ugrep-indexer's model, §4.3) — eliminates whole files
   within a surviving shard, ideally _before_ the file's bytes are even read off
   disk (this is the layer that actually saves I/O, which is usually the
   dominant cost on desktop hardware — see §5).
3. **Trigram posting-list AND/OR** (§1–2) over files that survive step 2 — gives
   you either a doc-only candidate set or, with positional trigrams, a
   candidate-offset set.
4. **Regex verify pass** (§2.4) over exactly the bytes/offsets that survive step
   3 — the only step that is ever allowed to produce a false negative if
   skipped, and the only step guaranteed exact.

Each layer should be **strictly cheaper per candidate than the layer below it**
— that ordering is the entire point; putting the regex verify before the cheap
filters, or skipping straight to full-corpus posting-list ANDs without a
per-shard filter, throws away most of the benefit for a large corpus.

---

## 5. The honest baseline: brute-force scanning

### 5.1 Measured numbers

`[measured-by-me]`, this session, on a 32-core desktop-class machine (`nproc` =
32), using `ripgrep 15.1.0` (`rg --version` reports `features:+pcre2`), against
a synthetic **1.6 GB** single text file (20,000,000 repeated 80-byte lines of
ASCII lorem-ipsum-style content, generated with
`yes '...' | head -n 20000000 > big.txt`), file pre-warmed into page cache
(`cat big.txt > /dev/null` immediately before timing), single file so `rg` uses
its single-file (not directory-recursive multi-threaded) code path:

```
$ time rg -c "zebra" big.txt          # no match anywhere; single literal, memmem fast path
0
real 0m0.219s   →  ~7.3 GB/s effective scan rate

$ time rg -c "lazy" big.txt           # literal present on every line
20000000
real 0m1.216s   →  ~1.3 GB/s effective scan rate (dominated by per-line match/count bookkeeping, not the scan itself)

$ time rg -c "j[uv]mps" big.txt       # small char-class regex, present on every line
20000000
real 0m1.683s   →  ~0.95 GB/s effective scan rate
```

Caveats on these numbers, stated plainly: this is **single-threaded,
single-file, warm-cache, one synthetic repetitive file** — it demonstrates
order-of-magnitude behavior, not a corpus-scale benchmark. The no-match case is
much faster because `rg`'s literal prefilter (memmem/Two-Way, §2.3) can skip
most of the buffer without ever constructing a full line/match record; the
match-everywhere cases pay the cost of materializing 20 million match records,
which is a realistic worst case for "how fast can literal/regex matching itself
go" but not representative of typical sparse-match search. Do not extrapolate
these exact GB/s figures to a different corpus without re-measuring — they are a
sanity-check floor/ceiling, not a specification.

For **multi-threaded, multi-file, directory-recursive** ripgrep performance (the
actually relevant mode for a desktop search tool scanning a real corpus), see
the [official ripgrep benchmarks](https://ripgrep.dev/benchmarks/) and
[BurntSushi/ripgrep discussion #2997 on line-counting performance](https://github.com/BurntSushi/ripgrep/discussions/2997)
for further reference points — the official benchmark page's exact current
numbers were not independently re-measured for this report; treat any number you
take from it as **[third-party-benchmark]**, not `[measured-by-me]`.

### 5.2 What a full scan costs at desktop-relevant corpus sizes

Using the single-threaded measured warm-cache literal-search floor of **~1.3
GB/s** (the "matches everywhere" number — the closer analogue to a regex needing
to examine essentially every byte, versus the "no match" number which benefits
unfairly from early-exit-style skipping) as a conservative per-core rate, and
noting ripgrep parallelizes across files (not necessarily within one huge file)
on a real multi-file corpus:

| Corpus size | Single-thread @ ~1.3 GB/s | 8-way parallel (many-file corpus, optimistic near-linear scaling) | Cold-cache disk-bound (SATA SSD ~0.5 GB/s, NVMe ~3–7 GB/s) |
| ----------- | ------------------------- | ----------------------------------------------------------------- | ---------------------------------------------------------- |
| 10 GB       | ~7.7 s                    | ~1 s                                                              | NVMe: ~1.5–3 s; SATA SSD: ~20 s                            |
| 50 GB       | ~38 s                     | ~5 s                                                              | NVMe: ~7–17 s; SATA SSD: ~100 s                            |
| 200 GB      | ~154 s (~2.6 min)         | ~19 s                                                             | NVMe: ~29–67 s; SATA SSD: ~400 s (~6.7 min)                |

(Parallel-scaling and disk-bandwidth figures in this table are **[estimated]**
extrapolations from the single measured data point and well-known consumer
storage bandwidth ranges — not independently benchmarked for this report —
flagged explicitly because this table is the kind of thing that gets mistaken
for a measurement later.)

### 5.3 The honest architectural conclusion

For a **cold, disk-bound, 100–200 GB corpus**, a full brute-force scan is
already in the tens-of-seconds-to-few-minutes range on modern NVMe, and is
disk-bandwidth-bound, not CPU-bound — an index cannot beat disk bandwidth on a
genuinely cold read of the whole corpus, it can only help by **reading less of
it**. That reframes what "the index" is actually for: its job is not to make the
CPU-bound regex-matching step faster (brute-force scanning is already close to
memory/disk-bandwidth-bound for the common case, per §5.1's ~1–7 GB/s
single-thread figures, further parallelizable across files/cores) — its job is
to **avoid reading and touching most of the corpus's bytes at all**, especially
on a cold cache, which is exactly what the layered filter cascade in §4.4 is for
(per-file Bloom/fuse filters short-circuit before a file is even opened) and
exactly the framing ugrep-indexer's own README uses (§4.3: framed around
cold/slow filesystems, not CPU speed).

The corollary for query planning (§2.2): a regex that **degenerates to "match
all"** (leading `.*`, short alternations, `\b`-heavy patterns) is not a failure
state requiring special apology — it is simply the case where the honest answer
is "do the brute-force scan", and given §5.1–5.2's numbers, that is a
**perfectly acceptable answer** for a desktop tool even at 200 GB (tens of
seconds to a few minutes on NVMe, seconds on a warm cache). The architecture
that follows from this: **build the trigram index + verify pipeline as the fast
path for the common, indexable case (literal search, phrase search,
well-anchored regex), and make brute-force scanning a first-class,
well-optimized (memory-mapped, multi-threaded, SIMD- prefiltered — i.e. reuse
`ripgrep`'s own approach/crates directly) fallback for the unindexable case,
rather than trying to force every regex through the index at the cost of
correctness or index bloat.** This hybrid is not a compromise forced by running
out of time to build a "real" solution — for this workload, at these corpus
sizes, it is the architecturally correct answer, and it should be sized as such
in the project plan.

### 5.4 Verify-pass cost and index selectivity

The verify pass (§2.4) costs roughly the brute-force per-byte rate (§5.1)
applied to **only the candidate bytes/files the index selects**, plus fixed
per-file overhead (open, mmap, decompress if applicable). For the index to be
worth having at all for a given query shape, its selectivity must beat:

```
index_is_worth_it  ⇔  (candidate_bytes_after_filtering / total_corpus_bytes)
                        + (index_lookup_cost / total_scan_cost_if_brute_forced)
                        < 1
```

In practice this means: an index that only narrows a 200 GB corpus down to, say,
2 GB of candidates for a common literal search is an easy win (index lookup is
sub-millisecond to low-milliseconds against a memory-resident postings
structure, verify cost drops from ~150 s to ~1.5 s); an index that degrades to
"match all" or narrows only to 150 GB of candidates provides essentially no
value and its lookup cost is pure overhead on top of the brute-force scan it
didn't avoid — which is exactly why §2.2's fallback threshold (skip the index
path entirely below some estimated selectivity) is not an optional nicety but
load-bearing for overall system performance.

---

## 6. Structure-selection matrix

Given the stated preference (density over speed), budget tiers below are defined
**in bytes-of-index-per-byte-of-text**, not in abstract small/medium/large
labels, and every row also states its **incremental-update cost** as a
first-order property, not a footnote — a structure that is small but
rebuild-only is disqualified from being the _live_ segment in a near-real-time
indexer even if it's the eventual resting place for cold data (§3.3/§3.4's
segment-merge answer).

Density tiers used below:

- **Dense** (≲0.1–0.3× text): binary fuse/blocked-Bloom filters (§4, a few
  bits/element ≈ well under 0.1× text for typical trigram cardinalities),
  FM-index/r-index/move-r (§3.3–3.4, at or below compressed-text size).
- **Moderate** (~1–1.5× text): positional trigram index (zoekt: ~1.2×,
  §1.2/1.8), doc-only trigram index (well under 1× in absolute terms but listed
  here because it's the natural companion to a moderate-budget build).
- **Heavy** (≥2–5× text): plain suffix array + LCP (4–5×, §3.1 —
  **disqualified** under a density-first mandate except as a build-time-only
  intermediate), suffix tree/CDAWG (§3.2, even worse, never recommended here).

| Query shape                                           | Dense (≲0.3× text)                                                                                                                                                                                                                                                                                                | Moderate (~1–1.5× text)                                                                                                                                                                                                                                                                                                                                                           | Heavy (≥2×, generally disqualified)                                                                                                                                                                                                  | Incremental-update cost                                                                                                                                                                                                                                                                                                                                                                                                                                                                                   |
| ----------------------------------------------------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| **Filename substring**                                | Trigram index over filenames only — the whole filename corpus is a rounding error at 1M–5M files (each path a few dozen bytes), so this is effectively free at any tier; always build this                                                                                                                        | Same — no reason to spend more                                                                                                                                                                                                                                                                                                                                                    | Never justified — filenames are too small a corpus for this axis to matter                                                                                                                                                           | Cheap: a single file rename/create/delete touches a handful of trigram postings; keep this structure fully mutable in place, no segment-merge needed at this scale                                                                                                                                                                                                                                                                                                                                        |
| **Content literal** (single exact string)             | Per-file binary fuse filter over the file's trigrams (§4.2/4.4) as the admission gate, backed by a doc-only trigram index (§1.2) for files that pass; brute-force verify the (now small) survivor set                                                                                                             | Positional trigram index (zoekt-style, §1.5) — verify jumps straight to candidate offsets, no full-file rescan; this is the "slightly slower, denser-than-naive-positional" middle ground the reader is asking for is actually the _reverse_ trade here — positional trigrams cost more than doc-only, so only spend this if literal-search latency on large files matters to you | Never — an SA's 4–5× cost buys locate-time that a trigram+verify pipeline already gets close enough to for a desktop tool (§5.4)                                                                                                     | Doc-only/positional trigram postings are cheaply mutable (append postings for new files, tombstone-and-later-compact for deletes/edits) — this is the live segment; fuse filters are immutable, so rebuild the per-file filter only for changed files, not the whole corpus (§4 filters are per-file, so this is already naturally incremental at file granularity)                                                                                                                                       |
| **Content phrase** (multi-word / multi-token literal) | Doc-only trigram AND across the phrase's rarest trigrams (§1.4) behind the same fuse-filter gate                                                                                                                                                                                                                  | Positional trigrams with the adjacency check (§1.5's successor-list technique) — built for exactly this                                                                                                                                                                                                                                                                           | Never, same reasoning as above                                                                                                                                                                                                       | Same as content-literal row; a phrase index is the same postings structure, just queried differently                                                                                                                                                                                                                                                                                                                                                                                                      |
| **Content regex** (the hard requirement)              | Cox trigram-query derivation (§2.1) over a doc-only trigram index behind a per-shard/per-file fuse-filter gate (§4.4); brute-force fallback (§5) for the unindexable fraction — this is the **recommended default given the density mandate**: it is the cheapest structure that still makes the common case fast | Same derivation over positional trigrams if verify-latency on large files becomes the bottleneck in practice; still needs the brute-force fallback for degenerate patterns (§2.2)                                                                                                                                                                                                 | An SA is not the answer even here — its 4–5× cost buys nothing a trigram+verify pipeline doesn't already get; if you want _exact, false-positive-free_ regex matching, the answer is not a plain SA but the r-index/move-r row below | Live segment: trigram postings, cheaply mutable as above. **Cold segment** (background-merged, per §3.3/§3.4): fold stable/unchanged files into an **r-index/move-r structure** (§3.4) for exact automaton regex search (§3.5) at or below compressed-text size — this is the structure that most directly serves "denser index, slower is fine," and it fits a source-code corpus's repetitiveness specifically; budget it as a genuine second-phase deliverable, not exotica, given the density mandate |
| **Fuzzy / typo-tolerant**                             | Not filter-accelerable with exact n-gram filters at all (§4.3: ugrep-indexer explicitly excludes fuzzy from its indexed path) — brute-force with an approximate-matching-capable scanner is the only honest option regardless of budget tier                                                                      | A q-gram-relaxed candidate generator ("at least k of n query trigrams present" rather than strict AND) over the same moderate-tier trigram index narrows candidates before an approximate verify, trading recall for speed                                                                                                                                                        | Not worth the space regardless of tier — fuzzy matching doesn't benefit from exact-substring structures the way literal/regex search does                                                                                            | Same live/cold segment story as content-regex, since the underlying trigram structure is shared; the approximate-verify step itself has no separate index to update                                                                                                                                                                                                                                                                                                                                       |

**One-line summary of the whole matrix**: trigram-index-plus-verify is the
correct default for every content query shape except fuzzy; positional trigrams
beat doc-only trigrams as soon as you can afford ~1.2× corpus size;
succinct/repetition-aware structures (move-r) are a genuine, current
(2024–2026), and probably underused opportunity specifically because source code
is repetitive — but they are a second-phase investment, not a first-version
requirement; and brute-force scanning is not a failure mode, it is the correct,
load-bearing fallback for the unindexable regex tail and is fast enough on
modern NVMe that the whole system's honesty depends on treating it as a
first-class code path rather than an apology.

---

## Done-note

**What I could not verify directly** (no primary source located, or figure is
community-reported rather than paper/vendor-documented): the
Google-Code-Search-era doc-only trigram index/corpus size ratio (Cox's article
does not state one); `pg_trgm`'s commonly-quoted 2–4× index size
(Postgres-community folklore, not a controlled benchmark — flagged and not
relied on for any recommendation); exact current crates.io maintenance status of
`sucds`, `succinct`, `fm-index`, `bio`, `suffix`, `divsufsort`-wrapping crates
(I described relative maintenance signal from general knowledge but did not
check crates.io publish dates live — verify before depending on any of them);
the official ripgrep.dev benchmark page's current numbers (cited as a pointer,
not independently re-fetched/re-measured).

**Contradictions between sources**: none outright contradictory, but a tension
worth flagging — Cox's original algorithm and zoekt's implementation both derive
from the same lineage but zoekt's positional/successor-list approach is a
substantial _engineering_ elaboration on top of Cox's _algorithmic_
contribution; treat "Cox's algorithm" (query derivation, §2) and "zoekt's index
design" (storage/successor lists, §1.5) as two separate, complementary things a
reader could easily conflate as one system.

**Most underrated technique for this build**: the move-r / move-structure line
of work (§3.4). It is recent (2023–2026, actively still moving, per b-move and
2026 SEA papers extending it further), directly matches the repetitiveness a
real source-code corpus has (vendored dependencies, near- duplicate generated
files, multiple versions of similar files), and would give **exact,
false-positive-free regex search** (§3.5) with no verify-pass cost at all —
genuinely differentiated versus every trigram-index-plus- verify competitor. It
is underrated specifically because it has no mature Rust implementation (§3.6)
and is invisible to anyone who only reads the 2010s-era suffix-array/FM-index
literature instead of the 2023-2026 papers; most "how to build a code search
engine" writeups (including zoekt's own design doc) predate or ignore it
entirely.

**Most overrated technique for this build**: suffix trees/CDAWGs (§3.2) and,
more provocatively, chasing a from-scratch bespoke regex engine instead of
reusing Rust's `regex`/`regex-automata` crates (§2.3–2.4) for both query- plan
derivation (walking the HIR) and the verify pass. `regex-automata` already
contains a production-grade, actively maintained, linear-time-safe,
SIMD-prefiltered engine with a literal extractor that does most of what a
bespoke Cox-planner's leaf-level work needs — the only genuinely novel work
required for this project is the _trigram-index-specific_ lattice computation
(`match`/`exact`/`prefix`/`suffix` over the HIR and translating that into a
boolean trigram query) and the storage/postings-list engineering around it (§1),
not a new regex engine. Building or reaching for Hyperscan for a **single-query,
single-machine desktop tool** is also likely overkill per §2.4 — its
multi-pattern-simultaneous design doesn't pay for itself until you have many
concurrent saved queries, which is not the base case here.
