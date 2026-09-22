# R6 — Inverted Index Internals: Postings, Dictionaries, and Top-k

Scope: personal-workstation search, ~1M files (up to ~5M), tens to a few hundred
GB, single machine. NRT updates via a nice/ionice'd background indexer are a
hard requirement. TB/NAS/web scale is explicitly out.

Written for someone who already built Ferret (the Ruby Lucene port) — the
mid-2000s classical-IR baseline (segments, BM25, skip lists, merge policies,
front-coded dictionaries) is assumed known and covered only where this build's
constraints change the answer. The budget goes to what changed since:
Elias-Fano/PEF, block-max WAND/MaxScore, recursive graph bisection, SIMD codecs,
FST-based dictionaries with automaton intersection, and 2020s work.

**Dave's stated tradeoff, which drives every either/or in this chapter:** _"I'd
go for slightly slower search performance for a more efficient denser index."_
Every comparison below leads with bits/posting; decode speed is the secondary
column, and any technique that buys latency at the cost of bytes is flagged as
the wrong side of this build's tradeoff, explicitly.

Substring/regex/suffix structures (trigram indexes, suffix arrays, FM-index) are
R7's territory — mentioned here only where they trade directly against the
classical index (e.g. next-word indexes vs. positional postings for phrases).

---

## 1. Index anatomy

### 1.1 Postings ordering: document-ordered vs. impact-ordered vs. frequency-ordered

- **Document-ordered (docID-sorted)**: postings for a term are sorted by
  document id. This is what almost every production system (Lucene, PISA,
  Elasticsearch) uses, because it is the only ordering that supports **Boolean
  AND/OR/NOT via merge/intersection** and **skip-list `nextGEQ`** cheaply.
  Delta-encoding (d-gaps) works because consecutive ids are close together,
  especially after document reordering (§5).
- **Frequency-ordered**: postings sorted by descending term frequency in the
  document. Used inside **impact-ordered** lists (below), not as a replacement
  for the outer docID order — you lose the ability to intersect cheaply if you
  sort a whole list by frequency and discard docIDs' order.
- **Impact-ordered / impact layering**: for a term, group documents by a
  quantized "impact" score (a small integer, e.g. 8 levels of tf·idf-like
  weight) and store one docID list per impact level, sorted by docID _within_
  each level, levels ordered high-impact-first. This directly supports
  **score-sorted early termination** (read the highest-impact block first, stop
  once you have k candidates that can't be beaten) without needing WAND-style
  max-score bookkeeping. Classic reference: Anh & Moffat's "impact
  transformation" and "score-sorted" index work (V. Anh, A. Moffat, _Pruned
  Query Evaluation Using Pre-Computed Impacts_, SIGIR 2006). The cost is that
  impact-ordered indexes cannot do exact Boolean AND efficiently (docIDs are
  non-monotonic across the whole term), so systems that need both correct
  Boolean filtering and ranked retrieval (which a desktop search tool almost
  always does — "ext:pdf AND foo") keep **document-ordered** as the primary
  layout and add **block-max scores** (§4) for pruning instead of going fully
  impact-ordered.
- **Verdict for this build**: document-ordered postings with per-block max score
  metadata (BMW-style, §4) is the right default. Impact ordering is a
  web-search-scale technique for pure ranked retrieval and adds real complexity
  for a system that also needs exact filters (path, extension, date range,
  boolean).

### 1.2 What to store per posting: docs-only → +freq → +positions → +offsets

| Level                          | Contents                                       | Size vs. docs-only                                             | Use case                                                |
| ------------------------------ | ---------------------------------------------- | -------------------------------------------------------------- | ------------------------------------------------------- |
| Docs-only                      | docID                                          | 1x (baseline)                                                  | Boolean filters, existence, tag/facet fields            |
| Doc+freq                       | docID, tf                                      | ~1.15–1.4x [estimated]                                         | BM25/tf-idf ranking without phrases                     |
| Doc+freq+positions             | docID, tf, position list per doc               | typically **3–10x** the docs-only size [third-party-benchmark] | Phrase queries, proximity, highlighting                 |
| Full positional + byte offsets | + start/end byte or char offset per occurrence | +20–40% over position-only [estimated]                         | Exact snippet highlighting without re-scanning the file |

The dominant real number: Lucene's own documentation and multiple IR-textbook
treatments state that **position data is the single largest contributor to index
size** for a positional index — commonly **2–4x the size of a frequency-only
index**, and position lists compress worse than docID gaps because
within-document offsets are not clustered the way docID-gaps are after
reordering. Manning/Raghavan/Schütze (_Introduction to Information Retrieval_,
§5.2) gives the empirically observed rule of thumb that a positional index is
**2–4x larger** than a non-positional (freq-only) index for English text
collections [paper-reported, textbook]. PISA and ds2i-family systems commonly report
**6–14 bits/posting** for docID+freq combined on TREC GOV2/ClueWeb-scale collections
with Elias-Fano/PEF-family codecs (see §2 comparison table), _before_ adding positions
— positions add a list-per-occurrence overhead on top, which is why systems that
don't need phrase queries drop them entirely (Lucene lets you configure `IndexOptions.DOCS_AND_FREQS`
per field precisely for this reason; see Lucene `IndexOptions` javadoc, https://lucene.apache.org/core/9_11_0/core/org/apache/lucene/index/IndexOptions.html
[upstream-documented]).

**Practical recipe for this build**: store positions only for the primary "body
text" field, and only doc-existence (no freq) for structural/filter fields
(extension, directory depth, mtime bucket). This is exactly Lucene's per-field
`IndexOptions` model — steal it directly rather than re-designing.

### 1.3 Skip lists, block layout, forward index / doc values

Skip lists over postings are assumed known (Moffat & Zobel 1996). What's changed
since Ferret's era is mainly the block size and codec pairing:

- **Lucene's actual layout (concrete, implementable)**: postings are grouped
  into **blocks of 128 docs** (`Lucene99PostingsFormat`, formerly BP_HEAD /
  ForUtil-based codecs going back to Lucene 4.1). Each full block of 128
  integers is bit-packed with a single "bits-per-value" chosen to fit the
  block's max delta (a PFOR/FOR-style scheme, §2.4), so decode is a tight
  SIMD-able unpack loop; a final partial block (<128) falls back to vInt
  (VByte). Skip data records, at intervals of `skipInterval^level`, the
  `(docID, freq-block-pointer, pos-block-pointer, payload-pointer)` needed to
  jump directly into the corresponding position/payload stream. Source: Lucene
  `Lucene99PostingsFormat` and `ForUtil` javadoc/source,
  https://lucene.apache.org/core/9_11_0/core/org/apache/lucene/codecs/lucene99/Lucene99PostingsFormat.html
  [upstream-documented]. **128 is the number to copy** if you want a codec
  that's SIMD-bitpackable and well-trodden.
- **Forward index / doc values**: a column-oriented, per-document store (docID →
  value) used for filtering/sorting/faceting without touching postings — e.g.
  mtime, size, path-depth. Lucene calls this "doc values" and stores it
  separately from postings, often delta+bit-packed similarly. For a desktop
  engine this is where you'd put mtime, size, extension-enum, and
  inode/generation for incremental-update bookkeeping.
- **Stored fields**: the actual retrievable content (e.g. path, a snippet
  source) kept row-oriented and typically block-compressed (Lucene uses 16KB
  blocks of LZ4/deflate for stored fields — irrelevant to ranking, but is what
  you use to reconstruct highlighted snippets without re-opening the original
  file).

### 1.4 Segment-based (LSM-like) design

An inverted index that must support incremental updates without a full rebuild
is almost universally built as **immutable, independently-searchable segments**,
exactly like an LSM tree:

- **Write path**: new/changed documents accumulate in an in-memory buffer (a
  small in-memory postings structure, e.g. a `BTreeMap<Term, Vec<Posting>>` or
  an FST-backed structure once large enough), which is periodically **flushed**
  to a new immutable segment on disk (its own dictionary + postings + doc
  values + stored fields).
- **Deletes/updates**: a segment is immutable, so a deleted or re-indexed
  document is marked in a **tombstone bitset** (one bit per segment-local docID)
  rather than physically removed. An "update" is logically **delete-old +
  insert-new**; the new version lands in whatever segment is currently being
  written. Query time: every posting-list iterator is wrapped in a live-docs
  filter that skips tombstoned ids. This is exactly Lucene's `liveDocs` bitset
  mechanism
  (https://lucene.apache.org/core/9_11_0/core/org/apache/lucene/index/LeafReader.html
  — `getLiveDocs()`) [upstream-documented].
