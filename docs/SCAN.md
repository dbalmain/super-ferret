# A pure scan against `rg`

Measured 2026-10-14 on branch `wt/scan` (d7160c9, from main at dc2530e, which
has threaded verification, D76 C). The aggregates are in
[`measurements/scan/baseline.txt`](measurements/scan/baseline.txt).

The goal is for Super Ferret to **match or beat `rg` on a pure scan**: every
live document verified against a regex, with no index screen helping. This page
is the baseline that later slices are judged by. It optimises nothing.
[After the scan work](#after-the-scan-work) records what the slices did.

## Summary

- **After the scan work (wt/scan, 0d51f02):** end to end at the default of 16
  threads we take 146–155 ms warm in ci against `rg`'s 162–180, and 136–151 in
  cs against its 147–163: level on cs rare literals, ahead everywhere else. Cold
  we still win, 3.5–3.9 s against 5.1–5.4. The rest of this summary is the
  baseline.
- **Cold, we already beat `rg`, by 1.3–1.6×.** Both read the same 4.29 GB from
  the device. We take 3.8–4.1 s and `rg` takes 5.2–6.1 s.
- **Warm, we lose on every class, by 3.2–6×.** `rg` takes 149–171 ms. Our best
  is 511–585 ms, at 8 threads. At the default of 32 threads we take 656–688 ms,
  and 983 ms on case-insensitive code patterns.
- **The floor is the calling thread.** It spends 425–615 ms of CPU per query,
  serially, whatever the thread count. Until that moves, no thread count gets a
  warm scan under about 450 ms.

## Method

`ferret-bench pure-scan` (`crates/ferret-bench/src/sparse/screens/scan.rs`).

- **Population.** It is the screens population: the D65C catalog and its frozen
  corpus.
  - 112,382 live documents, of which 112,376 are readable, holding 3,978,812,718
    bytes.
  - 250 regexes, 50 in each of five classes: rare literal, identifier pattern,
    alternation, broad and code class.
  - The seeds and their oracle come from the screens oracle caches
    (`oracle-ci.tsv`, `oracle-case-sensitive.tsv`), so no index is needed.
  - Each pattern is checked to round-trip through the cache's escaping.
- **Case.** The population runs both ways. `grep:` is case-sensitive by default
  (D67 A2). Case-insensitive prefixes `(?i)`, as `rg -i` folds.
- **Our scan.** It runs `Query::run_content_parallel` with
  `Pinned::new(None, live)`, so there is no content index and every live
  document is an uncovered Maybe. The bound is `None`, as `--scan` sets it.
  - The reader is the bench's: one `open` of the snapshot file and a
    `read_to_end`.
  - Every run must match the oracle's documents exactly, and must read every
    readable document and every byte.
  - The bench records, per run:
    - wall time and process CPU;
    - the calling thread's CPU (`/proc/thread-self/schedstat`);
    - the workers' summed busy time;
    - the time summed inside the reader (`read ms`);
    - the regex engine's time (`regex ms`).
- **`rg -l`.** It runs at its default thread count, which is 12: logical CPUs
  capped at 12. It is also timed warm at `-j32`. Its file set must equal the
  oracle's on every query and at every setting.
- **`rg`'s default output.** It prints every matching line into /dev/null, cold
  and warm, and again warm into a pipe the bench drains (ci only).
- **End to end.** This runs `ferret search --scan` as a process, on the first 5
  queries of each class, with `rg -l` beside it. Both must list exactly the
  oracle's files.
  - The catalog is `ferret index /tmp/d65c/corpus` (28 s, 112,376 documents),
    with its content index directory then removed, so the search is a pure scan.
  - `FERRET_NO_DAEMON=1` is set. Without it, `search` starts `ferretd`. The
    daemon follows the catalog and rebuilt 273 MB of content index within
    seconds, which would turn the scan into an indexed search.
- **Protocol.** It is the screens one: one cold/warm pair per query.
  - Cold evicts every readable corpus file with `posix_fadvise(DONTNEED)`. For
    the end-to-end runs it also evicts the catalog.
  - Warm is an immediate repeat.
  - Figures are medians/p90 over the 50 queries of a class, in ms.
- **Output order.** Ours keeps name order. `rg`'s is unordered, and any `--sort`
  forces it onto one thread, so it is not used.

### The fair `rg` flags

```text
rg --no-config --no-ignore --hidden -a -E none --no-messages \
   -g '!population' -l (-i | -s) -e <pattern> /tmp/d65c/corpus
```

- **The file set is already ours.** The corpus holds only catalogued documents,
  named by DocId. Ferret's ignore rules, size cap and binary check were applied
  when it was catalogued, so there is nothing more to exclude on `rg`'s side.
  `-g '!population'` drops the snapshot's own metadata file.
- **`-a` is required.** With `-uu` alone, `rg` stops reading a file at its first
  NUL. It then searched 3,930,833,755 bytes rather than 3,978,812,718, and
  missed a matching file on the broad query checked. With `-a`, `rg --stats`
  reports exactly the corpus: 112,376 files and 3,978,812,718 bytes, the same
  bytes we verify.
- **`-l` is the fair mode.** Our product emits one row per matching file and
  stops matching a file at its first match, which is what `-l` does.
  - Printing every line into /dev/null costs `rg` 0–21% warm (162–208 ms against
    161–171).
  - Into a drained pipe it costs up to 2.1× on dense classes: 358 ms for
    alternation, which printed a median of 811 MB.

## Baseline

Ours runs at its default of 32 threads; `rg` runs at its default of 12. Medians
in ms, with "×" as `rg` ÷ ours, so above 1 means we are faster.

| Case | Class        | Cold ours | Cold rg |    × | Warm ours | Warm rg |    × | Best warm ours |
| ---- | ------------ | --------: | ------: | ---: | --------: | ------: | ---: | -------------: |
| ci   | rare literal |     3,912 |   5,196 | 1.33 |       675 |     161 | 0.24 |        540 (8) |
| ci   | identifier   |     3,935 |   5,207 | 1.32 |       672 |     169 | 0.25 |        543 (8) |
| ci   | alternation  |     3,943 |   5,186 | 1.32 |       688 |     171 | 0.25 |        585 (8) |
| ci   | broad        |     3,902 |   5,201 | 1.33 |       666 |     165 | 0.25 |        545 (8) |
| ci   | code class   |     4,056 |   6,115 | 1.51 |       983 |     164 | 0.17 |       983 (32) |
| cs   | rare literal |     3,871 |   5,210 | 1.35 |       658 |     149 | 0.23 |        511 (8) |
| cs   | identifier   |     3,852 |   5,183 | 1.35 |       657 |     156 | 0.24 |        516 (8) |
| cs   | alternation  |     3,859 |   5,158 | 1.34 |       670 |     156 | 0.23 |        527 (8) |
| cs   | broad        |     3,841 |   5,215 | 1.36 |       656 |     155 | 0.24 |        511 (8) |
| cs   | code class   |     3,824 |   6,074 | 1.59 |       659 |     149 | 0.23 |        512 (8) |

- **p90.** Cold p90s are 4.0–4.4 s for us and 5.3–6.2 s for `rg`, with outliers
  of 8–17 s on both sides. The outliers coincide with device stalls seen during
  the ci run; one NVMe sensor read 79 °C. Warm p90s are within 10% of the
  medians. All the p90s are in `baseline.txt`.
- **Cold at 12 threads,** `rg`'s count, we take 4,053–4,276 ms (ci): 3–5% slower
  than at 32, and still 1.27–1.43× faster than `rg`.
- **Cold code class.** `rg`'s cold code class (6.1 s) is about 0.9 s slower than
  its other classes in both cases. Code ran last in each arm, so this may be
  thermal; it has not been explained.
- **Cold rate.** Cold, we read at about 1.1 GB/s against `rg`'s 0.82. That is
  below the 1.3–1.7 GB/s the screened product reached, because here every
  document is read.
- **`rg -j32`** is no faster warm (161–168 ms) and uses 2.3× the CPU (3.5–3.8 s
  against 1.45–1.7 s). `rg` is not thread-bound at 12.

### Thread curve, warm

Median ms.

| Case | Class        |     1 |     8 |    12 |    16 |  32 |  rg |
| ---- | ------------ | ----: | ----: | ----: | ----: | --: | --: |
| ci   | rare literal | 1,356 |   540 |   570 |   622 | 675 | 161 |
| ci   | identifier   | 1,386 |   543 |   572 |   611 | 672 | 169 |
| ci   | alternation  | 1,481 |   585 |   611 |   648 | 688 | 171 |
| ci   | broad        | 1,398 |   545 |   561 |   618 | 666 | 165 |
| ci   | code class   | 9,955 | 1,649 | 1,305 | 1,150 | 983 | 164 |
| cs   | rare literal | 1,072 |   511 |   537 |   565 | 658 | 149 |
| cs   | identifier   | 1,108 |   516 |   543 |   571 | 657 | 156 |
| cs   | alternation  | 1,212 |   527 |   552 |   570 | 670 | 156 |
| cs   | broad        | 1,195 |   511 |   536 |   560 | 656 | 155 |
| cs   | code class   | 1,090 |   512 |   543 |   582 | 659 | 149 |

8 threads is the best on every cell but ci code. From 8 to 32 threads, wall
grows by 18–29% and process CPU roughly doubles: 2.0–2.4 s at 8, 4.3–4.6 s
at 32.

### End to end, as processes

`ferret search --scan` against `rg -l`, 5 queries per class. Median ms.

| Case | Class        | Cold ferret | Cold rg | Warm ferret | Warm rg |
| ---- | ------------ | ----------: | ------: | ----------: | ------: |
| ci   | rare literal |       4,108 |   5,183 |         493 |     165 |
| ci   | identifier   |       4,064 |   5,116 |         498 |     164 |
| ci   | alternation  |       4,085 |   5,107 |         515 |     166 |
| ci   | broad        |       4,120 |   5,113 |         477 |     185 |
| ci   | code class   |       4,172 |   5,087 |         780 |     160 |
| cs   | rare literal |       3,982 |   5,126 |         478 |     148 |
| cs   | identifier   |       3,965 |   5,123 |         493 |     154 |
| cs   | alternation  |       4,021 |   5,093 |         491 |     154 |
| cs   | broad        |       4,002 |   5,166 |         493 |     158 |
| cs   | code class   |       4,051 |  12,586 |         495 |     148 |

- **Startup, catalog open and output cost little.** Cold, the process costs
  110–200 ms more than the bench arm.
- **Warm, the product beats the bench arm.** It takes 477–515 ms against the
  arm's 656–688 at the same 32 threads (780 against 983 on ci code).
  - The difference is the reader: the bench's `read_to_end` against the crawl
    reader's sized read.
  - The arm therefore overstates warm cost by about 170 ms. The end-to-end
    figures are the product's.
  - Even so, the product is 3.0–3.3× slower than `rg` warm, and 4.9× on ci code.
- **The cs code `rg` cold median (12.6 s)** is three of five runs hitting the
  stall noted above.

## Where the time goes

The figures come from the bench's breakdown columns: warm, ci broad unless
stated, summed over threads where marked. One `perf stat`, `perf record` and
`strace -c -f` run of `ferret search --scan` per class backs them. With
`perf_event_paranoid` at 2, perf sees user space only, so memory bandwidth could
not be counted.

### The top five costs

1. **The serial calling thread: 425–615 ms of CPU per query.**
   - It costs 425–460 ms cold, 445–490 at 8 threads and 585–615 at 32, in both
     cases and on every class.
   - At 32 threads that is 591 of 666 ms of wall: 89%. At 8 it is 457 of 545.
   - Wall tracks it, so it is the floor. It works out to about 4–5 µs for each
     of 112,376 documents: candidate rows, the ordered window and the handoff to
     workers.
   - The handoff dominates. `strace` counts 994,206 futex calls per query, 8.8
     per document, of which 770,209 returned EAGAIN. `rg` makes 113.
   - Mutex contention is 8% of user samples: 5.9% on the workers and 2.2% on the
     caller.
   - **This is the cost `rg` does not have at all.** It has no serial stage.
2. **Reading, which scales badly: summed read time grows from 574 ms at 1 thread
   to 900 at 8, 1,150 at 12, 1,430 at 16 and 2,400 at 32.**
   - That is for the same 3.98 GB, and it is the same in cs.
   - Per-thread throughput falls from 6.9 GB/s to 1.7: a 4.2× per-byte cost at
     32 threads. It points at memory bandwidth and page-cache contention.
   - The product reader adds to it:
     - It zero-fills its buffer before each read (`resize(…, 0)` in
       `ferret-crawl`'s `read_all`), which is 13–17% of user samples.
     - It makes 8.1 file syscalls per document: it reopens the root directory
       for every document, and makes three `fstat`s, two `open`s and two
       `close`s. `rg` makes 5.2.
   - Kernel time is 3.1 s against `rg`'s 0.9 s. User time is 2.2 s against its
     1.1 s.
3. **Case-insensitive code patterns in the regex engine: about 9 s of CPU.**
   - It is 8.9 s at 1 thread, 9.1 s summed at 8 and 9.6 s at 32: about 0.45 GB/s
     per thread. `perf` puts 89% of user samples in
     `regex_automata::hybrid::search::find_fwd`.
   - **The same patterns case-sensitive cost 166–300 ms** (cs, 8 threads).
     `rg`'s whole query costs 1.4–1.6 s of CPU either way.
   - This is the one class that 32 threads helps (983 ms). It still loses 6×.
4. **Threads past 8 cost more than they give.**
   - From 8 to 32 threads, wall rises 18–29% on every cell but ci code. Process
     CPU goes from 2.0–2.4 s to 4.3–4.6 s. The caller's CPU rises by about 130
     ms (cost 1).
   - The default of every core is the worst warm setting for a scan of non-code
     patterns. `rg` caps at 12 and does not slow down at 32.
5. **Regex on the other classes: 400–500 ms of CPU at 1 thread (ci).**
   - That is 8–10 GB/s, and 166–300 ms summed at 8 threads in cs.
   - It grows to 800 ms summed at 32 threads, which is contention again.
   - It is the smallest of the five, and is spread across the workers.

### Is the code-class regex compiled expensively?

From our own code (`ferret-verify/src/trigram.rs`, `GrepFlags::regex`, and
`ferret-query/src/content.rs`):

- **Bytes or `&str`: bytes.** It is `regex::bytes::RegexBuilder`, so there is no
  UTF-8 validation of documents.
- **Captures: none.** The verifier calls `grep.find(bytes)` and keeps only
  `span.start`.
- **Start of match: yes.** `find` reports the start, where `is_match` would not.
  Getting the start costs a reverse pass, but only on a document that matches.
  The profile's time is in the forward search (`find_fwd`), so this is not the
  cost on a scan where most documents do not match.
- **Unicode: on** (`unicode: true`), with `(?i)` from the inline flag. Under
  Unicode case folding, `[A-Z_]` also admits multi-byte characters: U+212A
  KELVIN SIGN folds to k, and U+017F LONG S folds to s. Each lowercase seed word
  also gains its case variants.
  - The cs/ci split (166–300 ms against 9.1 s for identical patterns) implies
    that folding takes away the fast path: the literal search for the seed word.
  - Without it, the lazy DFA walks every byte. This is an inference from the
    timings and the profile, not from the regex crate's internals.
  - Testing it would mean timing the ci code patterns with `(?i-u)` or with an
    ASCII-only class. That has not been done here.
  - It has since been tested, and the inference was wrong; see
    [the correction](#correction-folding-does-not-remove-the-literal-fast-path).

## After the scan work

Measured 2026-10-14 and 15 on `wt/scan`. Four steps, each aimed at one of the
costs above, then cuts to the serial setup of a one-shot search. Per-step
figures are a quick subset, the first 5 queries of each class, in
[`measurements/scan/steps.txt`](measurements/scan/steps.txt). The full 250-query
runs after step 4 are in
[`measurements/scan/after.txt`](measurements/scan/after.txt). Every arm stayed
correct on every query.

### Per step

Warm medians in ms, ranges over the five classes, with the ci code class
separately where it differs. Cold medians are in seconds. `rg` is 149–183 ms
warm and 5.1–5.3 s cold throughout.

| step                   | commit  | ci warm, ours               | cs warm, ours      | ci e2e warm       | cs e2e warm | cold, ours    |
| ---------------------- | ------- | --------------------------- | ------------------ | ----------------- | ----------- | ------------- |
| baseline               | 8112777 | 531–624 (8 thr); code 1686  | 515–533 (8 thr)    | 476–528; code 774 | 482–497     | 3.9–4.0 s     |
| 1. workers do the rows | c2733e7 | 206–254 (32 thr); code 1220 | 182–196 (32 thr)   | 252–276; code 834 | 234–250     | unchanged     |
| 2. streamed reads      | b74ef20 | 175–266 (16 thr); code 1056 | 153–161 (16 thr)   | 192–211; code 650 | 185–193     | 3.1–3.7 s     |
| 3. required run first  | 3fa8597 | 162–216 (16 thr), code 162  | 155–173 (16 thr)   | 187–214           | 182–191     | unchanged     |
| 4. default 16 threads  | 75db759 | 162–189 (full run)          | 154–165 (full run) | 174–204           | 171–176     | 3.5–3.9 s     |
| setup cuts             | c6335b4 | —                           | —                  | 163–175           | 158–171     | e2e 3.4–4.3 s |
| parallel setup         | 0d51f02 | —                           | —                  | 146–155           | 136–151     | e2e 3.5–3.9 s |
| review fixes           | 111d2bc | —                           | —                  | 139–151           | 141–154     | e2e 3.4–4.2 s |

1. **The calling thread only emits.** Workers claim batches from an atomic
   counter and do each position's tests, read, residual and path; the caller
   takes finished batches in order and calls emit. The caller's CPU fell from
   425–615 ms to 54–106, and futex calls per query from 994,206 to 766.
2. **The reader streams.** A grep that cannot see a newline or a text end reads
   each document through one reused 64 KiB buffer per worker, searches up to the
   last newline read, and stops once every atom has matched. There is no
   zero-fill. Directories on the path stay open per worker and are re-checked
   for every path. File syscalls per document fell from 8.1 to 5: open, read,
   fstat, close, and the held root's stat. Regexes are cloned per worker, and
   the result ring holds 16 batches per thread.
3. **A required run first, for ci code patterns.** See the correction below. For
   a line-local pattern with no finite literal prefix, `Search` (ferret-verify)
   finds the pattern's longest required run, searches for that, and runs the
   full pattern only over the window around each hit: the stretch of bytes a
   match can be made of, or the line when the pattern has assertions. The match
   and its offset are unchanged. The ci code class went from 1056 to 162 ms warm
   at 16 threads.
4. **The default is every core up to 16** (D77). See below.

The setup cuts are in [Remaining gap](#remaining-gap).

### Correction: folding does not remove the literal fast path

The baseline inferred that case folding takes away the seed word's literal
search. Timing refutes it. End to end at 32 threads, `(?i-u)[A-Z_]+worse` costs
the same as `(?i)[A-Z_]+worse`: 0.75 s wall, 9.9 s CPU. So does
`(?i:[A-Z_])+worse`, with only the class folded. `[A-Z_]+worse` costs 0.19 s,
and `(?i)worse` alone 0.19 s. The cost comes from the class matching any letter
in either case: the pattern has no literal prefix, so the engine runs its lazy
DFA over every byte, at about 0.4 GB/s per thread. Unicode and case variants are
not involved. Step 3 is the fix for this: `(?i)[A-Z_]+worse` now costs 0.20 s
and 1.2 s CPU.

### Threads

Full runs, warm medians in ms over the five classes, and process CPU:

| threads          | ci      | cs      | CPU, s  |
| ---------------- | ------- | ------- | ------- |
| 8                | 234–417 | 193–323 | 1.4–1.7 |
| 12               | 177–226 | 163–204 | 1.8–1.9 |
| 16               | 162–189 | 154–165 | 2.1–2.3 |
| 24               | 163–184 | 161–164 | 3.0–3.2 |
| 32               | 181–193 | 172–176 | 3.5–3.9 |
| `rg`, default 12 | 161–174 | 147–156 | 1.4–1.7 |

Cold, at 16 threads ours is 3.48–3.91 s in ci and 3.47–3.81 s in cs. At 32 it is
3.14–3.80 s and 3.16–3.67 s, which is 2–12% faster. `rg` takes 5.12–5.22 s.

Warm is fastest at 16 on every class in both cases. Past 16, each thread adds
CPU and no speed. The product's default is now `min(16, cores)`
(`SEARCH_THREADS` in `crates/ferret/src/engine.rs`); D77 asks whether that is
the right rule.

### End to end

`ferret search --scan` as a process at the default 16 threads, against `rg` on
the same files; 5 queries per class at 75db759 (after.txt), 10 per class later.

|                   | ci warm | cs warm | ci cold     | cs cold     |
| ----------------- | ------- | ------- | ----------- | ----------- |
| `ferret`, 75db759 | 174–204 | 171–176 | 3.49–3.84 s | 3.48–3.91 s |
| `ferret`, c6335b4 | 163–175 | 158–171 | 3.43–4.28 s | 3.45–3.88 s |
| `rg`, same run    | 164–174 | 146–159 | 5.16–6.21 s | 5.11–5.17 s |
| `ferret`, 0d51f02 | 146–155 | 136–151 | 3.49–3.88 s | 3.49–3.84 s |
| `rg`, same runs   | 162–180 | 147–163 | 5.13–5.22 s | 5.17–5.37 s |
| `ferret`, 111d2bc | 139–151 | 141–154 | 3.42–4.24 s | 3.39–3.84 s |
| `rg`, same run    | 157–178 | 154–164 | 5.15–6.14 s | 5.12–5.22 s |

At c6335b4 we tied `rg` warm in ci and trailed it by 0–20 ms in cs. The named
cause was serial setup, below. With the setup on several threads (0d51f02), we
beat `rg` warm on every class in ci, by 10–26 ms. In cs we are level on rare
literals (146.3 against 146.7 ms, and 146.8 against 149.1 in a repeat) and ahead
by 2–22 ms on the other four. Cold we beat it by 1.3–1.5×. The cs run was
repeated because two of its cold medians, and `rg`'s on the same classes, came
out 0.3–1 s high; the repeat is the cs cold row. The rows are in
[`measurements/scan/setup.txt`](measurements/scan/setup.txt) and
[`measurements/scan/parallel-setup.txt`](measurements/scan/parallel-setup.txt).

After Sol's round-1 review (111d2bc) we still beat `rg` on every class, warm and
cold, in both cases: by 9–39 ms warm in ci and 6–23 ms in cs. Those fixes post
each finished position rather than each batch, check for a stop around each
search in a stream, bound a long line's buffer by the document's length, and
start setup helpers that may fail. The reader is back to open, `fstat` and type
check, read, and `fstat` to bracket, so a document costs six file syscalls, not
step 2's five; the extra `fstat` measured about 1–2 ms warm. The ci cold rare
literal median (4.24 s) is high, as is `rg`'s on the same class (6.14 s). The
rows are in
[`measurements/scan/review-fixes.txt`](measurements/scan/review-fixes.txt).

### Remaining gap

The named cause of the warm gap was serial work before the first read. A
no-daemon process reached its first document read about 50 ms after exec,
instrumented at 16 threads, and `rg` has no such stage. Medians of a few
instrumented runs on the scan catalog (112,377 names), in ms:

| Stage                                | 75db759 | c6335b4 | 0d51f02 |
| ------------------------------------ | ------: | ------: | ------: |
| Catalog residency, in `Engine::open` |      33 |      26 |       9 |
| Document names, live set and pin     |     6–7 |     6–7 |       4 |
| S1's id pass                         |       4 |       4 |       4 |
| Certainty table                      |    3–20 | about 1 | about 1 |

- **03717d6** presizes the intern map and sorts distinct names on an 8-byte
  prefix first.
- **c6335b4**: with no content index every trigram key covers every live
  document, yet each key built a bitmap cursor over all of them. A tree whose
  keys have no postings is now the live bitmap itself.
- **e6b3134** builds the resident names on up to 8 threads. Names are split at
  sampled 8-byte-prefix ranges, so each part interns, sorts and posts its own
  names, and the parts' tables concatenate into the whole one: no merge, and the
  same bytes at any thread count, which a test checks at 1 to 255. The other
  catalog sections load on four threads during the build. `load_all` then checks
  every section in order, so a failure is reported as before.
- **0d51f02** builds the document names in one pass over the child column, not
  two over the names: 5.9 ms to 3.8.

What is left is about 18 ms, and the scan itself now takes less wall than `rg`'s
whole run. Taking each piece in turn:

- **Residency, 9 ms.** The names must load and be checked before the build
  starts (1.8 ms). The build's passes take about 3.5 ms on 8 threads, and its
  packing 2 ms on three, mostly bit-packing at about 8 ns a value. A daemon pays
  this once. Whether a one-shot search should build it at all is D78.
- **Document names, 4 ms.** Every content query needs them. They could be built
  alongside S1's pass, since neither reads the other, but `Content` takes them
  built, and making that lazy changes an interface every host and test uses. Not
  done.
- **S1's id pass, 4 ms.** It cannot be skipped: it is where liveness, the
  structural suppression and the result-kind domain apply to each name. Its path
  building is wasted on a content query, since the workers build the paths
  again, but skipping it saves only 0.6 ms (measured). Running it by name range
  on several threads would keep its order, since chunks concatenate; it is the
  next candidate if the gap reopens.
- **Overlapping setup with the reads** was not tried. The setup cuts closed the
  gap first. The reads need S1's ids, so starting them early means streaming S1
  into the pool, which touches the limit and count invariants.

## Reproduce

```sh
nix develop --command cargo build --release -p ferret-bench -p ferret
B=target/release/ferret-bench
# end-to-end catalog: index the corpus, then drop its content index
ferret --index /tmp/d65c/scan/e2e index /tmp/d65c/corpus
rm -r /tmp/d65c/scan/e2e/index
SCAN_FERRET=target/release/ferret SCAN_FERRET_INDEX=/tmp/d65c/scan/e2e \
SCAN_XDG=/tmp/d65c/scan/xdg \
  $B pure-scan /tmp/d65c/catalog /tmp/d65c/corpus \
     /tmp/d65c/screens/oracle-ci.tsv ci /tmp/d65c/scan/runs rg,rg-lines,ours,e2e
```

- `SCAN_THREADS` and `SCAN_COLD` replace the thread lists. They default to
  1,8,12,16,32 and 0,12 respectively, where 0 means every core.
- `SCAN_PER_CLASS` keeps the first n queries of each class.
- `SCAN_E2E_PER_CLASS` sets the end-to-end count, which defaults to 5.
- Per-query rows, which carry no pattern or path, go to the scratch directory.
