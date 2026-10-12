# Token policy: base64-like runs and timestamps

2026-10-12, branch `wt/tokens` from `wt/d65c` at `65f215f`, measured on the D65
slice C run directory: the refreshed `$HOME` catalog and the frozen corpus of
112,376 documents (3.98 GB). Codec 1 and today's tokenizer remain the default;
every policy here is an opt-in experiment. The decision brief is
[DECISIONS.md § D65 update — token policy](DECISIONS.md#d65-update--token-policy).
Raw aggregates are in [measurements/tokens/](measurements/tokens/).

**Where the code is.** Dave adopted T1, with blob-looking `text:` terms routed
to trigrams (Question 1, C), held T2 and left mtime and ctime alone
(2026-10-12). The experiment code this page describes, the policy API, T2's
detector, the date index and the bench arms, lives on branch `wt/tokens`. Main
carries only T1's base64-run detector, on by default, in
`ferret-text/src/blob.rs`; see [DESIGN.md](DESIGN.md).

- **T1**: base64-like runs leave the term index. Trigrams keep their bytes, so
  `grep:` and regex still find them.
- **T2**: recognised timestamps go to a date index. Their time-of-day and epoch
  pieces leave the term index; their date pieces stay.

## Method

### Policies

The policies live in `ferret-text` (`policy.rs`).

- `tokenize_with(bytes, scratch, policy, emit)` runs the D9 tokenizer and drops
  every token whose run overlaps a suppressed span. `Policy::NONE` is exactly
  `tokenize`.
- `TOKENIZER_VERSION` does not record the policy. Nothing persists it, so an
  index built under a policy must be read as an experiment.
- `IndexWriter::set_policy` applies the policy to follow.
- Codecs 4–6 tokenize documents again at merge, so the bench refuses a policy
  with them. Codec 1 and C4b (codec 7) never read documents at merge.

### T1 detector

The detector looks at each maximal run of `[A-Za-z0-9+/_-]`, allowing up to two
`=` of padding.

It first trims written segments off both ends of the run. Segments are split at
`-`, `_` or `/`. A segment is written if it is at most 16 lowercase letters and
digits (`sha512-…`, `forms/d/…`), or if it fails the randomness test (a bundler
chunk's name, a CSS-module class).

The rest of the run is suppressed if all of these hold:

- it has at least 20 characters;
- it is not all hex, with an optional `0x`;
- it has a digit;
- upper and lower case each make up a tenth of it;
- case flips between at least a quarter of adjacent letter pairs;
- its lowercase runs average at most 2.6 letters;
- under 60% of its lowercase runs of two or more letters hold a vowel.

### T2 detector

The detector recognises three forms.

- **ISO-8601 and its relatives, years 1970–2099.**
  - The date separator is `-` or `/`, and the date is validated.
  - A `T` or space follows, then `HH:MM`, then an optional `:SS` (up to 60).
  - An optional fraction of up to nine digits, after `.` or `,`.
  - An optional `Z` or `±HH[:]MM`. A timestamp with no zone is read as UTC.
- **A date alone**, read as midnight UTC.
- **Epoch seconds (10 digits) or milliseconds (13 digits) in 2010–2030**,
  accepted only after a time key. The key is read across up to three bytes of
  `"`, `:`, `=` or space before the number.
  - A key ending in `id` or `ids` refuses the number.
  - A time key contains time, date, stamp, epoch, created, updated, modified,
    expire, when, start, end or ms, or it ends in `at`, `ts` or `_ts`.

Each timestamp normalises to UTC seconds plus nanoseconds. The date index keeps
whole seconds. A query with a fraction is verified against the document
(`has_timestamp_in`).

### Census

`ferret-bench token-census <catalog> <corpus>` makes one pass over the corpus on
16 threads. It reports:

- per-policy term and hapax counts and bytes, and the postings each policy
  drops;
- epoch candidates by band and key context;
- the date layouts' sizes and query costs;
- the catalog's mtime and ctime in the same layouts, against a plain column.

As a positive control, its no-policy arm reproduces the slice C census exactly:
18,990,510 terms and 13,329,685 hapaxes. Review samples (`--samples`) go only to
the gitignored `.ai/` directory.

### Arms

`d65c.py <arm> build|query|churn --tokens t1|t2|t1t2` builds a fresh follow
under the policy. It then compares every warm query's row identities with the
plain codec-1 control.

- An arm may lack a row only if the document holds the term under plain
  tokenization and not under the policy.
- Any other missing row, and any extra row, fails the run.

Codec 1 and C4b ran under all three policies, one benchmark at a time. The arm
indexes were deleted afterwards.

### Date layouts

The layouts are in `ferret-index/src/dates.rs`, and are in memory only. They
follow Dave's framing: dates are numbers, not terms, stored as u32 UTC seconds,
which last until 2106.

Each layout keeps time to a resolution, and an entry stands for the cell
`[t, t + resolution)`. A range that covers a whole cell is exact. A cell the
range only clips gives a candidate that must be verified. Repeated `(cell, doc)`
entries are stored once.

- **Time-major.** Entries are sorted by `(cell, doc)` in blocks of 128.
  - A 13-byte block header holds the first and last cell, a bit offset and a
    width.
  - Cell deltas are bit-packed at the block's widest delta.
  - Doc ids are bit-packed at the width of the largest id: 17 bits here, 19 for
    the catalog's inodes. In time order they do not compress.
- **Bucketed.** The buckets are hours or days.
  - A directory has one 12-byte entry per bucket.
  - Each bucket holds a vbyte delta-coded doc posting list and a vbyte count of
    offsets per doc.
  - In-bucket offsets are bit-packed at `log2(bucket / resolution)` bits: 17 for
    the seconds in a day.
  - With the resolution equal to the bucket there are no offsets, and clipped
    buckets are verified.

## Results

### Terms and hapaxes (census)

| Policy | Terms      | Term bytes | Hapaxes    | Hapax bytes | Postings |
| ------ | ---------- | ---------- | ---------- | ----------- | -------- |
| none   | 18,990,510 | 305.0 MB   | 13,329,685 | 232.0 MB    | 87.31M   |
| T1     | 11,445,463 | 142.7 MB   | 7,393,844  | 97.8 MB     | 60.42M   |
| T2     | 18,209,876 | 295.1 MB   | 12,556,546 | 222.1 MB    | 85.60M   |
| T1+T2  | 10,664,815 | 132.8 MB   | 6,620,696  | 88.0 MB     | 58.69M   |

**T1 removes 5.94M hapaxes and 26.9M postings.** Of those postings, 14.1M belong
to the 407k terms that stay at df ≥ 2. By length, 10.07M of the 14.1M belong to
1–3 byte terms, the camelCase fragments of blobs. Another 4.04M belong to 4–7
byte terms, and only 18k to anything longer.

**T2 removes 0.78M hapaxes, about 10 MB.** By shape:

- 716k are 13-digit millisecond epochs;
- 21k are 10-digit epochs;
- 24k mix digits and letters (`12t03`, `123z`).

The brief's guess that ISO pieces explain the "mixed groups" hapaxes does not
hold. The tokenizer emits `12t03`, `12`, `t` and `03` from `12T03`, and those
values have few distinct spellings.

### False suppressions

**T1, by hand.** These counts come from the private `.ai/` sample review.

|             | Runs      | Bytes    | Documents | Sample reviewed | Not base64                                          |
| ----------- | --------- | -------- | --------- | --------------- | --------------------------------------------------- |
| Code files  | 4,410     | 17.5 MB  | 404       | 150             | about 6 written identifiers; about 13 SVG path data |
| Prose files | 317       | 2.5 MB   | 107       | 107             | about 0: all blobs, keys or ids                     |
| Other files | 1,946,348 | 277.9 MB | 7,297     | none            | none                                                |

- **Identifiers it still takes** have short capitalised words with digits
  between them.
- **SVG path data** is not base64, but its pieces are not words either.
- **The first-cut rule took many more**: bundler chunk names, CSS-module
  classes, `oAuth2…` identifiers and mixed-case hex addresses. The tests now pin
  made-up examples of each as kept.

**T2 epochs.** This table counts 10- and 13-digit standalone numbers by 20-year
band and by the key before them.

| Digits | Band      | id key | time key | no key  |
| ------ | --------- | ------ | -------- | ------- |
| 10     | 1990–2010 | 664    | 47       | 34,444  |
| 10     | 2010–2030 | 807    | 119,656  | 103,882 |
| 10     | 2030–2050 | 3      | 21       | 33,822  |
| 13     | 1990–2010 | 13     | 2,603    | 7,264   |
| 13     | 2010–2030 | 326    | 833,799  | 10,119  |
| 13     | 2030–2050 | 1      | 0        | 319     |

- **Without a key**, 10-digit numbers are about as dense in the neighbouring
  bands as in the accepted one. The 1990–2010 band holds only 8.3 years of
  10-digit values. A third or more of the in-range bare numbers are therefore
  ids, so bare numbers are not accepted.
- **With a time key**, the sidebands hold 68 numbers against 119,656 in range,
  an estimated false-positive rate of **≤ 0.3%**.
- **The cost**: a bare epoch in prose stays a term.

### The D65 arms

All arms are at steady state, measured against the plain codec-1 control: 450.28
MB and 269.05 s of CPU (follow plus steady merge). The full tables are in
[arms.md](measurements/tokens/arms.md).

| Arm            | Steady term index | Saving | Full merge | CPU    | df1 / df2 / absent p90 ms | Mid median | Common median |
| -------------- | ----------------- | ------ | ---------- | ------ | ------------------------- | ---------- | ------------- |
| codec 1        | 450.28 MB         | —      | 398.12 MB  | 1.000× | 0.14 / 0.15 / 0.12        | 1.88 ms    | 26.26 ms      |
| C4b            | 317.95 MB         | 29.4%  | 305.18 MB  | 1.056× | 0.45 / 0.39 / 0.21        | 1.51×      | 1.06×         |
| codec 1, T1    | 221.21 MB         | 50.9%  | 200.82 MB  | 1.026× | 0.10 / 0.11 / 0.10        | 0.88×      | 0.99×         |
| C4b, T1        | 193.82 MB         | 57.0%  | 188.81 MB  | 1.110× | 0.19 / 0.21 / 0.16        | 1.43×      | 1.03×         |
| codec 1, T2    | 442.63 MB         | 1.7%   | 389.94 MB  | 1.016× | 0.13 / 0.12 / 0.11        | 1.04×      | 1.01×         |
| C4b, T2        | 310.71 MB         | 31.0%  | 298.71 MB  | 1.095× | 0.22 / 0.26 / 0.18        | 1.54×      | 1.04×         |
| codec 1, T1+T2 | 215.62 MB         | 52.1%  | 192.62 MB  | 1.060× | 0.09 / 0.11 / 0.09        | 0.88×      | 0.96×         |
| C4b, T1+T2     | 189.94 MB         | 57.8%  | 182.97 MB  | 1.134× | 0.16 / 0.19 / 0.14        | 1.43×      | 1.01×         |

The T2 savings leave out the date index, which adds 3.3 MB (below).

The detectors cost CPU: 4–16 s on codec 1 and 10–21 s on C4b, compared with the
same codec without a policy. These are not contamination. The preflight was
clean and nothing else ran during the arms.

**Does T1+T2 bring C4b's mid median and churn penalty inside the gates? No.**

- **Mid median.** On the letter of the gate it passes, at 1.43× the plain
  control. But the query set changed underneath it.

  - T1 drops rows from 24 of the 50 mid queries, 11,227 rows in all.
  - With fewer rows to return, codec 1 under the same policy also speeds up, to
    1.65 ms.
  - Against same-policy codec 1, C4b is 2.69 / 1.65 = **1.63×**. That is worse
    than its 1.51× without a policy.
  - Mid p90 is 14.07 ms against 5.47 ms, still 2.6×.

  The policy shrinks the result set. It does nothing about C4b's window-miss
  full reads.

- **Churn.** The S2 trace is synthetic. Its vocabulary is lowercase words plus
  one `u{hex}q` term per document, with no base64 and no timestamps.
  - C4b under T1+T2 produced every churn size and count identical to C4b alone.
  - So the +17.3% full-merge penalty cannot move under any token policy on this
    trace.
  - Measuring a policy's effect on churn needs a trace built from real edits.

**Result-set changes.** Every query in every arm matched the control apart from
rows the policy explains. No arm added a row. The first, steady and full states
gave identical changes, and so did codec 1 and C4b.

| Policy | df1                        | df2–100        | Mid              | Common           |
| ------ | -------------------------- | -------------- | ---------------- | ---------------- |
| T1     | 42 of 200 queries, 43 rows | 39 of 175, 212 | 24 of 50, 11,227 | 42 of 50, 23,417 |
| T2     | 18 of 200, 18              | 3 of 175, 6    | 9 of 50, 2,008   | 1 of 50, 2,894   |
| T1+T2  | 60 of 200, 61              | 42 of 175, 218 | 24 of 50, 13,424 | 42 of 50, 26,393 |

**21–30% of the df1 queries sampled from the control are pieces of blobs**, and
they now find nothing. That matters more than the latency: once a policy runs,
the rare classes measure a different population.

**The mid and common drops** are documents whose only occurrence of a short word
lies inside a blob (T1) or a time of day (T2: `05`, `30`).

### Date layouts

There are 2,871,064 timestamp occurrences in 14,288 documents. They make
1,255,072 distinct (second, doc) pairs.

| Layout                         | Entries   | Bytes   | B/entry | Time bits | Doc bits | Header |
| ------------------------------ | --------- | ------- | ------- | --------- | -------- | ------ |
| time-major, 1 s                | 1,255,069 | 3.88 MB | 3.09    | 6.9       | 17.0     | 127 kB |
| time-major, 1 min              | 690,997   | 1.93 MB | 2.79    | 4.5       | 17.0     | 70 kB  |
| bucketed by day, 1 s offsets   | 1,255,069 | 3.29 MB | 2.62    | 17.0      | 3.4      | 85 kB  |
| bucketed by day, 1 min offsets | 690,997   | 1.57 MB | 2.27    | 11.0      | 6.2      | 85 kB  |
| bucketed by hour, 1 s offsets  | 1,255,069 | 3.45 MB | 2.75    | 12.0      | 5.6      | 681 kB |
| bucketed by hour, no offset    | 311,557   | 1.26 MB | 4.03    | 0         | 14.7     | 681 kB |
| bucketed by day, no offset     | 236,712   | 0.38 MB | 1.61    | 0         | 10.0     | 85 kB  |

The queries are 30 seeded picks per class:

- point: the occurrence's instant, to the millisecond;
- UTC day;
- local day, at UTC+11;
- UTC month.

Point queries always verify, because the layouts drop fractions. The table gives
medians, with p90 where it matters.

| Layout               | Point: verify docs / MB / ms       | Local day: verify docs / ms | Month lookup |
| -------------------- | ---------------------------------- | --------------------------- | ------------ |
| any 1 s resolution   | 2 / 4.0 / 8 (p90 4 / 9.0 / 16)     | 0 / 0                       | 2.0–2.5 ms   |
| any 1 min resolution | 7 / 15.7 / 47 (p90 12 / 32 / 88)   | 0 / 0                       | 0.8–1.0 ms   |
| hour, no offset      | 24 / 27.9 / 60 (p90 69 / 58 / 126) | 0 / 0                       | 0.15 ms      |
| day, no offset       | 187 / 171 / 328                    | 283 / 455                   | 0.06 ms      |

UTC days and months are exact at every resolution. A local day is exact at any
resolution finer than a day.

**Once dates are integers, uniqueness is cheap.**

- In the term-style first cut, second keys had a median of one document per key,
  making a hapax store of 4.86 MB.
- As integers, the same entries cost 2.6–3.1 bytes each, and doc ids are the
  larger part.
- The time-major layout spends 17 of its 24 bits per entry on the doc id.
- The bucketed layout moves the cost to the offset (17 bits). It makes doc ids
  nearly free (3.4 bits), because a day's posting list is delta-coded.

Dropping seconds to minutes halves the entry count, because log lines share a
minute. That saves 1.7 MB, but a point query then reads 16 MB instead of 4 MB.

### mtime and ctime

Each inode has one value. The catalog has 394,197 inodes, with 27,347 distinct
mtime seconds and 18,102 distinct ctime seconds. That is far more clustered than
a lived-in tree, because the catalog was refreshed from a frozen copy.

|                                      | Bytes           | Day lookup      | Month lookup | Last 7 days |
| ------------------------------------ | --------------- | --------------- | ------------ | ----------- |
| plain column, u32 per inode, scanned | 1.58 MB (4 B)   | 0.32 ms         | 0.32 ms      | 0.32 ms     |
| time-major, 1 s                      | 1.14 MB (2.9 B) | 0.16 ms         | 0.70 ms      | 0.52 ms     |
| bucketed by day, 1 s offsets         | 1.66 MB (4.2 B) | 0.06 ms         | 0.66 ms      | 0.53 ms     |
| bucketed by day, no offset           | 0.42 MB (1.1 B) | 0.03 ms (exact) | 0.56 ms      | 0.43 ms     |

- **Single-valued keys remove the bucketed layout's advantage.** Each inode
  appears once, so its id costs 16 bits in the posting list, where content dates
  needed 3.4.
- **A scan of the plain column beats every index for a month or a week.** Those
  ranges match 9% and 7% of the inodes.
- **The catalog already stores both as columns** (`Section::Mtime`,
  `Section::Ctime`). At 1M inodes a scan would take about 0.8 ms.

**So mtime and ctime need no new structure.** They belong in the existing
catalog column, scanned. A sorted permutation becomes worth building only for
selective ranges at about ten times this inode count.

## Semantics

- **`text:` under T1.**
  - A term no longer matches a document whose only occurrences of it lie inside
    suppressed runs. That covers the blob itself and every camelCase piece of
    it.
  - `grep:` and regex still match, through trigrams. `text:iVBORw0KGgo` finds
    nothing, while `grep:iVBORw0KGgo` finds the image.
  - A short word such as `ab` stops matching documents where it appeared only as
    a piece of a key. All 40,280 rows the T1+T2 arms dropped were of these
    kinds.
- **A per-document "has suppressed runs" flag** would restore the exact
  behaviour, by sending every T1 document to verification for any term the index
  misses.
  - The documents carrying runs total **2.24 GB**, more than half the corpus
    bytes, so any rare-term query would read gigabytes. Not recommended.
  - A cheaper fix: when a `text:` term itself looks like a blob by the same
    detector, send it to trigrams.
- **`text:2026` under T2 still matches.**
  - Date pieces stay as terms: year, month and day. Both `2026/10/12` and
    `2026-10-12` give `2026`, `10` and `12`.
  - Time-of-day pieces and keyed epoch numbers leave. `text:03` misses a
    document whose only `03` was an hour, and `text:1760238245123` misses an
    epoch after a time key. A date query finds both.
- **Date queries** are ranges of UTC seconds.
  - A local-time day is a range of seconds, exact at any resolution finer than a
    day.
  - A query with a fraction verifies its candidates against the document. The
    tests pin both outcomes: `.123` is confirmed, and a document whose only
    number at that second is an `id` is refused.
- **Policies are not versioned.** Nothing distinguishes an index built under a
  policy from one built without. Shipping either policy needs a manifest field
  and a check like `TOKENIZER_VERSION`, because codecs 4–6 tokenize again at
  merge.
