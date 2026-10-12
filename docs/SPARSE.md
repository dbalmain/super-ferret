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

## Volume interpretation and initial decision brief

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

**Initial recommendation (before latency):** advance **word four plus five** to a posting-read and
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

## Volume validation and run notes

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

## Latency

The rare-literal volume gain becomes execution-time gain. Against shipped
trigrams, word 4+5 cuts rare **p90 total time 87.0× cold and
99.1× warm**, versus the earlier 68.6× conservative candidate-byte
p90 reduction. Word-four buys 12.2× / 9.9× cold/warm p90 reductions
with the smaller index. Longer keys add whole-index storage; their selected
posting reads do not erase the verification savings. Rare posting bytes
actually fall from 24/44 KiB to 16/24 KiB median/p90: longer keys do not imply
longer selected lists. Identifier p90 posting bytes rise from 84 to 112 KiB,
but the posting phase remains small beside verification.

Same D65C catalog, paired control and frozen corpus as the volume experiment:
112,382 live documents, 112,376 readable, 3,978,812,718 readable bytes, and 250
regexes (50 per class). The completed trigram baseline was retained. Both word
segments reproduced the volume experiment's key counts, posting bytes and total
encoded bytes exactly. [Raw aggregates](measurements/sparse-latency.txt) contain
**120 class/cache/band rows**, byte counts and nanosecond sums; no corpus text or
individual document paths.

Cold uses **`posix_fadvise(DONTNEED)`**, since global cache dropping is unavailable
for uid 1000. Before each cold query, evict the index and every surviving readable
snapshot file, outside timing, then immediately repeat warm. This is advisory
file-page eviction; catalog, compiled regex and segment metadata remain resident.
There is one cold/warm pair per query. The baseline predates disk cleanup and the
word runs follow it; these are serial measurements, not confidence intervals.

Each layout uses the production codec-1 segment writer, positional reader,
checked postings and cursor intersection. Shipped selection comes from the public
query explanation; word selection uses the volume experiment's four-child rule
and full dictionary frequencies. Regex compilation and frequency ranking are
prepared outside timing. Posting time includes selected dictionary lookup,
checksum-aligned list reads and intersection. Warm rereads and decodes the same
encoded lists rather than retaining decoded postings. Verification includes
opening, reading, matching and freeing each surviving readable document. Total
is the measured full execution of both phases plus their bookkeeping; it excludes
preparation. Quantiles are computed separately, so phase quantiles need not sum
to the total quantile.

All entries below are **median/p90**, times in milliseconds. Posting bytes are
whole verified chunks actually requested; index bytes also include dictionary
reads. These are logical read bytes, including repeated chunks, not device
sectors. Inline singletons consume dictionary bytes but zero posting bytes.

| Class | Screen | Cache | Posting ms | Verify ms | Total ms | Posting KiB | Index KiB |
| --- | --- | --- | ---: | ---: | ---: | ---: | ---: |
| rare literal | trigrams | cold | 0.869/1.505 | 2951.365/3663.539 | 2952.403/3664.757 | 24/44 | 44/68 |
| rare literal | trigrams | warm | 0.265/0.388 | 244.430/296.936 | 244.707/297.236 | 24/44 | 44/68 |
| rare literal | word 4 | cold | 0.331/0.848 | 113.099/299.154 | 113.599/299.366 | 20/36 | 40/76 |
| rare literal | word 4 | warm | 0.128/0.176 | 13.349/29.925 | 13.465/30.035 | 20/36 | 40/76 |
| rare literal | word 4+5 | cold | 0.222/0.473 | 9.207/41.935 | 9.852/42.126 | 16/24 | 36/68 |
| rare literal | word 4+5 | warm | 0.060/0.119 | 0.468/2.914 | 0.563/2.998 | 16/24 | 36/68 |
| identifier pattern | trigrams | cold | 3.651/6.930 | 6644.977/8615.793 | 6649.651/8621.766 | 68/84 | 88/112 |
| identifier pattern | trigrams | warm | 1.377/2.999 | 500.925/625.998 | 502.260/629.330 | 68/84 | 88/112 |
| identifier pattern | word 4 | cold | 1.670/6.208 | 4616.435/7209.410 | 4617.481/7215.559 | 72/120 | 96/160 |
| identifier pattern | word 4 | warm | 0.891/2.613 | 396.916/582.023 | 397.641/585.408 | 72/120 | 96/160 |
| identifier pattern | word 4+5 | cold | 1.671/5.960 | 4542.545/7142.478 | 4544.362/7149.640 | 72/112 | 92/160 |
| identifier pattern | word 4+5 | warm | 0.915/2.789 | 400.972/597.515 | 401.742/599.912 | 72/112 | 92/160 |
| alternation | trigrams | cold | 10.064/15.601 | 8980.500/10639.372 | 8992.090/10650.757 | 180/252 | 232/324 |
| alternation | trigrams | warm | 3.961/6.263 | 541.844/654.405 | 544.304/659.544 | 180/252 | 232/324 |
| alternation | word 4 | cold | 6.092/8.146 | 5884.819/8553.479 | 5890.445/8564.776 | 136/164 | 200/240 |
| alternation | word 4 | warm | 2.045/3.744 | 351.002/447.597 | 352.853/452.062 | 136/164 | 200/240 |
| alternation | word 4+5 | cold | 5.298/8.393 | 5482.179/8651.491 | 5484.930/8659.648 | 112/140 | 176/208 |
| alternation | word 4+5 | warm | 1.739/3.431 | 299.711/406.701 | 301.555/409.051 | 112/140 | 176/208 |
| broad | trigrams | cold | 5.107/6.209 | 11210.190/11634.707 | 11212.788/11640.375 | 20/28 | 24/40 |
| broad | trigrams | warm | 2.154/3.323 | 837.953/1061.850 | 840.381/1063.889 | 20/28 | 24/40 |
| broad | word 4 | cold | 5.109/6.225 | 10985.289/11455.422 | 10990.092/11460.823 | 20/28 | 24/44 |
| broad | word 4 | warm | 2.082/2.929 | 883.305/1049.081 | 886.025/1051.912 | 20/28 | 24/44 |
| broad | word 4+5 | cold | 5.193/5.824 | 11158.808/11602.682 | 11164.002/11608.059 | 20/28 | 24/44 |
| broad | word 4+5 | warm | 2.059/2.786 | 878.344/1026.936 | 880.458/1028.993 | 20/28 | 24/44 |
| code class | trigrams | cold | 2.744/6.242 | 11659.059/13894.782 | 11664.175/13902.867 | 60/76 | 76/104 |
| code class | trigrams | warm | 1.243/2.817 | 5393.505/6669.222 | 5393.946/6670.787 | 60/76 | 76/104 |
| code class | word 4 | cold | 1.458/4.926 | 6200.807/9822.370 | 6202.369/9826.578 | 44/84 | 64/124 |
| code class | word 4 | warm | 0.708/1.760 | 3064.723/4117.411 | 3065.319/4117.943 | 44/84 | 64/124 |
| code class | word 4+5 | cold | 1.201/4.602 | 5637.659/8917.440 | 5638.349/8922.044 | 40/88 | 60/144 |
| code class | word 4+5 | warm | 0.541/1.407 | 2550.537/3531.906 | 2551.578/3532.550 | 40/88 | 60/144 |

