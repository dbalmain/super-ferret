# Suffix array and FM-index as regex screens

Measured 2026-10-13 on branch `wt/fm-index` (cut from `wt/screens` at 75d3ad0).
This is an experiment, not a product change. The code is the standalone crate
`experiments/fm-index` (`fm-screen`), kept on that branch only (pushed, not
merged; 91c6059), and the aggregates are in
[`measurements/fm-index/`](measurements/fm-index/). The decision is D76's
addendum in [DECISIONS.md](DECISIONS.md).

## Question

If index size were no limit, which structure gives the fastest regexes? The
hypothesis was:

1. A suffix array (SA) is the fastest structure for any regex with a selective
   required literal. It finds a literal of any length by binary search, with
   exact positions and no false candidates.
2. Exact positions let verification read only the **lines** around hits.
3. It does nothing for broad patterns.

An FM-index (BWT, rank and a sampled SA) trades locate speed for size. It was
measured beside the plain SA at three sampling rates.

## Verdict

- **The SA is the fastest screen measured for alternation and code-class
  regexes**: 2.7× and 3.9× faster than threaded trigrams warm, and 2.3× and 3×
  cold. Its alternation candidates are exactly the oracle's documents.
- **For rare literals it is not the fastest when cold.** Word 4+5 takes 9.9 ms
  cold against the SA's 58 ms. A binary search over a 1 GB segment takes about 30
  steps, each faulting a random page of the SA and one of the text, and the
  query pays that in each of four segments. Warm, the SA is 0.82 ms against word 4+5's 0.56 ms, both far
  under a 168 ms warm scan.
- **"No false candidates" holds for the literal, not the regex.** Identifier
  candidates are 2,023 MB median against 37 MB of oracle documents, because the
  identifier's literals occur far more often than the identifier does. Warm, the
  SA is _slower_ than threaded trigrams on this class (114 against 93 ms):
  combining 2 million located positions per query costs 51 ms.
- **"Any length, including 1–2 bytes" is moot.** `required_literals` reduces a
  literal shorter than three bytes to `All` before any screen sees it. None of
  the 250 queries reached the SA with a 1–2 byte literal.
- **Line verification is sound for all 250 queries.** It pays where hits are
  sparse in big files: code 3× warm, and every class cold (1.3–2.2×). It does
  not pay on broad, where documents are dense with hits.
- **Broad gets nothing**, as predicted: 3,566 MB of candidates, the same as
  trigrams.
- **The FM-index is viable only for rare literals and counts.** Locating costs
  3.6 µs per occurrence at sampling rate 32, 8.4 µs at 64 and 17.9 µs at 128.
  The median identifier query has 2.05 million occurrences, so locating every
  literal would take 7.5–37 s per query. Counting is cheap: 0.2–1.1 ms warm per
  query.

## Population and protocol

- **Population:** the screens population (SCREENS.md § Population). That is the
  D65C frozen corpus: 112,376 readable documents holding 3,978,812,718 bytes,
  and 250 regexes in five classes of 50, compiled case-insensitive.
  - The oracle is the screens harness's cached case-insensitive oracle.
  - The harness itself could not run, because `/tmp/d65c/screens/t1` (the paired
    index it checks for) no longer exists. The population was therefore loaded
    directly from the oracle cache and the corpus directory. The query order and
    class split were checked against the screens result rows.
- **Text:** the corpus concatenated in document order, ASCII-case-folded, with a
  `\n` separator after each document. It is split into four segments of whole
  documents: 1.072, 1.073, 1.073 and 0.760 GB. A segment of about 1 GB matches
  how the product merges, and keeps u32 suffix values. A document-start table
  maps positions to documents. A hit that crosses a document's end is dropped.
- **Query path:** take `GrepFlags::required_literals` (an AND/OR tree).
  1. Count each distinct literal (SA: binary search; FM: backward search).
  2. Locate every occurrence.
  3. Map occurrences to (document, offset) and combine per the tree.
     - An AND intersects documents. Its anchors are those of its part with the
       fewest hits, restricted to the surviving documents. Every match lies on
       one line and holds every part's literal, so that part's hit lines include
       every match line.
     - An OR is a union of documents and anchors.
  - Lookup runs segment by segment, one thread, and the reported time is the sum
    over the four segments. A product would run segments in parallel, which
    would cut the cold numbers most.
