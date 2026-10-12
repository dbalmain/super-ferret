# T1 screen: units (A) vs literal (B) for routed blob `text:` (aggregates)

2026-10-12, branch `wt/t1` at `633c292` plus the bench, on the D65 run
directory: the refreshed `$HOME` catalog (112,382 live documents) and the
frozen corpus (112,376 documents, 3.98 GB). The index was built by
`ferret-bench index-build --corpus` (product `IndexWriter::follow`, codec 1,
T1 on) and fully merged to one segment. Produced by
`ferret-bench blob-screen sample|run` and [compare.py](compare.py); the
sampled arguments and per-query rows stay private.

- **A** is the product: a routed atom's screen is the trigrams of its
  distinct ASCII units of 3 bytes or more (`Text::trigrams`).
- **B** is a throwaway patch, not committed: the same function returns the
  trigrams of the argument's maximal `[A-Za-z0-9]` runs, each a
  case-insensitive literal. Planner, 64-key cap, verifier and reader are
  unchanged, so only the screen differs.

**Sample.** 1,951,075 detector runs in the corpus; 9,024 longer than 4,096
bytes were left out (a paste, not an embedded file). Every remaining run is
routed on its own. One run per document per band, 75 per band by reservoir,
from 3,949 / 3,729 / 5,207 / 1,408 eligible documents. 50 df = 1 words from
the merged index as a comparison; they are not routed, so A and B run them
identically.

**Per query**: candidates (documents the screen leaves Maybe; every one is
read, so candidate bytes equal verified bytes), verified bytes, rows, warm
wall (second of two runs), whether the Maybe bound exceeds a quarter of live
documents (the refusal without `--scan`), and whether the argument's own
document is a row (positive control).

| band | arm | n | screen keys p50 | screen empty | refused | own doc missing | candidates p50/p90/p99 | verified MiB p50/p90/p99 | rows p50/p90/p99 | warm ms p50/p90/p99 |
| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |
| 20-31 | A | 75 | 6 | 3 | 3 | 0 | 1323/2502/112382 | 1462/1853/3794 | 2/194/256 | 2371/3892/17998 |
| 20-31 | B | 75 | 28 | 0 | 0 | 0 | 610/781/1189 | 932/1070/1411 | 2/194/256 | 1403/1770/8282 |
| 32-63 | A | 75 | 12 | 0 | 0 | 0 | 1205/2461/2461 | 1409/1537/1633 | 4/1869/1869 | 2562/3492/4752 |
| 32-63 | B | 75 | 52 | 0 | 0 | 0 | 539/2082/2082 | 873/1101/1370 | 4/1869/1869 | 1571/2066/2290 |
| 64-255 | A | 75 | 23 | 0 | 0 | 0 | 1130/1195/1264 | 1367/1408/1442 | 2/8/292 | 3402/4364/5833 |
| 64-255 | B | 75 | 99 | 0 | 0 | 0 | 411/644/780 | 727/979/1051 | 2/8/292 | 2016/2714/3737 |
| 256-4096 | A | 75 | 289 | 0 | 0 | 0 | 1107/1149/1185 | 1360/1395/1413 | 1/4/14 | 15615/32904/37543 |
| 256-4096 | B | 75 | 1361 | 0 | 0 | 0 | 319/344/468 | 636/684/779 | 1/4/14 | 8446/17881/22729 |
| df1 | A | 50 | 6 | 0 | 0 | 0 | 7/7/7 | 0/0/0 | 1/1/1 | 0/0/0 |
| df1 | B | 50 | 6 | 0 | 0 | 0 | 7/7/7 | 0/0/0 | 1/1/1 | 0/0/0 |

| band | p90 candidates B/A | p90 verified bytes B/A | median per-query candidates B/A | queries B rows < A rows | B rows > A rows |
| --- | --- | --- | --- | --- | --- |
| 20-31 | 0.31 | 0.58 | 0.47 | 0 | 0 |
| 32-63 | 0.85 | 0.72 | 0.46 | 0 | 0 |
| 64-255 | 0.54 | 0.70 | 0.38 | 0 | 0 |
| 256-4096 | 0.30 | 0.49 | 0.29 | 0 | 0 |

"Screen empty": A's units are all under 3 bytes, so the screen is every live
document. All three are in the 20–31 band (lengths 20, 25, 31), and they are
the only refusals in either arm.

## Reading

- **Positive control: 300 of 300** blob queries return their own document,
  under both arms.
- **Rows are identical** under A and B for all 300 queries, so B's
  respelling loss never showed in this sample.
- **A's screen is weaker by 1.4–2× in bytes.** B's p90 verified bytes are
  0.49–0.72 of A's per band; per query, B keeps a median 0.29–0.47 of A's
  candidates.
- **Both screens hit the same kind of floor.** No query in either arm went
  under 291 candidates. Base64's trigrams are close to uniformly random over
  the folded alphanumeric keys, so the large blob-heavy documents (D69's
  saturated documents) hold all of them. A verifies at least 939 documents
  (1.3 GB) per query, B at least 291 (about 0.6 GB). Even B takes 1.4 s warm
  at the median for the shortest band.
- **Long arguments verify slowly.** At 256–4,096 bytes, B reads 636 MiB at
  the median in 8.4 s (75 MB/s); at 20–31 bytes it reads 932 MiB in 1.4 s.
  That cost is in the matcher, not the screen, and A and B share it.
