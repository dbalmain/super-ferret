# Regex screens against a full scan

Measured 2026-10-12/13 on branch `wt/screens` (main at 3d26ff3 plus
`origin/wt/sparse`). Aggregates are in
[`measurements/screens/`](measurements/screens/); per-query rows, patterns and
paths stay out of the tree.

## Question

Dave is weighing an opt-in 1.5 GiB word 4+5-gram index, which in
[SPARSE.md](SPARSE.md) cut the rare-literal regex median from 2,952 ms cold /
245 ms warm (trigrams) to 9.9 / 0.56 ms. His test is "much faster than scanning
the filesystem", and nobody had timed a scan. He also asked whether the term
index, which already holds a rare literal's words, could screen regexes for most
of that gain at no extra bytes, and wanted hashed 4- and 5-grams at 15, 16 and
17 bits measured too.

**Short answer.**

- **A scan is fast.** `rg` scans the whole 3.98 GB corpus in about 170 ms warm
  and 5.2 s cold, using 32 threads. Warm, it beats shipped trigrams on every
  class. Only rare literals get much faster than a scan: word 4+5 at 526× cold
  and 298× warm, and the sound term screen below at 69× cold and 2.9× warm.
  On identifier, alternation, broad and code patterns no arm beats a warm scan.
- **The term screen can match word 4+5's rare-literal volume soundly** (8.9 MB
  median against 9.3 MB), but only with two pieces the product lacks: a flat,
  scannable copy of the dictionary (154 MB) and a residue store for the whole
  runs the term index loses (330 MB). Built as the brief described it, the
  term screen is unsound under case-insensitive search. Made sound with a
  blanket fallback, it is slower than trigrams.
