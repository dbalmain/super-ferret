# R8 — Content Extraction Pipeline + Filesystem Change Detection

**Scope note (per Dave's constraints, folded in after the first pass):** this is
scoped to a personal workstation — roughly 1M files, tens to a few hundred GB,
comfortably under 5M files. TB-scale/NAS fan-out is explicitly out of scope.
Ship tier 1 (source code + text/config), tier 2 (PDF/Office) and tier 5 (media
metadata) built into the core indexer; email/chat, OCR, and eventually
reverse-image and speech-to-text are **plugins**, not core — which changes the
right architecture question from "can we OCR 1M files" to "what's the plugin
interface, and how does an opt-in OCR-of-one-subdirectory job get scheduled so
it never competes with tier-1 indexing." Both additions below (plugin interface,
indexer politeness) were requested explicitly as first-class, not asides,
because Dave has built a search engine before (Ferret, the Ruby Lucene port) and
background-daemon resource hunger is the historically-proven reason these tools
get uninstalled (Beagle) or permanently distrusted (Baloo).

Scope: what actually gets text out of a heterogeneous 200GB/1M-file corpus, and
how a Linux desktop indexer learns that a file changed without re-crawling
everything. These two subsystems are unglamorous and they are exactly where such
projects die — either the extractor segfaults on a hostile PDF and takes the
indexer with it, or the watcher silently drops events on a big tree and the
index goes stale without anyone noticing.

Environment this was researched on: kernel `6.18.43` [measured-by-me:
`uname -r`], NixOS-style profile at `~/.nix-profile`. Installed at research
time: `exiftool`, `ffprobe`/`ffmpeg`, `rg`, `fd`. **Not installed**:
`pdftotext`/`pdfinfo` (poppler-utils), `tesseract`, `mediainfo`,
`antiword`/`catdoc`/`wvWare`, `libreoffice`/`soffice`,
`calibre`/`ebook-convert`, `notmuch`, `whisper.cpp`, `mutool`, `qpdf`,
`ocrmypdf`, `locate`/`updatedb` [measured-by-me: `which <tool>` for each,
2026-09-04]. This matters for the report only insofar as it means none of the throughput
numbers below could be measured live on this machine's own binaries — they are cited
to upstream sources instead, and are labelled accordingly. One thing _was_ measured
live: a cold-ish `find` walk (see Part B).

---

## Part A — Content extraction

### A1. Format coverage matrix

| Format family                         | Best-in-class tool(s)                                                                                     | Licence                                                                                                                                                                | Lang                  | Lib vs subprocess               | Robustness note                                                                                                                                                                                            |
| ------------------------------------- | --------------------------------------------------------------------------------------------------------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------- | --------------------- | ------------------------------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| PDF text                              | `poppler`/`pdftotext`                                                                                     | GPLv2                                                                                                                                                                  | C++                   | either (has CLI + `libpoppler`) | mature, big CVE history (see A2)                                                                                                                                                                           |
| PDF text (fast/robust)                | **MuPDF** (`mutool`)                                                                                      | AGPL / commercial                                                                                                                                                      | C                     | either                          | generally more format-tolerant than poppler on malformed files; AGPL is a real licensing decision for a shipped product                                                                                    |
| PDF text (Rust-native)                | **pdfium-render** (bindings to Google's pdfium)                                                           | Apache-2.0/BSD (pdfium) + wrapper licence                                                                                                                              | Rust binding over C++ | library (loads `libpdfium.so`)  | pdfium is what Chrome renders PDFs with — very battle-tested against hostile input, actively fuzzed by Google                                                                                              |
| PDF text (pure Rust)                  | `pdf-extract`, `lopdf`                                                                                    | MIT                                                                                                                                                                    | Rust                  | library                         | pure-Rust removes a native-code attack surface entirely, but text-extraction fidelity (ligatures, CID fonts, layout) lags poppler/MuPDF noticeably                                                         |
| PDF (Java, kitchen sink)              | Apache PDFBox                                                                                             | Apache-2.0                                                                                                                                                             | Java                  | library                         | used inside Tika; JVM cost applies                                                                                                                                                                         |
| Scanned PDF → text                    | **OCRmyPDF** (wraps Tesseract)                                                                            | MPL-2.0                                                                                                                                                                | Python+shell          | subprocess pipeline             | adds a searchable text layer to a scanned PDF; this _is_ the OCR path for PDFs, not a separate concern                                                                                                     |
| OOXML (docx/xlsx/pptx)                | direct zip+XML parse                                                                                      | —                                                                                                                                                                      | any                   | library                         | OOXML is just a zip of XML; parsing it directly (no LibreOffice) is fast and low-risk if you only need text, not layout                                                                                    |
| OOXML (Rust)                          | `docx-rs`, `calamine` (xlsx/ods/xls read), `zip`                                                          | MIT/Apache-2.0                                                                                                                                                         | Rust                  | library                         | `calamine` is genuinely good for spreadsheets; no equally mature pure-Rust pptx text extractor as of this research — expect to hand-roll XML walking over the zip                                          |
| Legacy binary Office (.doc/.xls/.ppt) | `antiword`, `catdoc`, `wvWare`, or LibreOffice headless                                                   | varies (GPL mostly)                                                                                                                                                    | C                     | subprocess                      | binary OLE formats are the worst-documented format family here; LibreOffice headless is the only option with genuinely broad fidelity                                                                      |
| ODF                                   | direct zip+XML (same shape as OOXML) or LibreOffice                                                       | —                                                                                                                                                                      | any                   | library or subprocess           | straightforward, same zip-of-XML shape as OOXML                                                                                                                                                            |
| RTF                                   | `unrtf`, LibreOffice                                                                                      | GPL                                                                                                                                                                    | C                     | either                          | RTF's control-word grammar is idiosyncratic; a purpose-built RTF library is worth it over regex-stripping                                                                                                  |
| Catch-all                             | **Apache Tika** (server or `tika-python`)                                                                 | Apache-2.0                                                                                                                                                             | Java                  | subprocess (JVM server)         | handles 1000+ formats via parser detection; costs a persistent JVM (~200-500MB heap) and network/pipe hop per file; good as a fallback tier, bad as the primary path for a fast native indexer             |
| Ebooks                                | `calibre`'s `ebook-convert`                                                                               | GPLv3                                                                                                                                                                  | Python/C++            | subprocess                      | de facto standard for EPUB/MOBI/AZW/FB2 conversion; heavyweight dependency (pulls in a GUI toolkit) for a headless indexer, but nothing else covers the format spread as well                              |
| DjVu                                  | `djvutxt` (djvulibre)                                                                                     | GPLv2                                                                                                                                                                  | C++                   | subprocess                      | niche but easy — DjVu often embeds an OCR text layer already                                                                                                                                               |
| CHM                                   | `archmage`, `7z x` + HTML extraction                                                                      | varies                                                                                                                                                                 | Python/C              | subprocess                      | CHM is effectively a compressed HTML help archive; treat as archive+HTML                                                                                                                                   |
| mbox/Maildir/.eml                     | any MIME library (Rust: `mail-parser`, `mailparse`)                                                       | MIT/Apache                                                                                                                                                             | Rust                  | library                         | prefer library-based MIME parsing; mbox/Maildir are just file layout conventions on top of RFC 822/2045 messages                                                                                           |
| PST/OST                               | `readpst` (libpff)                                                                                        | GPLv2                                                                                                                                                                  | C                     | subprocess                      | proprietary Microsoft format, reverse-engineered; readpst is the standard tool, fidelity is good but not perfect on very old PST versions                                                                  |
| Slack/Discord/Signal exports          | bespoke JSON parsers                                                                                      | —                                                                                                                                                                      | any                   | library                         | these are just JSON exports with per-product schemas; no shared tooling, budget custom parsers per product                                                                                                 |
| notmuch's approach                    | Maildir + **Xapian** index, MIME parsed via GMime                                                         | GPLv3                                                                                                                                                                  | C                     | library                         | see note below                                                                                                                                                                                             |
| Archives (zip/tar/7z/rar)             | `libarchive`, Rust: `zip`, `tar`, `sevenz-rust`                                                           | BSD (libarchive)                                                                                                                                                       | C or Rust             | library                         | see nested-extraction discussion in A2                                                                                                                                                                     |
| Source code                           | plain UTF-8/detected-encoding text                                                                        | —                                                                                                                                                                      | —                     | —                               | see structure-aware note below                                                                                                                                                                             |
| Symbol extraction                     | **tree-sitter** (grammars per language), ctags, GNU Global                                                | MIT (tree-sitter)                                                                                                                                                      | C/Rust bindings       | library                         | see A2 opinion                                                                                                                                                                                             |
| Media metadata                        | `exiftool`, `ffprobe`, `mediainfo`                                                                        | Artistic/GPL, LGPL, BSD-2                                                                                                                                              | Perl, C, C++          | subprocess (all three)          | `exiftool` is the widest-coverage metadata tool that exists (EXIF/XMP/IPTC/maker-notes across hundreds of formats); nothing else comes close for breadth                                                   |
| Media metadata (Rust)                 | `kamadak-exif`, `lofty` (audio tags), `symphonia` (audio decode+some tags)                                | MIT/Apache                                                                                                                                                             | Rust                  | library                         | good for the common cases (JPEG EXIF, MP3/FLAC/etc ID3/Vorbis tags); fall back to `exiftool` subprocess for the long tail (RAW camera formats, video container metadata)                                   |
| Subtitles                             | `.srt`/`.vtt` are plain text, trivial to parse                                                            | —                                                                                                                                                                      | —                     | —                               | —                                                                                                                                                                                                          |
| OCR                                   | **Tesseract**, **PaddleOCR**, **Surya**, **docTR**, VLM-based (e.g. Qwen2-VL/InternVL-style document OCR) | Apache-2.0 (Tesseract), Apache-2.0 (PaddleOCR), GPL-3.0 (Surya, non-commercial-ish licensing caveats apply — check current Surya licence before shipping), MIT (docTR) | C++/Python mostly     | subprocess or Python-process    | see throughput discussion below — this is the single biggest feasibility question in the whole subsystem                                                                                                   |
| ASR                                   | **whisper.cpp** (C/C++, no Python dep), **faster-whisper** (CTranslate2-backed Python)                    | MIT                                                                                                                                                                    | C++ / Python          | library or subprocess           | whisper.cpp is the natural fit for a Rust project (link as a C library or shell out) since it has zero Python/PyTorch dependency                                                                           |
| HTML                                  | `lol_html` (Rust, streaming rewriter/scraper), `html2text`, Mozilla `readability` (via subprocess/port)   | BSD-3 (lol_html), MIT (html2text)                                                                                                                                      | Rust, Rust            | library                         | `lol_html` is Cloudflare's streaming HTML rewriter — fast, low-memory, good fit for a Rust pipeline; pure text-extraction quality still needs a readability-style boilerplate stripper for saved web pages |
| Markdown                              | any CommonMark parser (`pulldown-cmark` in Rust)                                                          | MIT                                                                                                                                                                    | Rust                  | library                         | trivial                                                                                                                                                                                                    |
| LaTeX                                 | strip macros with `pandoc` or `detex`-style regex, or `pandoc` full conversion                            | GPL (pandoc)                                                                                                                                                           | Haskell               | subprocess                      | LaTeX parsing to _plain text_ is a rabbit hole; most desktop-search use cases only need macro-stripped fallback text, not real TeX semantics                                                               |
| Jupyter `.ipynb`                      | JSON parse, extract markdown+code cells, decode base64 outputs if needed                                  | —                                                                                                                                                                      | any                   | library                         | `.ipynb` is JSON; treat cell sources as source code, markdown cells as prose                                                                                                                               |
| Org-mode                              | plain text with light markup; a dedicated org parser is optional                                          | —                                                                                                                                                                      | any                   | library                         | mostly fine to index as plain text                                                                                                                                                                         |

**Licence ledger — why this matters for the default build.** Dave's stated
preference is permissive (MIT/Apache/BSD) for anything linked into the core
binary. Reading the table above straight through: `poppler` is **GPLv2**,
LibreOffice is **MPL-2.0** (permissive-ish but still copyleft-on-file), MuPDF is
**AGPL-or-commercial** (the most restrictive of the lot — AGPL's network-
copyleft clause is a real concern even for a desktop app if it ever gains a
sync/server component), Tesseract is **Apache-2.0** (clean), pdfium is
**BSD-3/Apache-2.0** (clean — Google dual-licenses it), calibre is **GPLv3**,
notmuch is **GPLv3**, readpst/libpff is **GPLv2**. **The honest state of the
world is that the best-in-class PDF and legacy-Office parsers are all copyleft
or worse (GPL/AGPL), and the only permissively-licensed alternatives (pdfium,
pure-Rust `lopdf`/`pdf-extract`) trade away either build complexity (pdfium
requires vendoring/building Google's prebuilt shared library, which is not a
small dependency) or extraction fidelity (pure Rust PDF text extraction lags on
ligatures/CID fonts per A1).** This is exactly the argument for the
subprocess/plugin boundary in the architecture section below: static-linking
GPL/AGPL code into an MIT/Apache-licensed core binary is the licensing mistake
to avoid, but **invoking a GPL binary as an arm's-length subprocess (calling
`pdftotext` the same way a shell script would) does not propagate the GPL to the
caller** — this is the standard, widely-relied-upon interpretation (the FSF's
own GPL FAQ treats "mere aggregation"/pipe-based invocation of a separate
program differently from linking:
[GNU GPL FAQ, "if a program calls another program via pipe/exec is it a derivative work"](https://www.gnu.org/licenses/gpl-faq.html)
— the FAQ's `GPLAndPlugins` and `MereAggregation` entries specifically
distinguish linking from process invocation). Practical recommendation:
**pdfium-render (BSD) as the default in-process PDF path**,
`poppler`/`pdftotext` as a subprocess fallback for the fidelity cases pdfium
struggles with, MuPDF avoided entirely as a default given AGPL, and
LibreOffice/calibre/readpst kept strictly at subprocess arm's length (never
linked) precisely because they're GPL/GPLv3/MPL. This ledger is also the single
strongest argument for making OCR, email/chat, and eventually reverse-image/ASR
**plugins** rather than core: it keeps every copyleft dependency (Tesseract is
the exception — Apache-2.0 — but PaddleOCR/Surya's licensing is murkier, calibre
and readpst are GPL) out of the core binary's link graph entirely, with the
plugin boundary doing double duty as both a sandboxing boundary (A2) and a
licensing boundary.

