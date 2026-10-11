# Sparse grams: regex candidate experiment

Complete: all nine experimental layouts and the shipped baseline pass the oracle.
Bench-only indexes; no product index or query changes. No network sources used.

## Question and design

Does a larger gram index screen enough false regex candidates to justify its
storage and build cost? Compare shipped trigrams (four rarest children per AND)
with hash and inverse-frequency sparse grams at byte-length caps 8/32. Include
raw fixed 4/8 and word-only fixed 4/4+5 controls, all retaining trigrams.

For a substring of at least three bytes, both endpoint bigrams must strictly
outrank every interior bigram. Endpoint ranks may equal one another. Every
trigram qualifies: it has no interior bigram. Thus this experiment augments the
trigram index; it does not establish that GitHub's replacement policy was this
exact definition. The historical claim was not checked offline.

Fold ASCII raw bytes **before** weighting and extraction, using
`ferret_text::trigram_key`'s rule. Hash ranks use fixed SplitMix64 of the folded
pair. Frequency ranks use inverse corpus **occurrence** counts. Both break ties
by folded pair number; repeated identical pairs stay equal. Absolute offsets
cannot break ties without breaking substring soundness. Frequency tables are
frozen and shared by index/query; retraining requires rebuilding the index.

The monotonic stack emits every qualifying gram in linear time, with at most
two emissions per bigram. Length caps filter emissions. Equal stack ranks are
replaced after visiting their pair. A bounded builder selects 32 contiguous
first-byte ranges, scanning forward from selected starts until the first right
rank at least as large as the left rank blocks all subsequent intervals. Its
worst case is `cap` comparisons per selected start. Exhaustive tests compare
both extractors with the definition.

Fixed controls emit all trigrams plus every window of their fixed length.
Word controls emit all trigrams plus 4-byte, or 4- and 5-byte, windows whose
bytes form valid UTF-8 consisting of alphanumeric characters or underscore.
This is a local selection predicate, without a whole-word boundary requirement;
keys remain folded raw bytes. Punctuation and partial UTF-8 windows fall back to
trigrams on both index and query sides.

## Necessary facts and soundness

`ferret-verify` exposes `GrepFlags::required_literals`, sharing the existing
Cox HIR fold through a second fact consumer. Product trigram caps stay unchanged.
The experiment retains exact strings up to 32 bytes and concatenation edges up
to 31; existing class/set caps still weaken facts. It preserves AND/OR, Unicode
case alternatives, raw-byte classes, optionality and repetitions. It introduces
no second regex parser. The result is necessary, never sufficient.

1. A matching document satisfies at least one alternative of the literal-fact
   tree. Unicode case equivalents stay separate HIR alternatives; only ASCII
   bytes are folded by the index.
2. Each required key is extracted wholly inside one such literal. Its predicate
   inspects only that key's bytes and the frozen weights. Any containing document
   therefore emits the same key, independently of text outside the literal.
3. Indexing deduplicates keys per document. Query AND selects four rarest children
   by actual document frequency, weakening a necessary conjunction. OR unions
   every branch, including an unconstrained branch. No oracle match can be lost.

Deleting an indexed trigram merely because a longer gram covers it is unsound:
a query for the shorter literal must still find documents where outside bytes
change the longer gram. Query-side omission is sound when an included longer
key implies it. The experiment retains all trigrams rather than assuming such
index savings.

Tests drive the real document extractor, shared derivation and candidate
planner: a selective gram crossing a literal edge, folded ASCII and Unicode
case alternatives, disjoint alternation, unconstrained branches, classes,
repetitions, invalid UTF-8, exhaustive/generated matches, caps 8/32/33 and
byte-identical whole versus partitioned segment encoding. A discriminating
synthetic case is `aaaaa`: identical pair ties permit only `aaa` for every
sparse weight/cap, while fixed four/five keys distinguish shorter documents.
Rare endpoints do not guarantee long or selective grams.

## Corpus and method

Use D65C's refreshed `/tmp/d65c/catalog`, codec-1 fully merged paired index at
`/tmp/d65c/control`, and frozen `/tmp/d65c/corpus`. `Snapshot::open` checks
incarnation and DocId bound. The checked-out S3 document describes an isolated
catalog but records no concrete catalog/snapshot paths; the supplied fresh D65C
paths supersede it. D65C construction is recorded on `wt/d65c:docs/D65C.md`.