- **Verification is single-threaded** in every indexed arm, and `rg` is not.
  Most of the gap to a warm scan on the broad classes is that, not the
  screen; see [Answers](#answers). Since threaded (D76 C), three classes beat
  a warm scan by 1.25–3.6× and every class beats a cold one; see
  [Parallel verification](#parallel-verification).
- **Hashed 4/5-grams buy nothing** at 15–17 bits. Every key is used, and the
  median key holds thousands of documents.

## Population and protocol

The population is the sparse experiment's: the D65C catalog and its frozen
corpus.

- 112,382 live documents, 112,376 of them readable, holding 3,978,812,718 bytes.
- 250 regexes in five classes of 50 each.
- Compiled **case-insensitive**, as SPARSE.md did, so the numbers line up with
  its own.

  The product's `grep:` is case-sensitive (D67 A2). The case-sensitive
  population is a poor test: 31 of its 50 rare literals match nothing, because
  the seeds are lowercased forms of mixed-case source.

- The oracle (the `regex` crate over every readable file) is cached per case.
- The index is a T1 build (`TERM_VERSION` 2) made with
  `ferret-bench index-build --corpus` under `/tmp/d65c/screens/t1`.
  - It holds 286,112,775 bytes of trigrams and 200,818,708 bytes of terms.
  - Its trigram file is byte-identical to `/tmp/d65c/control`'s.

The protocol is the sparse latency harness's:

- **Cold:** before each cold run, `posix_fadvise(DONTNEED)` evicts the index,
  the arm's own files and every candidate document.
- **Warm:** an immediate repeat.
- One cold/warm pair per query.
- Posting time covers dictionary and posting reads plus the cursor drain.
- Verification opens, reads and matches each candidate, single-threaded.
- Total covers both phases.
- Times are milliseconds, given as median/p90.
- Each query is checked twice:
  - **positive control:** every oracle match is a candidate;
  - **stability:** cold and warm agree on candidates, and matches equal the
    oracle.

For `rg`, cold evicts every readable corpus file before the run. `rg`'s file set
must equal the oracle's on every query.

The shipped trigram baseline reproduces SPARSE.md's within 5% on every class
(rare literal 2,966/249 ms against 2,952/245 ms). Word 4+5's latencies are
therefore cited from SPARSE.md rather than re-run, which would cost a 1.5 GiB
build.

## Arms

1. **`rg`**, run as:

   ```text
   rg --no-config --no-ignore --hidden -a -E none -l --no-messages \
      -g '!population' -i -e <pattern> <corpus>
   ```

   - These flags give the oracle's semantics: every file, binary searched as
     text, no BOM transcoding, and a file listed at its first match.
   - It uses the default 32 threads.
   - CPU and I/O come from `/proc/self/io` and the child's `cutime` and `cstime`
     deltas.

2. **Trigrams**: the product planner's selection (`explain_content`), verified
   against each seed's derived tree.
3. **Term screen**, in several forms; see [below](#term-index-screen).
4. **Word 4+5**: SPARSE.md's numbers.
5. **Hashed 4/5-grams**:
   - Every folded 4- or 5-byte window, hashed (SplitMix64) to 15, 16 or 17 bits.
   - Deduplicated per document.
   - Written by the product segment writer.
   - The literal's keys are ANDed with its trigrams, keeping the four rarest
     keys, as SPARSE's rule does.
   - Rare literals and identifiers are timed; the other classes are volume only.

## Results

Candidate bytes are median/p90 in MB (10^6 bytes). Times are total ms, as
median/p90. For `rg`, the "candidates" are the whole corpus.

| Class        | Arm                                 | Candidate MB |       Cold ms |      Warm ms |
| ------------ | ----------------------------------- | -----------: | ------------: | -----------: |
| rare literal | **rg**                              |        3,979 |   5,182/7,275 |      168/176 |
| rare literal | trigrams                            |  1,565/2,007 |   2,966/3,904 |      249/317 |
| rare literal | term screen, blanket fallback       |  1,543/1,905 |   4,681/5,560 |  1,939/2,019 |
| rare literal | term screen, resident + residue     | 8.9/25.3 | 75/103 † | 57/74 |
| rare literal | word 4+5 (SPARSE)                   |     9.3/27.1 |      9.9/42.1 |     0.56/3.0 |
| identifier   | **rg**                              |        3,979 |   6,135/6,227 |      172/182 |
| identifier   | trigrams                            |  2,783/3,098 |   6,356/8,581 |      501/641 |
| identifier   | term screen, blanket fallback       |  2,779/3,098 |  8,487/12,300 |  2,336/2,564 |
| identifier   | term screen, resident + residue     | 2,779/3,098 | 7,211/8,861 † | 595/745 |
| identifier   | word 4+5 (SPARSE)                   |  2,027/2,734 |   4,544/7,150 |      402/600 |
| alternation  | **rg**                              |        3,979 |   5,226/6,192 |      179/198 |
| alternation  | trigrams                            |  3,135/3,330 |   8,272/9,914 |      542/718 |
| alternation  | term screen, blanket fallback       |  2,880/3,291 |  8,996/11,653 |  2,467/2,653 |
| alternation  | term screen, resident + residue     | 2,148/3,291 | 6,347/19,372 † | 428/850 |
| alternation  | word 4+5 (SPARSE)                   |  2,198/2,952 |   5,485/8,660 |      302/409 |
| broad        | **rg**                              |        3,979 |  5,208/11,480 |      172/185 |
| broad        | trigrams                            |  3,571/3,731 | 10,915/11,474 |    837/1,051 |
| broad        | term screen (no literal to look up) |  3,571/3,731 | 11,053/11,568 |    844/1,065 |
| broad        | word 4+5 (SPARSE)                   |  3,571/3,731 | 11,164/11,608 |    880/1,029 |
| code         | **rg**                              |        3,979 |   5,142/5,276 |      169/206 |
| code         | trigrams                            |  2,796/3,009 | 11,137/13,297 |  5,316/6,784 |
| code         | term screen, blanket fallback       |  2,500/3,009 | 13,365/23,293 | 7,747/12,951 |
| code         | term screen, resident + residue     | 1,408/3,009 | 6,594/12,759 † | 3,012/5,357 |
| code         | word 4+5 (SPARSE)                   |  1,366/2,352 |   5,638/8,922 |  2,552/3,533 |

Word 4+5's candidate MiB from SPARSE.md is converted to MB.

† The resident arm's flat files sat on tmpfs (see [below](#term-index-screen)),
so its "cold" evicts the index and documents but not those 484 MB. Reading
them from NVMe would add roughly 0.2–0.3 s cold (sequential 484 MB; an
estimate, not measured). That arm also ran while other sessions' builds filled
the root filesystem; its identifier, alternation and code timings overlap that
and may be contaminated.

`rg` details:

- It reads 3.98 GB per query by `rchar`; cold, that is 4.28 GB from the device.
- It burns 2.5 s CPU cold and **1.7 s CPU warm**, across 32 threads.
- Every arm here verifies on one thread. For an equal-CPU comparison, set `rg`'s
  1.7 s of CPU against an arm's warm wall time.

**Index bytes per arm:**

| Arm         |                                                                Index bytes |
| ----------- | -------------------------------------------------------------------------: |
| trigrams    |                                                                286,112,775 |
| term screen | 0 new (terms 200,818,708 already exist); resident form adds 484,319,153 (flat dictionary 154,159,943 + residue 330,159,210) |
| word 4+5    |                                 1,532.78 MiB = 1,607,233,000 (whole index) |
| hashed      |                                             [grid below](#hashed-45-grams) |

### Times faster than a scan

These are `rg` median ÷ arm median. Below 1 means slower than a scan.

| Class        | trigrams cold / warm | term screen (resident) cold / warm | word 4+5 cold / warm |
| ------------ | -------------------: | ---------------------------------: | -------------------: |
| rare literal |         1.7× / 0.67× | **69× / 2.9×** † |      **526× / 298×** |
| identifier   |        0.97× / 0.34× | 0.85× / 0.29× |        1.35× / 0.43× |
| alternation  |        0.63× / 0.33× | 0.82× / 0.42× |        0.95× / 0.59× |
| broad        |        0.48× / 0.21× |                                  — |        0.47× / 0.20× |
| code         |        0.46× / 0.03× | 0.78× / 0.06× |        0.91× / 0.07× |

Against `rg`'s CPU rather than its wall time, warm trigrams use 6.7× less CPU on
rare literals (249 ms against 1,670 ms) and 3.3× less on identifiers. On code
classes they use 3.2× more.

## Term-index screen

### The brief's unit rules, and why they are the wrong shape

The brief proposed four rules:

1. Take each required literal's tokenizer units.
2. Look up interior units as whole terms.
3. Look up the last unit by prefix.
4. Drop the first.

These rules are implemented in the bench (`screens/units.rs`). A unit counts as
safe only if the real tokenizer produces it in all 144 combinations of
one-character contexts on either side. A test pins soundness against arbitrary
neighbours.

They are **sound only for case-sensitive search**, for two reasons:

- `required_literals` folds facts to ASCII lowercase.
- The document's case decides where camelCase splits fall.

So under `(?i)`, no unit is ever safe. Case-sensitively, 7 of 50 rare literals
get a safe unit (6 exact, 2 prefix, mostly digit-split units). The identifier,
alternation, broad and code classes get none.

The cause is the seeds themselves:

- They are plain terms, `[a-z][a-z0-9]*`.
- So a seed never contains an underscore, and is a single run unless it holds
  digits.
- A single run has no safe unit, because a document can hold it inside a longer
  token.

The brief's `deserialize_ledger` shape does not occur in the query sets.

**Share of rare literals with at least one safe whole unit:**

- 0 of 50 case-insensitive;
- 7 of 50 case-sensitive.

### The better shape: substrings of whole-run terms

A whole run is lowercased character by character. So a document holding the
literal in **any** case has a whole-run term containing each lowercased word run
of the literal.

A substring lookup ("every term containing `ledger`") is therefore:

- sound in both cases;
- covers single runs;
- covers 50/48/50/0/50 queries across the rare, identifier, alternation, broad
  and code classes.

It costs a pass over the dictionary. Two tests pin it:

- `substrings_are_lowercased_word_runs`;
- `every_substring_is_inside_a_term_of_any_containing_document`.

### What the term index loses

The argument fails where a whole run never became a term:

- **T1 base64 runs:** `tokenize_terms` drops every token overlapping one;
- **the 64-byte cap:** it cuts a long token's tail.

Without a fallback (`terms-raw`), the screen misses oracle matches on:

| Class        | Queries with a miss | Missed documents |
| ------------ | ------------------: | ---------------: |
| rare literal |            15 of 50 |               80 |
| identifier   |                   1 |                2 |
| alternation  |                  17 |              628 |
| code         |                   9 |              333 |

Every one of these misses is in a document with a base64 run or a token over 64
bytes. None is unexplained.

The blanket fix admits those documents whatever the query. It is sound: all 250
queries pass the positive control. But it admits a lot:

| Set                  | Documents |                          Bytes |
| -------------------- | --------: | -----------------------------: |
| blob documents       |     7,808 |                        2.24 GB |
| long-token documents |     5,700 |                        2.42 GB |
| union                |     9,980 | 2.58 GB, **65% of the corpus** |

Trigrams barely narrow these documents, since a blob holds most trigrams. So the
rare-literal candidates go from 6.4 MB (raw, unsound) to 1,543 MB (sound).

There is a second cost. The substring pass through the product's checksummed
dictionary reader takes about 1.7 s per query, and reads 1.59 GB of index bytes
as logical reads, repeated chunks counted.

So the blanket-sound screen is **slower than trigrams on every class**: 1,939 ms
warm on rare literals against 249 ms.

### The sound fix, measured

Two pieces:

1. **Residue.** Admit a fallback document for a query only when its **lost whole
   runs** (dropped by T1, or over the cap) contain one of the query's needles.
   - Over this corpus the residue is 330,159,210 bytes, from 9,980 documents.
   - This is sound by the same argument: a needle absent from a document's
     terms, in a document holding the literal, must lie in a lost run.
   - Every query passed the positive control.
2. **A flat dictionary.** Hold the 11,445,463 terms as one newline-separated
   file of 154,159,943 bytes, and scan it with memmem, in place of the
   checksummed block reader.

Volume and the cost of the scan alone (`residue` arm). Candidate bytes are
median/p90 MB:

| Class        | Raw (unsound) | Residue by trigram index | Residue exact | Flat-dictionary scan ms |
| ------------ | ------------: | -----------------------: | ------------: | ----------------------: |
| rare literal |      6.4/16.8 |              1,277/1,419 |  **8.9/25.3** |                6.2/11.2 |
| identifier   |   2,779/3,098 |              2,779/3,098 |   2,779/3,098 |               12.8/19.7 |
| alternation  |   2,101/3,291 |              2,674/3,291 |   2,148/3,291 |               23.5/36.0 |
| code         |   1,191/3,009 |              2,033/3,009 |   1,408/3,009 |               23.0/43.1 |

Read the residue columns this way:

- **A trigram index over the residue is useless**, with 70,391,005 pairs.
  Residue is mostly base64, which holds every trigram.
- **Scanning the residue itself is exact.** It brings rare literals to 8.9/25.3
  MB, the same volume as word 4+5 (9.3/27.1 MB).

Timed end to end, with both flat files scanned inside the timing (`resident`
arm):

Medians/p90, ms; candidate MB as above. Index bytes read per query are
the two flat files, 484.6 MB, plus postings.

| Class | Candidate MB | Lookup cold | Lookup warm | Total cold † | Total warm |
| --- | ---: | ---: | ---: | ---: | ---: |
| rare literal | 8.9/25.3 | 57/82 | 56/73 | 75/103 | 57/74 |
| identifier | 2,779/3,098 | 99/130 | 96/119 | 7,211/8,861 | 595/745 |
| alternation | 2,148/3,291 | 171/242 | 146/190 | 6,347/19,372 | 428/850 |
| code | 1,408/3,009 | 164/237 | 139/206 | 6,594/12,759 | 3,012/5,357 |

All 250 queries passed the positive control, with candidates stable and
matches equal to the oracle. The lookup phase is the cost now: about 56 ms
to copy and memmem 484 MB through `pread` in 8 MB chunks. mmap would drop
the copy; an index over the residue instead of a scan of it would drop
two-thirds of the bytes (the flat dictionary alone scans in 6–23 ms, from
the `residue` arm). Neither is measured.

**This arm runs on tmpfs.** At the time it ran, other sessions' builds had
taken the root filesystem from about 60 GB free at the start of this work to
4.5 GB, and then to 0; the bench's own guard refuses below 5 GB. The arm
writes only its two flat files and its rows, so it ran with its working
directory and scratch on `/dev/shm`, writing nothing to `/`.

## Hashed 4/5-grams

Every folded 4- or 5-byte window, hashed to 15, 16 or 17 bits. Sizes are
bytes; docs per key are over used keys; build CPU covers reading the corpus,
inverting and writing. Rare-literal and identifier rows are timed; the
other classes' volumes are in [`hashed.txt`](measurements/screens/hashed.txt).

| Variant | Whole / postings MB | Keys used | Pairs | Docs per key median / p99 | Build CPU s | Rare MB median/p90 | Rare cold / warm ms | Identifier MB median | Identifier warm ms |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| trigrams (shipped) | 286.1 / — | — | — | — | — | 1,565/2,007 | 2,966 / 249 | 2,783 | 501 |
| 4-gram, 15 bits | 180.3 / 179.4 | 32,768 of 32,768 | 253,384,855 | 6,308 / 26,824 | 22.3 | 1,530/1,823 | 3,005 / 244 | 2,631 | 479 |
| 4-gram, 16 bits | 242.6 / 241.2 | 65,536 of 65,536 | 312,000,652 | 3,736 / 20,039 | 24.4 | 1,401/1,595 | 2,540 / 221 | 2,571 | 459 |
| 4-gram, 17 bits | 322.0 / 319.8 | 131,072 of 131,072 | 375,321,185 | 2,164 / 14,766 | 35.1 | 1,044/1,240 | 1,882 / 169 | 2,429 | 457 |
| 5-gram, 15 bits | 215.4 / 214.4 | 32,768 of 32,768 | 299,755,468 | 8,234 / 22,850 | 25.9 | 1,556/1,948 | 3,304 / 261 | 2,783 | 513 |
| 5-gram, 16 bits | 295.6 / 294.0 | 65,536 of 65,536 | 374,913,689 | 4,984 / 16,836 | 28.9 | 1,475/1,677 | 2,740 / 229 | 2,783 | 506 |
| 5-gram, 17 bits | 399.6 / 397.1 | 131,072 of 131,072 | 458,672,780 | 2,949 / 11,986 | 33.5 | 1,159/1,433 | 2,262 / 184 | 2,783 | 508 |

All six pass the positive control on all 250 queries.

**Size grows with width.** Every key is used at every width, so a wider
hash does not leave keys empty; it splits them. Within a document,
distinct grams that collide share one posting; with more bits fewer
collide, so the per-document deduplication merges less and pairs rise:
4-grams go 253 M → 312 M → 375 M (+23%, +20% per bit). Postings bytes
rise faster (+34%, +33%), because the lists also get sparser and their
deltas longer. 5-grams hold more distinct windows per document than
4-grams, so more pairs at the same width. The limit is unhashed fixed-4
in SPARSE.md: 42.4 M keys, 1,104 MiB.

**What it buys.** The median key holds 2,164–8,234 documents of 112,382,
so a key screens out little and a blob document holds nearly every key.
The best variant, 4-gram at 17 bits, cuts the rare-literal median from
1,565 to 1,044 MB (1.5×) and warm time from 249 to 169 ms, which merely
equals a warm `rg`. Identifiers gain 1.1× at best, and 5-gram variants leave them at
exactly the trigram volume (not investigated further).

## Parallel verification

Measured 2026-10-13 on branch `wt/par-verify` (D76 C). Aggregates are in
[`measurements/par-verify/`](measurements/par-verify/).

**What changed.** A content query's Maybe documents are now verified on a
pool of threads (`ferret-query/src/verify.rs`). The calling thread still
produces candidate rows in name order. It hands each new document to a
worker, and it emits rows from the head of a window in the same order as
before. For files that do not change during the query, and without
cancellation, output is byte-identical at any thread count: rows,
`--limit`, refusals, exit status, stderr, stdout and the query log's
counts. Only the times and the thread count itself differ. Two tests
compare 1 thread against several, including files that vanished, changed,
became unreadable or grew too large after indexing. Tests with a scripted
reader in `ferret-query` force the completion orders that matter rather
than waiting for the scheduler to produce them.

Verification observes a file's bytes when it reads them. A worker can read
a document up to 16 batches of up to 256 positions per thread ahead of the
caller (docs/SCAN.md), so the read may
come before the result callbacks for earlier rows run. A callback that
deletes or rewrites a later file may therefore leave that file's row as it
was read, where one thread would see the change. No production caller's
callback changes files; a caller that must see its own effects uses one
thread. Where a cancellation lands is a matter of timing, so a cancelled
query's counts vary with the thread count.

The thread count is `FERRET_SEARCH_THREADS`, defaulting to every available
core (32 here). The workers' buffers hold at most 64 MiB of documents by
catalogued size, plus 1 MiB of kept buffer per worker, plus whatever the
files being read grew since they were indexed. The calling thread's buffer
is separate: it reads one document at a time and keeps the capacity of the
largest.

**The bench now calls the product.** The arms above verify through the
harness's own loop. The new `product` arm runs every seed as `grep:` through
`Query::run_content_parallel`: the planner, postings, candidate rows and the
pooled verifier. Only the reader is the bench's: a snapshot `open` and read,
not the crawl's checked re-walk. At 1 thread the product path costs 0–15%
more than the harness loop (broad: 965 against 837 ms warm). That extra is
the caller's per-row work.

Every run's matches equal the oracle's at every thread count. Every run
reads the same documents.

**Warm, by threads.** Median ms over 50 queries. `rg` is cited from
[the results above](#results), because the harness is unchanged.

| Class        |    1 |    4 |      8 |     16 |     32 |  rg | best ÷ rg |
| ------------ | ---: | ---: | -----: | -----: | -----: | --: | --------: |
| rare literal |  250 |   69 | **47** |     50 |     72 | 168 |  3.6× faster |
| identifier   |  514 |  148 |     99 | **93** |    128 | 172 |  1.9× faster |
| alternation  |  606 |  194 |    146 | **143** |   165 | 179 |  1.25× faster |
| broad        |  965 |  315 | **275** |    302 |    349 | 172 |  0.63× (slower) |
| code         | 5,340 | 1,367 |   714 |    359 | **240** | 169 |  0.70× (slower) |

**Cold, by threads.** The index and every candidate document are evicted
before each run. Median wall ms, with device read throughput in GB/s.

| Class        |      1 |     8 |    32 |    rg | 32 ÷ rg |
| ------------ | -----: | ----: | ----: | ----: | ------: |
| rare literal |  3,119 (0.52) |   930 (1.73) |   900 (1.73) | 5,182 (0.82) | 5.8× faster |
| identifier   |  6,848 (0.42) | 1,845 (1.52) | 1,738 (1.65) | 6,135 | 3.5× faster |
| alternation  |  8,617 (0.37) | 2,356 (1.32) | 2,061 (1.53) | 5,226 | 2.5× faster |
| broad        | 11,160 (0.33) | 3,532 (1.05) | 2,778 (1.33) | 5,208 | 1.9× faster |
| code         | 11,395 (0.24) | 2,195 (1.30) | 1,690 (1.66) | 5,142 | 3.0× faster |

`rg`'s cold rate is 4.28 GB from the device in about 5.2 s.

**Cold is bound by the device, near 1.7 GB/s.** From 8 to 32 threads, rare
literals and identifiers gain under 7%, and every class levels off at
1.3–1.7 GB/s. Process CPU stays at 0.5–6 s while the workers' summed busy
time reaches 24–85 s, so the threads are waiting for reads. This is about
twice `rg`'s cold rate. I did not measure the NVMe's raw ceiling. A
deeper queue (readahead hints, or more reads in flight per thread) might go
further, but that is a guess.

**Warm stops scaling at 8–16 threads, for two reasons:**

- **Memory-bound classes** (rare literal, identifier, alternation). The
  caller is idle: 5–65 ms of its CPU per query at 8 threads. The workers are
  busy over 90% of the time, but the CPU spent on the same bytes grows with
  thread count: rare literals use 0.33 s at 8 threads and 1.37 s at 32.
  Rare literals plateau at about 35 GB/s of document bytes through `read`
  into a buffer, which is a copy out of the page cache. Two process-wide
  measurements point at copying and memory bandwidth rather than locks:
  - sys time is a third of all CPU time;
  - keeping every buffer, instead of dropping those over 1 MiB, changed
    nothing (`experiments.txt`).
  Waking only a waiting thread, rather than signalling on every submit and
  finish, gained 0–8%. That change is kept.
- **Broad is bound by the caller.** The calling thread spends 181 ms on the
  CPU in a 258 ms query at 8 threads, and 262 of 324 ms at 32. Workers are
  busy 55% of the time at 8 threads and 28% at 32. The serial part is the
  caller's per-row work for 57,000 candidates: S1's rows, residual
  evaluation, a stat for each repeated document, and emitting. At 1 thread
  that is about 120 ms. Contention inflates it further as threads are added.
- **Code** is bound by the regex, so it scales to 32 threads (22× at 32).

32 threads is slower than 16 on four classes, by 15–46%: 22–47 ms against
`rg`'s 170. On code it is faster by 119 ms. The default stays at every core.
The absolute loss on the light classes is smaller than code's gain, and a
query cannot know its class before it runs.

**So: verification is no longer the bottleneck on any class but code.** Warm,
three classes beat a scan, but only by 1.25–3.6×. That is "faster than
scanning", not "much faster". Broad loses to a scan because the caller is
serial. Code loses because of regex cost per byte: `rg` beats it on the
regex itself, not on volume. Cold, every class beats `rg` by 1.9–5.8×,
because it reads 39–90% of the bytes at twice the rate.

Next levers, from these numbers:

1. **Move per-row work off the caller.** This is for broad. Workers could
   evaluate the residual and paths could be built lazily, or S1 could be
   split across threads in name-ordered chunks.
2. **Read without the copy.** This is for the memory-bound classes. `mmap`,
   or matching while reading in blocks, would remove one pass over memory.
3. **Fewer candidate bytes.** Screens (D76 A/B) remain the only lever that
   makes rare literals *much* faster than a scan. Threading alone gets them
   to 3.6× warm and 5.8× cold.

## Answers

**How many times faster than a scan is each arm?** See
[the table](#times-faster-than-a-scan). Warm, a full `rg` scan beats shipped
trigrams on every class: it takes 170 ms on 32 threads. Only word 4+5 on rare
literals is "much faster than scanning", at 526× cold and 298× warm.
The sound term screen (resident dictionary plus residue scan) is 69× cold and 2.9× warm on rare literals †. On every other class, no arm beats a warm scan, and on broad and
code patterns, nothing beats a cold one.

**Why does a warm scan beat the indexes?** Candidate volume is 1.4–3.6 GB
median on every class but rare literals, so verification dominates, and the
harness verifies on one thread. `rg` spends 1.7 s of CPU per warm query across
32 threads. Verifying trigram candidates on as many threads would bring them
near or under a warm scan (an estimate from the CPU figures, not measured);
it is the cheaper lever for four classes of five, and no screen here changes
those classes' volume by more than about 2×. *Since measured:* threaded, rare
literals, identifiers and alternations beat a warm scan by 1.25–3.6×; broad
and code patterns still do not. See
[Parallel verification](#parallel-verification).

**How much of word 4+5's rare-literal gain does the term screen capture?** That
depends on the form:

- **As briefed, case-insensitive:** none. No unit is safe.
- **Substring lookups with a blanket fallback:** none. It is 7.8× slower than
  trigrams warm.
- **Raw, unsound:** an upper bound. It narrows further than word 4+5, to 6.4 MB
  against 9.3 MB, but misses 80 matches across 15 queries.
- **With the residue scan and a flat dictionary:** **all of the volume gain** (8.9 against 9.3 MB median), and
most of the cold time: of the 2,942 ms word 4+5 saves cold over trigrams,
it saves 2,891 (98%); warm, 192 of 244 ms (79%). As a ratio it is far
behind: 40× faster than trigrams cold against word 4+5's 300×, because a
lookup scans 484 MB where word 4+5 reads 36 KB of index.

**What share of rare literals has a safe whole unit?** 0 of 50 case-insensitive,
and 7 of 50 case-sensitive. The substring lookup, which needs no unit, covers 50
of 50.

**What do hashed 4/5-grams cost and buy?** 15–17 bits cost 180–400 MB and 22–35 CPU s to
build, and buy at most 1.5× on rare literals (4-gram, 17 bits) and 1.1× on
identifiers. Every key is used at every width; the median key names
thousands of documents. Size grows with width. Dave's expectation holds: they
do not pay.

## What the brief got wrong

- **The merge was clean.** It had no conflicts in `ferret-verify`. Merge commit
  `8e1315b`.
- **`/tmp/d65c/control` no longer opens on main.** It was built at
  `TERM_VERSION` 1, and T1 moved the version to 2. `View::open` rejects it with
  "no paired control index". I used the T1 build, whose trigram file is
  byte-identical.
- **There is no FST.** The term dictionary is codec-1 front-coded blocks of 32
  terms with a resident first-term index, and it had no prefix API. The bench
  adds `Segment::terms_from`, an additive reader seek.
- **The unit rules are unsound case-insensitive**, and facts are ASCII-folded;
  see above. Even case-sensitive, they cannot apply to these seeds: the seeds
  never contain an underscore.
- **Rare literals are not "already in the term index as units"** in the way the
  brief assumed. A seed is a single run, which a document can hold inside a
  longer token. Only a substring lookup reaches it.
- **The T1 caveat is the smaller half.** The 64-byte cap loses as much (5,700
  documents) as base64 does (7,808).
- **The done-note is untracked.** `.ai/` is gitignored, so it cannot be
  committed.

## Reproduce

```sh
nix develop --command cargo build --release -p ferret-bench
B=target/release/ferret-bench
$B screens /tmp/d65c/catalog /tmp/d65c/screens/t1 /tmp/d65c/corpus /tmp/d65c/screens ci rg
# then, one at a time: trigrams, terms, terms-raw, residue, resident, hashed, product
```

`product` defaults to warm at 1/4/8/16/32 threads and cold at 1/8/32.
`PRODUCT_THREADS` and `PRODUCT_COLD` replace those lists (comma-separated; the
cold counts must be among the warm ones; empty means none).

Arms run in the order given and print aggregate tables. Per-query rows, which
carry no pattern or path, go to the scratch directory. `hashed` builds, times
and deletes each of the six segments. `resident` writes and deletes its two flat
files.