### PDF hard cases (detail)

- **Scanned PDFs.** No embedded text layer at all, or a garbage text layer from
  a bad prior OCR pass. Detection heuristic: extract text, if character count
  per page is near zero (or the extracted text is mostly control characters /
  one giant unbroken run with no spaces) treat as image-only and route to OCR.
  `OCRmyPDF` is the standard tool that does exactly this detection-and-fallback
  and burns the result back into the PDF as a hidden text layer — reusing it
  avoids reinventing the "does this PDF have real text" heuristic
  ([OCRmyPDF docs](https://ocrmypdf.readthedocs.io/)).
- **Ligature/CID font mangling.** PDFs frequently encode text with a font's
  internal glyph IDs (CID-keyed CJK fonts, subsetted Type 3 fonts, ligature
  glyphs like "ﬁ") rather than Unicode code points, and rely on a `ToUnicode`
  CMap to map back — when that CMap is missing, wrong, or the font is a Type 3
  with no CMap at all, text extraction produces garbage or nothing. This is one
  of the most common "PDF extraction returns mojibake" complaints and there is
  no general fix short of OCR as a fallback when extracted text fails a sanity
  check (non-printable ratio, dictionary-word ratio).
- **Columnar layout / reading order.** PDF has no semantic notion of "column" —
  it is absolute-positioned glyphs. `pdftotext -layout` attempts geometric
  reconstruction; it is heuristic and gets multi-column academic papers and
  newspaper-style layouts wrong often enough that reading order should be
  treated as "approximately right," not authoritative, especially for snippet
  generation.
- **Encrypted PDFs.** Owner-password-only encryption (permissions restriction
  with no user password) is legally and technically extractable — poppler and
  MuPDF both ignore the permission bits and decrypt when there's no user
  password required, since the encryption key is still derivable. True
  user-password encryption cannot be extracted without the password; the correct
  behavior is to index filename/metadata only and flag the file, not to attempt
  cracking.
- **Malformed files.** PDF has no strict validator most producers respect;
  poppler and MuPDF both contain extensive recovery logic for missing `xref`
  tables, truncated files, and out-of-spec object streams. This recovery logic
  is itself the historical source of most CVEs (see A2).

### Office: OOXML direct-parse vs LibreOffice headless

Direct XML parsing (docx/xlsx/pptx are zip archives of well-documented XML) is
the right default for a fast indexer: no subprocess, no JVM, no LibreOffice
startup cost (LibreOffice headless conversion is commonly reported in the
several-hundred-ms-to-multi-second range per document due to process startup and
font/config loading — this is a per-file tax that dominates for anything past
small collections). LibreOffice headless remains necessary for: (a) legacy
binary `.doc`/`.xls`/`.ppt` where no lightweight Rust/C parser exists with
acceptable fidelity, (b) any format where you need rendered/converted output
rather than raw text, (c) RTF and ODF edge cases direct parsing mishandles.
Practical shape: try direct parse first for OOXML/ODF; fall back to a
LibreOffice-headless _pool_ of long-lived worker processes (start once, convert
many, to amortize the startup cost) for everything direct parsing can't handle
or fails sanity-checking.

Apache Tika sits above all of this as a universal fallback (1000+ formats via
Tika's own detection+parser dispatch), at the cost of running a JVM — either as
a long-lived `tika-server` (recommended: amortizes JVM startup, adds an
HTTP/pipe hop per file) or spawned per-file (do not do this; JVM cold start is
commonly several hundred ms to >1s). For a project explicitly targeting a fast
native Rust indexer, Tika is best used as the tier-4 fallback for the long tail
of formats nothing else handles, not as the primary path.

### Email/chat detail

- **notmuch's approach**: notmuch stores mail unmodified in Maildir, parses MIME
  with GMime, and indexes into **Xapian** (a C++ search-library, not Sqlite-FTS
  or a custom engine). What it does well and is worth stealing: it treats mail
  as immutable on-disk files and keeps _all_ index state (tags, threading) in
  the Xapian database rather than mutating the mail files, so re-indexing after
  external mail-file changes is a diff against ground truth, not a merge. Its
  threading uses `Message-Id`/`References`/`In-Reply-To` headers plus subject
  normalization heuristics for mail without proper threading headers — thread
  reconstruction from headers alone is unreliable for real-world corpora
  (mangled References headers are common) and needs a fallback subject+time
  heuristic, exactly as notmuch does
  ([notmuch design docs](https://notmuchmail.org/design/)).
- **MIME pitfalls**: nested multipart/alternative (plain+HTML dupes of the same
  content — index once, prefer plain-text part if present and non-trivial, else
  HTML-to-text the HTML part), multipart/related (inline images referenced by
  `cid:` — usually skip for text extraction but index filenames),
  base64/quoted-printable decoding, charset declared in headers frequently wrong
  or absent (fall back to content-sniffing), and attachments that are themselves
  any format in this matrix (recursive extraction, see archives).
- **PST/OST**: `readpst` (from `libpff`) converts to mbox/EML; this is a
  reverse-engineered proprietary format so expect occasional corruption on very
  old Outlook PST versions and encrypted/compressible-encryption PSTs.
- **HTML-to-text for chat/email bodies**: same tool choice as the HTML row above
  (`lol_html` or a readability-style stripper) — chat exports (Slack, Discord)
  are JSON with message bodies in markdown-like dialects specific to each
  product, not HTML, so these need bespoke per-product markup stripping, not a
  generic HTML-to-text pass.

### Archives: nested extraction, zip bombs, recursion depth

Archive formats necessarily recurse (a `.tar.gz` inside a `.docx`'s embedded
object, a `.zip` inside an email attachment inside an `.mbox`). Two failure
modes must be bounded explicitly, because default library behavior does not
bound them:

1. **Zip bombs** — a small compressed file expanding to gigabytes/terabytes (the
   classic `42.zip` expands 42KB to 4.5PB through nested layers). Any extraction
   pipeline touching untrusted archives must enforce a **decompressed byte
   budget** (track cumulative bytes written per top-level file, abort past a cap
   — e.g. 10-20x the compressed size or an absolute cap like 2GB) and a
   **compression-ratio check** before fully decompressing (libarchive and most
   zip libraries expose compressed vs uncompressed size from the central
   directory before extraction, letting you reject absurd ratios up front).
2. **Recursion depth** — archive-in-archive-in-archive nesting must have a hard
   depth limit (3-5 is reasonable for real-world corpora; legitimate mail
   attachments rarely nest more than 2 deep) enforced independently of the byte
   budget, since a bomb can also be built from many shallow-ratio layers.

Neither of these is exotic engineering, but they are exactly the kind of "we'll
add it later" corner that turns into an actual outage when a user's corpus
contains one hostile or merely test/fixture zip bomb file.

### Source code: is symbol-aware indexing worth it?

**Opinion: yes, but as a second-pass enrichment, not the primary extraction
path.** Plain-text indexing of source code (tokenized appropriately — see the
tokenizer/dedup considerations from other slices of this report) already gets
you correct full-text and regex search over code, which is most of the value.
**tree-sitter** is worth adding on top because: (a) it has grammars for
essentially every mainstream language, (b) it is incremental and fast enough to
run per-file at index time without materially changing the cost model (parsing
is typically single-digit milliseconds per file for source-sized inputs), and
(c) symbol extraction (function/class/type definitions) lets you offer "go to
definition"-style structured search and rank symbol-name matches above
comment/string matches — a genuine desktop-search differentiator versus grep.
**ctags/GNU Global** are the older alternative; they're lower effort to shell
out to but tree-sitter's queries are more precise and, being a library, avoid a
subprocess per file. Do not build a full semantic/type-resolving index (that's
an LSP's job, not a desktop search engine's); symbol _names and kinds_ from a
syntax tree is the right stopping point.

### Media metadata

`exiftool` is the correct default for breadth — it reads/writes an enormous
range of maker-specific metadata across image, video, audio, and even some
document formats. `ffprobe` is the correct choice specifically for AV
container/stream metadata (codecs, duration, resolution, embedded
chapter/subtitle tracks) and is dramatically faster to invoke for that narrower
job. For a Rust pipeline: prefer in-process `kamadak-exif` (EXIF) and `lofty`
(audio tags) for the common formats to avoid a subprocess per file on the bulk
of the corpus, and fall back to `exiftool`/`ffprobe` subprocess calls only for
formats those crates don't cover (RAW camera formats, exotic video containers,
XMP sidecar edge cases). Embedded `.srt`/`.vtt` subtitle _tracks_ inside video
containers need `ffmpeg -map` extraction (a subprocess call) since no Rust
demuxer crate extracts subtitle streams as conveniently as ffmpeg's CLI does.

### OCR: throughput is the load-bearing number for this entire report

This is flagged as the most consequential number in Part A because it directly
determines whether OCR-tier indexing of a real corpus is feasible at all, or
needs to be opt-in/background/GPU-only.

- **Tesseract, single-threaded CPU**: ~2 seconds/page on a 12-core 4.3GHz
  desktop CPU for English text (~30 pages/minute single-threaded), rising to ~17
  seconds/page for Arabic (script complexity matters a lot)
  [third-party-benchmark, tesseract-ocr mailing
  list, https://groups.google.com/g/tesseract-ocr/c/5CSIYkba5Dc]. archive.org
  reports an average of ~7.5 seconds/page across their real-world mixed-quality
  scan corpus [third-party-benchmark, https://tesseract-ocr.github.io/tessdoc/Benchmarks.html].
- **Tesseract, parallelized CPU**: ~175 pages/minute (~2.9 pages/sec) on a
  16-core AWS `c5d.4xlarge` for 300dpi scans — reported as ~10x the sequential
  rate [third-party-benchmark; exact source page not independently re-verified
  beyond the search summary, treat as a rough anchor rather than a
  precise figure].
- **EasyOCR** (a common Tesseract alternative) is reported at roughly 2-3x
  _slower_ than Tesseract on CPU, ~8 pages/minute [third-party-benchmark]. This
  report doesn't have a directly comparable PaddleOCR/Surya/docTR CPU-throughput
  figure from a primary source — treat any number for those as **unverified**
  until measured; they are architecturally heavier (deep-learning
  detection+recognition pipelines rather than Tesseract's classical
  LSTM-per-line approach) and should be assumed slower per-page on CPU, faster
  with a GPU, not verified here.
- **VLM-based OCR** (2024-2026 document-understanding vision-language models) is
  qualitatively better on hard layouts (tables, forms, mixed scripts) but is
  orders of magnitude more expensive per page than Tesseract on CPU — these are
  billion-parameter models; running one per scanned page across a 1M-file corpus
  on CPU is not a realistic desktop workload. GPU-accelerated batched inference
  narrows this considerably but this report has no verified pages/sec figure for
  a specific model and should not present one.

**Honest bottom line**: Tesseract CPU OCR at roughly 1-3 pages/second (parallel,
multi-core) is the only OCR throughput that is remotely compatible with indexing
a large personal document corpus without a GPU and without OCR becoming an
open-ended background job that never finishes. Budget accordingly in the cost
model below — OCR is the tier where wall-clock estimates swing by an order of
magnitude depending on hardware, and it should be an explicit opt-in tier gated
behind "how many scanned pages does this user actually have," not something that
runs unconditionally on every PDF at index time.

### ASR (audio/video transcription)

- **whisper.cpp** is the right fit for a Rust project: pure C/C++, no
  PyTorch/Python runtime dependency, links as a library or shells out as a
  binary. GPU (CUDA) real-time factor for the large-v3 model is reported around
  8x real-time on an RTX 4070 [third-party-benchmark,
  https://github.com/ggml-org/whisper.cpp]. Apple-Silicon-with-Metal figures (~10x
  on M5 Pro, ~2.2x on M2, i.e. RTF 0.45) are reported but are not Linux-CPU numbers
  and should not be extrapolated to a Linux x86 CPU-only box [third-party-benchmark].
  **No verified Linux-x86-CPU-only RTF figure for large-v3 was found in this research
  pass** — treat CPU-only ASR at large-v3 quality as likely sub-real-time (RTF >
  1, i.e. slower than the audio's own duration) based on the GPU-vs-CPU gap implied
  by the above, and verify before committing to a cost estimate. Smaller models (`base`,
  `small`) trade accuracy for CPU-feasible real-time or faster throughput and are
  the pragmatic choice if ASR is offered at all for a bulk desktop-search feature
  rather than a dedicated transcription tool.
- **faster-whisper** (CTranslate2 backend) is the higher-throughput Python
  option when a Python dependency is acceptable; whisper.cpp remains preferred
  here specifically because a Rust indexer would rather not embed a Python
  runtime.

### A2. Architecture questions

**The extractor plugin interface — this is the design decision that matters most
in Part A.** Given tier 1/2/5 are core and everything else (email/chat, OCR,
later reverse-image and ASR) ships as a plugin added after the fact, the
interface has to be settled before any plugin is written, or every plugin
becomes a one-off integration. Four real shapes, in the order a Rust host should
consider them:

1. **Dynamic libraries behind a stable C ABI** (`dlopen` a `.so` exposing a
   `extern "C"` vtable — think GStreamer plugins, or PostgreSQL's extension
   ABI). _Pros_: fastest possible call path, no serialization, plugin can share
   the host's memory-mapped file if useful. _Cons_: this is exactly the
   crash/CVE blast-radius problem from the sandboxing discussion below turned up
   to maximum — a segfault in a `dlopen`ed plugin **is** a segfault in your
   indexer process, full stop, no isolation at all. It also locks the plugin to
   Rust's (or C's) ABI stability story, which for Rust specifically is unstable
   across compiler versions unless you go through `cbindgen`/a hand-frozen C API
   — real but fiddly engineering overhead. **Rule this out for anything that
   touches untrusted file content** (which is all of tier 2+ and certainly
   OCR/email); it's the right shape only for a plugin you trust as much as your
   own code, which in this project's threat model is basically nothing outside
   the core.
2. **Subprocess with a line- or protobuf-framed protocol on stdio.** The host
   spawns the plugin binary once (or per-batch, not per-file, to amortize spawn
   cost — see below), writes requests (file path or an fd passed via
   `SCM_RIGHTS`, plus any config) on stdin, reads back extracted text/metadata
   as length-prefixed protobuf or even newline-delimited JSON on stdout, and the
   crash/hang/memory-cap machinery from A2's isolation section applies directly
   and cheaply (kill the child, mark the file poisoned, move on). This is
   essentially **Recoll's `mimeview`/filter architecture**, generalized with a
   real wire format instead of "print text to stdout and hope." _Pros_: total
   memory/crash isolation for free, plugin can be written in _any_ language (a
   Python OCR plugin, a Go email-parser plugin — the interface doesn't care),
   trivially versioned (protobuf schema evolution is a solved problem), and
   composes naturally with the `bubblewrap`/uid-separation sandboxing from the
   isolation section — the subprocess boundary IS the sandbox boundary, no extra
   abstraction needed. _Cons_: per-call IPC overhead (real but small relative to
   parse times already in the tens-to-hundreds-of-ms range for tier 2+), and
   passing large files means either a shared temp path or `SCM_RIGHTS`
   fd-passing (not hard, but one more thing to get right).
3. **WASM/WASI components.** Genuinely the most interesting option because it
   collapses two separate problems (sandboxing, and cross-platform/cross-
   language plugin portability) into one mechanism: a WASI component is
   sandboxed by construction (no ambient filesystem/network access unless
   explicitly granted via WASI's capability-based preopens — this is _stronger_
   isolation than a bare subprocess without `bubblewrap`, and gets it without
   needing root or `landlock`), and the component model gives you a typed
   interface (WIT) across languages without hand-rolling a wire protocol. **But
   the throughput penalty is real and must be quantified, not waved away**,
   since it's the obvious objection: PolyBench/C-style micro-benchmarks put
   general WASM at roughly **1.3x slower than native** compute-bound code
   [third-party-benchmark, https://arxiv.org/pdf/1901.09056], which is a tolerable
   tax for CPU-bound parsing work (regex, text layout reconstruction, tokenization).
   The actual danger zone is **I/O-heavy code**, not compute: one WASI file-I/O benchmark
   found a simple write-heavy workload **10x slower** in Wasmtime than native, traced
   to Wasmtime's WASI implementation routing even synchronous writes through an async
   Tokio engine, tripling syscall count versus the native equivalent [third-party-benchmark,
   https://eunomia.dev/blog/2025/02/16/wasi-and-the-webassembly-component-model-current-status/].
   For document parsing — read the whole file once, do CPU-bound work, write
   text out once — this I/O penalty is largely avoidable by structuring the
   host/component boundary to pass the file content as an in-memory buffer (one
   big host-to-guest copy) rather than having the guest do its own
   syscall-per-read against a WASI preopen. Ecosystem maturity is the other real
   cost: **very few of the mature C/C++ parsers this report recommends (poppler,
   MuPDF, LibreOffice, Tesseract) have production-ready `wasm32-wasi` builds
   today** — compiling them yourself is a genuine project, not a flag flip.
   Pure-Rust extractors (`lopdf`, `pdf-extract`, `calamine`, `lofty`) compile to
   `wasm32-wasi`/`wasm32-wasip2` easily since they're already memory-safe,
   dependency-light Rust — meaning WASM components are realistic **today** only
   for the tier where you'd least need the sandboxing (pure-Rust libraries have
   no native-code attack surface to sandbox in the first place), and not yet
   realistic for the tier where sandboxing matters most
   (poppler/LibreOffice/Tesseract).
4. **Declarative "shell out to this binary" config** (Recoll's
   `mimeconf`/`mimeview` model: a config file mapping MIME type → command line
   template, e.g. `application/pdf = pdftotext %f -`). _Pros_: zero
   plugin-interface code at all, trivially extended by end users without
   recompiling anything (drop a line in a config file), and it's proven — this
   is what Recoll has shipped for two decades. _Cons_: no structured output
   beyond plain text on stdout (no metadata, no error codes beyond exit status,
   no way for the plugin to report "I found a rename" or anything richer than
   "here is some text"), and no natural place to put the
   batching/timeout/memory-cap machinery beyond what you bolt onto the
   process-spawning code yourself.

**Recommendation: subprocess-with-protobuf-on-stdio (option 2) as the plugin ABI
now, with the door deliberately left open to migrate specific already-pure-Rust
plugins to WASI components (option 3) later without changing the _interface
contract_ the host presents to plugin authors** — the host-side "send a request,
get back {text, metadata, error}" contract can be identical whether the
implementation behind it is a spawned process or a loaded WASI component, so
this isn't a fork in the road so much as a phasing decision. Reasoning: option 2
gets you the sandboxing and crash-isolation win _today_, works with the actual
best-in-class parsers as they exist right now (none of which have WASI builds),
keeps every GPL/AGPL dependency (per the licence ledger above) at arm's length
rather than linked, and is a shape Recoll has already proven works at this exact
scale and use case for twenty years. Revisit WASI specifically once (a) the
pure-Rust extractor tier has matured enough to not need poppler/MuPDF at all, or
(b) upstream poppler/Tesseract gain first-class WASI builds — neither is true
today, and committing to WASI as _the_ plugin ABI now would mean either shipping
without real PDF support or falling back to subprocess anyway for that tier,
defeating the point of picking one ABI.

**Subprocess-per-file vs in-process library.** Default to **in-process library
calls for trusted, well-scoped formats** (Rust crates: `calamine`, `lofty`,
`kamadak-exif`, `lol_html`, `zip`/`tar`) and **subprocess isolation for anything
parsing a large, historically-CVE-heavy format from untrusted input** — PDF via
poppler/MuPDF/pdfium, legacy Office via LibreOffice, PST via readpst, OCR via
Tesseract. The reasoning is entirely about blast radius: a memory-corruption bug
in an in-process PDF parser crashes (or, worse, compromises) the indexer process
that holds your whole index open; the same bug in a subprocess costs you one
worker restart. The subprocess overhead (process spawn, IPC) is genuinely cheap
relative to the multi-hundred-millisecond-to-second parse times these formats
already take — it's not the bottleneck.

**Parser sandboxing — this is a real attack surface, not theoretical.** poppler,
LibreOffice, and Tika (via the libraries it wraps — PDFBox, POI, etc.) all have
substantial CVE histories, including memory-corruption bugs reachable from a
crafted document with no user interaction beyond "let this file be indexed" —
exactly the desktop-search threat model. A non-exhaustive but representative
sample: poppler has had numerous heap-buffer-overflow and use-after-free CVEs in
its font/JBIG2/CCITT decoders over the years (search `CVE poppler` on the NVD
for the current list — the count is in the dozens and growing); LibreOffice has
had macro-execution and memory-safety CVEs in its document filters (e.g. RTF/DOC
parsing); Tika inherits every CVE in every library it embeds and has had its own
directory-traversal and XML-external-entity issues historically. Given this,
**an indexer that feeds arbitrary user files into these parsers must sandbox
them**:

- **Options, roughly ordered by isolation strength vs implementation cost**: a
  full container/namespace + seccomp-bpf sandbox (strongest, most engineering:
  separate mount/PID/network namespace, `seccomp` syscall filtering, `landlock`
  for filesystem-access confinement even without root)
  > `bubblewrap` (`bwrap`) as a lower-effort userspace wrapper achieving
  > namespace isolation without writing raw namespace/seccomp code yourself > a
  > dedicated unprivileged uid per worker (cheap, prevents cross-user file
  > access and simplifies `ulimit`/cgroup accounting, but doesn't stop the
  > process reading other files the uid can see) > WASM-compiled parsers (strong
  > sandboxing by construction, but very few of these mature C/C++ parsers have
  > production-ready WASM builds today — feasible for pure-Rust-first choices
  > like `lopdf`/`pdf-extract` compiled to `wasm32-wasi`, not realistic for
  > poppler/MuPDF/LibreOffice at this time).
- **What Recoll, Tracker, and sist2 actually do today**: Recoll shells out to
  external filter scripts/programs per MIME type (its long-standing
  `rclhelper`/filters architecture) — this gets you subprocess isolation "for
  free" as an architectural side effect, but it does **not** apply seccomp or
  namespace sandboxing on top; a malicious file exploiting poppler still runs as
  the invoking user with full filesystem access, just in a separate process that
  can be killed. Tracker (GNOME's indexer, `tracker-extract`) similarly runs
  extraction in a separate `tracker-extract` process from the main
  `tracker-miner`, and it is documented to apply **resource limits** (CPU time,
  memory) to extractor subprocesses and to blacklist files that repeatedly crash
  the extractor, but does not, by default, run a full seccomp/namespace jail per
  file. **sist2** (a modern Rust-adjacent — actually C — desktop-search- like
  indexer aimed at exactly this workload) similarly relies on process-per-file
  isolation with timeouts rather than deep sandboxing. **The honest finding here
  is that none of the mainstream Linux desktop indexers do full
  seccomp/namespace sandboxing of format parsers as of this research** — they
  all rely on process isolation + resource limits + crash quarantine as the
  practical middle ground, not on the strongest available primitives. This is
  worth calling out explicitly as a gap a new project could actually close
  (bubblewrap wrapping is not exotic engineering) rather than assuming prior art
  already solved it.

**Crash and hang isolation.** Independent of sandboxing for malice, extraction
workers need, unconditionally: a **wall-clock timeout per file** (kill and
quarantine on timeout — a hung regex or a pathological PDF can spin forever;
30-60s is a reasonable default with a longer allowance explicitly for the OCR
tier), a **memory cap per worker** (cgroup or `RLIMIT_AS`; OOM-killing a rogue
extraction should never take the parent indexer down with it — this is another
argument for subprocess-per-format-family rather than in-process), and a
**poison-file quarantine**: when a file crashes or times out its extractor N
times (2-3), record it in a "known bad" table with the failure signature (crash
signal, exit code) and stop retrying it every re-index cycle, surfacing it in
diagnostics instead. Without this, a single hostile or merely malformed file in
a user's corpus becomes a Sisyphean retry loop on every incremental re-index
forever.

**Format detection: extension vs libmagic vs content sniffing.** Extension-only
detection is wrong often enough to matter (misnamed files, extensionless files,
email attachments with generic names) but is nearly free and a fine first
filter. Content sniffing (magic-byte matching) is the correct primary mechanism;
the Rust ecosystem has two real options — **`infer`** (small, dependency-light,
checks magic bytes for a curated list of common formats, MIT licence) and
**`tree_magic`**/`tree_magic_mini` (ports of the freedesktop `shared-mime-info`
database, broader coverage including many text/XML-based formats that pure
magic-byte sniffing struggles with, since e.g. all OOXML formats share the same
zip magic bytes and need a peek inside the zip to distinguish docx/xlsx/pptx).
Recommended default: extension as a hint to order which detector to try first
(perf optimization, not correctness), `infer`/ `tree_magic_mini`
content-sniffing as the actual routing decision, and an explicit "peek inside
the zip" step for the OOXML/ODF family since magic bytes alone can't distinguish
them.

**Text normalisation vs regex-over-original-bytes — the real design trap.** This
deserves its own paragraph because it is genuinely underappreciated and will
bite whichever indexer ships without thinking about it up front. Every
extraction pipeline needs some normalisation to make full-text search usable at
all: encoding detection and conversion to a canonical internal representation
(`chardet`/`uchardet` heuristics, or Rust's `encoding_rs` which implements the
WHATWG encoding standard and is both fast and has none of `chardet`'s
probabilistic false-positive problems on short text), Unicode normalisation (NFC
vs NFD matters — "é" as one codepoint vs "e"+combining-accent must compare
equal, or search silently misses matches depending on which form a document
happened to use), dehyphenation (PDF line-wrap hyphens splitting
"infor-\nmation" across lines, which should probably rejoin for phrase search),
and whitespace collapse (multiple spaces/tabs/newlines from PDF layout
reconstruction). **The trap**: every one of these transformations changes byte
offsets relative to the source file, and a user who types a regex expects it to
match bytes _as they exist in the file_ — not a normalized, dehyphenated,
whitespace-collapsed proxy of it. If the index stores only normalized text and
serves regex search against that, results will (a) fail to explain why a regex
"worked" when opening the actual file shows different bytes at the reported
location, and (b) actively miss or spuriously match depending on which
normalisation happened to run. The two honest ways to resolve this: (1)
**maintain an offset map** from normalized-text positions back to original-byte
positions (expensive to build and keep correct through every normalisation step,
but lets you report byte-accurate match locations and even re-run the regex
against the raw slice for confirmation), or (2) **run two passes**: a fast
normalized/tokenized full-text index for ranked relevance search, plus a
separate, unmodified raw-byte grep-style index or on-demand raw scan for
regex/exact-match queries, and be explicit in the UI about which mode the user
is in. Recoll and most FTS-based tools quietly pick option (2) by degrading
regex to "grep the original file at query time" rather than solving the offset
problem — which is honest and also means regex queries pay the cost of reading
files from disk at query time rather than being index-accelerated. For a project
explicitly ambitious about matching ripgrep-quality regex semantics, this needs
a real decision, not a default; recommend deciding it explicitly (see the note
at the end of this section) rather than letting it default to "normalized text
only" and discovering the mismatch after users start filing "my regex should
have matched" bugs.

**Extraction cost model inputs (rough per-format seconds/GB or per-file).**
These are order-of-magnitude planning inputs, not benchmarks — every one is
labelled `[estimated]` unless otherwise cited, and is used only to build the
Part B cost-model arithmetic further down:

| Tier                              | Rough cost                                                                                                                                              | Basis                                                                                                                                                                                  |
| --------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Plain text / source code          | near-free; dominated by disk read + tokenization, sub-millisecond to low-single-digit ms per typical file                                               | [estimated] — this is essentially `read()` + UTF-8 validate + tokenize, no parsing                                                                                                     |
| PDF/Office text extraction        | tens to low-hundreds of ms per document for typical office-doc sizes (a few pages to a few dozen pages); scales with page count, not file size directly | [estimated], consistent with typical `pdftotext`/direct-XML-parse invocation costs reported anecdotally across many projects' issue trackers — no single authoritative benchmark found |
| Media metadata (exiftool/ffprobe) | tens of ms per file (metadata-only, not full decode)                                                                                                    | [estimated] based on typical CLI-tool startup + header-parse cost                                                                                                                      |
| OCR (Tesseract, CPU)              | ~0.3-2 pages/sec single-threaded, ~3 pages/sec parallel per the benchmarks cited above                                                                  | [third-party-benchmark], see OCR section                                                                                                                                               |
| ASR (whisper.cpp, GPU)            | ~8x real-time (i.e., 1 hour of audio in ~7.5 min) on a modern GPU; CPU-only likely sub-real-time, unverified                                            | [third-party-benchmark] for GPU figure, [estimated]+flagged-unverified for CPU                                                                                                         |

**Caching extracted text: worth it, and by how much.** Yes, unambiguously worth
it, for two independent reasons beyond avoiding re-extraction cost: (1)
**snippet generation** at query time needs the actual extracted text, not just
index postings — without a cache you must re-open and re-extract the source file
on every search result rendered, which for PDF/Office/OCR tiers is exactly the
expensive path you were trying to avoid; (2) **re-indexing after a schema or
tokenizer change** (a near-certainty over a project's life — new language
support, a stemmer bug fix, a ranking-function change) becomes a pure
text-processing pass over cached extracted text instead of a full re-extraction
pass, which is the difference between minutes and the OCR-tier wall-clock
numbers above. The size cost is real but bounded: extracted plain text is
typically far smaller than source documents (a 500KB PDF might yield 5-20KB of
text), so even at 1M files the extracted-text cache is plausibly in the
single-digit-GB range for the text/office tiers, growing meaningfully only if
OCR output (which can be verbose, page-by-page) is cached at similar
granularity. Store it compressed (zstd) alongside or inside the index; treat
cache invalidation on source-file-change exactly like the change-detection
problem in Part B — same mtime/hash-based staleness check should govern both
"needs re-extraction" and "needs re-indexing."

---

## Part B — Change detection on Linux

### inotify: limits, cost, and where it actually breaks

- **Watch-descriptor limits.** `fs.inotify.max_user_watches` (default
  historically 8192, raised to 65536 by many distros, and further raised in
  newer kernels — measured on this machine: **524288**) caps watches _per real
  user id_, system- wide across all inotify instances that user owns
  [measured-by-me: `cat /proc/sys/fs/inotify/max_user_watches`, 2026-09-04]. `fs.inotify.max_user_instances`
  (measured: **524288** on this machine — note this is unusually high; many distros
  default this one much lower, e.g. 128, since each `inotify_init()` call consumes
  one) caps the number of separate inotify file descriptors a user may hold open.
  `fs.inotify.max_queued_events` (measured: **16384**) caps the _per-instance_ pending-event
  queue depth before the kernel drops events and raises `IN_Q_OVERFLOW` ([`inotify(7)`](https://man7.org/linux/man-pages/man7/inotify.7.html)).
- **Memory cost per watch.** No single authoritative figure exists across kernel
  versions; community measurements converge on roughly **~1KB of unswappable
  kernel memory per watch** on 64-bit kernels (estimates in the wild range from
  ~160 bytes to ~1760 bytes depending on kernel version and what's counted — the
  inode being watched is pinned in memory too, which is the dominant cost for
  directory-heavy trees) [community-anecdote, multiple sources
  including
  https://metaprogrammingguide.com/code/what-are-the-costs-of-increasing-proc-sys-fs-inotify-max-user-watches-value
  and LKML discussion threads on raising the default
  — https://lkml.iu.edu/hypermail/linux/kernel/2010.3/08749.html]. **Because
  inotify only watches directories (not recursively) for structural changes**,
  the number of watches you need equals the number of _directories_, not files —
  for a 1M-file corpus the real driver is directory count, not file count. If a
  typical corpus has on the order of 50,000-200,000 directories (source trees,
  package caches, and Maildir-style layouts inflate this considerably), watch
  memory is roughly 50-200MB at ~1KB/watch — non-trivial but not disqualifying
  on a modern desktop, _provided_ `max_user_watches` is raised well above the
  historical 8192 default, which this system's 524288 already accommodates.
- **The recursive-watch problem.** inotify has **no native recursive watch**.
  You must `inotify_add_watch()` every directory individually and, critically,
  **add a watch to every newly-created subdirectory as you discover it via
  `IN_CREATE` events**, with an inherent race: a directory can be created and
  populated between your `IN_CREATE` event and your `inotify_add_watch()` call
  on it, silently losing early events in that new subtree
  ([`inotify(7)`](https://man7.org/linux/man-pages/man7/inotify.7.html),
  "Limitations and caveats"). This is a real, frequently-hit correctness bug in
  naively written recursive watchers, not a theoretical edge case.
- **Queue overflow (`IN_Q_OVERFLOW`).** When the per-instance queue (default
  16384 events here) fills faster than userspace drains it — a large `rsync`, a
  build system touching thousands of files, a `git checkout` across branches —
  the kernel drops events and delivers a single `IN_Q_OVERFLOW` pseudo-event
  with watch descriptor `-1`
  ([`inotify(7)`](https://man7.org/linux/man-pages/man7/inotify.7.html)). **You
  cannot know which files/directories were affected** — the only correct
  response is to fall back to a full (or scoped-to-affected-subtree, if you can
  bound it) re-crawl-and-diff of the watched tree, since the event stream is now
  known-incomplete. Any indexer that treats `IN_Q_OVERFLOW` as "just log a
  warning and continue" will silently drift out of sync with the filesystem.
- **Rename tracking cookie.** A rename generates `IN_MOVED_FROM` on the old
  parent and `IN_MOVED_TO` on the new parent, correlated by a shared 32-bit
  `cookie` field in `struct inotify_event`, letting you reconstruct "this was a
  rename, not a delete+create" (important for preserving document IDs / ranking
  history across a move) — **but only when both watches are on the same inotify
  instance and the rename stays within paths you're watching**; a move to/from
  an unwatched directory (or across the boundary of your watch set) arrives as
  an unpaired `IN_MOVED_FROM` or `IN_MOVED_TO` with no partner, which you must
  handle as delete/create respectively
  ([`inotify(7)`](https://man7.org/linux/man-pages/man7/inotify.7.html)).

### fanotify: does it solve inotify's scaling problem? (the important question)

**Short answer: yes, specifically for the recursive-watch and
per-directory-watch-count problem, but with real caveats.**

- `FAN_MARK_FILESYSTEM` lets a single mark cover an **entire mounted
  filesystem** — every directory and file on it, present and future, with
  **one** mark rather than one watch per directory. This directly eliminates
  both the inotify "must watch every directory individually" scaling cost and
  the create-then-race-to-watch-the-new-directory problem, since there is
  nothing to add a watch to — the mark already covers everything on the
  filesystem
  ([`fanotify(7)`](https://man7.org/linux/man-pages/man7/fanotify.7.html), see
  the `FAN_MARK_FILESYSTEM` section).
- `FAN_REPORT_FID` (added in Linux 5.1, refined since) changes fanotify's event
  reporting from "here's an open file descriptor to the changed file" (the
  original, pre-5.1 fanotify model — expensive and, worse, requires the kernel
  to open a file descriptor in _your_ process for every event, which doesn't
  scale and doesn't even work for events on files you don't have permission to
  open) to "here's an opaque file handle (`struct file_handle`) plus a
  filesystem id," which you resolve to a path (or just an inode identity)
  yourself via `open_by_handle_at()` — **this is the change that makes fanotify
  usable for a whole-filesystem watch at all**, not an incidental improvement
  ([`fanotify(7)`](https://man7.org/linux/man-pages/man7/fanotify.7.html)).
  `FAN_REPORT_DFID_NAME` (Linux 5.9, and improved further around 5.17 with
  `FAN_REPORT_NAME`/`FAN_REPORT_TARGET_FID` refinements for rename events)
  additionally reports the **parent directory's fid plus the child's filename**
  for create/delete/rename-style events, which is what actually lets you
  reconstruct a changed _path_ rather than just "some inode changed" — critical
  since a bare inode-changed notification is not directly actionable for a
  path-indexed search engine.
- **The `CAP_SYS_ADMIN` requirement is the real caveat, and it is a hard no for
  an unprivileged desktop app**: `fanotify_init()` with `FAN_MARK_FILESYSTEM`
  (and most of the useful whole-tree functionality) requires `CAP_SYS_ADMIN` —
  the caller must be root or hold that specific capability
  ([`fanotify_init(2)`](https://man7.org/linux/man-pages/man2/fanotify_init.2.html)).
  A desktop search indexer running as the logged-in user cannot use
  filesystem-wide fanotify marks without either (a) running a privileged helper
  daemon (a setuid binary or a systemd service with `CAP_SYS_ADMIN` granted via
  `AmbientCapabilities=`/file capabilities — this is exactly the shape GNOME
  Tracker's privileged-helper discussions and some enterprise EDR/backup tools
  use), or (b) asking the user to grant the capability explicitly, which is a
  meaningfully worse install experience than "just works as your own user" that
  inotify offers. This single requirement is why fanotify's whole-filesystem
  mode, despite solving the scaling problem cleanly on paper, is not the default
  choice for a consumer desktop tool — it's a legitimate option for a
  **root-installed system service** (which is closer to how Tracker's
  `tracker-miner-fs` and system-wide indexers are actually deployed on some
  distros) but a real friction point for a user-installed,
  no-privilege-escalation tool. Note there is a **non-privileged** fanotify mode
  (per-mountpoint or per-directory marks without `FAN_MARK_FILESYSTEM`, using
  `FAN_REPORT_FID` for events on files the caller can already access) that does
  not need `CAP_SYS_ADMIN`, but it does not give you the
  single-mark-covers-everything scaling win — you're back to marking individual
  directories, similar in cost shape to inotify, so it does **not** solve the
  scaling question this section is about; it solves a smaller, different problem
  (getting stable file-handle-based identity instead of path-based, useful for
  rename-robust tracking, but not for watch-count scaling).

**Conclusion for a desktop tool**: inotify (per-directory watches, raised
`max_user_watches`) is the pragmatic default for an unprivileged desktop
indexer; `FAN_MARK_FILESYSTEM`-based fanotify is the _correct_ answer for the
scaling problem but requires either running as an installed system service with
granted capabilities, or accepting the friction of asking for elevated
privileges — a real product decision, not a technical one, and it should be
surfaced as an open question rather than silently defaulted (see Done-note).

### Alternatives to event-driven watching

- **Periodic mtime crawl.** A `statx()`-based walk comparing mtimes against a
  stored baseline is the fallback of last resort (and the _only_ mechanism that
  also covers filesystems where notification doesn't work at all — see network
  filesystems below). Cost: this report measured a **warm-cache** `find`
  traversal of ~1.55M directory entries under `$HOME` in **0.57s wall / 1.25s
  user / 1.82s sys** on this machine [measured-by-me: `time find $HOME -xdev
  2>/dev/null | wc -l`, 2026-09-04, kernel 6.18.43]. This is a **warm page-cache**
  number — the directory metadata was almost certainly already cached from prior
  use — and is not representative of a **cold** crawl (first access after boot, or
  a tree not recently touched), which is dominated by actual storage I/O latency
  and can be one to two orders of magnitude slower depending on storage medium (NVMe
  vs spinning disk) and directory fragmentation; this report did not have a way to
  drop caches without root and did not attempt it, so no cold-crawl number is reported
  — treat the warm number only as a ceiling on best-case crawl cost, not a general
  answer.
- **btrfs send/receive and subvolume diffing.** btrfs can compute the exact set
  of changed files between two snapshots of a subvolume via
  `btrfs send --no-data -p <parent> <snap> | btrfs receive --dump`-style
  introspection (or purpose-built tools parsing the send-stream), giving an
  authoritative, crawl-free changed-file list — but only for trees that are (a)
  on btrfs and (b) actually snapshotted on a schedule the indexer can rely on.
  This is a real option for a subset of installations (increasingly common as a
  distro default root filesystem) but not a general solution, since a huge
  fraction of real desktop corpora live on ext4, on a home directory that isn't
  a snapshotted subvolume, or on a completely different filesystem.
- **ZFS snapshot diff** — `zfs diff <snap1> <snap2>` gives the same
  authoritative changed-file capability on ZFS, same caveat about requiring the
  filesystem choice and a snapshot cadence.
- **ext4 has no equivalent.** No filesystem-level change-journal exposed to
  userspace exists for ext4 (its journal is for its own crash-consistency
  metadata, not a user-visible changed-file log) — ext4 users are limited to
  inotify/fanotify plus periodic crawl, full stop.
- **`fsnotify`** is the Linux _kernel-internal_ subsystem
  inotify/fanotify/dnotify are all built on — not a separate userspace-facing
  API. Mentioning it mainly to be precise: there's no additional userspace
  surface here beyond inotify/fanotify.

### What existing tools actually do

- **Tracker/localsearch** (GNOME's indexer): runs `tracker-miner-fs` which uses
  inotify (via GLib's `GFileMonitor` abstraction, which itself wraps inotify on
  Linux) for live monitoring, plus a full crawl on first run and periodic
  reconciliation; extraction happens in a separate `tracker-extract` process
  specifically so a crashing extractor doesn't take the miner down, and it
  applies resource limits/blacklisting to repeatedly-crashing files (see A2
  above for detail). Does not, to this report's knowledge, use fanotify's
  filesystem-wide mode (which would require the privilege escalation discussed
  above).
- **Baloo** (KDE's indexer): also inotify-based via Qt's file-system-watcher
  abstraction, same per-directory-watch model and the same practical watch-count
  ceiling concerns that have historically generated user complaints about
  Baloo's memory/CPU footprint on large home directories.
- **plocate's `updatedb`**: does not use inotify/fanotify at all — it is a
  **pure periodic full-filesystem crawl** (traditionally run via a daily cron/
  systemd timer), rebuilding its database from scratch each run. This is the
  simplest possible design and it works precisely because `plocate`'s job is
  filename-only indexing, not content indexing — a full crawl for filenames is
  cheap (metadata-only `getdents`+`stat`, no file content read), which is
  exactly the class of walk this report measured warm-cache above. This doesn't
  transfer to a content-indexing tool, where crawl cost is dwarfed by extraction
  cost for any tier past plain text.
- **Windows' USN journal** (NTFS's "Update Sequence Number" change journal,
  `FSCTL_QUERY_USN_JOURNAL`/`FSCTL_READ_USN_JOURNAL`): NTFS maintains an
  **append-only, filesystem-level, kernel-maintained log of every metadata
  change** (create/delete/rename/write) to every file on the volume, queryable
  after the fact without having had a live listener running at the time of the
  change — a fundamentally different capability than inotify/fanotify, which are
  both purely event-_streaming_ (miss anything that happens while you're not
  listening, e.g. while the indexer isn't running) with no persistent record to
  query later. **This is the honest, structural gap between Linux and NTFS for
  this use case**: Windows Search and other NTFS-aware indexers can catch up
  after being offline by reading the USN journal's backlog; Linux has no
  equivalent, and any Linux indexer that isn't running continuously **must**
  fall back to a full or heuristic-scoped mtime crawl on every startup to catch
  changes made while it wasn't watching. This is not a fixable-with-better-code
  gap — it's an absent kernel/filesystem primitive on the ext4/xfs mainstream,
  and btrfs/ZFS snapshot-diffing (above) is the closest Linux analogue,
  available only on those filesystems.

### Crawl efficiency

- **`getdents64`** is the modern raw directory-read syscall (returns multiple
  directory entries per call, replacing the one-entry-at-a-time historical
  `readdir()` overhead at the syscall layer — `readdir()` in glibc is itself
  implemented on top of `getdents64`)
  ([`getdents64(2)`](https://man7.org/linux/man-pages/man2/getdents64.2.html)).
- **`statx()`** batches what `stat()`/`lstat()`/`fstatat()` needed multiple
  variants for, and importantly supports requesting only the fields you need
  (`STATX_MTIME` alone, say) — on filesystems/kernels that honor the mask this
  can reduce work versus a full stat, though the win is filesystem-dependent and
  not guaranteed to be dramatic on every backend
  ([`statx(2)`](https://man7.org/linux/man-pages/man2/statx.2.html)).
- **io_uring for metadata walks**: io_uring supports `getdents64` and `statx` as
  async opcodes, and the theoretical win is overlapping syscall latency across
  many outstanding requests instead of paying it serially per file/directory —
  genuinely useful for network filesystems or spinning disks where per-call
  latency (not CPU) dominates. On local NVMe with a warm cache, the syscalls are
  already cheap enough that io_uring's batching advantage is much smaller; this
  report did not find a rigorous, current (2024-2026) third-party benchmark
  specifically isolating io_uring's win for a metadata-only directory walk
  versus a well-written synchronous multi-threaded walker, and does not want to
  assert a number it can't back — **treat "io_uring meaningfully speeds up
  metadata crawl" as plausible but unverified by this research pass**, worth a
  direct experiment before relying on it architecturally.
- **Parallel directory traversal** (`jwalk`, `ignore` crate): both are Rust
  crates that parallelize directory-tree walking across threads; **`ignore`**
  (from the ripgrep project) is the one that actually explains `rg`'s and `fd`'s
  traversal speed — it combines a work-stealing parallel walker with
  gitignore-aware pruning (skipping whole subtrees early based on
  `.gitignore`/`.ignore` rules avoids descending into e.g. `node_modules` or
  `target/` entirely, which is often a bigger win than raw syscall throughput
  for real-world dev-heavy corpora) and is the crate to reach for directly
  rather than reimplementing this. `jwalk` is a lighter-weight
  parallel-walk-only alternative without the ignore-file awareness.

### Handling special cases

- **Hardlinks**: multiple directory entries, one inode — a naive indexer will
  index the same content twice under two paths unless it dedupes by
  `(st_dev, st_ino)` and decides a policy (index once, list both paths as
  aliases, is the right behavior; indexing twice wastes space and pollutes
  ranking with duplicate hits).
- **Symlinks**: decide explicitly whether to follow them for crawling (default
  should be **do not follow** by default, matching `find`/`fd`'s default and
  avoiding infinite loops from symlink cycles — `fd`/`ripgrep` both require an
  explicit flag to follow symlinks) and definitely track `(st_dev, st_ino)` of
  the _target_ to avoid infinite loops if you do follow them.
- **Bind mounts**: the same inode/directory subtree can appear at multiple mount
  points in the path namespace; without dedup by `(st_dev, st_ino)` this
  produces the same double-indexing problem as hardlinks, and inotify/fanotify
  watches placed at each mount point will each report the same underlying change
  independently, requiring the same identity-based dedup on the event side too.
- **Network filesystems (NFS/SMB)**: **no reliable inotify/fanotify support** —
  both are fundamentally local-filesystem mechanisms; NFS in particular has no
  server-to-client push notification of changes made by other clients in the
  general case (client-side caching + periodic revalidation is the NFS
  consistency model, not event push). The only correct approach for
  network-mounted content is a **periodic mtime crawl**, with the crawl interval
  being a real UX tradeoff (staleness vs the load a crawl puts on the remote
  server) rather than a technical one.
- **FUSE**: whether inotify events fire depends entirely on the specific FUSE
  filesystem implementation choosing to support them (FUSE forwards notification
  support is opt-in per filesystem, not a guarantee of the FUSE layer itself) —
  treat any FUSE mount as "unreliable notification, fall back to crawl" unless
  you've specifically verified the mount type supports it.
- **Containers/overlayfs**: inotify/fanotify watches placed inside a container
  observe the container's merged overlayfs view; changes to lower layers made
  from outside the container's mount namespace are generally invisible to a
  watcher inside it (namespace isolation is the point of
  overlayfs-in-containers), which matters only if the indexer itself is expected
  to run inside or watch across container boundaries — worth flagging as a scope
  question rather than assuming it "just works" if that's ever a requirement.
- **Huge single files**: no different at the watch level (one inode, one event
  stream) but matters for the extraction cost model — a single 50GB video or VM
  disk image file changing should not trigger a full re-extraction of the same
  size; content-addressed chunk hashing or simply "media metadata only, skip
  full-content indexing past a size threshold" is the standard mitigation, and
  either way it's a policy decision belonged in the indexing layer, not the
  change-detection layer, which just needs to report "this inode's mtime/size
  changed" correctly regardless of the file's size.

---

## Part C — Indexer politeness: staying invisible on a live desktop

Dave's requirement in his own words: near-real-time indexing that queues up work
under load and has "minimal affect on system performance (nice process with very
low priority)." This is not polish — Beagle (GNOME's early desktop search
project) died largely of resource complaints, and Baloo (KDE) is _still_ the
subject of "why is my disk thrashing" bug reports over a decade in, per the Arch
Linux forum and KDE bugzilla threads found below. Getting this wrong is the
single most reliable way for a technically excellent indexer to get uninstalled
in week one.

**`nice` — still works, but only under contention.** `nice`/`setpriority()`
tilts the CFS scheduler's fairness calculation; it is a _relative_ hint that
only matters when two processes actually want the CPU at the same instant, and a
background indexer running alone on an otherwise-idle machine gets full CPU
regardless of its nice value — which is fine (an idle machine _should_ be used),
but means nice alone does nothing to prevent an indexer from being first-to-grab
a burst of CPU the instant the user's foreground app also wants it (there's a
window, however short, before the scheduler's fairness correction kicks in).
Cheap, correct, necessary, not sufficient.

**`ionice` — the commonly repeated claim that it "just works" is stale, and this
needed checking, not assuming.** `ionice` only has an effect under I/O
schedulers that implement I/O-priority-aware queuing: **BFQ** (and the
long-obsolete CFQ) genuinely convert ionice classes into proportional bandwidth
shares; **`mq-deadline`** gives only a coarse ordering (RT class requests drain
from a separate high-priority FIFO ahead of best-effort/idle FIFOs — real, but
not proportional fairness); **`none`/`noop`** — the default scheduler on
**NVMe** in modern kernels, precisely because NVMe's own internal queue depth
and multi-queue hardware design make software reordering mostly redundant —
**ignores ionice entirely** [third-party-benchmark/kernel-doc
synthesis, https://docs.kernel.org/block/ioprio.html
confirms scheduler-dependent
support;
https://www.pistack.xyz/posts/2026-05-23-self-hosted-linux-io-priority-management-ionice-cgroup-iopriority-guide/
and community sources confirm the practical mq-deadline/none gap]. **The
practical consequence for this project: on the fast-NVMe desktop this report was
researched on (and increasingly the default consumer configuration),
`ionice IOPRIO_CLASS_IDLE` on the indexer process may do close to nothing**,
because the active scheduler is `none`/`mq-deadline`, not BFQ. Check the live
scheduler with `cat /sys/block/<dev>/queue/scheduler` before relying on ionice
as the primary throttle — don't assume it from tribal knowledge, that's exactly
the "commonly repeated stale claim" flagged as worth re-verifying. Set it anyway
(it's free and helps on the HDD/BFQ-configured minority of machines), but design
the actually-effective throttle around cgroups.

**cgroup v2 (`io.weight`, `cpu.weight`, `memory.high`) is the mechanism that
actually works regardless of I/O scheduler**, because it operates at the
block-cgroup layer beneath the scheduler choice — `io.weight` (a proportional
1-10000 weight, default 100) throttles the indexer's share of disk bandwidth
even under `none`/`mq-deadline` where ionice is inert, since cgroup I/O control
is enforced by the `blk-cgroup` controller independent of which request-queue
scheduler is selected. `cpu.weight` gives the same proportional-share throttling
for CPU that `nice` gives only a weak hint toward. `memory.high` (a soft cap —
the kernel throttles/reclaims aggressively above it rather than OOM-killing
outright, unlike `memory.max`) is the right tool for bounding an indexer's
page-cache and heap growth without a hard kill-on-exceed. Concretely: put the
indexer's worker processes in their own cgroup (`systemd`'s `--slice=`/`Slice=`
unit directive is the ergonomic way to do this without hand-rolling cgroupfs
writes — a user-level indexer already running as a systemd `--user` service can
just set `IOWeight=`, `CPUWeight=`, `MemoryHigh=` in the unit file) and set
weights low relative to the rest of the user's session slice. This is strictly
more portable and more effective than the ionice/nice pair, and should be the
primary mechanism, with nice/ionice kept as a cheap secondary hint for the
systems/schedulers where they still help.

**`sched_setscheduler(SCHED_IDLE)`** goes further than a low `nice` value: a
`SCHED_IDLE` process **only** runs when nothing else on the system wants the CPU
at all, rather than getting a small-but-nonzero share under contention the way
even `nice 19` still does. This is the right scheduling class for the
bulk-extraction worker pool specifically (tier 2+/OCR-plugin work, which is
latency-insensitive by nature — nobody's blocked waiting on a background PDF
re-index) while the filesystem-watch/event-ingestion process itself should stay
at normal priority (`SCHED_OTHER`) since it needs to keep up with `inotify`
queue draining in real time (an idle-scheduled watcher risks queue overflow
under load, which is the one failure mode Part B flags as silently corrupting
correctness — starving the watcher to be polite would trade a performance
problem for a correctness one, the wrong trade).

**`posix_fadvise(fd, 0, 0, POSIX_FADV_DONTNEED)` — easy to skip, and skipping it
is a real bug, not a nice-to-have.** A crawl or extraction pass that reads every
file's content will, by default, populate the page cache with all of it — a
100GB extraction pass can evict the user's actual working set (their open IDE's
file-backed mmaps, their browser's cached data, anything else relying on the
page cache) purely as a side effect of the indexer having read those bytes once
and never needing them again. Calling `fadvise(DONTNEED)` after reading each
file (or `POSIX_FADV_SEQUENTIAL` before, to hint the readahead pattern, plus
`DONTNEED` after to evict) tells the kernel to drop those pages immediately
rather than let them compete for cache residency with data the user's foreground
applications are actually using. This is the kind of detail that separates "the
index took an hour and I didn't notice" from "the index took an hour and now my
browser is swapping" — same wall-clock cost to the indexer, very different
experienced cost to the user, and it's one syscall per file.

**Backpressure and coalescing for save-storms and queue overflow.** Two related
but distinct bursts need explicit handling, not just "the queue is a Vec, we'll
get to it":

- **Save-storm coalescing**: an editor doing atomic-save-via-rename (write temp
  file, rename over the original — the standard pattern, used by vim, most IDEs,
  and `rsync --inplace`'s opposite) generates multiple inotify events per
  logical "user saved a file" (a `CREATE` for the temp file, a `MOVED_TO`/rename
  pair, sometimes an intervening `CLOSE_WRITE`). Naively re-extracting and
  re-indexing on every raw event wastes work and, worse, can race — indexing the
  temp file's transient content under the final filename. The correct pattern is
  a **debounce window per path** (tens to a few hundred ms — long enough to
  coalesce an editor's save sequence, short enough to still feel near-real-time)
  before enqueueing the extraction job, keyed on final resolved path after
  rename-cookie correlation (Part B).
- **inotify queue overflow backpressure**: this is the flip side of the
  `IN_Q_OVERFLOW` correctness problem already covered in Part B — when the
  extraction/index-write pipeline can't keep up with the event rate (a
  `git checkout` touching thousands of files, a large `rsync`), the fix is not
  to read events faster (you can't outrun `IN_Q_OVERFLOW` by reading harder if
  extraction is the bottleneck) but to **let the queue drain into a persistent,
  boundless work queue** (a small embedded DB / append log, not an in-memory
  `Vec`) as fast as inotify delivers, decoupling "note that this path needs
  re-indexing" (cheap, must never block or the kernel's own 16384-deep queue
  overflows) from "actually extract and index it" (expensive, can legitimately
  queue for minutes under load without correctness loss, _unlike_ falling behind
  on draining the kernel's inotify fd itself).

**What Baloo and Tracker actually do today, and whether it's enough.** Baloo's
own documentation and community guidance point users toward wrapping it in
`nice`/`ionice` and toward `balooctl suspend`/config-file exclude-lists as the
practical throttle — i.e., **manual, user-driven mitigation**, not an automatic
adaptive throttle built into the daemon, and Arch/KDE bug reports of
Baloo-induced stalls persist years after these mitigations were documented,
which is reasonably strong evidence that nice/ionice-only throttling is **not
enough** on its own (consistent with the ionice-is-often-inert finding above —
if a meaningful fraction of Baloo's user base is on `none`/`mq-deadline` NVMe
setups, the documented ionice advice may be doing far less than users are told
to expect from it) [community-anecdote,
https://bbs.archlinux.org/viewtopic.php?id=231709,
https://kde-bugs-dist.kde.narkive.com/jBC6jvOB/baloo-bug-333655-baloo-indexing-i-o-introduces-serious-noticable-delays].
Tracker's `tracker-miner-fs` similarly runs at a reduced scheduling priority and
applies its own internal rate-limiting/batching for the extraction pipeline, and
(per A2 above) isolates and quarantines crashing extractors, but this report
found no evidence either project uses cgroup v2 weights or `SCHED_IDLE` as their
throttle mechanism — both predate cgroup v2's current maturity in their original
design, and retrofitting is apparently incomplete. **The gap this leaves open
for a new project**: cgroup v2 weights + `SCHED_IDLE` + `fadvise(DONTNEED)`,
used together, is a strictly stronger and more portable politeness story than
what either mainstream Linux desktop indexer ships today, and is worth treating
as a genuine differentiator rather than assuming "do what Baloo does" is
sufficient — what Baloo does is demonstrably not sufficient, per its own bug
tracker.

---

## Cost model: 200GB / 1M-file mixed corpus

**Stated assumptions** (all `[estimated]` — this is arithmetic built from the
cited per-unit figures above, not a measured end-to-end run):

- Average file size ≈ 200GB / 1,000,000 files ≈ **200KB/file** — consistent with
  a real mixed personal corpus (lots of small source/config/text files dragging
  the average down against a smaller number of large media files).
- Rough corpus-tier split (a guess, stated as one): 70% code/text/config (700K
  files), 20% PDF/Office documents (200K files), 8% media files needing only
  metadata extraction (80K files) plus a **scanned-PDF/image-OCR-eligible**
  subset of 5% of the PDF/Office tier (10K pages, assuming ~1 "page" per
  scanned-image file, likely an undercount if some are multi-page PDFs — treat
  as a floor), and 2% audio/video needing ASR (20K files, assumed average 5
  minutes/file ⇒ ~1,667 hours of audio, a large number chosen deliberately to
  show ASR is the tier most likely to blow the budget).
- Single modern desktop machine, no GPU assumed for (a)-(c), OCR/ASR estimates
  given both CPU-only and noting where GPU changes the picture.

**(a) Metadata-only crawl** (`statx` every file + directory, no content read):
This report's own warm-cache measurement was 0.57s wall for 1.55M entries
[measured-by-me]. Scaling roughly linearly and adding real disk I/O for a
cold/uncached 1M-file corpus on a mix of storage: **estimate a few seconds
warm-cache, low tens of seconds to a few minutes cold**, dominated entirely by
storage latency rather than CPU — this is the cheapest tier by a wide margin and
is not a meaningful part of the total budget. `[estimated]`, anchored to one
warm-cache measurement.

**(b) + text extraction of the code+text tier** (700K files, near-zero cost each
per the extraction cost table): at even a generous 5ms/file average (covering
UTF-8 validation, encoding-fallback detection, and tokenization), 700,000 × 5ms
= **~58 minutes single-threaded**, trivially parallelizable across cores (an
8-core machine at reasonable parallel efficiency brings this to **under 10
minutes wall-clock**). `[estimated]`.

**(c) + PDF/Office tier** (200K documents at an estimated 100-300ms/document
average per the extraction cost table, split between direct-XML-parse OOXML —
fast end — and PDF/legacy-Office — slow end): 200,000 × ~0.2s average = **~11
hours single-threaded**, parallelizing to roughly **1.5-3 hours wall-clock** on
an 8-core machine (subprocess/library parse work parallelizes nearly linearly
since documents are independent). This tier is the first one where wall-clock
time becomes a real product concern — "index my whole document folder" going
from minutes to hours is the point where background/incremental indexing design
(index what's touched/recent first, backfill the rest) stops being a
nice-to-have. `[estimated]`.

**Reframing for the confirmed scope: OCR and ASR are opt-in plugins, not core
tiers.** The useful question per Dave's constraint is no longer "can the indexer
OCR the whole corpus" (it structurally won't try to, by design — tier 1/2/5 are
what runs unconditionally) but **"what does opt-in OCR of one chosen
subdirectory cost, and does the scheduling guarantee it never blocks tier-1
indexing."** The scheduling answer is straightforward given Part C: the OCR/ASR
plugin's worker pool runs at `SCHED_IDLE` in its own cgroup with a low
`cpu.weight`/`io.weight`, pulling from the _same_ persistent work queue Part C
describes but at the lowest priority tier in it — tier-1 (code/text) and tier-2
(PDF/Office, since a user enabling OCR still wants their normal documents
indexed promptly) jobs always drain first, and the OCR queue simply grows during
bursts of normal activity and drains during idle. This requires no new mechanism
beyond what Part C already specifies — it's a priority-queue discipline over the
same infrastructure, not a separate system — but it does mean OCR job admission
must be size-bounded per request (a user pointing OCR at "my whole Documents
folder" should see an up-front estimate, from the per-page rate below, before
the job is queued, not discover the queue is now hours deep).

**(d) + OCR tier** (10K scanned pages at the cited Tesseract parallel-CPU rate
of ~3 pages/sec on an 8-16 core machine): 10,000 / 3 ≈ **~56 minutes** added
wall-clock at that parallel rate — modest _for this assumed page count_, but
this number is extremely sensitive to how many actually-scanned (image-only)
pages a real corpus contains; a corpus with 100K scanned pages instead of 10K
turns this into **~9 hours**, and one with 1M scanned pages (a plausible count
for someone with a large personal scanned-document/photo-of-document archive)
turns it into **~93 hours (~4 days)** at the same parallel rate. This
non-linearity is the reason OCR must be gated behind an explicit "how many
actually need it" detection pass (the text-layer-sanity-check described in A1)
before committing to running it unconditionally — the total budget for this tier
is dominated entirely by an input-count variable this report cannot know for any
specific user's corpus, not by the per-page rate. `[estimated]`, built on a
`[third-party-benchmark]` per-page rate.

**(e) + ASR tier** (1,667 hours of audio, at an **unverified** CPU-only RTF —
using the GPU figure of ~8x real-time as an optimistic anchor and assuming,
conservatively, CPU-only is somewhere between 1x and 3x slower than real-time
i.e. RTF 1-3, since no verified CPU figure was found): 1,667 hours × (RTF 1
to 3) = **1,667 to ~5,000 hours of CPU-only processing** — i.e., **weeks to
months** of continuous single-machine processing for this assumed audio volume,
versus **~1,667/8 ≈ 208 hours (~8.7 days)** with GPU acceleration at the cited
8x real-time figure. This is the tier where the arithmetic itself is the
finding: **ASR at any meaningful volume of personal audio/video is not a
background-indexing-time operation on CPU**, full stop, and even GPU-accelerated
it is measured in days not hours for a genuinely large personal media archive.
Any product decision to offer ASR-based search must either (i) require a GPU and
set user expectations about multi-day initial processing, (ii) sharply limit
scope (recent files only, or an explicit user-triggered per-file action rather
than bulk indexing), or (iii) use a much smaller/faster model that trades
transcription quality for throughput. `[estimated]`, explicitly built on an
unverified CPU RTF assumption — flagged, not asserted as measured.

**Total, tiers (a)-(c) only** (the realistic default scope for "index my
documents and code," excluding OCR/ASR which should be opt-in per the above): on
the order of **2-4 hours wall-clock** for a full initial index of a 200GB/
1M-file corpus on an 8-core desktop, dominated almost entirely by the PDF/Office
tier. This is the number worth quoting as "how long does first-run indexing
take" for the product's default configuration.

---

## Done-note

**What could not be verified in this pass**:

- No primary-source, current (2024-2026) CPU-only real-time-factor figure for
  whisper.cpp at large-v3 quality on Linux x86 was found; the ASR cost-model
  section above is built on an explicitly-flagged assumption bridging from a GPU
  figure, not a measurement. This is the single most consequential unknown in
  the cost model — it directly gates whether ASR is offered at all.
- No rigorous, current third-party benchmark isolating io_uring's win for pure
  metadata-walk (`getdents64`/`statx`) workloads versus a well-written
  synchronous multi-threaded walker was found. The theoretical case (overlap
  syscall latency) is sound for network/spinning-disk backends; whether it's
  worth the implementation complexity for a desktop tool targeting mostly local
  NVMe is an open empirical question, not settled here.
- PaddleOCR/Surya/docTR CPU throughput figures were not found from a primary
  source strong enough to cite as a benchmark; only Tesseract has a solid
  multi-source throughput figure. Treat any comparative OCR-engine choice as
  needing its own direct measurement before committing.
- Could not measure a **cold-cache** directory walk (no root access to drop page
  cache in this environment) — only the warm-cache figure is reported, and it
  should not be mistaken for a worst-case number.
- The actual scanned-page count and audio-hour count for "a typical 200GB/
  1M-file personal corpus" in the cost model are invented planning inputs, not
  derived from any real corpus census. The cost-model arithmetic is only as good
  as those two inputs, and they are exactly the two inputs this report cannot
  supply — they belong to whoever's corpus is actually being indexed.

**Contradictions found across sources**: memory-cost-per-inotify-watch estimates
in the wild range across roughly a 10x band (160 bytes to ~1.76KB) depending on
kernel version and what's counted (mark structure alone vs mark plus pinned
inode data) — no single number should be treated as authoritative; this report
used ~1KB as a middle estimate and labelled it as such rather than picking one
source's number and presenting it as precise.

**The single biggest risk in this subsystem**: it is not any individual format
parser or any individual kernel API — it's that **the two halves of this slice
have incompatible failure modes that compound silently**. Change detection
(inotify) can silently miss events (queue overflow, the create-then-watch race,
anything on a network mount) with no error surfaced to the user; extraction can
silently fail or degrade (a PDF that "extracts" zero useful text without a hard
error, a poison file quietly quarantined and never retried, an
encoding-detection false guess producing readable-looking but wrong text).
Neither failure mode crashes anything or shows up in a log anyone reads by
default. The compound risk is an index that has been silently diverging from the
real filesystem for months, in a way indistinguishable from "working correctly"
until a user searches for something they know exists and gets nothing — at which
point there is no single log line to point to, only a diffuse "the index is
stale" bug report with no reproduction. The mitigation is not more sophisticated
extraction or a fancier watcher; it's **making staleness observable**: a
periodic reconciliation crawl (even a cheap metadata-only one, per the (a)
cost-model tier, which this report measured at well under a second warm-cache
for 1.5M entries) that's always running as a backstop regardless of how good the
event-driven watching is, plus surfacing "N files failed to extract, M files
pending re-index, last full reconciliation was T" as first-class, user-visible
index-health status rather than a debug log. Treat "does the watcher ever miss
anything" as a question the product must be able to answer for a user, not just
an internal implementation detail.