Population: **112,382 live documents**, **112,376 readable**, **6 unreadable**,
**3,978,812,718 readable bytes**. All layouts conservatively admit the six
unreadable documents, adding **4,830,452 candidate bytes** per query. The oracle
covers every frozen readable document with the actual `regex` verifier.

Use 50 queries each from the five `screen::regex_classes` classes, with the
same deterministic snapshot-derived identifier helper as `regex-volume`.
The existing `regex-volume` command itself measures only rare literals and
identifiers; this experiment also measures alternation, broad and code classes.
Shipped candidates come from the real `GrepAtom` planner. The longer-facts
trigram control separates derivation effects from additional indexed keys.
Experimental ANDs use four rarest children with deterministic tie ordering.
The changed fact representation can change ties/budgets, so this control is
not assumed identical to shipped planning.

Encode whole posting lists with the real segment writer: relative DocIds,
PFor blocks, front-coded dictionary, inline singletons and checksums. One writer
spans all partitions in global lexical order, making whole-index byte counts
exact; only maps are partitioned. The sink avoids scratch index files.
`postings_bytes` excludes singletons inlined in the dictionary; total includes
them. `singleton_bytes` counts their complete dictionary entries, not just IDs.

Build CPU includes extraction, deduplication, sorting and encoding. Product
trigrams use their optimized inverter; prototype maps are deliberately reported
separately. Trigrams, hash-8 and word-4 use whole maps; other rows include 32
corpus passes for bounded maps. These are complete prototype build costs, not
isolated weight-function timings or a production builder projection. Frequency
training adds **3.93 CPU s / 4.09 wall s**, once per frozen corpus. The first
oracle pass costs **506.34 CPU s / 513.06 wall s**. One build per completed
variant; the median/p90 describe 50 query populations, not repeated latencies.

Candidate bytes count entire candidate files. Oracle bytes count entire files
with a match, not matched spans. Median uses M5's lower middle and p90 nearest
rank. False share is `(sum readable candidate bytes - sum oracle bytes) /
sum readable candidate bytes`, not a ratio of independent medians.

## Results

MiB = 2^20 bytes. Candidate columns are **median/p90 MiB**. Index cells are
**whole index / postings MiB**. Full candidate-document and byte median/p90,
oracle-document and byte median/p90, sums, wall times, singleton/dictionary
bytes and file-size bands are in [`measurements/sparse.txt`](measurements/sparse.txt).
Only aggregate counts, bytes and timings are tracked.

| Variant | Index / postings MiB | Distinct keys | Build CPU s | Rare literal MiB med/p90 | Identifier MiB med/p90 | Alternation MiB med/p90 | Broad MiB med/p90 | Code MiB med/p90 | Rare false share |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| shipped four | 272.86 / 217.77 | 10158272 | 22.310 | 1469.29/1774.99 | 2654.28/2954.09 | 2959.49/3193.48 | 3405.53/3558.59 | 2537.48/2945.89 | 99.52% |
| longer-facts trigrams | 272.86 / 217.77 | 10158272 | 162.900 | 1469.29/1774.99 | 2654.28/2954.09 | 2959.49/3195.53 | 3405.53/3558.59 | 2537.48/2945.89 | 99.52% |
| sparse-hash-8 | 1587.85 / 842.06 | 116535553 | 634.510 | 27.17/1327.09 | 2549.43/2914.99 | 2735.66/3095.89 | 3405.53/3558.59 | 1778.24/2849.80 | 97.62% |
| sparse-hash-32 | 3550.01 / 1167.84 | 268757928 | 1154.940 | 27.17/1327.09 | 2549.43/2914.99 | 2735.66/3095.89 | 3405.53/3558.59 | 1778.24/2849.80 | 97.62% |
| sparse-frequency-8 | 1527.20 / 794.63 | 114965290 | 726.480 | 300.14/1464.51 | 2319.50/2867.31 | 2637.91/3096.23 | 3405.53/3558.59 | 1723.12/2739.59 | 98.24% |
| sparse-frequency-32 | 3615.68 / 1150.22 | 278334955 | 1149.070 | 300.14/1464.51 | 2319.50/2867.31 | 2637.91/3096.23 | 3405.53/3558.59 | 1723.12/2739.59 | 98.24% |
| fixed-4 | 1103.88 / 853.59 | 42441594 | 591.330 | 69.14/164.90 | 1933.45/2607.16 | 2275.54/2830.22 | 3405.53/3558.59 | 1431.50/2242.83 | 92.67% |
| fixed-8 | 4939.70 / 1446.62 | 448754825 | 1400.400 | 1441.54/1568.56 | 2654.28/2954.09 | 2915.27/3175.76 | 3405.53/3558.59 | 2320.21/2910.63 | 99.30% |
| word-fixed-4 | 656.92 / 588.11 | 12225432 | 336.810 | 69.14/164.90 | 1933.45/2607.16 | 2275.54/2830.22 | 3405.53/3558.59 | 1431.50/2242.83 | 92.67% |
| word-fixed-4+5 | 1532.78 / 1164.73 | 68638052 | 884.150 | 8.86/25.86 | 1933.45/2607.16 | 2096.02/2815.61 | 3405.53/3558.59 | 1302.38/2242.83 | 12.90% |

