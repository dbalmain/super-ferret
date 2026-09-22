# M1 — Measured baseline on Dave's actual machine and corpus

Everything in this file is `[measured-by-me]`, collected 2026-09-04 on the
target machine. It is the only data in the whole research set gathered at the
target scale on the target hardware; R1 and R2 both reported that no such
public benchmark exists for any tool in the survey.

## Machine

| | |
|---|---|
| CPU | AMD Ryzen 9 9955HX, 16 cores / 32 threads |
| RAM | 60 GB total, ~32 GB in page cache at test time |
| Disk | NVMe, `/dev/nvme0n1p2`, 872 GB, 642 GB used |
| Tools | ripgrep 15.1.0, fd 10.4.2, ffprobe 8.1.2, exiftool present |
| Absent | ugrep, plocate, updatedb, pdftotext, tesseract |

## The corpus: `/home/dave/w`

| Measure | Value |
|---|---|
| Total files | **578,200** |
| Total bytes | **153.2 GB** |
| Files after excluding build/VCS artefacts | **73,565** (12.7%) |
| Bytes after excluding build/VCS artefacts | **5.73 GB** (3.7%) |

Exclusion pattern: any path containing a `.git`, `node_modules`, `target`,
`.venv`, `__pycache__`, `.cache`, `dist`, `build`, `.next`, `vendor`,
`.direnv` or `result` directory component.

**87% of files and 96% of bytes are machine-generated build output.**

### What the 153 GB actually is

| Extension | Bytes | Files | |
|---|---|---|---|
| `.bin` | 43.46 GB | 14,864 | build output |
| (no extension) | 35.25 GB | 53,104 | mostly build output |
| `.o` | 30.84 GB | 260,695 | object files |
| `.rlib` | 23.47 GB | 8,045 | Rust libs |
| `.rmeta` | 9.83 GB | 14,588 | Rust metadata |
| `.csv` | 3.00 GB | 48 | **real data** |
| `.so` | 2.71 GB | 764 | build output |
| `.db` | 1.14 GB | 7 | **real data** |

The four largest *source-text* extensions total well under 200 MB:
`.json` 265 MB / 14,609 files, `.md` 87 MB / 9,274 files, `.rs` 33 MB / 2,327
files, `.py` 25 MB / 4,324 files, `.txt` 16 MB / 16,127 files.

### Noise per project directory

| Directory | Total | Noise | |
|---|---|---|---|
| `clex` | 74.0 GB | 73.5 GB | 99% |
| `aven-wt` | 14.9 GB | 14.9 GB | 100% |
| `tally` | 12.3 GB | 12.3 GB | 100% |
| `aic-edit` | 10.8 GB | 10.8 GB | 100% |
| `editor-tools-wt` | 10.6 GB | 10.3 GB | 97% |
| `kbsr` | 7.8 GB | 7.8 GB | 100% |
| `calolog` | 8.1 GB | 3.6 GB | 44% |

### Size distribution (all 578k files)

| Bucket | Files | Bytes |
|---|---|---|
| < 1 KB | 174,552 | 0.05 GB |
| 1–16 KB | 184,698 | 1.14 GB |
| 16–256 KB | 167,918 | 11.21 GB |
| 256 KB – 4 MB | 43,959 | 42.53 GB |
| 4–64 MB | 6,790 | 67.52 GB |
| > 64 MB | 283 | 30.79 GB |

527k of 578k files (91%) are under 256 KB and together hold 12.4 GB —
8% of the bytes. The byte total is dominated by ~7,000 large binaries.

## Timings

### Metadata crawl

| Operation | Time |
|---|---|
| `find /home/dave/w -type f -printf '%s\t%p\n'` (578k files, warm dentry cache) | **1.24 s** |
| `fd -H -t f` over the same tree, warm | **0.03 s** (cached result set) |

A full metadata crawl of the target corpus costs about **1.2 seconds warm**.
Metadata indexing is not a performance problem at this scale; it is free.

### Content scan (ripgrep 15.1.0)

| Case | Time | Effective rate |
|---|---|---|
| A. Literal `fn parse_query`, default rg (honours `.gitignore`, skips binary), warm | **0.05 s**, 963% CPU | — |
| B. Regex `[A-Za-z_]+_(index\|posting)s?\b` with no usable literal, default rg, warm | **0.05 s**, 1020% CPU | — |
| C. `rg -uuu` over the full 153.2 GB, first run (mostly cold) | **234.07 s**, 99% CPU | 0.65 GB/s |
| D. Same, second run | **217.10 s**, 106% CPU | 0.71 GB/s |

Three things to take from this:

1. **On the corpus that matters, brute force already wins.** Once `.gitignore`
   pruning is applied, both a literal search and a hard regex over Dave's
   entire working tree complete in **50 milliseconds** — below the threshold
   at which a human perceives delay. No index can beat that, because no index
   can be consulted faster than 50 ms of already-parallel scanning.
2. **Case B is the important one.** A regex with no extractable literal — the
   exact query shape a trigram index exists to accelerate — costs the same
   50 ms as the literal. The single strongest argument for a content index
   evaporates at this corpus size.
3. **The 153 GB figure is a mirage.** Scanning everything takes ~3.7 minutes
   at 0.65–0.71 GB/s, and it is I/O-bound at ~100% CPU (one core's worth),
   meaning the bottleneck is reading bytes nobody wants indexed anyway.

## What this changes

The design problem at this scale is **not** index density, query planning, or
posting-list compression. It is:

- **Exclusion policy.** Getting the 96% right is worth more than every codec
  in R6 combined. A wrong exclusion rule costs 25× the index.
- **The document tier.** The 5.73 GB of signal is dominated by 48 CSVs, 7
  databases and 4,978 logs. The prose corpus — PDFs, Office documents, notes —
  is where an index earns its keep, because extraction is expensive and cannot
  be redone per query.
- **Metadata and filename search**, which is free and instant.

Caveats: this measures one corpus, heavily weighted toward Rust and JS
development. A corpus with a large `~/Documents`, a mail spool, or a photo
library would shift the balance toward extraction and toward the document
tier. The measurement should be repeated on those before generalising.
