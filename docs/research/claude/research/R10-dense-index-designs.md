# R10 — Dense-index designs from the log/observability and columnar world

Scope: systems that deliberately traded query speed for index density — the
opposite corner of the design space from Lucene/Tantivy (covered in R5/R6).
Every number below is labeled by provenance; arithmetic is shown so you can swap
your own assumptions in.

## Comparison table

| System                                                 | Index model                                                                                                                                                           | Index size vs. raw data                                                                                                                                         | Typical query latency                                                                         | Regex over content                                                                                                                                | Licence                                           |
| ------------------------------------------------------ | --------------------------------------------------------------------------------------------------------------------------------------------------------------------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------- | --------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------- | ------------------------------------------------- | ------------- |
| VictoriaLogs                                           | Columnar per-field blocks + per-block-column bloom filter over tokens; no posting lists                                                                               | ~3–10% (1/10–1/30 of raw, i.e. compressed store _is_ the index) `[vendor-claimed]`                                                                              | ms–low-hundreds-ms depending on selectivity and block-skip rate                               | Yes, via `re()` — full block-column scan with regex applied per row after bloom pre-filter (no accelerated regex structure)                       | Apache 2.0                                        |
| Grafana Loki                                           | Labels only indexed (TSDB/boltdb-shipper index of label sets → chunk refs); log body **not indexed** at all; optional per-chunk bloom filters (2024+)                 | ~1–2% (index is just label metadata; chunks are gzip/zstd/snappy blobs) `[vendor-claimed]`                                                                      | seconds (brute-force decompress+grep of matching chunks) unless bloom filters cut chunk count | Yes, LogQL `| ~` — brute-force RE2 over decompressed chunk text | AGPLv3 (Loki) |
| ClickHouse                                             | Sparse primary index (1 mark/8192 rows) + optional secondary skip indexes (`minmax`, `set`, `ngrambf_v1`, `tokenbf_v1`) + new native inverted "text index" (GA ~2026) | Sparse PK index: <<1%. New inverted text index: comparable order to Lucene's (adds a real posting-like structure) `[upstream-documented]`                       | ms (columnar vectorized scan) to sub-ms with skip-index hits                                  | `LIKE`/`match()`/`multiSearchAny` — scanned per-granule after skip index prunes; true regex works but is a granule-level linear scan, not indexed | Apache 2.0                                        |
| Quickwit                                               | Tantivy inverted index + columnar doc-values, packaged per "split," fetched lazily from object storage via a small "hotcache"                                         | Full Tantivy-sized index (10–100%+ of source with positions) — this is _not_ a dense design, included for contrast                                              | tens–hundreds of ms incl. object-store fetch                                                  | Yes, via Tantivy's regex query on a subset of fields (needs the term dictionary)                                                                  | Apache 2.0/AGPL split (core AGPL)                 |
| Splunk                                                 | tsidx: per-bucket lexicon (posting-like) + bloom filter for bucket-skip                                                                                               | Not published precisely; comparable order to a classical inverted index (tsidx duplicates raw data) `[vendor-claimed: "tsidx often ≈ raw data size or larger"]` | ms (warm/hot), seconds+ (cold/bloom-filtered buckets)                                         | Yes, `regex` command scans matched events post-search, not indexed                                                                                | Proprietary                                       |
| Elasticsearch/OpenSearch (`text`)                      | Full Lucene-style inverted index with positions                                                                                                                       | 30–100%+ of source `[vendor/community reported]`                                                                                                                | sub-10 ms                                                                                     | Regex on `wildcard`/`regexp` query types over the term dictionary (indexed but expensive for leading wildcards)                                   | Elastic License / SSPL / Apache (OpenSearch)      |
| Elasticsearch (`match_only_text`, synthetic `_source`) | Positions dropped from index; term-only postings; phrase queries reconstructed from doc_values/synthetic `_source`                                                    | ~10% smaller than full `text` in logging workloads `[vendor-claimed]`                                                                                           | term queries: same speed; phrase queries: slower (reconstruct+verify)                         | N/A (not a regex field type)                                                                                                                      | Elastic License                                   |
| Parquet (+ bloom filter, page stats)                   | Columnar row groups + page-level min/max stats + optional split-block Bloom filter per column chunk                                                                   | Bloom filter overhead only (bits/key you choose); no inverted index at all                                                                                      | scan-dominated: ms–seconds depending on row-group pruning                                     | No native regex; done at the query-engine layer (e.g., DataFusion) after column decode                                                            | Apache 2.0                                        |
| Lance                                                  | Columnar + versioned, optimized for random access and vector search, secondary "scalar" indexes incl. inverted and bitmap                                             | Comparable to Parquet plus optional index structures you opt into                                                                                               | ms (point/random access is the design target, unlike Parquet)                                 | Depends on chosen scalar index; not primarily a text-regex engine                                                                                 | Apache 2.0                                        |

---

## 1. VictoriaLogs — deep dive

### Storage model