Oracle document median/p90 and byte median/p90 are common to every layout:

- Rare literal: **2/14 docs**, **4,033,515/22,288,165 B**.
- Identifier: **64/376 docs**, **36,785,558/238,940,235 B**.
- Alternation: **10,221/22,526 docs**, **2,154,123,266/2,903,629,998 B**.
- Broad: **943/1,290 docs**, **1,087,134,189/1,240,328,547 B**.
- Code class: **694/3,779 docs**, **474,794,928/1,524,143,713 B**.

All-query bands are recomputed separately for <1 MiB, 1–16 MiB and >=16 MiB.
The last band is empty on this policy-filtered corpus. Band medians are not
additive. The aggregate artifact retains both smaller bands for every class
and layout.

## Interpretation and decision brief

Question: which screen should follow D17's instruction to measure candidate
screening before changing the regex engine?

Options, with costs and buys:

- **Shipped trigrams:** 273 MiB and optimized 22.31 CPU s build; rare false
  readable share 99.52%, median 1,469 MiB and p90 1,775 MiB. No additional
  storage, but most verification reads remain false candidates.
- **Full sparse augmentation:** hash-8 costs 1,588 MiB / 634.51 CPU s and buys
  a 54× rare median reduction, but p90 remains 1,327 MiB. Frequency-8 costs
  1,527 MiB / 726.48 CPU s; it is worse for rare literals (300 MiB median),
  modestly better for identifier/code classes. Cap 32 more than doubles either
  cap-8 index without improving any class's headline median/p90. The four-child
  budget can change chosen constraints, so larger caps need not monotonically
  reduce every individual candidate set.
- **Raw fixed additions:** fixed-4 costs 1,104 MiB / 591.33 CPU s, with a much
  better rare p90 (165 MiB) than sparse grams. Fixed-8 costs 4,940 MiB / 1,400.40
  CPU s yet rare median/p90 remain 1,442/1,569 MiB. It requires 8-byte literal
  facts and supplies no 4–7-byte coverage; rare seeds allow 5–30-byte words.
- **Word four:** 657 MiB / 336.81 CPU s, preserving all 20 reported fixed-4
  class/band aggregate rows exactly at 40.5% less storage. Rare median/p90 are
  69/165 MiB. It is the smaller-storage screen; retaining every raw trigram
  preserves queries across punctuation and partial UTF-8 boundaries.
- **Word four plus five:** 1,533 MiB / 884.15 CPU s, with rare median/p90
  **8.86/25.86 MiB** and rare false readable share **12.90%**. Its whole index
  is 3.5% smaller than hash-8 and 0.4% larger than frequency-8, while screening
  more candidate bytes in every class that benefits from longer facts. It
  costs 876 MiB more than word-four, improves rare p90 by 6.4×, and improves
  alternation/code medians. Identifier headline volumes remain equal to
  word-four. The CPU comparison includes its bounded builder's extra passes.

**Recommendation:** advance **word four plus five** to a posting-read and
verification-latency experiment; use word-four as the smaller-storage comparison.
Keep the product unchanged until that measurement supports an index policy.
Do not advance full sparse augmentation, cap 32, or raw fixed-8 on this evidence.
Word 4+5 cuts rare p90 candidate volume 68.6× against shipped trigrams, at
5.62× their whole-index storage. Hash ranks do not provide reliable selective
coverage of short literals; frequency ranks choose rare endpoints, not rare
whole literals. The original all-trigrams guess was right, while the expectation
that rare stretches necessarily yield useful long grams was not.

Broad-class candidate bytes are identical across all layouts: their usable
literal facts are only three bytes, so longer-key screening cannot help those
queries. Class/set caps and Unicode alternatives can also fragment literals
and limit other screens. This experiment measures the shared necessary-fact
derivation, not every possible extraction policy. Residual identifier and broad
volume remains large; the volumes do not justify changing regex engines.