- **Line soundness:** a query may be verified line by line only if no match can
  span a newline. A walk of the regex's HIR refuses a query if any class,
  literal or `\A`/`\z` anchor can reach `\n`. The oracle and the product's
  `grep:` both report a document as matching if the regex matches anywhere in
  it. Under line mode, a document is a match if any hit line matches. That is
  equivalent exactly when no match can contain `\n`.
- **Verification:** 16 rayon threads, the same count as the threaded trigrams
  cited below.
  - Line mode reads a growing window around each anchor until it holds the whole
    line, then runs the regex over that line. The first match ends the document.
  - A document of 64 KiB or less is read whole, and only its hit lines are
    matched.
  - A document with hits × 256 ≥ its size is verified whole ("dense"): the lines
    would cover most of it anyway.
  - Whole mode runs the regex over every candidate document.
- **Cold and warm:**
  - Cold lookup: index files evicted with `posix_fadvise(DONTNEED)` and mapped
    `MADV_RANDOM`.
  - Cold verification: every candidate document evicted.
  - Warm: an immediate repeat.
  - Lookup and verification were timed separately and summed per query.
  - Before every step the driver logged `pgrep -x cargo`. It was 0 throughout,
    so no run is contaminated.
- **FM residency:** the FM-index's rare-literal and count-only numbers are
  demand-paged, like the SA's.
  - The non-rare classes were measured **resident**: index files pre-read, which
    costs about 0.8 s per GB, cold.
  - A demand-paged FM locate over the alternation class ran for 9 minutes with 5
    s of CPU, fault-bound, before it was stopped.
  - Locate cost per occurrence was measured resident on segment 0 over the
    alternation class (3.9 million occurrences). The FM's totals for the other
    classes are **estimates**: measured count time, plus occurrences × measured
    locate cost, plus the SA's measured combine and verification of the same
    candidates.

## Index size and build

The SA uses libsais 0.2 (OpenMP, 16 threads) per segment. The FM-index is built
from that SA and its text by this crate.

- The BWT is held in a Huffman-shaped wavelet tree. Each bitvector has a u32
  rank entry per 512 bits.
- Marks are a plain bitvector over text positions, plus the sampled SA values at
  that rate.
- The wavelet tree runs at 5.30–5.54 bits per symbol, its H0.

| Index            |     Bytes | Of corpus | Of trigrams |
| ---------------- | --------: | --------: | ----------: |
| Trigrams (cited) |    286 MB |     0.07× |          1× |
| Word 4+5 (cited) |  1,607 MB |     0.40× |        5.6× |
| FM s=128         |  3,523 MB |     0.89× |       12.3× |
| FM s=64          |  3,647 MB |     0.92× |       12.7× |
| FM s=32          |  3,896 MB |     0.98× |       13.6× |
| SA + folded text | 19,894 MB |      5.0× |       69.5× |

- **FM breakdown:**
  - wavelet tree 2,870 MB;
  - mark bitvector 528 MB;
  - samples 497, 249 or 124 MB at s=32, 64 and 128.
- An Elias-Fano mark set would save about 420–490 MB. That is estimated, not
  built.
- The SA needs its text to binary-search against, hence 4 bytes of SA plus 1
  byte of folded text per corpus byte.

| Build, whole corpus         |   Wall |   CPU | Peak RSS |
| --------------------------- | -----: | ----: | -------: |
| SA (libsais, 16 threads)    | 83.7 s | 847 s |  5.26 GB |
| FM from the SA (one thread) |  158 s | 140 s |  6.29 GB |
| FM including its SA         |  242 s | 987 s |  6.29 GB |
| Word 4+5 (cited)            |      — | 884 s |        — |

Per segment of about 1 GB, the SA takes 14–30 s wall and 160–230 CPU s.

**Disk while measuring.** At most one segment's plain SA and text existed at a
time, about 5.4 GB, for that segment's 2–5 minute lookup window. Each was then
deleted. The FM files for all three rates (about 4 GB) were kept until
verification finished, and were then deleted.

## Lookup

Median / p90 ms, summed over the four segments; cold / warm.

| Class        | Occurrences (median) |    SA cold |       SA warm | SA combine, warm |
| ------------ | -------------------: | ---------: | ------------: | ---------------: |
| rare literal |                    3 | 46.1 / 208 | 0.506 / 0.842 |            0.009 |
| identifier   |            2,051,286 |  234 / 759 |    53.4 / 144 |             51.0 |
| alternation  |              112,259 |  202 / 283 |   8.36 / 33.1 |             6.34 |
| broad        |            3,608,541 |  204 / 485 |    87.9 / 154 |             85.7 |
| code         |            1,438,488 |  189 / 351 |    33.1 / 120 |             31.6 |