VictoriaLogs organizes data as: tenant → daily partitions → immutable "parts"
(the merge-tree unit) → **streams** (a stream = one distinct set of non-message
labels, analogous to a Loki stream) → **blocks** → **columns**. Logs belonging
to the same stream are stored physically adjacent, which is what actually drives
the compression ratio — repeated field values compress far better when clustered
[[VictoriaLogs internals: columnar storage on disk]](https://victoriametrics.com/blog/victorialogs-internals-columnar-storage-on-disk/).

Per part, the file layout is roughly:

- `metaindex.bin` — kept in RAM; summarizes index blocks by stream+time range.
- `index.bin` — per-block headers: stream identity, row count, min/max
  timestamp. No log content here — this lets a query discard whole blocks by
  time/stream before touching any column data.
- `column_names.bin` / `column_idxs.bin` — maps field names to numeric IDs and
  shards them (up to 128 shards per part) so unrelated fields don't collide in
  one file.
- `values.binN` / `message_values.bin` — the actual compressed column blobs, one
  blob per block per column-shard. Encoding is chosen per value type: dictionary
  encoding for repeated strings, delta/bit-packing for timestamps and numbers,
  fixed 4-byte layout for IPv4.
- `bloom.binN` / `message_bloom.bin` — one bloom filter per block per column,
  built over the **tokens** (runs of letters/digits/underscore) found in that
  block's values for that column.
- `columns_header.bin` / `columns_header_index.bin` — per-block, per-column
  metadata: value type, min/max (for numeric/timestamp/IP columns), byte
  offset+size into `values.binN`, and offset+size into the bloom blob. This
  index-of-indexes lets a query jump straight to a column's bytes for a block
  without scanning sibling columns
  [[VictoriaLogs internals]](https://victoriametrics.com/blog/victorialogs-internals-columnar-storage-on-disk/).

Block size target: **~2 MiB uncompressed per block** (grows to ~4 MiB when parts
merge). Rows within a block are sorted by timestamp; blocks are ordered by
stream then time
[[VictoriaLogs internals]](https://victoriametrics.com/blog/victorialogs-internals-columnar-storage-on-disk/).

### The bloom filter, sized

Per block-column, VictoriaLogs builds a bloom filter over the set of **unique
tokens** appearing in that block for that field, costing **~2 bytes per unique
token** `[upstream-documented]`. Worked example from their own blog: a
block-column with 1,000 unique words needs ~2 KB of bloom; 20,000 unique tokens
costs ~40 KB
[[VictoriaLogs internals]](https://victoriametrics.com/blog/victorialogs-internals-columnar-storage-on-disk/).
This is markedly smaller than a classical bloom filter tuned to a fixed FPR —
VictoriaLogs is trading a higher false-positive rate (more "maybe, go check"
answers) for a flat, predictable per-token cost, since the whole point is to
skip _some_ blocks cheaply, not to guarantee minimal false positives.

Contrast: VictoriaMetrics' own comparison states inverted indexes (e.g.
Elasticsearch/Lucene-style) need "at least 8 bytes per token" (postings + term
dictionary entry overhead), vs. VictoriaLogs' 2 bytes/token bloom entry
[[ITNEXT: How do open source solutions for logs work]](https://itnext.io/how-do-open-source-solutions-for-logs-work-elasticsearch-loki-and-victorialogs-9f7097ecbc2f)
`[vendor-claimed]` — this 4x figure is the whole thesis of the architecture in
one number, and it is directly checkable with the posting-list arithmetic in §3.

### What LogsQL execution actually is

It is **not** an inverted index: there is no global term → posting-list
structure at all. A query executes as: (1) prune by tenant/time-partition (free
— directory structure), (2) prune blocks by stream and min/max timestamp using
the in-RAM `metaindex.bin`+`index.bin` (cheap — headers only, no decompression),
(3) for each surviving block, for each filter term, check the block-column's
bloom filter — "definitely not" skips the block's actual values entirely,
"maybe" pays for one decompress-and-scan, (4) only surviving blocks get their
`values.binN` decompressed and linearly scanned/matched
[[VictoriaLogs internals]](https://victoriametrics.com/blog/victorialogs-internals-columnar-storage-on-disk/).
This is a classic _filter-pushdown-with-prefilter_ scan architecture, the same
family as ClickHouse skip indexes and Parquet row-group pruning, not a
Lucene-style term lookup with O(matching-docs) posting traversal. The practical
consequence: a query for a rare term across a huge unfiltered time range still
touches every non-skipped block's bloom filter (O(blocks in range)), where a
real inverted index would jump straight to the term's posting list in O(log
terms). VictoriaLogs wins when blocks are effectively prunable (stream/time
selective, or the bloom filters reject most blocks); it loses on "needle in
haystack, no time/stream narrowing" queries where a real posting list would
dominate.

### Regex support

Yes — LogsQL has a `re()` filter operator. Given the architecture, regex cannot
be accelerated by the token bloom filter (a regex has no fixed token set to
hash), so `re()` filters get **no block-skip benefit** beyond whatever
time/stream pruning applies, then RE2 (or equivalent) runs against every
surviving row of every surviving block. This is documented behavior, not a gap
they hide, but it's the sharpest edge of the design for a workload like Dave's
that leans on regex-over-content.

### Compression and real numbers

- Compression: VictoriaLogs "usually compresses logs by 10x or more"
  `[vendor-claimed]`; a third-party technical breakdown states "compression
  ratios of 15–30x for typical log data... up to 100x and more" for
  low-cardinality/repetitive logs
  [[VictoriaLogs: The Log Database for Terabytes]](https://converter.brightcoding.dev/blog/victorialogs-the-revolutionary-log-database-for-terabytes)
  `[vendor-claimed, restated by 3rd party without independent measurement]`.
- vs. Elasticsearch: "up to 30x less RAM," "up to 15x less disk space"
  `[vendor-claimed]`
  [[VictoriaLogs FAQ]](https://docs.victoriametrics.com/victorialogs/faq/).
- vs. Loki: "performs typical full-text search queries up to 1000x faster than
  Grafana Loki" `[vendor-claimed]` (same FAQ page) — this figure is explainable
  mechanically: Loki does zero indexing of log bodies at all (see §2), so any
  query that isn't fully satisfied by label selection is a full
  decompress-and-grep of every matching chunk; VictoriaLogs' bloom-based
  block-skip avoids decompressing most blocks. The 1000x is plausible as an
  upper bound for a low-selectivity time range with a rare term, not as a
  typical case.
- Third-party (not fully independent — a vendor-adjacent blog, TrueFoundry)
  benchmark: VictoriaLogs "3× higher ingestion... 72% less CPU... 87% less
  memory... 94% [lower] query latencies... 12x faster search" vs Loki
  [[TrueFoundry: VictoriaLogs vs Loki]](https://www.truefoundry.com/blog/victorialogs-vs-loki)
  `[third-party-benchmark, but methodology/independence unverified — treat as directionally consistent with vendor claims, not confirmation]`.
  I could not find a benchmark run by a party with no commercial stake in either
  product (e.g., an academic paper or a neutral SRE blog with disclosed
  methodology and raw data); this is a real gap — see Done-note.

### The honest downside

- No posting lists at all means **rare-term-across-huge-corpus** queries with
  poor time/stream selectivity degrade toward a full scan of block bloom checks,
  unlike a real inverted index's O(matching docs).
- Regex gets none of the bloom-filter's benefit — full row scan on every
  surviving block.
- Block granularity (2–4 MiB) is coarse: a single matching row anywhere in a
  block forces decompression of the whole block, unlike a posting list which
  addresses exact document offsets.
- It is architected for the log shape specifically: append-mostly, time-
  ordered, one dominant "message" column plus a modest number of structured
  fields per stream. It has no positional index at all, so there is no
  correctness path to phrase search beyond string containment inside the matched
  block's raw text — fine for logs (where "phrase" ≈ substring of one line),
  much weaker for prose/code where meaningful phrase queries span tokenization
  decisions a bloom-over-raw-tokens doesn't preserve order for.

---

## 2. The rest of the log-search lineage

### Grafana Loki

Loki's bet, stated plainly by its own docs and reiterated by third parties: **do
not index the log body at all.** The index (originally BoltDB-shipper, now
TSDB-based per Loki 2.8+) maps label-set → chunk references only; chunk bodies
are gzip/zstd/snappy-compressed blobs of raw log lines with no term index
whatsoever
[[Loki components]](https://grafana.com/docs/loki/latest/get-started/components/).
A `|~ "regex"` filter is executed by decompressing and brute-force scanning
every chunk that survived label selection. Index footprint is therefore "index
is just label metadata," commonly cited at roughly 1–2% of log volume, vs.
Elasticsearch's inverted index which the same sources put in the tens-of-
percent range — Loki's own positioning is "storage 10x–100x cheaper than
Elasticsearch for the same volume"
[[Atatus: Beginner's Guide for Grafana Loki]](https://www.atatus.com/blog/a-beginners-guide-for-grafana-loki/)
`[vendor/community-claimed]`.

The bloom-filter accelerator ("bloom-compactor" → later renamed
`bloom_build`/`bloom_gateway`) was added specifically to blunt the brute-force
cost for filter expressions: a background job builds bloom filters over tokens
in chunks, and a gateway component uses them to skip chunks before the query
path decompresses them
[[Loki configuration parameters]](https://grafana.com/docs/loki/latest/configure/).
As of the docs reviewed here these components are still marked **experimental**;
this is functionally VictoriaLogs' bloom-per-block idea retrofitted onto an
architecture that didn't have it at launch — evidence that the "small
skip-structure over otherwise unindexed blobs" pattern is converging across this
whole lineage, not just a VictoriaLogs idiosyncrasy.

### ClickHouse

Two separate mechanisms, easy to conflate:

**1. Sparse primary index.** ClickHouse's MergeTree stores rows physically
sorted by the `ORDER BY` key and keeps **one index entry ("mark") per granule**,
not per row — default granule = **8192 rows**
[[ClickHouse docs: sparse primary indexes]](https://clickhouse.com/docs/guides/best-practices/sparse-primary-indexes).
This is the single cheapest form of "index" here: for N rows you store N/8192
mark entries, each just the ORDER BY key value at that granule's start — the
index is smaller than a B-tree over every row by a fixed 8192x factor by
construction, at the cost of always scanning up to one full granule (8192 rows)
per matched range even for a point lookup.

**2. Data-skipping secondary indexes**, opt-in on top of that: `minmax` (min/
max per granule, catches range predicates), `set` (small enumerated value sets
per granule), `ngrambf_v1` (bloom filter of n-grams — substring/LIKE
acceleration), `tokenbf_v1` (bloom filter of whitespace/punctuation-delimited
tokens — accelerates `hasToken()`, `LIKE`, `IN`). Each of these indexes lets the
query skip whole granules that provably cannot match, without ever building a
term→row posting list
[[Altinity: Skipping Indices]](https://altinity.com/blog/clickhouse-black-magic-skipping-indices),
[[ClickHouse docs: skipping index examples]](https://clickhouse.com/docs/optimize/skipping-indexes/examples).

**3. Native inverted/full-text index — status as of 2026.** This one moved fast
and matters for your comparison table. It started experimental (2022–2023), was
found wanting, and was **re-implemented from scratch starting Feb 2025**,
reaching **beta Dec 2025** and **General Availability around March 2026**
[[ClickHouse: Full-text search GA release]](https://clickhouse.com/blog/full-text-search-ga-release).
It's a real token-based inverted index (not a bloom filter), reported to deliver
**10–50x better query latency than the old bloom-filter-based
`tokenbf_v1`/`ngrambf_v1` approach, and 7–10x faster than before for cold
queries** `[vendor-claimed]`
[[ClickHouse: Inside ClickHouse full-text search]](https://clickhouse.com/blog/clickhouse-full-text-search).
Explicitly **not** a relevance engine — no BM25/TF-IDF scoring, purpose-built
for filter+aggregate over billions of rows, not ranked retrieval
[[ClickHouse: Inside ClickHouse full-text search]](https://clickhouse.com/blog/clickhouse-full-text-search).
This is a live signal that "just enough inverted index, no ranking" is the
direction this whole product family is converging on when bloom filters alone
stop being good enough — worth noting since it undercuts a pure "columnar +
bloom is always sufficient" framing.

### Quickwit

Genuinely different lineage from the rest of this section — it is Tantivy (a
real Lucene-class inverted index with positions) underneath, so it is **not** a
dense-index design in the VictoriaLogs sense. What it contributes to this slice
is the _packaging_ trick: each immutable "split" bundles the full inverted
index, columnar doc-values, and row storage together, but ships a tiny
**hotcache** (metadata blueprint, reported at **under 0.1% of split size, ~10
MB** for a typical split) that is fetched first and lets a searcher open a split
from S3 in **under 60 ms** by fetching only the byte ranges it actually needs,
rather than downloading the whole split
[[Quickwit 101]](https://quickwit.io/blog/quickwit-101),
[[Quickwit: Data directory]](https://quickwit.io/docs/operating/data-directory).
The lesson transferable to Super Ferret even without object storage: a tiny,
always-resident "which byte ranges do I need" summary in front of a heavy
on-disk structure is a cheap win regardless of whether the heavy structure
itself is dense or sparse.

### Splunk

Publicly documented (not open source) as: each index "bucket" holds a compressed
rawdata directory plus **tsidx** files (lexicon + posting-like value arrays —
this is much closer to a classical inverted index than VictoriaLogs' design)
plus a **bloom filter file** built when a bucket rolls to warm. The bloom
filter's job is purely to let the search head skip whole buckets (not blocks
within a bucket) without touching their tsidx files at all
[[Splunk: Reduce tsidx disk usage]](https://docs.splunk.com/Documentation/Splunk/9.3.2/Indexer/Reducetsidxdiskusage),
[[Splexicon: Bloomfilter]](https://docs.splunk.com/Splexicon:Bloomfilter). So
Splunk's architecture is "real inverted index per bucket, bloom filter only at
the coarse bucket-skip layer" — a hybrid, and notably tsidx files are commonly
_as large as or larger than_ the raw data they index, which is the opposite of
VictoriaLogs' philosophy despite both using bloom filters. Regex (`| regex`)
executes against already-matched events post-search, not against the index.

### Elasticsearch/OpenSearch — for contrast, and the interesting middle ground

The reason a Lucene-style `text` field is large: it stores, per token per
document, position and (unless disabled) frequency/norm data to support phrase
queries and relevance scoring — 30–100%+ of source size is a widely repeated
community/vendor figure for indexes with positions and stored `_source`. Three
levers pull it toward the dense end without abandoning the inverted-index model
entirely, and they're directly relevant to Dave's "denser index, willing to give
up some speed" preference:

- **`index: false` / `doc_values` off** — simply don't build a term index for a
  field you'll never full-text search, keep only what's needed for
  filtering/sorting/aggregation (doc_values are a columnar structure, not an
  inverted index) — free space if the field doesn't need text search
  [[doc_values docs]](https://www.elastic.co/docs/reference/elasticsearch/mapping-reference/doc-values).
- **`match_only_text`** — same tokenization and term dictionary as `text`, but
  **drops positions and norms** (`index_options: docs`). Term queries are as
  fast as full `text`; phrase queries get slower because the position
  information isn't there — Elasticsearch falls back to loading `_source` (or
  synthetic source) for the candidate documents and verifying the phrase by
  re-scanning the actual text
  [[match_only_text docs]](https://www.elastic.co/docs/reference/elasticsearch/mapping-reference/match-only-text),
  [[Opster: match_only_text]](https://opster.com/guides/elasticsearch/data-architecture/elasticsearch-match-only-text-field-type/).
  Reported saving: **up to 10% disk on logging datasets**
  [[Elastic blog: Save 10% disk space with match_only_text]](https://www.elastic.co/blog/save-10-percent-disk-space-on-your-logging-datasets-with-match-only-text)
  `[vendor-claimed]`. This is exactly the "denser index, slower query" tradeoff
  point Dave has already staked out a preference on — it's a smaller win than
  VictoriaLogs' approach (10% vs. an order of magnitude) because it only removes
  positions, keeping the full term-document postings.
- **Synthetic `_source`** — don't store the original document at all;
  reconstruct it on demand from doc_values (the columnar store). Always slower
  to reconstruct than reading a stored blob, but removes a full copy of the
  source text from disk
  [[_source field docs]](https://www.elastic.co/docs/reference/elasticsearch/mapping-reference/mapping-source-field).

### Parquet + Arrow, and Lance

Parquet's index-equivalent structures, from coarse to fine: **row groups**
(large horizontal partitions, prune by file-level stats), **column chunks**
within a row group, **pages** within a chunk each carrying min/max statistics
for range pruning, and optionally a **split-block Bloom filter** per column
chunk — 256-bit blocks of eight 32-bit words, the only Bloom filter
representation the spec currently defines
[[Parquet: Bloom Filter]](https://parquet.apache.org/docs/file-format/bloomfilter/).
The bloom filter exists specifically for the case dictionary encoding can't
help: high-cardinality columns where min/max is too wide to prune anything
[[DataFusion blog: user-defined Parquet indexes]](https://datafusion.apache.org/blog/2025/07/14/user-defined-parquet-indexes/).
DuckDB added bloom-filter-aware row-group skipping on read in 2025
[[DuckDB: Parquet Bloom Filters]](https://duckdb.org/2025/03/07/parquet-bloom-filters-in-duckdb/).
There is **no term index at all** in vanilla Parquet — any text search is a
column-decode-then-scan, exactly the VictoriaLogs philosophy but without even
the bloom filter unless the writer opted in. Regex is entirely a query-
engine-layer concern (e.g. DataFusion's `regexp_match` over decoded UTF-8
columns), not accelerated by anything in the file format.

**Lance** targets the workload Parquet is bad at — random point access and
vector search on top of columnar storage — via a different physical layout and a
family of optional secondary "scalar" indexes (including inverted and bitmap
indexes you opt into per column) rather than Parquet's append/scan-optimized row
groups. It's the modern answer to "I want columnar compression but also need
fast random reads," which is closer to a desktop search workload's access
pattern than Parquet's log/analytics-scan assumption — worth a mention as a
candidate on-disk container format, not as a text-search engine.

---

## 3. Extracted techniques — the reusable part

### 3.1 Sparse/coarse indexes (index every Nth row, then scan the block)

Mechanism: store one index entry per block of N rows/bytes instead of per row.
Cost: index size = (row count / N) × entry size, independent of what's inside
the block. Query cost: binary-search the sparse index to find candidate blocks,
then linear-scan up to N rows/bytes per matched block.

- ClickHouse default N = 8192 rows/granule
  [[ClickHouse docs]](https://clickhouse.com/docs/guides/best-practices/sparse-primary-indexes).
- VictoriaLogs' analogous unit is a ~2 MiB block, sized by bytes not rows.

Choosing N: the sparse index only helps when a query's predicate correlates with
the sort order (time, path, whatever you sorted by). N trades index size (∝ 1/N)
against wasted-scan-per-hit (∝ N rows scanned per matching block, even for a
single matching row). For Dave's corpus, an analogous knob would be "one entry
per N files" or "one entry per compressed content block" sorted by, e.g., path
or mtime — cheap, but only as good as the correlation between sort order and
query predicate. It's a poor substitute for a term index when queries aren't
naturally range-shaped (which is most full-text queries), and a near-free win
when they are (path-prefix, time-range, size-range).

### 3.2 Block-level bloom/xor/binary-fuse filters, and the arithmetic Dave asked for

**Bit costs at given FPRs** (standard results, not specific to any one system):

- Classical Bloom filter: bits/element ≈ −log₂(p) / ln(2) ≈ 1.44·log₂(1/p). At
  p=1%: ~9.6 bits/element. At p=0.1%: ~14.4 bits/element.
- Xor8: ~10 bits/key, FPR ≈ 0.39% `[upstream-documented]`
  [[FastFilter: xorfilter README]](https://github.com/FastFilter/xorfilter/blob/master/README.md).
- Binary Fuse8: ~9 bits/key, FPR ≈ 0.39%; Binary Fuse16: ~18 bits/key, FPR ≈
  0.0015% `[paper-reported]`
  [[Binary Fuse Filters paper]](https://arxiv.org/pdf/2201.01174.pdf). Binary
  fuse filters are also gzip/zstd-compressible after construction, unlike
  classical Bloom filters, which is a real edge for an on-disk format you're
  already compressing with zstd anyway.
- VictoriaLogs' own filter is unusual: ~2 bytes = **16 bits per unique token**,
  but that's not tuned to a target FPR the way the above are — it's a fixed
  budget accepted as "good enough to skip most blocks," and per their own
  framing they accept a higher FPR than a textbook Bloom filter at that bit
  budget would, in exchange for simplicity
  `[upstream-documented, interpretation mine]`.

**Bloom filter vs. compressed posting list — the actual crossover, worked out.**
This is the central question of this slice, so here's the arithmetic with
assumptions stated so you can substitute your own numbers.

Setup: a corpus of `D` documents (or blocks), a term `t` appearing in `n` of
them. Compare two structures whose job is "given `t`, produce the set of matches
(or block candidates)":

- **Posting list**: store the `n` doc-IDs directly. With delta encoding + a
  variable-byte or PForDelta-style codec, a well-clustered/token-frequent
  posting list costs roughly **1–2 bytes per posting** in practice for sorted
  integer deltas at typical gap sizes (this is the standard Lucene/
  Tantivy-class regime — see R6 for the codec-level numbers; I'm using their
  ballpark here as the comparison baseline, `[estimated-arithmetic]`, since
  exact bytes/posting depend heavily on document ID assignment order and gap
  distribution).
- **Bloom-style block filter**: a filter over `B` blocks (`B = D/blockrows`),
  each needing ~1–2 bytes to encode "this term is/isn't in this block" at a
  workable FPR (VictoriaLogs' empirical 2 bytes/unique-token-per-block, or ~9-18
  bits/key for a tuned xor/fuse filter) — but critically, this cost is paid
  **once per block the term appears as a distinct token in**, not once per
  occurrence, and it does not distinguish _which rows_ in the block matched —
  the block still has to be scanned.

The crossover, stated as a ratio: a bloom-style block filter is a
**membership-per-block** structure costing `O(B) × const` where B = number of
blocks the corpus is divided into, essentially **independent of how often the
term occurs inside a block** (a term appearing once in a block costs the same
bloom bits as a term appearing 1000 times in that block). A posting list costs
`O(n) × bytes/posting`, strictly linear in occurrence count. Therefore:

- **Low-frequency, well-distributed terms** (the term appears in many
  _different_ blocks, each just once or a few times) — bloom filters win hugely,
  because you pay per-block, not per-occurrence, and a posting list would need
  one entry per block-scattered hit anyway (n ≈ number of blocks hit, so the two
  converge, but the bloom filter avoids ever writing exact positions).
- **High-frequency terms concentrated in few blocks** (e.g. a common source-
  code identifier appearing 500 times in one file/block) — the posting list for
  that block is one entry (or a handful, with in-block positions), while the
  bloom filter costs the same flat per-block bit budget it always does. Bloom
  filters are indifferent to intra-block frequency, which is exactly why they're
  cheap, but it also means once you're inside a bloom-flagged block you get
  _zero_ help finding the actual matching rows/positions — you linear-scan the
  whole block regardless of whether the term hits it once or a thousand times.
- **Concretely, at VictoriaLogs' own numbers**: 2 bytes/unique-token-per- block,
  ~2 MiB blocks. A block holding, say, 20,000 unique tokens (their own example)
  costs 40 KB of bloom regardless of total token _occurrences_, which could
  easily be 200,000+ in a 2 MiB text block. A classical positional inverted
  index over that same block, storing every occurrence with position, would cost
  meaningfully more than 40 KB the moment average term frequency exceeds roughly
  1 (i.e. almost always) — this is the mechanical reason VictoriaLogs' index is
  an order of magnitude smaller: it throws away _all_ intra-block addressing,
  not just positions.
- **The number that actually matters for Super Ferret**: this trade is a clean
  win when your unit of "found it" is coarse (a whole log line, a whole block)
  and a bad one when you need to _locate_ the match cheaply (phrase search,
  syntax highlighting a hit, code navigation to the exact line). A desktop
  file-search tool answering "which files contain X" can tolerate
  block-then-scan; a tool also expected to jump to line N inside a 50k-line file
  cannot, without paying for at least an in-block offset table — which is a much
  smaller structure than full positions but isn't free either.

### 3.3 Columnar layout + general compression vs. specialized posting codecs

"Just zstd the column" wins when: (a) the column's byte layout naturally
clusters similar values (sorted/dictionary-friendly, e.g. all timestamps or all
paths together), (b) you don't need random access to a single value (zstd frames
typically must be decompressed from a frame boundary, though block-level framing
with zstd dictionaries or independent seekable frames per row-group avoids
this), and (c) the win is dominated by removing _cross-row_ redundancy (repeated
field values, common substrings across rows) rather than by exploiting a known
integer distribution the way delta/FOR/RLE do.

Specialized codecs still win for: monotonic or near-monotonic integer sequences
(doc IDs, offsets, timestamps) — delta+variable-byte or FOR (frame-of-reference)
beats general compression because it encodes the _model_ (small deltas) rather
than relying on the compressor rediscovering it, and it supports O(1) or O(log
n) random access to the Nth element, which general-purpose compression usually
does not.

zstd with a **trained dictionary** narrows the gap for smaller values/short
strings (paths, field names, common tokens) by amortizing the dictionary cost
across the whole file rather than embedding it once per block — this is directly
why VictoriaLogs bothers with per-column dictionary encoding _and_ zstd on top
rather than picking one.

### 3.4 Zone maps / min-max, and time-partitioning as a free index

A zone map (ClickHouse's `minmax` index, Parquet's page/row-group stats) is
essentially the sparse index's metadata taken to its logical minimum: for a
block, store just (min, max) of a column. Cost: two values per block,
independent of block contents. Only useful for range predicates and requires the
data to be sorted or at least locally clustered by that column to be selective —
an unsorted min-max index degenerates to "min=global-min, max=global-max" and
prunes nothing. Time-partitioning (VictoriaLogs' daily partitions, ClickHouse's
typical `PARTITION BY toDate(...)`) is the special case where the partition
boundary itself _is_ the zone map, and it's free because directory/file naming
does the pruning before any index is even opened. For Dave's corpus this maps to
mtime-based partitioning (recent files vs. archival) — free pruning for
"modified in the last week" queries, no value for content queries.

### 3.5 Scan-first architectures — how fast can you scan, and what that buys

Decompression throughput sets the ceiling on "how much can I afford to
under-index and just scan instead." Representative numbers, single-core:

- LZ4 decompression: commonly cited **~4–5 GB/s** on the Silesia corpus (v1.9.0)
  `[third-party-benchmark]`.
- zstd level 1 decompression: **~1.3–1.4 GB/s**, and — the operationally
  important property — **zstd decompression speed is roughly independent of the
  compression level used to produce the frame**, unlike compression speed
  [[TrueNAS community zstd benchmarks discussion; general zstd documentation consensus]](https://www.truenas.com/community/threads/zstd-speed-ratio-benchmarks.89429/)
  `[third-party-benchmark]`. This is why VictoriaLogs and ClickHouse can both
  compress aggressively at write time without paying for it at query time.

Arithmetic: at ~1.3 GB/s single-core zstd decompression, scanning a fully
decompressed 10 GB "hot set" of source code (a plausible upper bound for the
actively-relevant slice of a 200 GB desktop corpus) costs **~7.7 seconds
single-threaded**, trivially parallelizable across cores for a multi-second-
to-sub-second wall time on a modern desktop. This is the mechanical
justification for "scan-first": if your corpus's _selective_ working set after
cheap block/zone-map pruning is in the single-digit-GB range, brute- force
decompress+scan is entirely competitive with maintaining a term index, and
categorically simpler.

### 3.6 Tiered/hot-cold designs

The general pattern across this whole lineage (VictoriaLogs' in-memory recent
parts vs. merged historical parts, Splunk's hot/warm/cold buckets, ClickHouse's
default MergeTree background merges consolidating into fewer larger parts over
time) is: **richer, larger structures over recent/hot data; denser, cheaper
structures over old/cold data**, because access patterns are skewed toward
recency in logs. This is _not_ Dave's workload — a desktop file corpus has no
equivalent recency skew (a five-year-old PDF is queried as often, in
expectation, as yesterday's), so this specific technique transfers weakly; see
the synthesis below.

---

## 4. Synthesis — which philosophy fits desktop file search?

**(a) Lucene/Tantivy**: positional inverted index, 30–100%+ of source with
positions, sub-10ms queries, index rebuild/merge cost scales with churn.

**(b) VictoriaLogs/Loki/ClickHouse**: near-zero-to-small skip index over
compressed columnar/blob storage, 100ms–seconds queries, index a few percent of
data, near-zero merge cost because you're mostly just compressing new data, not
rebuilding term structures.

The key variable the log-search lineage optimizes for that a desktop corpus does
**not** share: **append-mostly, recency-skewed, throwaway-after-
retention-window data, ingested continuously at high volume, queried relatively
rarely per byte stored.** Every design decision here — daily partitions,
hot/cold tiering, "don't bother indexing what's rarely re-queried," accept
100ms-plus latency because operators are running ad hoc investigations not
sub-10ms autocomplete — follows from that shape. A desktop corpus is the mirror
image: **largely static, read-mostly, queried interactively and repeatedly
against the same working set** (you re-search your own codebase dozens of times
a day), and the total corpus (10-200GB) is two to four orders of magnitude
smaller than what these systems are built for (VictoriaLogs/ClickHouse target
TB-PB scale).

That mismatch cuts in a specific direction, and it's worth being blunt about it
rather than reflexively endorsing the system Dave admires:

- The log-lineage's central saving — _not_ building per-occurrence structure
  because you'll likely never query most of the data before it ages out — has
  **no analog** on a desktop. Files don't age out; a search over "all my files"
  is the common case, not the exception, and it recurs. That undermines the
  strongest argument for the bloom-over-blocks approach.
- The log-lineage's central cost — coarse, block-granularity results with no
  intra-block addressing — is a **worse fit** for code/text search than for
  logs, because a log line is already the natural unit of a "hit," while a
  50,000-line source file is not: you want line/offset-level results, which
  pushes back toward wanting _some_ per-occurrence structure (even if
  block-relative, per §3.2's last point) rather than none.
- Regex-over-content, an explicit hard requirement here, is precisely the query
  type **none** of these bloom-based systems accelerate — VictoriaLogs' `re()`,
  Loki's `|~`, ClickHouse's raw regex `match()` all fall through to linear scan
  once past coarse pruning. Given Dave has stated true regex as a requirement,
  not a nice-to-have, that alone argues against relying on a bloom-only skip
  layer as the _primary_ mechanism — it needs a real substring/n-gram index
  (R7's territory) for the regex path regardless.
- Where the log-lineage's ideas transfer cleanly and cheaply, independent of
  corpus shape: (1) **columnar per-field storage + zstd with a trained
  dictionary** for metadata fields (path, extension, mtime, size, owning
  git-repo) — pure win, no downside, this is just good columnar hygiene; (2)
  **zone maps/time-partitioning** for mtime-range and path-prefix queries —
  free, orthogonal to the content-search question entirely; (3) a **coarse
  block-level bloom filter as a first-pass prefilter in front of a real
  (smaller-than-Lucene-but-real) per-block posting or n-gram structure**, i.e.
  use the bloom filter to skip whole files/blocks cheaply before paying for the
  more expensive exact structure on the survivors, rather than using it as a
  _substitute_ for that structure. This is actually closer to what Splunk does
  (bloom at the bucket layer, real tsidx underneath) than to VictoriaLogs'
  "bloom is most of the story" design.

**Where a desktop workload actually sits, argued with numbers**: at ~10–200GB
and read-mostly with repeated re-querying, the corpus fits comfortably in page
cache on any workstation with 16-32GB+ RAM for the _hot_ portion (recent project
files), which is exactly the regime where §3.5's scan-first arithmetic applies
without even needing disk I/O — decompressing and re-scanning a working set of a
few GB costs low single-digit seconds single-threaded, sub-second
multi-threaded. That argues a **hybrid**: a real, but deliberately
non-positional-by-default, structure (n-gram/trigram index for regex — see R7 —
plus a term-level structure without positions, closer to `match_only_text`'s
tradeoff than to full Lucene) sized to be genuinely small, backed by columnar
zstd storage for everything else, with phrase queries reconstructed by
re-scanning the (cheap-to-decompress) matched region rather than stored as
positions. That is a middle point between (a) and (b), and it matches Dave's own
stated preference — "slightly slower search... for a more efficient, denser
index" — without inheriting the specific worst fit of the log-lineage designs
(no regex acceleration, no intra-file addressing, and a bet on data aging out
that doesn't hold for personal files).

**What would change this recommendation**: if regex-over-content turns out to be
rare in practice relative to plain-term/boolean queries (measure it — instrument
actual query logs once Super Ferret has users), the case for a
VictoriaLogs-style bloom-dominant design gets much stronger, since the weakest
part of that design (unaccelerated regex) stops mattering. Conversely if the
corpus grows toward the high end (500GB+, many machines' worth of data
federated) or moves toward genuinely append-only/archival content (backups, mail
archives), the log-lineage's core assumptions start applying for real and its
techniques should move from "borrow ideas" to "adopt the architecture."

---

## Done-note

**Could not verify independently**: every VictoriaLogs-vs-Elasticsearch/Loki
performance number in this document traces back to VictoriaMetrics itself or to
a vendor-adjacent blog (TrueFoundry, a partner/consulting shop) restating those
figures; I found no benchmark run by a party with no commercial relationship to
either product, disclosed raw data, and a repeatable methodology. Treat the
"15-30x disk," "30x RAM," and especially the "1000x faster than Loki" figures as
directionally plausible (they follow mechanically from the architecture — Loki
genuinely does zero indexing of log bodies) but unconfirmed in magnitude. The "8
bytes per token" figure attributed to Elasticsearch-style inverted indexes, used
as VictoriaMetrics' own justification for the 4x-smaller-bloom claim, is also
vendor-stated without a cited derivation — R5/R6 should be checked for whether
Tantivy's actual per-posting bytes at typical term frequencies supports or
contradicts that number.

**Contradiction found**: VictoriaMetrics' FAQ and blog materials describe
VictoriaLogs as architecturally _not_ an inverted index and imply this is
categorically different from (and superior to) Elasticsearch/Loki — but
ClickHouse's 2025-2026 trajectory (dropping its own bloom-filter-only
`tokenbf_v1`/`ngrambf_v1` approach in favor of a real native inverted index for
a **10-50x** query-latency win) is direct evidence from a comparable system that
"bloom filters over columnar blocks" has a real ceiling once query latency (not
just index size) is prioritized, and that the observability/columnar world is
not converging uniformly toward VictoriaLogs' end of the spectrum — it's
bifurcating, with ClickHouse moving back toward Lucene-adjacent territory for
exactly the query patterns bloom filters serve worst.

**The one technique from this slice most worth building into Super Ferret**: not
VictoriaLogs' bloom-filter-as-primary-index (wrong fit, as argued in §4 — no
regex acceleration, no intra-file addressing, and its core savings assumption
doesn't hold for static personal data), but **Splunk's layering pattern**: a
cheap, flat-cost bloom filter at the coarse (file/block) granularity purely to
skip whole files before touching anything more expensive, sitting in front of a
real (but non-positional, or sparsely-positional) per-file structure for actual
matching — combined with VictoriaLogs' specific columnar-dictionary-then-zstd
encoding for all the metadata columns (path, size, mtime, extension, mime-type)
where there is no argument at all for anything heavier. That combination gets
the bulk of the density win with none of the regex/addressing regressions the
pure log-search designs carry.

Sources consulted (full list, deduplicated):
[VictoriaLogs internals: columnar storage on disk](https://victoriametrics.com/blog/victorialogs-internals-columnar-storage-on-disk/),
[VictoriaLogs FAQ](https://docs.victoriametrics.com/victorialogs/faq/),
[ITNEXT: how open source log solutions work](https://itnext.io/how-do-open-source-solutions-for-logs-work-elasticsearch-loki-and-victorialogs-9f7097ecbc2f),
[VictoriaLogs: The Log Database for Terabytes](https://converter.brightcoding.dev/blog/victorialogs-the-revolutionary-log-database-for-terabytes),
[TrueFoundry: VictoriaLogs vs Loki](https://www.truefoundry.com/blog/victorialogs-vs-loki),
[Loki components](https://grafana.com/docs/loki/latest/get-started/components/),
[Loki configuration parameters](https://grafana.com/docs/loki/latest/configure/),
[Atatus: Beginner's Guide for Grafana Loki](https://www.atatus.com/blog/a-beginners-guide-for-grafana-loki/),
[ClickHouse: sparse primary indexes](https://clickhouse.com/docs/guides/best-practices/sparse-primary-indexes),
[Altinity: Skipping Indices](https://altinity.com/blog/clickhouse-black-magic-skipping-indices),
[ClickHouse: skipping index examples](https://clickhouse.com/docs/optimize/skipping-indexes/examples),
[ClickHouse: full-text search GA release](https://clickhouse.com/blog/full-text-search-ga-release),
[ClickHouse: Inside ClickHouse full-text search](https://clickhouse.com/blog/clickhouse-full-text-search),
[Quickwit 101](https://quickwit.io/blog/quickwit-101),
[Quickwit: Data directory](https://quickwit.io/docs/operating/data-directory),
[Splunk: Reduce tsidx disk usage](https://docs.splunk.com/Documentation/Splunk/9.3.2/Indexer/Reducetsidxdiskusage),
[Splexicon: Bloomfilter](https://docs.splunk.com/Splexicon:Bloomfilter),
[Elasticsearch: match_only_text](https://www.elastic.co/docs/reference/elasticsearch/mapping-reference/match-only-text),
[Elastic blog: Save 10% disk space with match_only_text](https://www.elastic.co/blog/save-10-percent-disk-space-on-your-logging-datasets-with-match-only-text),
[Elasticsearch: doc_values](https://www.elastic.co/docs/reference/elasticsearch/mapping-reference/doc-values),
[Elasticsearch: \_source field](https://www.elastic.co/docs/reference/elasticsearch/mapping-reference/mapping-source-field),
[Opster: match_only_text](https://opster.com/guides/elasticsearch/data-architecture/elasticsearch-match-only-text-field-type/),
[Parquet: Bloom Filter spec](https://parquet.apache.org/docs/file-format/bloomfilter/),
[DataFusion blog: user-defined Parquet indexes](https://datafusion.apache.org/blog/2025/07/14/user-defined-parquet-indexes/),
[DuckDB: Parquet Bloom Filters](https://duckdb.org/2025/03/07/parquet-bloom-filters-in-duckdb/),
[FastFilter: xorfilter README](https://github.com/FastFilter/xorfilter/blob/master/README.md),
[Binary Fuse Filters paper (arXiv)](https://arxiv.org/pdf/2201.01174.pdf).