**Fact that would change it:** a representative workload where sparse grams
beat word 4+5 after including posting reads and surviving verification, with
acceptable whole-index/build cost; or a storage budget that cannot afford the
additional 876 MiB over word-four. Candidate volumes alone do not establish
latency, and one refreshed developer corpus does not establish a general
production policy.

## Validation and run notes

Pinned formatting, full workspace clippy with warnings denied, and full
workspace tests pass: **941 passed / 13 ignored**, **zero Rust warnings**
(baseline 928 passed; 13 added). An existing batch test hit a transient catalog
lock failure on the first resumption run; the complete unchanged retry passed.
All **250 regexes × 10 screens = 2,500 distinct query/screen checks** include
all oracle matches; all required boundary, folding and alternation regressions
pass. The three shipped baseline replays have byte-identical aggregate rows.
The protected remaining job finished successfully and its foreground wait
returned exit status 0. Complete aggregates contain 10 index rows and 200
class/band rows. No Cargo manifest/lockfile edits, dependencies, pushes or agent
CLI launches.

An unbounded hash-32 attempt was stopped at heavy swapping; its incomplete
measurement is discarded. Bounded builds preserve exact encoded bytes. A usage
cutoff killed fixed-8 at range 13/32; only missing layouts were restarted with
hangup protection and a foreground `while kill -0` wait. Host preflight checks
`ferret-bench`, `term-dict-bench` and `pair-fp-bench`; only one benchmark runs.
Free disk was 11 GB initially, 7.7 GiB at resumption, and 7.0 GiB at completion.
Corpus passes check free disk every 1,024 documents and stop below 5 GB. All
index encoding goes to a sink.

Build `nix develop --command cargo build --release -p ferret-bench`, then run:

```sh
target/release/ferret-bench sparse /tmp/d65c/catalog /tmp/d65c/control /tmp/d65c/corpus
```

An optional comma-separated label list selects only named layouts, e.g.
`fixed-8,word-fixed-4,word-fixed-4+5`. Unknown labels fail before corpus access.

## Latency (follow-up in progress)

Question: does the candidate-volume gain survive real posting reads and regex
verification? Measure shipped four-child trigrams, word-four and word 4+5 on
the same D65C snapshot and 250 regexes. This remains measurement only.

Use complete codec-1 segments, one intermediate layout at a time, deleting
only files this experiment creates. Reuse the real segment reader and posting
cursors, including checksum-aligned reads and singleton inlining. Shipped
selection comes from the public query explanation; experimental selection must
match the existing volume experiment. Dictionary ranking and regex compilation
are prepared outside timing; timed posting reads include selected dictionary
lookups, checksums and cursor intersection.

Global cache dropping is unavailable for uid 1000. Cold runs therefore use
`posix_fadvise(DONTNEED)` on the index and every surviving readable snapshot
file, outside timing; warm runs immediately repeat the same query. Catalog,
compiled regex and segment resident metadata stay resident. Report logical
posting bytes actually requested, including checksum chunk amplification,
separately from total index read bytes. Advisory eviction is not a cold boot.

Verification reads each surviving frozen document and applies the real regex.
Record full-query posting, verification and total elapsed time. Attribute
per-document verification work to the existing size bands; band totals include
the shared posting phase plus that band's measured verification work, so they
are not additive. Empty bands report zero verification and the shared posting
cost. Verify candidate inclusion and exact matched DocIds against the oracle
for both temperatures, with missing snapshot documents admitted conservatively.

The file-backed harness and four regressions are implemented. Formatting,
workspace clippy with warnings denied, and workspace tests pass: **945 passed /
13 ignored**, zero Rust warnings. The release rebuild passed and the protected
foreground run is active. Oracle preparation matched the previous population
(112,382 live, 112,376 readable, 3,978,812,718 readable bytes, 250 queries),
taking 506.480 CPU seconds / 514.659 wall seconds. All 250 shipped cold/warm query pairs passed candidate and oracle identity
checks. Their 40 complete class/cache/band rows are in
[the latency aggregates](measurements/sparse-latency.txt). Rare total median/p90
is 2,952.403/3,664.757 ms cold and 244.707/297.236 ms warm; posting read median/p90
is 24,576/45,056 bytes. Word-four is building next. The final recommendation
awaits both longer-key layouts. Initial
disk headroom was about 14 GiB, about 11 GiB after rebuilding test artifacts;
stop below 5 GB. No Cargo manifests or lockfiles changed.