For the SA, locating is reading a contiguous run of suffix values (0.3 ns per
occurrence warm). Mapping millions of positions to documents and sorting them
dominates warm lookup on the broad, identifier and code classes.

**FM-index, rare literals.** These are measured, demand-paged cold.

| Rate  |       Cold |         Warm | Locate share of warm |
| ----- | ---------: | -----------: | -------------------: |
| s=32  | 65.5 / 206 | 0.705 / 1.92 |                  54% |
| s=64  | 89.8 / 378 | 0.952 / 2.53 |                  68% |
| s=128 |  143 / 577 |  1.52 / 5.33 |                  78% |

**FM-index, count only.** These are measured; median ms, cold / warm. They are
the same at every rate within noise, since counting does not touch the samples.

| Class        | s=32 cold | s=32 warm |
| ------------ | --------: | --------: |
| rare literal |      32.4 |     0.400 |
| identifier   |      54.8 |     0.620 |
| alternation  |       110 |      1.17 |
| broad        |      16.9 |     0.260 |
| code         |      86.8 |     0.831 |

**FM-index, locate.** Measured resident, warm: 3,617 ns per occurrence at s=32,
8,430 ns at s=64 and 17,881 ns at s=128. That is about 15.5, 31.5 and 63.5 LF
steps per occurrence, each a wavelet-tree rank of five or six levels that misses
cache.

Estimated warm lookup if every literal is located (median ms):

| Class       |   s=32 |   s=64 |  s=128 |
| ----------- | -----: | -----: | -----: |
| identifier  |  7,478 | 17,350 | 36,738 |
| alternation |    413 |    953 |  2,014 |
| broad       | 13,137 | 30,505 | 64,610 |
| code        |  5,243 | 12,166 | 25,761 |

The FM can count every part and locate only each AND's rarest part. The rarest
part's lines already hold every match line, so this is sound, though it admits
more candidate documents. That plan locates 141,016 occurrences for the median
identifier query and 30,396 for code (measured counts). The estimated warm
lookups are then 564 ms for identifier and 137 ms for code at s=32. Alternation
and broad are unchanged: alternation is an OR, and every broad query has a
single literal.

## Verification

16 threads, SA candidates, median / p90. The FM's candidates are the SA's where
both were run: asserted per query at lookup time.

| Class        | Candidate docs | Candidate MB | Oracle MB | Line: regex MB | Line cold | Line warm | Whole cold | Whole warm |
| ------------ | -------------: | -----------: | --------: | -------------: | --------: | --------: | ---------: | ---------: |
| rare literal |              2 |         4.06 |      4.06 |           1.29 |      4.12 |     0.236 |       4.82 |      0.254 |
| identifier   |          4,613 |        2,023 |        37 |            825 |     1,018 |      64.8 |      1,288 |       60.6 |
| alternation  |          8,020 |        2,074 |     2,074 |            468 |       657 |      41.6 |      1,457 |       57.6 |
| broad        |         57,627 |        3,566 |     1,087 |          2,233 |     2,729 |       134 |      2,740 |        105 |
| code         |          2,008 |        1,186 |       377 |            272 |       369 |      43.2 |        710 |        131 |

- In line mode, broad reads 7,149 MB to regex 2,233 MB: windows overlap and
  regrow on long lines. That is why whole mode is faster warm there.
- A retune (256 B first window; read whole when hits × 2048 ≥ size) gave 38.1 ms
  warm for alternation and 122 ms for broad. Its rare-literal cold run rose from
  4 to 27 ms on identical device bytes, so that run is treated as noise. The v1
  numbers are primary. Both are in `measurements/fm-index/report.txt`.

**Fallbacks.** No query fell back to whole-document verification for soundness:
all 250 are line-sound. Per document, the density rule sent these to whole
verification (summed over all 50 queries; whole / line):

| Class        |   Whole |      Line |
| ------------ | ------: | --------: |
| rare literal |       0 |       187 |
| identifier   |  11,908 |   317,905 |
| alternation  |  44,012 |   545,065 |
| broad        | 576,839 | 2,424,432 |
| code         |  10,342 |   229,723 |

**Positive control.** No oracle document was missed in any run. Matched
documents equal the oracle's on 250 / 250 queries, in line mode and whole mode,
in both verification runs.