The same size bands are in the raw report: below 1 MiB, 1–16 MiB and at least
16 MiB. Verification is attributed from each full query's per-document work.
Each band repeats the shared posting phase and adds its own verification, so
band totals and posting bytes are not additive. No surviving readable candidates
occur in the largest band. The six missing snapshot documents remain conservative
candidates and contribute candidate sizes but no verified bytes or oracle matches.
For rare literals, the 1–16 MiB band's verification median/p90 is
1887.229/2266.023 ms cold / 165.045/197.124 ms warm with trigrams,
111.510/275.145 / 13.022/27.659 with word-four, and
6.179/39.858 / 0.368/2.714 with word 4+5.

Question: which longer-key screen should advance after measuring actual posting
reads and surviving verification for D17's rare-literal problem?

Options, with costs and buys (whole-index and build CPU from the volume run):

- **Keep shipped trigrams:** 273 MiB / 22.31 CPU seconds, no extra index cost.
  Rare total p90 is 3664.757 ms cold / 297.236 ms warm.
  Verification dominates; this leaves the measured false-candidate problem.
- **Word four:** 657 MiB / 336.81 CPU seconds. Rare total p90 falls to
  299.366 / 30.035 ms, with improvements for identifier,
  alternation and code classes. It is the lower-storage comparison and preserves
  all raw-trigram fallback coverage.
- **Word four plus five:** 1,533 MiB / 884.15 CPU seconds, **876 MiB more** than
  word-four. Rare total p90 falls again to 42.126 / 2.998 ms.
  Its main extra buy is the rare-literal tail; the broader classes gain less.
  Across the deliberately balanced 250-query set, mean total time is
  8575.8/1539.2 ms cold/warm for trigrams,
  5862.4/937.2 for word-four, and
  5747.0/834.6 for word 4+5. These means are not workload weights.

**Recommendation:** advance **word 4+5 to a controlled prototype that runs
complete rare-literal queries**, with word-four retained as the option for
a smaller storage budget.
This measurement supports the rare-screening direction: the additional posting
reads do not consume the gain. Include frequency planning, resident metadata
and a representative query mix in that prototype before choosing a product
index policy. Broad candidate volumes remain identical and their timing
variation is a control for run noise; longer grams cannot improve facts that
remain only three bytes. Keep regex-engine work behind the remaining screening
measurements. The product is unchanged in this slice.

**Fact that would change it:** a representative query log showing word-four
already meets the latency target and few queries benefit from five-byte facts;
then its 876 MiB saving should decide the policy. Planning costs that consume
the extra rare-literal gain would likewise favour word-four. One developer
corpus and prepared execution phases do not establish an unconditional default.

Validation: all **250 queries × three layouts × two temperatures = 1,500**
completed-run checks include every readable oracle match and return exactly the
oracle's matched DocIds. File-backed candidates also equal the real volume
planner's identities; aggregate candidate/read/oracle bytes equal the original
volume rows. Cold/warm read counts and size-band accounting pass. Formatting,
workspace clippy with warnings denied and full workspace tests pass:
**945 passed / 13 ignored**, zero Rust warnings (four latency regressions added).

The initial word-four run stopped at the 5 GB disk floor after 150 query pairs;
its owned file was cleaned up and unemitted timings discarded. After Dave freed
storage, only the two missing word layouts were rerun, preserving the completed
baseline. Preflight checked all three benchmark process names; one benchmark
ran at a time with hangup protection and a foreground `while kill -0` wait.
The resumed job finished successfully; both owned intermediate segments were
removed. Disk headroom was 69 GiB at resumption and 73 GiB at completion. No
other files were deleted. No Cargo manifests or lockfiles changed, and no
pushes or agent CLI launches occurred.

Reproduce all layouts after the same host preflight, or pass the optional
comma-separated labels to run only the missing layouts:

```sh
target/release/ferret-bench sparse-latency /tmp/d65c/catalog /tmp/d65c/control /tmp/d65c/corpus /tmp/sparse-latency
```