- **Merge policies**: segments accumulate and must be periodically merged
  (rewrite N small segments into one larger one, physically dropping tombstoned
  docs and re-optimizing dictionaries/postings). Two canonical policies:
  - **Tiered merge** (Lucene's default, `TieredMergePolicy`): groups segments
    into size tiers and merges within a tier when a tier has "too many"
    segments, targeting a bounded number of segments per tier and a max segment
    size. This amortizes merge cost logarithmically in the number of documents
    ever written, similar to leveled/tiered LSM compaction. See
    https://lucene.apache.org/core/9_11_0/core/org/apache/lucene/index/TieredMergePolicy.html
    [upstream-documented].
  - **Logarithmic merge** (older `LogByteSizeMergePolicy` /
    `LogDocMergePolicy`): segments sized in geometric levels (each level ~M× the
    previous); merge when a level accumulates M segments. Simpler, less adaptive
    to deletion skew than tiered.
  - **Cost model**: each document is rewritten O(log_M(N/flush_size)) times over
    its lifetime in the index (M = merge fan-in), the same amortized write
    amplification argument as leveled LSM trees. A single-document **update**
    costs: (a) O(1) amortized to append to the in-memory buffer, but (b)
    triggers eventual rewrite of every segment that ever contains that doc
    during merges — i.e. update cost is dominated by merge write-amplification,
    not by the update itself. For 1M files with modest churn (typical desktop: a
    few thousand file changes/day), this is cheap; it only matters if you're
    indexing something like a build directory with very high churn.
  - **Deletion-percentage-triggered merging**: Lucene also force-merges a
    segment once its tombstone ratio crosses a threshold (default ~20%
    reclaimable-deletes weight in tiered merge scoring), to bound the fraction
    of dead space walked at query time.
- **Why this matters for a desktop tool specifically**: this design gives you
  (a) crash safety for free (a segment is written once, then only read —
  atomicity is one `rename`/commit-point away, §5.3), (b) NRT (near-real-time)
  search by exposing the in-memory buffer as a zero-flush-cost searchable
  segment, and (c) bounded rebuild cost when the user edits one file among a
  million.

---

## 2. Postings compression

The central design decision. Everything below assumes **document-ordered,
delta-encoded (d-gap)** postings unless noted.

### 2.1 Variable-byte family

- **VByte (byte-aligned varint)**: 7 data bits + 1 continuation bit per byte.
  Simple, byte-aligned, universally supported (Lucene's fallback for partial
  blocks is exactly this — `vInt`). Typical cost: **~8–16 bits/posting** after
  delta-gapping on a docID-ordered, well-clustered collection [estimated
  based on textbook gap distributions]; decode speed is branch-heavy (~1–2 ns/int
  scalar) because of the per-byte continuation check, but is trivial to implement
  correctly, which is why it's the fallback path even in highly optimized systems.
- **Group varint (Google's, aka "varint-GB")**: pack 4 integers' length-tags
  into one selector byte, then the four values contiguous — removes the per-byte
  branch, decode ~2–4x faster than plain VByte with SIMD-friendly layout. Used
  in early Google/Chromium-adjacent systems; described in Dean's "Challenges in
  Building Large-Scale Information Retrieval Systems" (WSDM 2009 keynote
  slides).
- **StreamVByte** (Lemire & Boytsov successor to group-varint): separates the
  "how many bytes per integer" control stream from the data stream entirely
  (control bytes packed 4-per-byte, data written as a flat byte stream),
  enabling **SIMD-vectorized decode**. Benchmarked by Lemire at roughly **3-4x
  faster decode than VByte**, in the same compression ratio ballpark (control
  overhead ~2 bits/int extra vs plain vbyte in the worst case). Source: D.
  Lemire, N. Kurz, L. Boytsov, "Decoding billions of integers per second through
  vectorization"-adjacent StreamVByte writeup,
  https://github.com/lemire/streamvbyte and associated blog posts
  [third-party-benchmark]. **Rust crate: `streamvbyte64` / `stream-vbyte`
  (community ports; check maintenance — these are thin, low-churn crates, last
  meaningful IR-crate activity should be checked at implementation time)**.

### 2.2 Frame of Reference (FOR) and patched variants

- **FOR (Frame of Reference)**: within a block, subtract the block minimum,
  bit-pack all values to the number of bits needed for the block's max residual.
  Extremely fast decode (fixed-width unpack, fully SIMD-able) but a single large
  outlier in a block blows up the bit-width for the whole block.
- **PForDelta (Patched Frame of Reference)**: pick a bit-width b that covers
  ~90% of the block's values; store the rest ("exceptions") as out-of-band
  (docID/value) patches with an escape marker in the bit-packed stream, patched
  back in after unpacking. Reference: Zukowski, Heman, Nagel, Boncz,
  "Super-Scalar RAM-CPU Cache Compression", ICDE 2006 (the original
  PFOR/PFOR-DELTA paper).
- **NewPFD / OptPFor**: improved exception encoding (store exceptions in a
  separate contiguous list rather than interleaved escape codes) and optimal
  per-block bit-width selection to minimize total size including exception
  overhead. Described and benchmarked in Lemire & Boytsov, "Decoding billions of
  integers per second through vectorization", Software: Practice and Experience
  2015, https://arxiv.org/abs/1209.2137 [paper-reported]. Their reported numbers
  (Lemire & Boytsov 2015, on the ClueWeb09/Gov2-class test sets used in that
  paper): **OptPFD achieves roughly 3–6 bits/int** on typical d-gapped postings
  and decodes at **~1000–2500 million ints/sec** on the hardware of that paper
  (2015-era x86, single core, SSE) [paper-reported] — treat the absolute ns
  numbers as dated (10+ year old CPUs) but the _relative_ ranking vs VByte
  (5-10x faster decode) has held up in later re-benchmarks.
- **SIMD-BP128 / SIMD-FastPFOR** (Lemire & Boytsov, same paper): applies
  FOR/PFOR bit-packing in blocks of 128 with an SSE/AVX-vectorized pack/unpack
  kernel. This is the fastest-decode family in that paper's benchmarks, at a
  small compression cost vs OptPFD. **This is the practical default to
  implement** — it's what the `bitpacking` Rust crate and PISA's `block_codecs`
  both target.

### 2.3 Simple9 / Simple16 / Simple8b

- **Simple9**: pack as many integers as fit into a 32-bit word, using a 4-bit
  selector (9 possible layouts, e.g. 28×1-bit, 14×2-bit, ... 1×28-bit)
  - 28 data bits. Simple to decode (one selector-indexed unpack), wastes bits
    when a word can't be fully packed (up to ~3 bits/word waste). Anh & Moffat,
    "Inverted Index Compression Using Word-Aligned Binary Codes", Information
    Retrieval 2005.
- **Simple16**: 16 selector patterns instead of 9, better packing efficiency
  (less wasted space per word), same decode structure.
- **Simple8b**: 64-bit word version (used in Facebook's Gorilla / InfluxDB and
  other time-series systems as much as in IR) — up to 240 1-bit values or as few
  as 1×60-bit value per word, with a 4-bit selector. Good when gaps are
  extremely skewed (many 1s from adjacent postings, occasional large gaps).
- These word-aligned codes generally sit **between VByte and FOR/PFOR-family
  codecs** in both compression ratio and decode speed — Lemire & Boytsov's 2015
  benchmarks put Simple9/Simple16 clearly behind SIMD-BP128/OptPFD on decode
  speed, at comparable-to-slightly-worse bits/int [paper-reported]. Mostly of
  historical interest now; implement FOR/PFOR or Elias-Fano instead unless you
  already have a Simple9 codec lying around.

### 2.4 QMX, Masked-VByte, Varint-G8IU

- **QMX** (Trotman, "Compression, SIMD, and Postings Lists", ADCS 2014): a SIMD
  word-aligned scheme combining Simple-family selector packing with 128-bit SIMD
  lanes and a run-length extension for repeated selectors; reported to beat
  SIMD-BP128 on decode speed on some collections in the original paper
  [paper-reported], used in the `JASSjr`/JASS search engines (Trotman's group).
  Less widely adopted outside academic engines.
- **Masked-VByte** (Plaisance, Kurz, Lemire, "Vectorized VByte Decoding",
  2015/2016, https://arxiv.org/abs/1503.07387): a SIMD reformulation of plain
  VByte that keeps VByte's compression ratio but decodes several bytes at once
  using SSE shuffle/mask instructions — reported **2-4x faster than scalar VByte
  decode** at _identical_ compressed size [paper-reported]. Attractive when you
  want VByte's simplicity/robustness (no block bit-width selection, no
  exceptions) with much better decode speed. Rust: no widely-maintained crate as
  of this writing — porting the reference C implementation
  (https://github.com/lemire/MaskedVByte) is the realistic path if you want this
  exact scheme; otherwise VByte-family wins are more easily had via
  `bitpacking`'s BP128 or `streamvbyte`.
- **Varint-G8IU**: a group-varint variant packing into 8-byte SIMD lanes with a
  per-group descriptor; competitive with Masked-VByte in the same papers, not
  commonly packaged as a standalone library.

### 2.5 Elias-Fano and Partitioned Elias-Fano

This is the scheme most likely to be under-weighted by an engineer coming from a
"just VByte + PFOR" mental model, and it is the single highest-value addition to
this chapter's implementation list — get it right.

**Construction.** Given a monotonically non-decreasing sequence of n values
drawn from universe [0, u), Elias-Fano encodes each value x_i by splitting its
binary representation into:

- **low bits**: the l = ⌈log2(u/n)⌉ least-significant bits of x_i, stored
  **explicitly and contiguously** for every element (n·l bits total, simple
  fixed-width array — O(1) random access to the low bits of element i).
- **high bits**: the remaining (⌈log2 u⌉ − l) most-significant bits, stored
  **unary-differentially** in a single bitvector of length n + 2^(⌈log2 u⌉−l):
  for each element in order, emit a `0` for each unit increment its high part
  makes over the previous element's high part, then a `1` to mark "here is an
  element". Concretely: maintain a bitvector B of n + u/2^l bits, initialized to
  zero; for element i with high part h_i, set `B[h_i + i] = 1`. Because elements
  are sorted, h_i is non-decreasing, so this bitvector has exactly n ones and is
  monotone-consistent; reading it left to right and counting
  zeros-before-each-one recovers each h_i.

**Space bound**: total space is **n·l + n + o(n)** bits (the "+n" from the unary
high-bit encoding, o(n) from the select-support structure below) which Elias &
Fano (1974/1971, classical result) prove is at most **2 bits/element above the
information-theoretic optimal** for storing n values from a universe of size u,
i.e. ≈ n(2 + log2(u/n)) bits total, or **~2 + log2(u/n) bits per element**
[paper-reported theoretical bound; matches the summary given by Ottaviano
& Venturini]. For a term appearing in m out of N total documents (u = N, n = m),
this is exactly the right asymptotic — it gets _better_ as term frequency (n/u)
rises, degenerating gracefully instead of blowing up the way naive delta+vbyte
can on a very common term.

**`nextGEQ(x)` in ~O(1) amortized**: this is Elias-Fano's killer feature for
query processing. To find the first element ≥ x:

1. Compute x's high part h = x >> l.
2. Use a **select-on-zeros / rank structure** over the high-bit bitvector to
   jump directly to the position of the h-th group of ones (skip all the
   zero-runs before it) — this is what makes it O(1)-ish rather than a linear
   scan; the select structure itself costs o(n) extra bits (see Vigna's
   simple-select / Okanohara-Sadakane's structures; the practical Rust source of
   this is `sucds`, below).
3. From that starting bit position, scan forward through the 1s (each 1 found =
   one candidate whose high part == h, or advance to the next nonzero high value
   if the run is empty) and compare their low bits to x's low bits to find the
   first one ≥ x.
4. Because postings lists in a merge/intersection are walked with steadily
   increasing target values (leapfrog intersection, §4.1), sequential calls to
   `nextGEQ` from a previous returned position amortize to O(1) per call in
   practice, without needing to re-run global select each time — you keep a
   cursor and only fall back to select when the jump is large.

```text
// Elias-Fano encode (sketch)
fn ef_encode(values: &[u64], universe: u64) -> (BitVec /*low*/, BitVec /*high*/) {
    let n = values.len() as u64;
    let l = if n == 0 { 0 } else { (universe / n).max(1).ilog2() as u64 };
    let mut low = BitVec::with_capacity(n * l);
    let mut high = BitVec::zeros(n + (universe >> l) + 1);
    for (i, &v) in values.iter().enumerate() {
        low.push_bits(v & ((1 << l) - 1), l);
        let h = v >> l;
        high.set(h + i as u64, true); // monotone => strictly increasing set positions
    }
    (low, high)
}

// nextGEQ via select-on-high-bitvector + cursor (sketch)
fn next_geq(ef: &EliasFano, cursor: &mut EfCursor, x: u64) -> Option<u64> {
    let l = ef.low_bits;
    let target_high = x >> l;
    // jump the high-bitvector position to the first '1' whose *rank among 1s*
    // corresponds to target_high zeros having been consumed; select1 gives
    // position of the k-th 1, but we need "first 1 with h >= target_high",
    // which is: position = select0(target_high) + target_high, then scan 1s from there.
    cursor.seek_high(target_high);
    while let Some((pos_in_seq, v)) = cursor.next_candidate() {
        if v >= x { return Some(v); }
    }
    None
}
```

**Partitioned Elias-Fano (PEF)**: Ottaviano & Venturini, "Partitioned Elias-Fano
Indexes", SIGIR 2014
(https://www.di.unipi.it/~ottavian/files/elias_fano_sigir14.pdf). Plain EF's
bound of 2+log2(u/n) bits/element is tight only when the sequence is _uniformly_
distributed over [0,u) — real postings lists are bursty (clustered runs of a
common term, sparse elsewhere), so plain EF wastes bits on the sparse regions.
PEF splits a list into variable-length **chunks**, each re-based to its own
local universe [chunk_min, chunk_max], EF-encodes each chunk independently, and stores
a top-level EF-encoded array of chunk endpoints; chunk boundaries are chosen (dynamic
programming) to minimize total encoded size. `nextGEQ` becomes: binary-search/EF-search
the chunk-endpoint index to find the target chunk, then `nextGEQ` inside that chunk.
Ottaviano & Venturini report PEF matches or beats the previously best schemes (PFOR-family)
in space **while being nearly as fast to decode sequentially and much faster for
skipping**, on the Gov2 and ClueWeb09 collections used in that paper [paper-reported].

**Rust**: `sucds` (https://github.com/kampersanda/sucds) implements Elias-Fano
and succinct rank/select structures (`EliasFano`, `EliasFanoBuilder`) and is
reasonably maintained (Japanese IR-research lineage, used in production-grade
succinct-structure work). There is no widely-adopted, actively-maintained
standalone **partitioned** EF crate as of this writing — implementing PEF as a
thin layer over `sucds::EliasFano` per chunk is the realistic path. `quickwit`'s
and `tantivy`'s own codecs do _not_ use EF for postings (tantivy uses a
block-based VInt/bitpacking codec, `bitpacking` crate, by default) — EF shows up
in tantivy mainly for **doc-values / fast-fields** (e.g. `tantivy-common`'s
columnar crate uses bit-packing + EF-like techniques for sparse column
encoding), not the primary postings codec. Worth checking `tantivy`'s `columnar`
crate source directly if you want a working Rust EF reference to crib layout
decisions from.

### 2.6 Binary Interpolative Coding (BIC)

Moffat & Stuiver, "Binary Interpolative Coding for Effective Index Compression",
Information Retrieval 2000. Recursively encodes a sorted list by picking the
middle element, encoding it as an offset within the range implied by its known
lower/upper bounds and position, then recursing on the left and right halves.
Achieves **the best known compression ratio of any practical scheme on clustered
postings lists** (regularly cited as 1-2 bits/posting better than
PForDelta-family codecs on clustered data [paper-reported, per multiple
follow-up benchmark papers e.g. Trotman's ADCS work]), because it exploits _local_
clustering (not just global gap distribution) via the recursive range-narrowing.
The cost: **decode is inherently sequential and recursive — no known practical SIMD
vectorization**, and it is asymmetrically slow (recursive division-heavy decode,
no random access without decoding a whole subtree). Used in practice mainly for **archival
/ cold-tier index segments** where you're willing to trade decode speed for size
(e.g. an old, rarely-queried segment tier) or in explicit space-vs-speed studies
— not as a hot-path top-k codec. No maintained Rust crate is known; if wanted, it's
a same-file recursive implementation of a few dozen lines.

### 2.7 Roaring bitmaps

Chambi, Lemire, Kaser, Godin, "Better bitmap performance with Roaring bitmaps",
Software: Practice and Experience 2016 (https://arxiv.org/abs/1402.6407), plus
Lemire's ongoing benchmarks
(https://github.com/RoaringBitmap/RoaringFormatSpec). A Roaring bitmap
partitions the 32-bit value space into 2^16 "chunks" of 2^16 possible values
each; each chunk is stored as whichever of three container types is smallest:

- **array container**: sorted u16 array, used when the chunk has ≤4096 set
  values (below that, listing is cheaper than a 8KB bitmap).
- **bitmap container**: a dense 8KB (2^16 bits) bitmap, used for
  dense/mid-density chunks.
- **run container** (added later, "Roaring++"/RLE): run-length pairs (start,
  length), used when values inside the chunk are long contiguous runs — e.g.
  "every doc from 10000 to 10500 matches".

**When Roaring beats delta+codec lists**: Roaring is not competitive with
Elias-Fano/PFOR on _pure size_ for a sparse, evenly-scattered postings list of a
rare term — a plain delta-coded list wins there. Roaring wins when you need
**fast set operations** (AND/OR/NOT/XOR between many postings lists, or between
a postings list and a filter bitset like "files under ~/Documents") because
container-level operations (bitmap-AND, array intersection, run-intersection)
are extremely fast and vectorizable, and it degrades gracefully from sparse to
dense without a mode switch you have to manage yourself. This is exactly the
profile of a desktop search engine's **filter layer**: "extension == pdf" AND
"mtime > X" AND "under this directory" are usually large, dense-ish sets better
served by Roaring than by re-decoding delta-VByte lists on every query.
Concretely: **use document-ordered compressed postings (FOR/PFOR/EF) for ranked
term postings, and Roaring bitmaps for boolean filter fields** (tombstones,
extension buckets, directory-scope filters) — this is the same split Lucene
effectively makes (postings vs. `liveDocs`/point-range doc-value filters) even
though Lucene's liveDocs itself is a plain bitset, not Roaring.

**Rust**: `roaring` crate (https://github.com/RoaringBitmap/roaring-rs) —
actively maintained, used in production systems (e.g. Meilisearch uses it for
exactly this filter-bitset role: https://github.com/meilisearch/roaring-rs
history / Meilisearch's own docs), a very safe choice.

### 2.8 Recent work (2020–2026)

- **Learned/adaptive codecs**: research on ML-selected per-block codec choice
  (rather than fixed FOR/PFOR heuristics) has appeared in the ADCS/
  SIGIR-adjacent literature; results are collection-specific gains of a few
  percent over well-tuned OptPFD/PEF baselines, not a step change — treat as
  **not yet worth the implementation complexity for a desktop-scale index**
  [estimated judgment, not a specific number — no single canonical result
  to cite here; flagged rather than fabricated].
- **`sucds`-style succinct rank/select postings**: continued refinement of
  Elias-Fano/wavelet-tree-based succinct data structures for postings and
  doc-values (Japanese IR-research lineage — Kampersanda et al., multiple
  SIGIR/ECIR short papers 2020-2024 on succinct inverted indexes) —
  directionally: EF-family codecs remain the most actively developed
  "principled" compression family, vs. the largely-settled PFOR/BP128
  engineering family. This matches this chapter's recommendation to implement
  Elias-Fano rather than treat it as legacy.
- **PISA project itself** (https://github.com/pisa-engine/pisa) is the most
  actively maintained open reference for exactly this stack (postings
  compression + BMW/MaxScore + document reordering) and is the single best "read
  the source" target for a Rust reimplementation — its docs
  (https://pisa.readthedocs.io/) directly document compress_index and
  document_reordering as first-class pipeline steps [upstream-documented].
- Nothing found in this research pass amounts to a genuinely new _codec family_
  superseding Elias-Fano/PFOR/Roaring since ~2015; the 2020s literature is
  mostly (a) systems papers combining these with learned sparse retrieval
  (SPLADE-style) scoring, which is a ranking-model change not a postings-codec
  change, and (b) incremental engineering (variable-block BMW, superblock
  pruning — see §4) rather than storage format innovation. Flagging this
  explicitly rather than inventing a "2024 breakthrough codec" that doesn't
  exist in what was found.

### 2.9 Comparison table

| Scheme                      | Bits/posting (typical)                                                                    | Decode speed                                                   | Implementation complexity           | Where used                                        |
| --------------------------- | ----------------------------------------------------------------------------------------- | -------------------------------------------------------------- | ----------------------------------- | ------------------------------------------------- |
| VByte                       | ~8–16 [estimated, gap-distribution dependent]                                             | ~1-2 ns/int scalar [estimated]                                 | Trivial                             | Universal fallback (Lucene partial blocks)        |
| Masked-VByte                | same as VByte                                                                             | 2-4x scalar VByte [paper-reported, Plaisance/Kurz/Lemire 2015] | Low-moderate (SIMD shuffle table)   | Research systems; portable to Rust via port       |
| StreamVByte                 | ~VByte +small control overhead                                                            | ~3-4x scalar VByte [third-party-benchmark, Lemire]             | Moderate                            | General-purpose int compression (not IR-specific) |
| Simple9/16                  | ~similar to PFOR, slightly worse                                                          | Behind BP128/OptPFD [paper-reported]                           | Moderate                            | Legacy IR systems; largely superseded             |
| PForDelta/NewPFD/OptPFor    | ~3–6 bits/int on TREC-scale collections [paper-reported, Lemire & Boytsov 2015]           | ~1000-2500 M ints/s (2015 hw) [paper-reported]                 | Moderate-high (exception handling)  | Many production IR systems, ds2i/PISA             |
| SIMD-BP128                  | close to OptPFD, slight overhead                                                          | fastest in Lemire & Boytsov's 2015 comparison [paper-reported] | Moderate (SIMD pack/unpack kernels) | PISA `block_codecs`, `bitpacking` crate           |
| Elias-Fano                  | ≈2+log2(u/n) bits/elt [paper-reported theoretical bound]                                  | fast sequential decode; O(1)-ish nextGEQ [paper-reported]      | Moderate-high (select structure)    | ds2i, PISA, MG4J, `sucds`                         |
| Partitioned EF              | matches/beats PFOR-family in Ottaviano & Venturini's benchmarks [paper-reported]          | similar sequential decode, much faster skip than plain EF      | High (chunk DP + nested EF)         | ds2i, PISA                                        |
| Binary Interpolative Coding | best known on clustered lists, ~1-2 bits/posting better than PFOR-family [paper-reported] | slow, recursive, no SIMD                                       | High                                | Cold-tier/archival segments, research             |
| Roaring bitmap              | worse than EF/PFOR on sparse lists; near-optimal on dense/filter sets                     | very fast set ops (AND/OR)                                     | Low (use the crate)                 | Filter/tombstone/facet bitsets                    |

All "decode speed" figures above with a 2015 citation are on hardware over a
decade old; treat the _ranking_ (BP128 ≥ Masked-VByte > VByte, EF-family
competitive on both size and skip) as the durable fact, and re-benchmark
absolute ns/int on your own target hardware before making a final codec choice —
do not carry the absolute numbers forward as current.

### 2.10 Rust crates for this section — licence and last-release check

Checked directly against the crates.io API on 2026-09-04 [upstream-documented,
live-checked, not from memory]:

| Crate                                        | Licence                                      | Latest version | Last release                                                                                                           |
| -------------------------------------------- | -------------------------------------------- | -------------- | ---------------------------------------------------------------------------------------------------------------------- |
| `fst` (BurntSushi)                           | Unlicense OR MIT                             | 0.4.7          | 2021-06-06 — no releases in ~5 years, but stable/feature-complete; widely depended-on (ripgrep) so bit-rot risk is low |
| `roaring` (RoaringBitmap/roaring-rs)         | MIT OR Apache-2.0                            | 0.11.5         | 2026-08-12 — actively maintained                                                                                       |
| `sucds` (kampersanda)                        | MIT OR Apache-2.0                            | 0.9.1          | 2026-08-29 — actively maintained                                                                                       |
| `bitpacking` (quickwit-oss, SIMD-BP128 impl) | MIT                                          | 0.9.3          | 2026-01-08 — actively maintained                                                                                       |
| `stream-vbyte`                               | **non-standard licence — verify before use** | 0.4.1          | 2023-05-23                                                                                                             |
| `streamvbyte64`                              | MIT OR Apache-2.0                            | 0.2.0          | 2023-07-26                                                                                                             |

All permissively licensed except `stream-vbyte`'s "non-standard" declaration,
which needs a manual read of its `LICENSE` file before vendoring — treat as a
caution flag, not a rejection, since crates.io shows this label for several
legitimate MIT-equivalent texts that just aren't SPDX-recognized verbatim. Given
`fst`'s five-year release gap, plan to vendor/fork it mentally (i.e. be ready to
patch it yourself) rather than expecting upstream responsiveness — its scope
(ordered set/map FST) is narrow and stable enough that this is low risk. No
maintained Rust crate was found for partitioned Elias-Fano or recursive graph
bisection (§2.5, §4.5) — both are build-yourself territory regardless of
licence.

---

## 3. Term dictionary

| Structure                           | Size/term (rough)                                                                                                                                                                                    | Lookup cost                               | Supports automaton/regex enumeration?                                                          |
| ----------------------------------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ----------------------------------------- | ---------------------------------------------------------------------------------------------- |
| Sorted blocks + front coding        | Compact, needs term-block scan (linear in block size) after binary search on block heads                                                                                                             | O(log(#blocks) + block scan)              | Not directly — needs the whole block decoded to compare                                        |
| FST / MA-FSA (Lucene `.tim`/`.tip`) | Very compact for large shared-prefix vocabularies (Lucene reports typical FST term-index sizes as a small fraction of the term bytes, no single universal number) [upstream-documented, qualitative] | O(term length) traversal                  | **Yes** — this is the standout property (§3.1)                                                 |
| MARISA trie                         | Compact (bit-parallel, cache-friendly LOUDS-based)                                                                                                                                                   | O(term length)                            | Yes, similarly to FST, less common in IR use                                                   |
| Succinct trie / LOUDS               | Near information-theoretic minimum for the tree shape                                                                                                                                                | O(term length), some rank/select overhead | Yes                                                                                            |
| Hash table (term → term-id)         | Larger (needs stored keys or good hash+collision handling)                                                                                                                                           | O(1) average                              | **No** — hashing destroys order, no prefix/range/automaton traversal                           |
| Adaptive Radix Tree (ART)           | Compact for skewed key distributions, cache-friendly (fixed node sizes 4/16/48/256)                                                                                                                  | O(key length / node fanout)               | Yes, in principle (ordered radix structure), less commonly used for term dictionaries than FST |

### 3.1 FST-based dictionaries and automaton intersection (the important part)

Lucene's `.tim`/`.tip` files build the term dictionary as a **minimal acyclic
finite-state transducer (FST)**: input = term bytes, output = metadata (a file
pointer into the postings, or a term-id). Because it's an FST (not just an FSA),
_shared suffixes_ are merged the same way shared prefixes are, giving very high
compression on realistic vocabularies. Lucene codec reference:
`Lucene90BlockTreeTermsReader`/`FST` implementation,
https://lucene.apache.org/core/9_11_0/core/org/apache/lucene/codecs/blocktree/package-summary.html
[upstream-documented]. Rust: **`fst`** crate by BurntSushi
(https://github.com/BurntSushi/fst) — implements exactly this (an
ordered-set/map FST with automaton-based search), extremely well maintained and
widely used (ripgrep's ecosystem, tantivy's term dictionary in some
configurations).

**Why this is the structure that matters for wildcard/regex-over-terms**: an FST
is a DFA over the term-byte alphabet. Enumerating all terms matching a query
automaton Q (a wildcard pattern compiled to a DFA, a regex compiled to a DFA, or
a Levenshtein-automaton for fuzzy matching) is a **product automaton walk**:
simultaneously step the FST and Q from their respective start states on the same
byte, only continuing down paths where _both_ automata have a live state, and
only reporting a term as a match when the FST is in a state with an associated
output/final flag AND Q is in an accepting state.

```text
// FST ∩ DFA term enumeration (sketch, matches `fst::Automaton` design)
fn enumerate(fst: &Fst, dfa: &Dfa, out: &mut Vec<(String, Output)>) {
    fn go(fst_node: FstNode, dfa_state: DfaState, prefix: &mut Vec<u8>,
          fst: &Fst, dfa: &Dfa, out: &mut Vec<(String, Output)>) {
        if fst_node.is_final() && dfa.is_accepting(dfa_state) {
            out.push((String::from_utf8(prefix.clone()).unwrap(), fst_node.output()));
        }
        for (byte, next_fst_node) in fst_node.transitions() {
            if let Some(next_dfa_state) = dfa.step(dfa_state, byte) {
                prefix.push(byte);
                go(next_fst_node, next_dfa_state, prefix, fst, dfa, out);
                prefix.pop();
            }
            // if dfa.step returns None (dead state), this whole subtree is
            // pruned WITHOUT visiting it — this is the entire point: cost is
            // proportional to the number of *matching* FST paths explored,
            // not to the vocabulary size.
        }
    }
    go(fst.root(), dfa.start(), &mut Vec::new(), fst, dfa, out);
}
```

This is literally how `fst::Automaton` + `regex-automata`'s DFA (or a
hand-rolled Levenshtein automaton, à la `fst`'s `Levenshtein` type) power
wildcard/fuzzy search over the term dictionary in the `fst` crate today — see
the crate's `set::Automaton` documentation
(https://docs.rs/fst/latest/fst/trait.Automaton.html) [upstream-documented]. A
hash-table dictionary cannot do this at all (no ordering, no shared-prefix
structure to prune on); this is the concrete, load-bearing reason to prefer
FST/trie dictionaries over a hash map even though a hash map is faster for pure
exact-term lookup.

### 3.2 MARISA tries

MARISA (Matching Algorithm with Recursively Implemented StorAge) tries are
another LOUDS-family succinct trie, popular in CJK-text search engines and
available as a mature C++ library with Python/other bindings; conceptually
interchangeable with FST for the automaton-intersection use case, generally
slightly better on _very_ large, highly prefix-shared vocabularies at some cost
in update-friendliness (MARISA tries are typically built as a batch, not
incrementally, similarly to Lucene's `.tim`/`.tip` — both are "build once per
segment" structures, which fits the segment-based design in §1.4 well: you
rebuild the dictionary structure once per new immutable segment, never mutate
one in place).

### 3.3 Practical recommendation

Use `fst` for per-segment term dictionaries (this is a drop-in, well-tested
choice with automaton support built in), keep a small **term-id → postings file
offset** side table if you want term-ids decoupled from FST output values, and
skip ART/hash approaches unless a profiler shows FST traversal is your
bottleneck (unlikely at 1M-file / desktop scale — vocabularies are in the low
millions of terms at most, well within FST's comfortable range).

---

## 4. Query evaluation and top-k

### 4.1 Boolean AND/OR/NOT: leapfrog / galloping intersection

The general **leapfrog join** primitive for intersecting k sorted iterators,
each exposing `nextGEQ(x)`:

```text
fn leapfrog_and(iters: &mut [PostingIter]) -> Option<DocId> {
    let mut i = 0;
    let mut candidate = iters[0].current()?;
    loop {
        let mut advanced = false;
        for _ in 0..iters.len() {
            let v = iters[i].next_geq(candidate)?;
            if v != candidate {
                candidate = v;
                advanced = true;
            }
            i = (i + 1) % iters.len();
            if advanced { break; } // restart the round from the iter that moved
        }
        if !advanced {
            return Some(candidate); // all iterators agree on `candidate`
        }
    }
}
```

Practical variant: **galloping/exponential search** inside `nextGEQ` on a plain
sorted array (probe at offsets 1, 2, 4, 8, ... then binary search the bracket)
beats linear scan when the two lists have very different lengths (a rare term
AND'd with a common one) — this is the standard "skip the long list using the
short list's values" trick, and is exactly what skip lists (§1.3) and
Elias-Fano's `nextGEQ` (§2.5) are built to make cheap. For OR, merge iterators
by always advancing the minimum-current-value iterator(s); for NOT/AND-NOT, wrap
an iterator to skip ids present in the excluded set (cheapest when the excluded
set is a Roaring bitmap, §2.7).

### 4.2 Phrase queries — and the biggest density lever after codec choice

- **Positional postings** (§1.2): intersect "a" and "b" by docID (leapfrog AND,
  §4.1), then walk both position lists on survivors, checking pos("b") ==
  pos("a") + 1. Correct, fast, and it's the one thing that costs real bytes —
  full position data is routinely **2-4x the freq-only index size** (§1.2), so
  given the stated density preference this is the first thing to interrogate
  rather than accept as the default the way you would have in Ferret's era.
- **Position-free phrase resolution by re-scanning the source file**
  (density-optimal): drop position storage entirely (docs+freq only, §1.2's
  cheapest positional tier), and when a phrase query survives docID
  intersection, open the candidate file(s) and verify the phrase by
  substring/tokenize-and-check on the raw bytes. This is exactly what
  Elasticsearch's `match_only_text` field type does — it stores no positions and
  falls back to the stored `_source` for phrase/highlighting needs (see
  Elastic's docs on `match_only_text`,
  https://www.elastic.co/guide/en/elasticsearch/reference/current/text.html#match-only-text-field-type
  [upstream-documented]). **Cost model, concretely**: each surviving candidate
  costs one `open()` + `pread()` of (part of) the file instead of an in-index
  position-list decode. At this build's scale (local NVMe/SSD, candidate sets
  after AND are typically small — tens to low thousands, not millions), that's
  single-digit-ms per candidate file, dwarfed by the space saved. This is
  squarely the right side of "slightly slower search for a denser index" **as
  long as you keep the file's inode/mtime/generation in doc values so you can
  detect a since-modified file and fail the phrase check safely rather than
  reading stale content** — worth the one doc-value field it costs.
- **Next-word / bigram indexes** (middle ground, NOT recommended here): index
  bigrams as if they were terms — no per-occurrence positions, but vocabulary
  size grows by roughly an order of magnitude or more over unigram vocabulary
  [estimated, standard IR-textbook claim, no single agreed number], which is itself
  a density cost (more terms → bigger term dictionary and more distinct postings
  lists with worse compressibility each). Only serves the indexed n-gram length directly;
  3+-word phrases still chain bigram intersections or fall back to positions/re-scan.
  Given this build already needs the re-scan fallback for the rare case of a file
  that's since changed, bigrams add a second, more complex mechanism to get a benefit
  re-scanning already gets more simply — **skip this one**.
- **Recommendation for this build**: store **docs+freq only** for the body field
  (no positions), resolve phrase/proximity/highlighting by re-scanning the
  candidate file's current content, gated on an mtime/inode doc-value check.
  This is the single biggest concrete density win in this chapter after codec
  choice, and it is explicitly the "slower search, denser index" trade Dave
  asked for — accept it as the default, not as a fallback mode.

### 4.3 Proximity / NEAR

Same positional machinery as phrase, generalized: after docID intersection, for
each pair of terms compute the minimum position-distance across all occurrence
pairs (a merge-like sweep over both sorted position lists, O(p1

- p2) rather than O(p1 × p2)), and accept if within the NEAR window. Multi-term
  NEAR extends this to a sliding-window check over all terms' merged, tagged
  position streams.

### 4.4 WAND, Block-Max WAND, MaxScore, Block-Max MaxScore

All four are **dynamic pruning** algorithms for top-k ranked retrieval: they
skip fully decoding/scoring documents that provably cannot enter the current
top-k, using precomputed **upper bounds on each term's contribution to the
score**.

- **WAND** (Broder, Carmel, Herscovici, Soffer, Zien, "Efficient Query
  Evaluation using a Two-Level Retrieval Process", CIKM 2003): maintain a
  per-term global max-score upper bound (max score this term can ever contribute
  to any document, e.g. from BM25's max possible tf-component). Sort query
  terms' iterators by current docID; find the **pivot**: the first term (in
  docID order) such that the cumulative max-score of all terms up to and
  including it exceeds the current top-k threshold θ. All terms before the pivot
  are guaranteed unable to reach θ alone, so you advance them to the pivot's
  docID (skipping, not scoring), then fully evaluate the pivot document if all
  iterators actually align there.
- **Block-Max WAND (BMW)** (Ding & Suel, "Faster Top-k Document Retrieval Using
  Block-Max Indexes", SIGIR 2011): the key refinement — instead of one global
  max-score per term, store a **max score per block** (aligned with the
  postings' physical block layout, e.g. the same 128-doc blocks as the codec,
  §1.3) alongside the postings. This lets WAND's pivot check use a much tighter,
  _locally accurate_ bound before committing to full evaluation, skipping far
  more documents in practice than global WAND because a term's true max only
  rarely occurs near any given candidate block. **This is why the codec choice
  and the pruning algorithm are coupled**: block-max scores must be computed and
  stored at _index build time_, at the same block granularity your postings
  codec already uses — if your codec's fixed block size is 128 (Lucene-style),
  store one max-score float/quantized-int per 128-doc block per term.

```text
// Block-Max WAND main loop (sketch)
fn bmw_topk(mut cursors: Vec<TermCursor>, k: usize) -> TopK {
    let mut heap = MinHeap::with_capacity(k);
    let mut theta = f32::MIN; // current k-th best score, -inf until heap full
    loop {
        cursors.sort_by_key(|c| c.doc_id()); // reorder by current position
        // find pivot: smallest prefix whose cumulative UPPER bound > theta
        let mut acc = 0.0;
        let mut pivot_idx = None;
        for (i, c) in cursors.iter().enumerate() {
            acc += c.term_max_score();
            if acc > theta { pivot_idx = Some(i); break; }
        }
        let Some(pi) = pivot_idx else { break }; // no prefix can beat theta -> done
        let pivot_doc = cursors[pi].doc_id();
        if pivot_doc == SENTINEL_END { break; }

        if cursors[0].doc_id() == pivot_doc {
            // all cursors before pivot already at pivot_doc: must fully score,
            // but first check the BLOCK-MAX bound for a cheap reject
            let block_bound: f32 = cursors[..=pi].iter()
                .map(|c| c.block_max_score_for(pivot_doc)).sum();
            if block_bound <= theta {
                // skip the whole block for every term whose block ends before
                // the next block boundary, without decoding any postings
                for c in cursors[..=pi].iter_mut() { c.skip_to_next_block_or(pivot_doc + 1); }
                continue;
            }
            let score = full_score(&cursors[..=pi], pivot_doc);
            if heap.len() < k || score > theta {
                heap.push(pivot_doc, score);
                if heap.len() > k { heap.pop_min(); }
                theta = heap.min_score();
            }
            for c in cursors[..=pi].iter_mut() { c.next(); }
        } else {
            // advance the cursor furthest before the pivot up TO the pivot doc
            cursors[0].next_geq(pivot_doc);
        }
    }
    heap.into_sorted_topk()
}
```

- **MaxScore** (Turtle & Flood, "Query Evaluation: Strategies and
  Optimizations", IPM 1995 — older than WAND, re-popularized alongside WAND/BMW
  in modern re-implementations): splits query terms into "essential" (cannot be
  skipped, sum of _other_ terms' max scores can't reach θ without this one) and
  "non-essential" (can be looked up opportunistically only for docs the
  essential terms already produced) — a different partitioning strategy from
  WAND's pivoting, often competitive, sometimes faster when few terms dominate
  the score budget.
- **Block-Max MaxScore**: the same block-max augmentation applied to MaxScore
  instead of WAND. Recent comparative work (Mallia et al. and successors, e.g.
  "Faster Learned Sparse Retrieval with Block-Max Pruning", SIGIR 2024,
  https://arxiv.org/pdf/2405.01117) continues to refine block-max techniques
  specifically for **learned sparse retrieval** (SPLADE-style scores), and
  reports that superblock/adaptive-block pruning further reduces scored postings
  vs fixed-block BMW/MaxScore on those workloads [paper-reported, but scoped
  to learned-sparse scoring — not a direct drop-in number for classic BM25]. For
  classical BM25-style scoring on a desktop corpus, plain fixed-block BMW (Ding
  & Suel 2011) is the well-proven, simplest-to-implement choice; treat 2024+
  refinements as "worth revisiting if profiling shows BMW isn't pruning enough",
  not as a must-have on day one.
- **Practical note for this build**: these algorithms need θ to be meaningful
  early, which is why they're paired with heap-based top-k (a min-heap of size
  k) rather than full sort-then-truncate — θ only becomes useful once the heap
  is full, so the first k candidates are scored unconditionally.

### 4.5 Document identifier reassignment

Assigning docIDs isn't free — the natural order (e.g. inode order, crawl order,
insertion order) is essentially random with respect to term co-occurrence, which
makes d-gaps large and incompressible. Two concrete techniques, in increasing
sophistication:

- **URL/path-sorted ordering**: simply sort documents by their path/URL string
  before assigning docIDs. This alone gives large gains because files under the
  same directory tend to share vocabulary (same project, same file type) — this
  is a nearly-free win for a desktop file index (sort by path at index-build
  time) and is exactly what early web-search-engine papers found for URL-sorted
  crawls.
- **Recursive graph bisection (BP)** (Dhulipala, Kabiljo, Karrer, Ottaviano,
  Pupyrev, Shalita, "Compressing Graphs and Indexes with Recursive Graph
  Bisection", KDD 2016, https://arxiv.org/pdf/1602.08820 and
  https://www.cs.umd.edu/~laxman/papers/RecursiveBisection.pdf): builds a
  document-similarity graph (edges = shared terms, weighted) and recursively
  partitions it into two roughly-equal halves minimizing a log-gap objective
  directly tied to the delta-encoded size, assigning docIDs so that documents in
  the same half get contiguous ranges, recursing within each half. This is the
  current state of the art for minimizing _both_ compressed size and (as a
  direct consequence of denser/shorter gaps) query-time decode/skip cost — it is
  not merely a compression trick, it also **speeds up intersection and BMW
  pruning** because shorter gaps mean tighter block-max clustering. A
  reproducibility study (ECIR 2019,
  https://link.springer.com/chapter/10.1007/978-3-030-15712-8_22) confirms the
  original paper's gains hold up under independent re-implementation
  [paper-reported, cross-validated by a second team]. Reference implementation: https://github.com/mpetri/recursive_graph_bisection
  (C++, from one of the ECIR reproducibility-study authors) [upstream-documented,
  community
  reference]. **No maintained Rust crate found** — this would need porting; given
  it's a build-time-only batch step (run once per full re-merge, not per query),
  a C++ subprocess or a from-scratch Rust port are both reasonable. PISA documents
  BP-based reordering as a standard pipeline step: https://pisa.readthedocs.io/en/latest/document_reordering.html
  [upstream-documented].
- **For this build**: path-sort is nearly free and should be the default at
  minimum. Recursive graph bisection is a genuine engineering investment
  (building and partitioning a co-occurrence graph over up to ~1M documents) —
  worth doing only if profiling after path-sort shows index size or intersection
  speed still matters; it is unambiguously a should-do for a system chasing the
  smallest possible index, but it's the first thing to defer if the schedule is
  tight, since path-sort already captures much of the easy win for a filesystem
  corpus (unlike a web crawl, path-sort here is unusually strong because
  directory structure IS a real topical signal).

### 4.6 Tiering / early termination / static index pruning

- **Static pruning**: drop postings below a term-specific impact/score threshold
  at index-build time (e.g. keep only the top-N scoring documents per term).
  Note this is the one density technique that runs _backwards_ for this build:
  it trades size for lost recall, not size for latency, and a desktop tool's
  users expect exhaustive recall ("did I definitely search my whole disk").
  Reject it outright rather than filing it under the speed/density tradeoff — it
  isn't on that axis.
- **Tiering**: split each term's postings into a small high-quality "tier 0"
  (e.g. top-scoring documents) queried first, falling back to full postings only
  if tier 0 doesn't produce k confident results — safer than static pruning
  because it preserves exhaustive recall as a fallback path, at the cost of
  maintaining two postings copies per term.
- **Early termination**: any of §4.4's algorithms _are_ early termination in the
  ranked-retrieval sense; for pure Boolean/filter queries (no ranking), early
  termination doesn't apply — you need every matching document, so the
  leapfrog/galloping intersection speed (§4.1) is what matters, not pruning.

### 4.7 Scoring: BM25, BM25F, and desktop-specific priors

- **BM25** (Robertson & Zaragoza, "The Probabilistic Relevance Framework: BM25
  and Beyond", Foundations and Trends in IR, 2009 — the canonical reference for
  the formula and its parameters k1, b): the standard choice, needs per-document
  length and average document length (both cheap doc-values fields, §1.3) plus
  term IDF (derivable from postings list length) and per-doc-per-term tf.
- **BM25F**: extends BM25 to multiple weighted fields (e.g. filename vs. body
  vs. path-component tokens) by combining per-field tf with per-field weights
  before applying the saturation function, rather than scoring fields
  independently and summing — avoids over-rewarding a term that appears in many
  low-value fields. Directly applicable to a desktop search tool: filename
  matches should outweigh body matches for the same term.
- **Why a desktop tool wants more than BM25**: pure term-frequency relevance
  ignores signals a filesystem search cares about that a web corpus doesn't have
  in the same form —
  - **Recency**: mtime-based boost (a linear or log-decay function of file age)
    — trivial to add as a doc-value-driven final-score multiplier applied after
    BM25, not something to bake into the inverted index itself.
  - **Path affinity**: matches in the filename or a shallow path outrank matches
    buried in a deeply-nested directory or a generated/build artifact path —
    implementable as a BM25F field weight (filename field weighted higher) plus
    a path-depth doc-value penalty.
  - **File-type priors**: a hit in a `.md`/`.rs`/`.py` source file is usually
    more actionable than the same term appearing in a `.lock` or minified `.js`
    file — implementable as a small per-extension multiplier table, again
    applied post-BM25 rather than requiring index changes. These are all
    **score-combination-layer** additions on top of a standard BM25F core — none
    of them require changes to postings format or dictionary structure, which is
    why they're covered briefly here rather than in the codec sections.

---

## 5. Practical engineering

### 5.1 Index construction

- **In-memory accumulation + spill (SPIMI — Single-Pass In-Memory Indexing)**:
  accumulate postings in memory (hash map term → growing postings list) until a
  memory budget is hit, then sort terms and write the whole in-memory index to
  disk as one **sorted run** (no need to sort postings globally first — SPIMI's
  whole point is avoiding a global sort by writing complete-but-partial inverted
  indexes and merging them after). Reference: Manning/Raghavan/Schütze, IIR,
  §4.3 (https://nlp.stanford.edu/IR-book/pdf/04const.pdf) — the SPIMI algorithm
  is given as pseudocode there directly [textbook, well-established].
- **External merge**: once you have R sorted runs on disk, merge them with a
  k-way merge (priority queue over run heads, by term) into final segments —
  this is a textbook external-sort merge, one pass, I/O-bound.
- **Parallel construction**: shard the document set across threads/cores, each
  building its own SPIMI runs independently (share nothing, no locking), then
  merge all runs (from all shards) together at the end. This parallelizes almost
  perfectly for the accumulation phase; the final merge is the serial bottleneck
  unless you also parallelize the merge by term-range (partition the term space
  into disjoint ranges up front, e.g. by first-byte or a hash bucket, and merge
  each range's runs independently on a different core — this is straightforward
  because postings for different terms never interact).
- **Memory budget vs. throughput**: bigger in-memory buffers before a spill mean
  fewer, larger runs (less merge overhead) but slower time-to-first-
  searchable-segment and higher peak RSS; for a desktop tool doing background
  indexing, biasing toward smaller buffers (faster, smaller spills, lower peak
  memory, more merge passes) is usually the right tradeoff since you're not
  trying to minimize total indexing throughput on a fixed cluster budget, you're
  trying to stay unobtrusive on someone's laptop.

### 5.2 mmap vs. read

- **mmap upsides**: zero-copy access to postings/dictionary data, lets the OS
  page cache do the caching work for you, straightforward random access into
  large files without manual buffer management — this is what Lucene
  (`MMapDirectory`, default on 64-bit JVMs) and most C++ IR engines (PISA) do
  for postings/term-dictionary files.
- **mmap downsides / "mmap is not always the answer"**: page faults are
  synchronous and opaque to your scheduler — a query thread can block on disk
  I/O invisibly inside what looks like a memory read, with no ability to time
  out, retry elsewhere, or account for it in a latency budget. This is the core
  argument made in the LMDB-vs-alternatives and broader DB-engineering
  literature (see Andy Pavlo/CMU's widely-cited "Are You Sure You Want to Use
  MMAP in Your Database Management System?" VLDB 2022,
  https://db.cs.cmu.edu/mmap-cidr2022/ — arguing that buffer-pool-managed
  explicit I/O gives you back control over eviction policy, error handling, and
  concurrency that mmap silently takes away) [paper-reported,
  industry-recognized argument]. For a desktop search index specifically: the
  **failure mode that actually matters here** is a removable/network drive going
  away mid-mmap (SIGBUS on a page fault to a now-invalid mapping) — something a
  `read()`-based design turns into a normal, catchable I/O error instead of a
  process-terminating signal. Given a desktop tool must tolerate USB drives
  being yanked and network mounts dropping, this is a concrete, non-academic
  reason to at least **wrap mmap access with SIGBUS handling** (or default to
  buffered reads for anything not proven to be on stable local storage), rather
  than adopting mmap uncritically because "that's what Lucene does."
- **`madvise`**: use `MADV_RANDOM` for dictionary/term-index files accessed by
  point lookups (defeats readahead that would otherwise waste I/O bandwidth
  pulling in irrelevant neighboring pages), `MADV_SEQUENTIAL` or `MADV_WILLNEED`
  when merging segments or doing a full postings-list scan.
- **Practical default for this build**: mmap for read-only, already-fsynced
  segment files (the common case, and where the page-cache-sharing benefit is
  real — many segments, most read rarely, OS manages eviction for free), plain
  buffered reads for anything still being written or on storage you can't trust
  to stay mounted.

### 5.3 Crash safety and atomic index swap

- Segments are **immutable once written and fsynced**; the only mutable state is
  a small **commit point / manifest file** (list of live segment ids + a
  generation number), analogous to Lucene's `segments_N` file. Writing a new
  commit point is: write to a new temp file, `fsync` it, `rename()` over the
  well-known manifest path (atomic on POSIX filesystems for same-directory
  renames), then `fsync` the containing directory (needed on Linux to make the
  rename itself durable across a crash — this last step is the one most often
  forgotten).
- Crash recovery is then trivial: on startup, read the current manifest, open
  exactly the segments it lists, and ignore/garbage-collect any segment files
  not referenced by any manifest generation (they're either half-written or
  already-superseded-by-merge leftovers).
- This is the same commit protocol as Lucene's `IndexWriter.commit()` /
  `SegmentInfos`
  (https://lucene.apache.org/core/9_11_0/core/org/apache/lucene/index/SegmentInfos.html)
  and is worth copying close to verbatim rather than re-deriving.

### 5.4 Real-time / near-real-time indexing — the merge/nice-ionice cost model

NRT is a hard requirement here (filesystem-watcher-driven updates), paired with
a low-priority background indexer (`nice`/`ionice`), which changes the usual
Lucene-style calculus in one specific way: **merge I/O now competes with the
user's foreground disk activity, on purpose throttled to lose that contention**,
so merge scheduling has to be _rate-limited_, not just _deferred_.

- **Write/flush path** (unchanged from Lucene's NRT model): file-change event →
  re-tokenize just that file → append to the in-memory buffer/current
  segment-in-progress → flush to an immutable segment on a time or buffer-size
  trigger, exposing a fresh read-only view over buffer+unflushed-segments
  without a full commit (`DirectoryReader.open(IndexWriter)` is the reference
  shape,
  https://lucene.apache.org/core/9_11_0/core/org/apache/lucene/index/DirectoryReader.html
  [upstream-documented]). This gives sub-second visibility of an edit without a
  full-corpus rebuild.
- **Merge scheduling under `ionice -c3` (idle class)**: Lucene's own
  `ConcurrentMergeScheduler` throttles merge I/O by measuring achieved MB/s and
  inserting sleeps to hit a target rate
  (`ConcurrentMergeScheduler.setMaxMergesAndThreads` / auto-IO-throttle,
  https://lucene.apache.org/core/9_11_0/core/org/apache/lucene/index/ConcurrentMergeScheduler.html
  [upstream-documented]) — worth copying directly rather than relying on
  `ionice` alone, because `ionice -c3` only helps against _other_ idle-class or
  best-effort I/O; it does nothing to bound how much of your own merge work
  queues up when the disk is briefly saturated by something the OS scheduler
  treats as equal priority (e.g. another best-effort process, or best-effort vs.
  best-effort ties). A self-throttled merge scheduler (cap merge threads at 1,
  cap merge MB/s, back off further under CPU niceness) is the belt to `ionice`'s
  suspenders.
  - **Concrete throttle knob**: track bytes-merged-per-wall-second over a
    trailing window (e.g. 2s), sleep proportionally when over budget — this is
    exactly Lucene's `IOThrottle`/`MergeRateLimiter` and is a few dozen lines to
    reimplement.
- **Cost model to size the flush/merge parameters against**: at ~1M files and
  modest desktop churn, expect single-file re-index events to dominate (not bulk
  crawls after the first index build), so bias toward a **small flush
  threshold** (fast visibility, cheap individual flushes) and a **conservative
  tiered-merge trigger** (don't merge on every flush — batch several flushed
  segments before triggering a background merge), since each merge is the
  expensive, throttled operation and update-cost is dominated by merge
  write-amplification (§1.4). A reasonable starting point mirroring Lucene's
  defaults: flush at ~a few MB or ~1000 docs buffered (whichever first),
  tiered-merge trigger once ~10 segments accumulate in a size tier, merge fan-in
  of ~10 — then tune against observed background-indexer CPU/IO share on real
  hardware rather than these numbers, which are Lucene defaults transplanted
  here as a starting point, not a measurement of this build [estimated
  starting point].
- **Why this matters more here than in a typical Lucene deployment**: Lucene's
  usual deployment (a server with a dedicated indexing window or spare I/O
  headroom) can afford a merge scheduler that just goes as fast as allowed. A
  background indexer sharing a laptop's single NVMe with the user's actual work
  cannot — an unthrottled merge burst is exactly the kind of thing that makes a
  background search-indexer visible and resented. Treat the merge-rate-limiter
  as a first-class component, not an afterthought bolted onto
  `ConcurrentMergeScheduler`'s defaults.

---

## Done-note

**What I could not verify**: exact modern bits/posting and decode ns/int numbers
on _current_ hardware for the codec comparison table — the best citable numbers
(Lemire & Boytsov 2015, Ottaviano & Venturini 2014) are all 9-12 years old, and
I did not find a canonical recent (2023-2026) re-benchmark of the full codec
family on modern (AVX-512/ARM NEON) hardware in this pass; the _relative
rankings_ are corroborated across multiple sources (PISA docs, Lemire's own
blog, multiple follow-up papers) so I'm confident in the ordering, not the
absolute numbers. I also could not find a maintained, production-grade
**partitioned Elias-Fano** or **recursive graph bisection** Rust crate — both
would need to be ported from the reference C++ (ds2i / mpetri's
recursive_graph_bisection) if you want them rather than a from-scratch
re-derivation.

**Contradictions between sources**: none of real substance — the IR literature
here is unusually convergent (PISA, Lucene, and the academic papers all describe
the same block-based, skip-augmented, docID-ordered design). The only soft
tension is Manning/Raghavan/Schütze's textbook "2-4x" rule of thumb for
positional-index overhead vs. more recent PISA/ds2i papers which report
positions as a separate, additively-large cost rather than a flat multiplier —
both are directionally right but measure different things
(whole-index-with-positions vs. positions-list-alone), and I've labeled each
accordingly rather than merging them into one false-precision number.

**The three techniques that matter most for this specific build, ranked against
the stated preference for a denser index over a faster one**:

1. **Drop positions; resolve phrase/proximity/highlighting by re-scanning the
   candidate file (§4.2), `match_only_text`-style.** This is the single biggest
   density lever in the whole chapter — bigger than any codec choice, because it
   eliminates the 2-4x positional multiplier (§1.2) entirely rather than
   compressing it better. It is also the most direct possible instantiation of
   "slower search, denser index": candidate sets after AND-intersection are
   small at this scale, so the cost is a handful of extra file reads per query,
   paid only on phrase/proximity queries, against a permanent, corpus-wide
   storage saving. Given the explicit tradeoff statement, this should be the
   default, not a fallback — and it's the item most likely to be dismissed
   unexamined by someone carrying Ferret's positions-always assumption forward.
2. **Elias-Fano for docID postings** (§2.5), with SIMD-BP128/PFOR (§2.2) for the
   (now smaller, since positions are gone) frequency stream. EF is near-optimal
   in bits/element and gets you `nextGEQ` for free, which both leapfrog AND
   (§4.1) and Block-Max WAND (§4.4) need — it is the modern-IR piece most likely
   to be skipped by an engineer defaulting to "VByte + PForDelta because that's
   familiar," and unlike technique #1 it costs real implementation complexity
   (the select structure) for a real, quantifiable size return rather than a
   structural one.
3. **Document ID reassignment by path-sort at minimum, recursive graph bisection
   if time allows** (§4.5). Free at query time, shrinks every downstream
   postings list, and unusually for this technique is _more_ effective on a
   filesystem corpus than a web crawl, since directory structure is a free,
   strong topical signal.

The segment/tombstone/tiered-merge design (§1.4, §5.3) and the throttled-merge
NRT model (§5.4) aren't ranked above because they aren't optional the way the
compression choices are — get them right regardless of where the codec decisions
land, since a background indexer that stalls foreground disk I/O or loses an
update on crash undermines everything above it.

Everything else in this chapter (Simple9/16, QMX, Varint-G8IU, Binary
Interpolative Coding) is worth knowing exists and worth deliberately _not_
implementing — they're either superseded (Simple-family), niche (QMX/G8IU, thin
ecosystem support), or wrong for a hot path that needs random access and fast
updates (BIC's slow, non-vectorizable decode belongs on a cold/archival tier at
most).