## Totals against the comparators

Median ms, cold / warm. SA = SA lookup + 16-thread line verification. The
comparator rows are cited, not re-run:

- `rg`, trigrams (1 thread) and word 4+5 (1-thread verification) come from
  `measurements/screens/` and SPARSE.md.
- Threaded trigrams come from `wt/par-verify`'s
  `measurements/par-verify/product.txt`: warm at 16 threads. Cold has no
  16-thread row; the 8- and 32-thread rows bracket it.

| Class        | rg (32 thr) | Trigrams, 1 thr |  Trigrams, threaded |      Word 4+5 |          SA |         FM s=32 |
| ------------ | ----------: | --------------: | ------------------: | ------------: | ----------: | --------------: |
| rare literal | 5,182 / 168 |     2,966 / 249 |      900–930 / 49.5 |    9.9 / 0.56 | 57.9 / 0.82 |     70.5 / 1.16 |
| identifier   | 6,135 / 172 |     6,356 / 501 |  1,738–1,845 / 92.8 |   4,544 / 402 | 1,447 / 114 |  est. — / 620\* |
| alternation  | 5,226 / 179 |     8,272 / 542 | 2,061–2,356 / 142.9 |   5,485 / 302 |  878 / 53.3 |    est. — / 438 |
| broad        | 5,208 / 172 |    10,915 / 837 | 2,778–3,532 / 301.7 |  11,164 / 880 | 2,949 / 225 | est. — / 13,276 |
| code         | 5,142 / 169 |  11,137 / 5,316 | 1,690–2,195 / 359.2 | 5,638 / 2,552 |  568 / 92.4 |  est. — / 185\* |

\* The rarest-part plan, as a lower bound: it verifies the full plan's
candidates, and the rarest plan admits more.

**SA against threaded trigrams:**

| Class        | Warm        | Cold            |
| ------------ | ----------- | --------------- |
| rare literal | 60× faster  | 15–16× faster   |
| identifier   | 1.2× slower | 1.2–1.3× faster |
| alternation  | 2.7× faster | 2.3–2.7× faster |
| broad        | 1.3× faster | 0.94–1.2×       |
| code         | 3.9× faster | 3.0–3.9× faster |

**Against word 4+5:** word 4+5 is 6× faster cold and 1.5× faster warm on rare
literals, at 8% of the SA's bytes. On every other class the SA is 3–28× faster
than word 4+5, but word 4+5's numbers there are single-threaded.

## Costs a product would pay

**Size.** The plain SA is 5× the corpus: about 20 GB here, and about 50 GB at
the 10 M-entry target (D40) if documents stay this size. The FM is about 0.9×
the corpus, and only its rare-literal and count paths are fast.

**Merge.** Suffix arrays do not merge cheaply; a merged segment is rebuilt.

- SA: about 20 s wall and 230 CPU s per GB at 16 threads, writing 5 bytes per
  corpus byte.
- FM: another 37 s per GB, single-threaded here.
- Trigram postings, by contrast, merge by concatenation.

Rebuilding needs the documents' bytes. Either the folded text is kept, which is
the SA's 1×, or every document is re-read at merge.

**Updates.**

- A changed or deleted document goes into a tombstone bitmap over the document
  table.
- A new or changed document goes into a small delta segment. Segments are
  searched in turn, so each one adds a binary search (about 60 cold page faults) per literal.
- Compaction rebuilds. This is the same lifecycle the product's segments already
  have; only the rebuild is dearer.

**Case.** The index holds ASCII-folded text.

- Case-insensitive queries need nothing more: `required_literals` emits Unicode
  case variants (for example, `k` and the Kelvin sign) as OR alternatives, and
  each alternative is looked up folded.
- Case-sensitive queries look up the folded literal, and verification filters.
  That admits candidates that differ only in case. Their volume was not measured
  here, because the screens population is case-insensitive.
- An exact-case SA would double the size.

## Brief claims found wrong

- **"Any length, including 1–2 bytes":** unreachable through
  `required_literals`, which drops literals shorter than three bytes.
- **"No false candidates":** true for the literal only. Identifier candidates
  run 55× the oracle's bytes.
- **"Fastest for anything with a selective literal":** not cold. Word 4+5 beats
  it on rare literals.
- **The screens harness:** it can no longer run, because its paired T1 index
  under `/tmp/d65c/screens/t1` is gone. This experiment reads the oracle cache
  directly instead.
