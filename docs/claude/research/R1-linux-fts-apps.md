# R1 — Linux desktop full-text search applications

Scope: apps that index and search **file contents** on Linux. Filename/code
search tools (fzf, ripgrep-based launchers, fd, etc.) are a sibling slice. Local
environment has **none of these tools installed** (checked `which` for recoll,
baloosearch/balooctl, tracker3/localsearch/tinysparql, docfetcher, sist2,
beagle, regain, albert, ulauncher, cerebro, krunner, catfish — all absent, no
relevant packages found via `pacman -Q`/`dpkg -l`). All findings below are from
upstream docs/source/issue trackers, not local measurement.

## Summary table

Licence column matters for a permissively-licensed (MIT/Apache/BSD) build: a
GPL/LGPL/AGPL entry means the **design** is freely readable intel but the
**code** is not vendorable without relicensing the whole project copyleft.

| Tool                                                    | Language                          | Licence                                                                               | Index structure                                                                                                                                                                                                      | Regex query?                                                                                                                                                  | Formats                                                                                   | Status (2026-09)                                                                                |
| ------------------------------------------------------- | --------------------------------- | ------------------------------------------------------------------------------------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------- | ----------------------------------------------------------------------------------------- | ----------------------------------------------------------------------------------------------- |
| Recoll                                                  | C++ (core), Python (helpers)      | GPLv2                                                                                 | Xapian Glass: copy-on-write B+-tree, postlist + termlist tables                                                                                                                                                      | No native regex; wildcard, phrase, proximity, field filters                                                                                                   | ~200 via built-in + external helpers (antiword, pdftotext, catdoc, etc.)                  | Active. 1.38.x, 2025 releases                                                                   |
| Xapian (Recoll's backend)                               | C++                               | **GPLv2+**                                                                            | Glass B+-tree, see Recoll row                                                                                                                                                                                        | —                                                                                                                                                             | —                                                                                         | Active                                                                                          |
| Tracker (→ TinySPARQL) / Tracker Miners (→ LocalSearch) | C, Vala                           | **LGPLv2.1+** (library) / **GPLv2+** (miner/indexer daemon)                           | SQLite (custom "Tracker Store" engine on top of SQLite), full RDF/SPARQL triple store; FTS via SQLite FTS extension                                                                                                  | SPARQL query language (not free-text regex); `fts:match` for full-text                                                                                        | GNOME default parsers (poppler, GStreamer, exempi, etc.)                                  | Active, renamed 2024                                                                            |
| Baloo                                                   | C++/Qt                            | **LGPLv2.1-or-later** (per-file SPDX headers; some tri-licensed LGPL2/LGPL3/KDE-LGPL) | LMDB-backed key/value engine: PostingDB, PositionDB, DocumentDB (content/filename/xattr term lists), DocumentTimeDB, DocumentIdDB, IdTreeDB, IdFilenameDB, MTimeDB                                                   | **Partial** — `PostingDB::regexpIter()` scans the term dictionary under a given prefix with a `QRegularExpression` (term-level regex, not full-content regex) | Extractors via KFileMetaData (poppler, taglib, exiv2, etc.)                               | Active but chronically criticized for perf; part of default Plasma                              |
| DocFetcher / DocFetcher Pro                             | Java                              | **EPL 1.0** (free); Pro is commercial/closed                                          | Apache Lucene (classic segment format)                                                                                                                                                                               | Lucene query syntax: boolean, phrase, wildcard, fuzzy; no true regex in free client                                                                           | doc/docx/pdf/odt/rtf/html/chm/zip via Apache POI, PDFBox, etc.                            | Community-maintained fork; slow but not dead (1.1.27, Jan 2026)                                 |
| sist2                                                   | C (indexer) + Rust/TS web UI      | **GPLv3**                                                                             | Elasticsearch (external, Apache 2.0-then-SSPL-then-AGPL depending on ES version — check the ES version pinned) as the search backend; sist2 itself produces a portable `.sist2` archive of extracted docs/thumbnails | Whatever Elasticsearch query DSL supports (yes, regex via ES)                                                                                                 | Wide — Tika-like extraction via built-in libs + `--ocr`, `--fast-epub`, archive recursion | Active, small single-maintainer project                                                         |
| Beagle                                                  | C#/Mono                           | **(L)GPL mix**                                                                        | Custom Lucene.NET-based index                                                                                                                                                                                        | Basic boolean                                                                                                                                                 | Broad (Firefox history, email, IM logs, files) via helper "backends"                      | **Dead** — last release 0.3.9, Jan 2009                                                         |
| regain                                                  | Java                              | **Apache 2.0**                                                                        | Apache Lucene                                                                                                                                                                                                        | Lucene syntax                                                                                                                                                 | Files, mail via IMAP crawler, web crawler                                                 | **Dead** — last file posted 2013-06-09; SourceForge project page itself last updated 2014-07-30 |
| Nextcloud Full Text Search                              | PHP (framework) + external engine | **AGPLv3** (Nextcloud app norm)                                                       | Delegates to Elasticsearch or Solr, or a bundled SQL fallback                                                                                                                                                        | Whatever backend supports (ES: full DSL incl. regex)                                                                                                          | PDF/Office via platform-side extraction, config-limited by file size                      | Active as an official Nextcloud app family                                                      |
| Paperless-ngx                                           | Python                            | **GPLv3**                                                                             | **Migrating Whoosh → Tantivy** (Rust, via Python bindings) as of 2025                                                                                                                                                | Whoosh-compatible query lang preserved via regex-rewrite shim over Tantivy; advanced query syntax                                                             | OCRmyPDF pipeline; content is OCR'd then indexed                                          | Active, fast-moving                                                                             |
| Zotero                                                  | JS/XUL (client)                   | **AGPLv3** (client), some components MPL                                              | SQLite FTS (attachment full-text stored in `fulltextContent`/`fulltextWords` tables, SQLite FTS-driven)                                                                                                              | Simple/phrase; no regex                                                                                                                                       | PDF/HTML/EPUB via built-in pdf.js text extraction                                         | Active                                                                                          |
| Obsidian                                                | TypeScript/Electron               | **Proprietary/closed-source**, free-to-use                                            | Proprietary in-memory inverted index (not persisted as a real "index" file — rebuilt on load); community "Omnisearch" plugin adds a persisted index                                                                  | `/regex/` supported in core search; boolean, phrase, path filters                                                                                             | Markdown vault only (core); other formats via plugins                                     | Active, closed-source core                                                                      |
| Logseq                                                  | ClojureScript/Electron            | **AGPLv3**                                                                            | Datascript (in-memory Datalog DB) + on-disk edn/sqlite graph; full-text search via client-side index (uses `flexsearch` JS lib)                                                                                      | Basic query + advanced Datalog queries; no regex in the FTS box                                                                                               | Markdown/org files                                                                        | Active but roadmap uncertain (DB version rewrite ongoing)                                       |
| Albert                                                  | C++/Qt                            | **GPLv3**                                                                             | No persistent content index; "Files" extension indexes filenames/paths only (SQLite cache) via `locate`-like crawl                                                                                                   | No content search at all                                                                                                                                      | N/A (filename only)                                                                       | Active — **out of scope**, no content indexing                                                  |
| Ulauncher                                               | Python                            | **GPLv3**                                                                             | No built-in content index; content search only via third-party extensions that shell out to Recoll/Tracker/DocFetcher/locate                                                                                         | Depends on backend used                                                                                                                                       | Depends on backend                                                                        | Active, extension-dependent                                                                     |
| KRunner                                                 | C++/Qt (KDE)                      | **LGPLv2.1+/GPLv2+ mix** (KDE Frameworks norm)                                        | Delegates to Baloo for file search plugin                                                                                                                                                                            | Same as Baloo                                                                                                                                                 | Same as Baloo                                                                             | Active (ships with Plasma)                                                                      |
| GNOME Shell search providers                            | C/JS                              | **GPLv2+** (gnome-shell)                                                              | Delegates to Tracker/LocalSearch via its D-Bus SPARQL search-provider protocol                                                                                                                                       | Same as Tracker                                                                                                                                               | Same as Tracker                                                                           | Active                                                                                          |
| Catfish                                                 | Python/GTK                        | **GPLv2+**                                                                            | Frontend only — shells out to `locate`/`find`/`grep`, or to Tracker if present, for content search                                                                                                                   | Whatever backend gives (grep = full regex)                                                                                                                    | Whatever backend gives                                                                    | Active, lightweight                                                                             |
| Cerebro                                                 | Electron/TS                       | **MIT**                                                                               | Plugin architecture; no core content index; file-search plugins are filename-only or wrap `mdfind`(macOS)/other                                                                                                      | N/A on Linux content search                                                                                                                                   | N/A                                                                                       | Largely stalled; not chased further per scope call below                                        |
| DEVONthink-alikes on Linux                              | —                                 | —                                                                                     | No credible native equivalent found; closest are Recoll+web UI, or paperless-ngx for docs                                                                                                                            | —                                                                                                                                                             | —                                                                                         | N/A — gap, not a product                                                                        |

Licence figures above are `[upstream-documented]` from each project's stated
licence/package metadata, except Cerebro's and Beagle's exact per-component
split, which are `[community-anecdote]`-level confidence in this pass — check
`LICENSE`/`COPYING` directly before citing either in the final report. The one
fact that matters most for a permissively-licensed build: **the two designs
every long-lived tool in this survey converges on split cleanly down the licence
axis that matters.** Lucene (and by extension Tantivy, its spiritual Rust
successor) is **Apache 2.0** — permissive, freely referenceable at the code
level. Xapian, and Recoll on top of it, are **GPLv2** — freely readable as
design, off-limits as code for an MIT/Apache/BSD target.

## Detailed notes

### Recoll (+ Xapian)

- Language: C++ core with Python for some indexing helpers and the GUI is
  Qt/C++. License: GPLv2. Actively maintained by Jean-Francois Dockes; release
  notes page shows 1.3x line continuing into 2025
  ([release notes](https://www.recoll.org/pages/release-1.25.html)).
- **Index backend**: Xapian. Since Xapian 1.4 the default backend is **Glass**:
  a copy-on-write B+-tree structure. Two core tables matter: the **postlist
  table** (per-term posting lists: which documents contain the term, with
  position/frequency data) and the **termlist table** (per-document list of
  terms, used for relevance/highlighting and for deleting/updating a document's
  postings efficiently). Older Chert backend used a similar B+-tree shape; both
  are B-tree-with-freelist designs, not LSM.
  [Xapian admin notes](https://xapian.org/docs/admin_notes.html),
  [Getting Started with Xapian — Databases](https://getting-started-with-xapian.readthedocs.io/en/latest/concepts/indexing/databases.html),
  [Recoll — Xapian index formats](https://www.recoll.org/usermanual/webhelp/docs/RCL.INDEXING.STORAGE.FORMAT.html).
  Recoll 1.24/1.25 changed the on-disk format again to speed up phrase search —
  new indexes store document text inside the index itself, which is a meaningful
  growth-vs-speed tradeoff `[upstream-documented]`.
- **Index size**: Recoll's own perf page reports, for a test corpus of **18,000
  random PDFs totalling ~30 GB**, an index size of **1.2 GB — about 4% of corpus
  size** `[upstream-documented]`
  ([recoll.org/pages/perfs.html](https://www.recoll.org/pages/perfs.html)).
  Recoll's docs explicitly warn this ratio is corpus-dependent: mbox archives
  can make the index _bigger_ than the source text (because compressed mail gets
  fully re-extracted and stored), while media files with little extractable text
  index far smaller.
- **Indexing**: full recursive crawl on first run; incremental updates via
  **mtime/inode comparison** on subsequent runs (a full directory walk that
  skips unchanged files), plus optional real-time monitoring via inotify on
  Linux (`recollindex -m`) for near-real-time updates. There is no
  fanotify-based whole-filesystem watch — inotify watches are set up
  per-directory and the standard Linux inotify instance/watch limits apply,
  which is the practical ceiling this hits at very large trees (documented as a
  known limitation requiring `fs.inotify.max_user_watches` tuning).
- **Query language**: field filters (`author:`, `ext:`, `date:`), phrase search,
  proximity (`NEAR`/`ADJ`), wildcards (`*`, `?`), boolean AND/OR/NOT, and
  Xapian's probabilistic ranking (BM25-family weighting) — not boolean-only.
  **No regex** in the query language itself.
- **Formats**: Recoll doesn't parse most formats itself; it dispatches to
  external helper filters — `pdftotext`/`pdfinfo` for PDF, `antiword` or
  `catdoc` for legacy Word, `unrtf`, `wpd2text`, `libreoffice --headless` as a
  fallback, `7z`/`unzip` for archives — configured in `recoll.conf`'s mimemap.
  This external-helper-per-format design is a deliberate architecture choice:
  easy to extend, but indexing throughput is bottlenecked by spawning a process
  per document for many formats.
- **Footprint / architecture**: GUI (`recoll`) + CLI (`recollindex`,
  `recollq`) + optional daemon mode for real-time indexing. No always-on
  background daemon required for basic operation, unlike Tracker/Baloo — you can
  run indexing on demand. This is one reason it's often cited as the least
  resource-hungry option.
- **Strengths**: mature, scriptable, no forced background daemon, very
  configurable per-format handling, works well as a batch/cron-driven indexer.
  **Why people leave it**: dated Qt/GTK GUI, no incremental-index GUI feedback
  comparable to modern tools, external-helper dependency chain is fragile
  (missing `antiword`/`catdoc` silently degrades extraction), and it has no
  daemon-level "live as you type in Nautilus" integration the way Tracker/Baloo
  do.

### Tracker → TinySPARQL / Tracker Miners → LocalSearch (GNOME)

- **The rename** (verify before citing casually — commonly garbled online): in
  2024, during the GNOME 47 cycle, GNOME renamed the `tracker` GitLab project to
  **TinySPARQL** (the SPARQL library/database engine) and `tracker-miners` to
  **LocalSearch** (the actual file-content indexer/miner daemon). Debian ITP
  bugs: [#1072711 localsearch](https://bugs.debian.org/1072711),
  [#1072712 tinysparql](https://bugs.debian.org/cgi-bin/bugreport.cgi?bug=1072712).
  Maintainer's own announcement:
  [Carlos Garnacho, "Goodbye Tracker, hello TinySPARQL and LocalSearch"](https://blogs.gnome.org/carlosg/2024/07/14/goodbye-tracker-hello-tinysparql-and-localsearch/).
  Stated reason: the process name `tracker-miner-fs-3` read as
  privacy-hostile/crypto-mining-adjacent to users, and "Tracker" had already
  been overloaded once when the SPARQL library and the indexer split for Tracker
  2.x. So: **binary/package names changed, the underlying engine and file
  formats did not** — this is a rebrand, not a rewrite.
- **Index backend**: TinySPARQL is a standalone RDF triple-store / SPARQL 1.1
  query engine, implemented **on top of SQLite** (its own storage engine
  translates the RDF ontology into SQLite tables, with SQLite's FTS extension
  used for the free-text-match predicate `fts:match`). This is a fundamentally
  different architecture from Xapian/Lucene: everything — filesystem metadata,
  EXIF, ID3 tags, and extracted document text — lives as RDF triples queryable
  via SPARQL, and full-text search is one predicate among many rather than the
  primary interface.
- **Indexing / incremental update**: `localsearch` (formerly `tracker-miner-fs`)
  uses **inotify** for live monitoring of watched directories (typically XDG
  user dirs by default, configurable), plus periodic/triggered rescans. It runs
  as a set of D-Bus session services (miner-fs, extractor, etc.), always-on by
  default on GNOME.
- **Query language**: SPARQL, not a simple free-text query box — GNOME Shell and
  Nautilus expose only a thin free-text layer over it. No end-user regex.
- **Formats**: uses GNOME's own extraction stack — poppler (PDF), GStreamer
  discoverer (audio/video metadata), libgxps, exempi/EXIF libs for photos — not
  Tika, not spawned external CLIs the way Recoll does; extractors are in-process
  GLib plugins.
- **Footprint**: always-on daemon architecture (miner-fs + extractor +
  writeback + store as separate D-Bus-activated processes), by design tied into
  GNOME's file manager and Shell search. This is the daemon most likely to be
  "always running" on a stock GNOME desktop even if the user never opens a
  search UI.
- **Strengths**: deep desktop integration (Shell search provider, Nautilus
  search-as-you-type, Files metadata), genuinely general triple-store useful
  beyond search. **Why people disable it**: the historical `tracker-miner-fs`
  reputation for indexing loops, disk churn, and the rename itself is partial
  evidence of a PR/trust problem — see the Arch forum thread title
  ["re-disable tracker/miner in 2024 (rebranded as localsearch/tinysparql)"](https://bbs.archlinux.org/viewtopic.php?id=299586),
  which shows the disable-it instinct outlived the rename.

### Baloo (KDE Plasma)

- Licence: **LGPLv2.1-or-later** (verified against the SPDX headers on the
  actual engine source files, e.g.
  [`postingdb.h`](https://github.com/KDE/baloo/blob/master/src/engine/postingdb.h):
  `SPDX-License-Identifier: LGPL-2.1-or-later`; some files carry a tri-license
  `LGPL-2.0-only OR LGPL-3.0-only OR LicenseRef-KDE-Accepted-LGPL`). Actively
  shipped as part of KDE Frameworks/Plasma; source at
  [github.com/KDE/baloo](https://github.com/KDE/baloo).
- **Index backend — resolved.** The KDE wiki's framing ("decentralized...no
  central database, a set of 3 services") describes Baloo's _architecture_, not
  its storage engine, and is a different claim from "what file format is on
  disk" — the two were previously conflated in this file. Read directly from
  `src/engine/` on the current `master` branch
  ([github.com/KDE/baloo/tree/master/src/engine](https://github.com/KDE/baloo/tree/master/src/engine)):
  every database class (`postingdb.h`, `positiondb.h`, `documentdb.h`,
  `documenttimedb.h`, `documentiddb.h`, `idtreedb.h`, `idfilenamedb.h`,
  `mtimedb.h`) `#include <lmdb.h>` directly and operate on `MDB_dbi`/`MDB_txn`
  handles — **Baloo's storage engine is, unambiguously and currently, LMDB** (a
  memory-mapped, copy-on-write B+-tree), not a design in flux or a
  `[community-anecdote]`-level guess. The individual databases, per their own
  doc comments and the KDE design ticket
  ([Phabricator T9805, "Overhaul Baloo database scheme"](https://phabricator.kde.org/T9805)):
  - **PostingDB**: `<term> → <id1> <id2> <id3> ...` — the main term-to-document
    posting list, the thing you query when searching for a term.
  - **PositionDB**: `<term> → <docID>:<n positions>:[positions...]` per matching
    document — concatenated position lists per term, used for phrase/proximity
    queries.
  - **DocumentDB** (instantiated three times — content terms, filename terms,
    xattr terms): `<docID> → <list of terms>` — the reverse of PostingDB, kept
    so that deleting or reindexing one document can find every posting list it
    needs to update without a full scan.
  - **DocumentTimeDB / MTimeDB / DocumentIdDB / IdTreeDB / IdFilenameDB**:
    auxiliary stores for mtimes (incremental-update change detection), numeric
    document-ID ↔ inode/path mapping, and directory-tree structure. Document
    IDs are 64-bit integers built directly from filesystem identity — the low 32
    bits of `st_dev` concatenated with the low 32 bits of `st_ino` — which ties
    Baloo's index tightly to the local filesystem's device/inode numbering (a
    design constraint worth noting for anyone considering bind-mounts, network
    filesystems, or filesystem migration).
  - This is a genuinely different design point from Xapian/Lucene's
    segment-and-merge model: **no segments, no merge/compaction step** — LMDB is
    a single-file copy-on-write B+-tree updated in place, transactionally, per
    write batch. That buys simpler crash-consistency (LMDB transactions are
    ACID) at the cost of the read-optimized, append-then-merge write pattern
    that lets Lucene/Tantivy batch large indexing runs cheaply — a real
    tradeoff, not a "worse" or "better" one in the abstract.
  - **Regex — a correction to this file's first pass**: `PostingDB` exposes
    `regexpIter(const QRegularExpression &regexp, const QByteArray &prefix)`
    ([`postingdb.cpp`](https://github.com/KDE/baloo/blob/master/src/engine/postingdb.cpp)),
    which walks the term dictionary under a given prefix and validates each term
    against the supplied `QRegularExpression`. That is real regex support — but
    it is regex **over indexed terms**, i.e. equivalent to `grep` against your
    vocabulary list, not arbitrary regex over document content or across term
    boundaries. Worth stating precisely: Baloo is not "no regex" as originally
    written here, it is "regex constrained to single indexed terms."
- **Indexing**: daemon (`baloo_file`) + a separate content extractor process
  (`baloo_file_extractor`, using KFileMetaData) that Baloo intentionally spawns
  and kills per batch to bound memory. inotify-based live watching of home
  directory (or configured directories) for changes.
- **Query language**: simple term search, `balooshow`/`baloosearch` CLI, and a
  KRunner-facing field-filter mini-syntax (`rating>3`, `tag:foo`,
  `filetype:pdf`); no free-content regex, no real boolean algebra beyond
  implicit AND (though see the term-regex nuance above).
- **Formats**: extraction delegated to **KFileMetaData**, a separate KDE library
  wrapping poppler (PDF), taglib (audio), exiv2 (photo EXIF), libarchive, etc. —
  same "wrap existing C libraries in-process" pattern as Tracker, not
  external-process-per-file like Recoll.
- **Resource footprint at scale — this is the tool's defining reputation
  problem**: extensive, recent, and recurring user reports of
  `baloo_file_extractor` pegging a full CPU core and consuming multi-GB RAM,
  including as recently as **August 2024**
  ([chimera-linux/cports#2677](https://github.com/chimera-linux/cports/issues/2677)),
  and reports that full content indexing "will enable indexing each word inside
  files, causing a massive slowdown and being impossible to use in case of
  millions of files"
  ([Manjaro forum](https://forum.manjaro.org/t/baloo-indexing-high-cpu-and-memory-usage-when-idle/96726))
  `[community-anecdote]`, but it is a _repeated, multi-year_ anecdote across
  independent users/distros, which is itself signal. The standard community fix
  is literally "delete `~/.local/share/baloo` and let it rebuild" or disable
  content indexing outright and keep filename-only indexing. At the ~1M files /
  10–200 GB scale in this brief's target, Baloo content indexing is widely
  reported as **not viable without disabling content extraction**.
- **Strengths**: zero-config KDE integration (Dolphin, KRunner). **Why people
  disable it**: the CPU/RAM/disk-churn reputation above, plus the ~15-minute
  database-refresh-on-restart behavior reported by users.

### DocFetcher / DocFetcher Pro

- Language: Java (SWT GUI). License: EPL (free version), commercial Pro adds
  Outlook PST/OST and network share indexing.
- **Index backend**: Apache **Lucene**, classic segment-file format
  (`.cfs`/per-segment files, inverted index with term dictionary + postings +
  norms — standard Lucene, not a custom structure).
- **Status**: the canonical upstream (`docfetcher/DocFetcher` on GitHub /
  SourceForge) still cuts occasional releases — **1.1.27 dated Jan 19, 2026**,
  following 1.1.26 from **Oct 5, 2023** — via a community-maintained fork rather
  than the original author, per SourceForge changelog and GitHub releases pages
  ([SourceForge ChangeLog](https://sourceforge.net/p/docfetcher/wiki/ChangeLog/),
  [GitHub releases](https://github.com/docfetcher/DocFetcher/releases)). So:
  **not dead, but essentially maintenance-only** — treat "abandoned" claims
  commonly repeated online as slightly overstated
  `[upstream-documented, contradicts common anecdote]`.
- **Query language**: full Lucene query syntax — boolean, phrase, wildcard,
  fuzzy (`~`), field filters (filename, path, type) — no true regex mode in the
  free client.
- **Formats**: Apache POI (doc/xls/ppt), PDFBox (pdf), plus html/rtf/odf/chm and
  archive recursion (zip/7z/tar).
- **Architecture**: pure desktop GUI + portable index folders you can copy
  between machines (a genuinely nice property for offline/USB use). No daemon.
- **Strengths**: portable indexes, good archive-recursion support, doesn't
  require a system daemon. **Why abandoned by most**: Java/SWT UI feels dated,
  single-maintainer bus factor, Windows-first history (DocFetcher was originally
  the most Windows-centric of this list), Pro version paywalls Outlook support
  that competing free tools don't need.

### sist2

- Language: C for the crawler/indexer core, TypeScript/Svelte for the web UI.
  License: GPLv3. Single primary maintainer (`simon987`). Note: the _original_
  "Simple Incremental Search Tool" name belongs to an older, unrelated abandoned
  project
  ([simon987/Simple-Incremental-Search-Tool](https://github.com/simon987/Simple-Incremental-Search-Tool),
  an Elasticsearch-frontend prototype); the live project is
  [sist2app/sist2](https://github.com/sist2app/sist2), and sources sometimes
  conflate the two — worth flagging since a naive search will surface the dead
  repo first.
- **Index backend**: sist2 itself is a **crawler + extractor**, not a search
  engine — it walks the filesystem, extracts text/metadata/thumbnails, and
  writes a portable `.sist2` archive (SQLite-based document store + thumbnails).
  The actual _search_ backend is **Elasticsearch**, run as a separate service
  that sist2 bulk-indexes into (`sist2 index` pushes to `http://localhost:9200`
  by default). This means sist2 inherits Elasticsearch's on-disk format (Lucene
  segments under the hood) and its full query DSL, including regex, wildcard,
  and fuzzy queries, for free — but it also means the real memory/CPU cost of
  this stack is Elasticsearch's, not sist2's own.
- **Indexing**: supports incremental scanning (re-scans changed files by mtime),
  tagging (manual + script-driven auto-tagging by file attributes), recursive
  extraction inside archives, and Tesseract OCR integration.
- **Formats**: broad — sist2 bundles its own extraction rather than shelling to
  Tika; covers common office/PDF/image/audio/video/ebook formats plus archive
  recursion.
- **Footprint**: the sist2 binary itself is lightweight and single-purpose; the
  practical resource cost of running this stack at 1M files / 10–200 GB is
  dominated by Elasticsearch's JVM heap and index size, which is the standard ES
  tradeoff (heavy at rest, fast and rich at query time).
- **Strengths**: excellent web UI (thumbnails, tags, disk-usage visualization),
  genuinely good for large personal archive/NAS use cases, actively developed.
  **Limits at this brief's scale**: requires standing up and tuning
  Elasticsearch, which is a heavyweight dependency for a single-user desktop
  search tool — not a lightweight embedded option the way
  Xapian/Lucene-embedded/Tantivy are.

### Beagle (historical — the instructive post-mortem)

- Language: C#, built on **Mono**. Backend: a custom index built on Lucene.NET.
  License: (L)GPL mix.
- **Timeline**: final release **0.3.9, January 26, 2009**
  ([Wikipedia — Beagle (software)](<https://en.wikipedia.org/wiki/Beagle_(software)>));
  effectively dead since. Source archived at
  [github.com/joeshaw/beagle](https://github.com/joeshaw/beagle).
- **Why it died — this is the useful lesson for a builder**:
  1. **Mono dependency was a distribution/packaging tax**, not just a runtime
     cost — distros without a mature Mono stack (or users hostile to Mono on
     ideological/footprint grounds) simply didn't ship it, capping its
     addressable install base regardless of quality.
  2. **Performance and resource use were never solved.** The project's own
     GUADEC talk material and contemporaneous reviews describe unresolved
     CPU/memory hogging and admit "the tools to profile and debug them were
     largely non-existent" at the time
     ([Joe Shaw, GUADEC 2006 slides](https://www.joeshaw.org/talks/Beagle-GUADEC2006.pdf))
     — i.e., the team was flying blind on the exact class of problem (indexer
     daemon eating CPU/RAM) that later sank Baloo's reputation too. **This is
     the load-bearing pattern**: every daemon-based content indexer on Linux has
     hit the same "background extractor eats a core" failure mode (Beagle in
     2006-2009, Baloo in 2020s); the ones that survived (Tracker/LocalSearch,
     Baloo) survived by staying attached to a desktop environment's default
     install, not by solving the resource problem outright — Baloo still hasn't,
     per the 2024 reports above.
  3. It grew from **Dashboard**, an ambitious "index everything for contextual
     computing" project, and inherited scope creep (chat logs, IM, web history,
     contacts) beyond "index my files" — a cautionary note on scope discipline
     for a from-scratch build.
  4. GNOME moved to Tracker as its own project's answer around the same period,
     and once the desktop environment itself doesn't depend on your indexer, a
     community project without that institutional backing has no forcing
     function to keep going.

### regain

- Licence: **Apache 2.0**. Java, Apache Lucene-based, historically an
  alternative to Beagle aimed at file-server/network-share indexing rather than
  a single desktop.
- **Last release, resolved**: checked the project's SourceForge page directly
  ([sourceforge.net/projects/regain](https://sourceforge.net/projects/regain/)).
  Its news-post timestamps show the last dated activity was **2013-06-09** (a
  "Posted 2013-06-09" news entry, preceded by 2013-05-28 and 2012-06-13
  entries), and the project page's own `datetime="2014-07-30"` "Last Update"
  stamp — attached to project metadata, not a release — is the most recent
  timestamp of any kind found on the page. So: **effectively dead since 2013**,
  with no evidence of activity in the 12+ years since `[upstream-documented]`
  (direct read of the SourceForge project page, not a third-party summary).

### Nextcloud Full Text Search

- Architecture is a plugin framework, not a monolithic search engine:
  [nextcloud/fulltextsearch](https://github.com/nextcloud/fulltextsearch) (the
  core PHP framework) requires (a) a "provider" app per content type (files,
  bookmarks, mail, deck cards) that knows how to extract content, and (b) a
  "platform" app that talks to a search backend — officially
  [fulltextsearch_elasticsearch](https://github.com/nextcloud/fulltextsearch_elasticsearch)
  or a Solr platform app, with a SQL-only fallback platform for those who don't
  want to run a separate search cluster.
- Index backend is therefore whatever the platform app delegates to —
  Elasticsearch (Lucene segments) or Solr (also Lucene-based) — Nextcloud itself
  defines no on-disk format of its own.
- Relevant to a desktop-search evaluation mainly as a **server-side** comparison
  point: it validates the "bolt Elasticsearch onto a document store" pattern
  (same shape as sist2) rather than an embedded/local-first design, and it
  inherits ES/Solr's operational weight (JVM, cluster/single- node tuning) for a
  use case (self-hosted personal cloud) that's arguably closer to desktop scale
  than to enterprise scale — a design mismatch worth noting if the final report
  weighs "should a Linux desktop tool depend on a JVM search server."

### Paperless-ngx

- Python/Django. Historically used **Whoosh** (pure-Python search library) for
  its full-text index. In 2025, the project undertook a **migration from Whoosh
  to Tantivy** (Rust, via Python bindings), motivated explicitly by Whoosh's
  performance ceiling on larger document collections —
  [GitHub discussion #11352](https://github.com/paperless-ngx/paperless-ngx/discussions/11352)
  and the implementing PR
  [#12471](https://github.com/paperless-ngx/paperless-ngx/pull/12471). This is a
  directly relevant data point for Dave's build-vs-buy question: **a real,
  actively-used Python document-management project concluded Tantivy was worth
  the migration cost over a pure-Python inverted index**, and had to build a
  regex-based query-rewrite compatibility shim to keep old Whoosh query syntax
  working over the new engine
  ([PR #13010](https://github.com/paperless-ngx/paperless-ngx/pull/13010) shows
  this shim reaching its limits on some v2 query shapes) — i.e., even a clean
  backend swap under an existing query surface has real edge-case cost, which
  bears on any plan to swap search backends under a stable Rust-native search
  engine's query language.
- Content arrives via an OCR pipeline (OCRmyPDF), so "indexing throughput" for
  Paperless is dominated by OCR cost, not by the FTS engine — a different
  bottleneck profile than a generic desktop indexer working over already-text
  files.

### Zotero, Obsidian, Logseq (adjacent, personal-knowledge-tool search)

- **Zotero**: full-text of attached PDFs/HTML is extracted (via bundled pdf.js
  text layer extraction) into SQLite tables that use SQLite's FTS capability;
  search is boolean/phrase over that FTS index, no regex, no proximity operators
  exposed to the user. Purely single-library-scale (thousands, not millions, of
  items) — not architected for the 1M-file target scale.
- **Obsidian**: the _core_ search plugin builds an **in-memory** index over the
  vault's Markdown files at startup rather than persisting a real on-disk
  inverted index — this is confirmed by community discussion of the core search
  being effectively "read everything into memory and grep-like scan with a
  word→file inverted lookup," not a durable index file
  ([forum: "How exactly does Obsidian's search work?"](https://forum.obsidian.md/t/how-exactly-does-obsidians-search-work/90905)).
  Core search does support **regex** via `/pattern/` syntax — notable, since
  most of the "real" desktop indexers above (Recoll, Tracker, Baloo) do not
  expose regex. The popular **Omnisearch** community plugin instead builds a
  persisted index (uses MiniSearch, a JS library) precisely because the core
  search doesn't scale/persist well on very large vaults — reported directly by
  users with huge vaults switching to OS-level indexing tools (Everything on
  Windows, similar tools on Linux) to compensate
  ([note.com — "How I Fixed Broken Obsidian Search in a Huge Vault Using OS Indexing"](https://note.com/biomatter/n/n5166a67be971?hl=en),
  `[community-anecdote]`). Obsidian is fundamentally scoped to a single-vault,
  all-Markdown corpus — not comparable in scale ambition to the
  1M-file/mixed-format target here.
- **Logseq**: uses an in-browser/in-Electron **Datascript** (Datalog) database
  for its structured graph queries, and a separate client-side full-text search
  index built with the JS library **FlexSearch** for the free-text search box.
  Same single-graph, Markdown/org-only scale envelope as Obsidian; not a general
  desktop file indexer.

### Launcher-integrated search (Albert, Ulauncher, Cerebro, KRunner, GNOME Shell, Catfish)

The important finding here is negative: **most desktop launchers do not do
file-content indexing themselves.**

- **Albert**: its "Files" extension indexes **filenames/paths only** (an
  SQLite-cached crawl), explicitly not content — confirmed by Albert's own
  plugin docs describing it as a directory-monitoring filename index, and by the
  complete absence of a content-search option in its plugin list per
  [AlternativeTo's feature comparison](https://www.alternativeto.net/software/albert/).
  Out of scope for this brief except as a negative data point.
- **Ulauncher**: same story — core has no content index; content search requires
  third-party extensions that literally wrap **Recoll, GNOME Tracker,
  DocFetcher, or `locate`** as backends, e.g.
  [dalanicolai/gnome-tracker-extension](https://github.com/dalanicolai/gnome-tracker-extension)
  ("Ulauncher extension for (deep) search filesystem via the gnome tracker,
  recoll, docfetcher, locate or calibre index"). This is strong independent
  confirmation that Recoll/Tracker are treated by the ecosystem as **the** two
  content-search backends worth wrapping — nobody wraps Baloo this way, which is
  itself a signal about Baloo's reputation/API accessibility outside KDE.
- **KRunner** (KDE): its file-search plugin is a thin front-end over **Baloo** —
  same backend, same limitations, same query syntax subset.
- **GNOME Shell search providers**: the Nautilus/Files search provider and any
  content-search provider are thin front-ends over **Tracker/LocalSearch** via
  its D-Bus search-provider protocol — again, no independent index.
- **Catfish**: explicitly a GUI _frontend_ over `locate`, `find`, and `grep`
  (content search = literally shelling to `grep -r`), falling back to Tracker if
  present on the system for indexed search. Zero independent indexing logic of
  its own — this makes it the "honest" option: whatever regex/perf
  characteristics you get are exactly `grep`'s.
- **Cerebro**: Electron-based, plugin-architecture launcher; on Linux its
  file-search capability is limited/community-plugin-dependent and there's no
  evidence of an independent content-index engine — treat as effectively out of
  scope, `[community-anecdote]`-level confidence since I did not find a primary
  doc confirming current Linux plugin state (project activity appears low;
  verify before citing in the final report).

### Recent (2024–2026) Rust/Go newcomers found

Actively searched for new entrants rather than relying on the well-known list,
per the brief's instruction. Findings, all early-stage/niche and none appear to
be a general "index my whole disk for content" desktop app at production
maturity comparable to Recoll/Tracker/Baloo:

- **[Universal-Local-AI-Indexer](https://github.com/RobinsonBeato/Universal-Local-AI-Indexer)**
  — Rust, **Tantivy + SQLite**, Windows-focused, incremental indexing,
  privacy-first framing, JSON CLI output. Worth watching as a Tantivy
  architecture reference even though it targets Windows.
- **[ultrasearch](https://github.com/Dicklesworthstone/ultrasearch)** — Rust,
  combines NTFS MFT enumeration (Windows-only, Everything-style instant filename
  search) with **Tantivy** for content, multi-process architecture.
  Windows-only, but the split-process design (fast filename path separate from
  content-index path) is directly relevant to a Linux design discussion.
- **[ygrep](https://github.com/yetidevworks/ygrep)** — Rust, **Tantivy**,
  explicitly framed as code search "optimized for AI coding assistants," BM25
  ranking, mtime-based incremental reindex. Closer to the sibling
  filename/code-search slice than to this one, but shows Tantivy is the emerging
  default choice for new Rust entrants in this space generally.
- No credible new **Go**-based full-content desktop indexer for Linux was found
  in this pass; the newcomer energy in 2024–2026 is concentrated in
  Rust+Tantivy.

## Self-throttling: what these three daemons actually do, and what worked

This follows directly from the strongest finding in this file — daemon resource
consumption, not query features, is what kills adoption. All three mechanisms
below were read from primary source (upstream git, not blog posts), because
"does it throttle itself" is exactly the kind of checkable claim that gets
garbled in third-party summaries.

### Baloo — most mechanisms, still the worst reputation

Source: `src/file/priority.cpp`
([github.com/KDE/baloo](https://github.com/KDE/baloo/blob/master/src/file/priority.cpp))
and the systemd unit `src/file/kde-baloo.service.in`.

- **`lowerPriority()`**: `setpriority(PRIO_PROCESS, 0, 19)` — standard nice 19.
- **`lowerIOPriority()`**:
  `ioprio_set(IOPRIO_WHO_PROCESS, 0, IOPRIO_CLASS_IDLE)` via the raw
  `ioprio_set` syscall, falling back to best-effort class 7 if idle-class isn't
  available.
- **`setIdleSchedulingPriority()`**: `sched_setscheduler(0, SCHED_IDLE, ...)` —
  the Linux-specific "only run when literally nothing else wants the CPU"
  scheduling class, one step below nice 19.
- **Battery/AC awareness — in `fileindexscheduler.cpp`**: `m_powerMonitor` is
  checked directly; `powerManagementStatusChanged()` **stops content indexing
  outright** the moment the machine goes on battery
  (`if (isOnBattery && m_indexerState == ContentIndexing) { ... stop }`), and
  resumes when back on AC. This is a real, unconditional behavioral gate, not
  just a priority hint.
- **systemd unit-level cgroup limits** (`kde-baloo.service.in`), which is the
  most concrete and most quotable part of this whole investigation:

  ```
  Slice=background.slice
  CPUWeight=1
  IOWeight=1
  MemoryHigh=25%
  ```

  The unit file's own comment is worth quoting directly: _"We'll basically only
  want to consume resources if they aren't needed anywhere else, hence weights
  are way low."_ `CPUWeight=1`/`IOWeight=1` are the cgroup v2 proportional-share
  minimums (weight is relative to other units in the same slice, floor is 1);
  `MemoryHigh=25%` is a soft memory ceiling that throttles (not kills)
  allocation once crossed.

**Verdict**: Baloo has the _most_ self-throttling machinery of the three —
thread-level scheduling class, I/O class, nice, battery-gating, and cgroup-level
weight/memory limits, layered — and still has the worst, longest-running
reputation for eating a CPU core in this entire survey (reports as recent as
August 2024, see the Baloo section above). The mechanisms are real and correctly
implemented against the Linux APIs; they visibly **do not** prevent the
user-facing complaints. Two candidate explanations, both consistent with the
evidence gathered here and worth distinguishing rather than collapsing into
"throttling doesn't work": (1) `nice`/`ioprio`/`SCHED_IDLE` only matter under
_contention_ — on an otherwise idle desktop a SCHED_IDLE process still gets 100%
of a free core, so a single-core extraction job on a multi-core machine can peg
one core "for free" while looking like the throttling failed; cgroup `CPUWeight`
has the same contention-relative property. (2) `MemoryHigh` throttles rather
than caps, and the reports are frequently about _memory_, not just CPU — a soft
throttle that slows allocation without bounding it can still let RSS climb to
multi-GB under sustained extraction load before the throttle meaningfully bites.
Neither explanation is confirmed against a live measurement in this pass;
flagging both because the evidence doesn't cleanly pick one.

### Tracker / LocalSearch — same OS primitives, explicit about it, no cgroup limits

Source: `src/indexer/tracker-main.c`
([gitlab.gnome.org/GNOME/localsearch](https://gitlab.gnome.org/GNOME/localsearch/-/blob/main/src/indexer/tracker-main.c))
and `src/indexer/tracker-miner-fs.service.in`.

- `initialize_priority_and_scheduling()` is called unconditionally at daemon
  startup and does, in order:
  1. `pthread_setschedparam(pthread_self(), SCHED_IDLE, &sp)` — same idle
     scheduling class as Baloo, applied at the thread level via POSIX threads
     rather than `sched_setscheduler` directly, with an explicit comment noting
     `SCHED_IDLE` is Linux-specific and the FreeBSD-style fallback is to rely on
     the platform's already-low default priority.
  2. Raw `syscall(SYS_ioprio_set, IOPRIO_WHO_PROCESS, 0, ioprio | ioclass)` with
     `IOPRIO_CLASS_IDLE` — same primitive as Baloo, same class.
  3. `nice(19)` — belt-and-suspenders on top of `SCHED_IDLE`. All three calls
     log a plain `g_message` warning on failure rather than hard-failing, and
     the source comments are refreshingly direct about intent
     (`"Setting scheduler policy to SCHED_IDLE"`,
     `"Setting priority nice level to 19"`).
- **Battery awareness exists as a library** (`src/common/tracker-power-upower.c`
  wraps `libupower-glib`, exposing `on_battery`/`on_low_battery`), but unlike
  Baloo I did not find, in this pass, the equivalent of Baloo's explicit "stop
  content indexing entirely on battery" call site wired to it inside the current
  indexer daemon — `TrackerPower` looks consumed elsewhere in the codebase
  (worth a deeper read before asserting it does or doesn't gate indexing;
  flagging this as **unconfirmed either way** rather than assuming parity with
  Baloo).
- **systemd unit** (`tracker-miner-fs.service.in`): sets
  `Slice=background.slice` and `Type=notify`/`Restart=on-failure`, but — checked
  directly against the current file — carries **no `CPUWeight`, `IOWeight`, or
  `MemoryHigh`** analogous to Baloo's. It relies entirely on the in-process
  nice/ioprio/ SCHED_IDLE calls above plus whatever default weight
  `background.slice` itself carries.

**Verdict**: functionally the same core OS-level throttling as Baloo (nice 19 +
ioprio idle + SCHED_IDLE), applied just as explicitly, with **less** cgroup
backing than Baloo's unit file provides. Tracker/LocalSearch's public reputation
for CPU complaints is real but reads as less severe and less persistent than
Baloo's in the community threads surveyed for this file — a genuinely surprising
result given it has _fewer_ declared throttling layers, which argues against
"more throttling mechanisms = fewer complaints" as a simple causal story, and
for something else being the dominant variable (possibly: how much text
extraction work Tracker's default in-process GLib extractors do per file versus
Baloo's KFileMetaData/poppler pipeline; not verified in this pass).

### Recoll — the most layered self-throttling, and it explicitly documents its own limits

Source: `index/recollindex.cpp`, read via
[Fossies' cross-reference](https://fossies.org/linux/recoll/index/recollindex.cpp),
which is the clearest of the three because Recoll's own docs cite the exact
config knobs by name.

- `setMyPriority()` (the equivalent of Baloo's/Tracker's startup call) reads a
  config key `idxniceprio` (default not fully re-derived from source in this
  pass, but the comment states the intent precisely) and calls
  `setpriority(PRIO_PROCESS, 0, prio)` — same `nice` syscall as the other two,
  but **user-configurable via `recoll.conf`** rather than a hardcoded 19. The
  source comment is explicit about a real interaction: _"will be clamped to 19
  on Linux. Allows the user to set `idxniceprio` to 19 to avoid SCHED_IDLE use"_
  — i.e. Recoll's own authors flag that requesting nice 19 is the escape hatch
  for a user who wants nice-level throttling _without_ the more aggressive
  SCHED_IDLE behavior, because the two are not the same thing and one is not
  simply "more of" the other.
- `#ifdef SCHED_IDLE` block: same
  `sched_setscheduler(getpid(), SCHED_IDLE, &param)` primitive as both other
  tools, used **by default** unless the user has opted for the plain-nice path
  above.
- `rclIxIonice()`: reads `monioniceclass` / `monioniceclassdata` config keys and
  calls a `rclionice()` helper — Recoll's ioprio setting is also
  **user-configurable per role** (the "mon" prefix suggests this applies to the
  real-time monitoring/inotify daemon specifically, separate from batch
  indexing), rather than a single hardcoded idle class.
- **OOM-killer adjustment — unique to Recoll among the three surveyed here**: if
  the `choom` utility is present on the system, `rclIxIonice()` shells out to
  `choom -n <oomadj> -p <pid>` with a default `oomadj` of `300` (also
  user-configurable via the `oomadj` config key). This adjusts the Linux
  OOM-killer's score for the indexing process, making it a **preferred victim**
  if the system runs low on memory — a throttling dimension neither Baloo nor
  Tracker/LocalSearch appears to touch in the source read for this file. Given
  the reader's own stated concern about resource behavior at 200GB/1M-file
  scale, this is arguably the single most directly relevant design detail found
  in this entire slice: making your indexer die first, cleanly, under memory
  pressure, rather than let it (or something else) get killed unpredictably.
- Recoll's **own FAQ** goes further and tells the administrator to layer
  **cgroups or `systemd-run --property=CPUQuota=`** on top of its built-in
  scheduling/priority settings for a hard ceiling
  ([recoll.org — Using Linux cgroups to limit indexing CPU usage](https://www.recoll.org/faqsandhowtos/cgroups_instructions.html)),
  i.e. Recoll's authors are explicit that nice/ioprio/SCHED_IDLE alone are
  **known not to be a hard guarantee** (per the "nice is purely about
  contention" property noted under Baloo above) and a determined user who wants
  a real ceiling needs the cgroup layer regardless of tool.
- Issue history: a user-filed bitbucket issue titled "Recoll needs ionice and
  nice" predates all of the above and is presumably what prompted the current
  implementation — i.e. this was reactive, added after real user complaints
  about indexing slowing down interactive use, the same shape of complaint that
  dogs Baloo and Tracker.

**Verdict**: Recoll has the same core primitives (nice, ioprio, SCHED_IDLE) as
the other two, but is the only one of the three that (a) exposes every knob as
user-configurable config rather than a hardcoded value, (b) adjusts OOM-killer
preference, and (c) has its own docs candidly telling the administrator that the
built-in throttling is necessary-but-not-sufficient and to reach for
cgroups/systemd for an actual ceiling. Whether this translates into fewer
real-world complaints than Baloo's is not something this pass measured —
Recoll's reputation as "the one that doesn't hurt" (stated earlier in this file)
may be as much a function of **not running as a forced daemon by default** as of
its throttling code specifically; the two are easy to conflate and this file has
not cleanly separated them.

### Bottom line for a from-scratch design

1. **All three tools converge on the identical baseline: `nice(19)` + `ioprio`
   idle class + `SCHED_IDLE`.** This triad is clearly the community-standard
   answer to "don't disturb the interactive session," not a differentiator — any
   new tool should treat it as table stakes, not as a design decision to
   deliberate over.
2. **None of that triad is a hard ceiling, and all three ecosystems seem to know
   it** — Recoll's own FAQ says so outright, and the "nice is purely about
   contention" property (a nice-19/SCHED_IDLE process on an _otherwise-idle_
   machine still gets a full core at full speed, cost-free) is a real gap
   between what these mechanisms are commonly believed to do and what they
   actually guarantee. **A cgroup-level hard limit (`CPUQuota=`,
   `MemoryHigh=`/`MemoryMax=`, `IOWeight=`) is the only mechanism in this survey
   that bounds resource use in absolute terms rather than relative-to-contention
   terms**, and only Baloo ships one by default in its unit file;
   Tracker/LocalSearch and Recoll leave it to the administrator.
3. **Baloo is the one data point that most directly answers "does doing all of
   this actually work"**, because it has the most complete set of mechanisms
   (including the only default cgroup limits and the only confirmed
   unconditional battery-based indexing pause) and still has the worst,
   longest-running reputation for exactly the failure mode all of that machinery
   targets. Read generously, this says the mechanisms are real but insufficient
   at the _default_ settings chosen (`MemoryHigh=25%` is generous on a low-RAM
   machine; `CPUWeight=1` only helps under contention). Read less generously, it
   suggests the actual driver of complaints may be something the priority/cgroup
   layer cannot fix at all — total work volume (indexing 1M files' worth of
   content extraction is simply a lot of CPU-seconds no matter the scheduling
   class) or extractor correctness (a single file that hangs/loops inside a
   poppler or exiv2 call burns its allotted low-priority slice indefinitely
   rather than finishing). This file cannot distinguish those two explanations
   with the primary sources gathered — it's the most important open question
   this slice surfaced and did not answer.
4. **Recoll's OOM-adjustment (`choom -n 300`) is the one mechanism this survey
   found that none of the others use**, and it directly targets a distinct
   failure mode (getting OOM-killed unpredictably, or worse, causing something
   _else_ to be OOM-killed) rather than the CPU-hogging failure mode the
   nice/ioprio/SCHED_IDLE triad targets. For a build explicitly prioritizing a
   dense index over a fast one — meaning more RAM pressure during
   merge/compaction, not less — this is worth treating as a first-class design
   requirement, not an afterthought bolted on after user complaints the way
   Recoll's own history suggests it was.
5. **Queueing/backoff on user activity** (the reader's own phrase: "queue on
   burst") is not something any of the three tools implement as a distinct
   mechanism beyond the OS-level scheduling primitives above — none of the three
   source trees read for this file contain input-idle detection (e.g.
   X11/Wayland idle time, or watching for foreground-app CPU spikes) that pauses
   or resumes indexing based on _interactive_ activity specifically, as opposed
   to battery state (Baloo) or raw scheduling priority (all three). If "don't
   index while the user is actively compiling/building" is a design goal, none
   of these three projects is prior art for it — that would be new design work,
   not a pattern to copy.

## Done-note

**What I could not verify to my own bar, and should not be cited more firmly
than labeled here:**

- ~~Baloo's exact current on-disk storage engine/format~~ — **resolved.** Read
  `github.com/KDE/baloo/tree/master/src/engine` directly: every database class
  `#include <lmdb.h>` and operates on `MDB_dbi`/`MDB_txn`. LMDB is confirmed
  current, not a stale or in-flux detail. Schema written up in the Baloo section
  above (PostingDB/PositionDB/DocumentDB/etc.), sourced from the header comments
  plus [Phabricator T9805](https://phabricator.kde.org/T9805). One thing I did
  _not_ resolve: whether the LMDB corruption-on-crash complaints referenced in
  older forum threads are still live on current LMDB versions bundled by
  distros, or were fixed upstream in LMDB itself — that's a version-pinned claim
  I didn't chase and shouldn't be repeated without a date attached.
- ~~**regain**'s exact last-release date~~ — **resolved.** SourceForge project
  page's own timestamps put last activity at 2013-06-09, page metadata last
  touched 2014-07-30. Dead since 2013, `[upstream-documented]` now rather than a
  search-summary guess.
- Cerebro's current Linux content-search state — coordinator said to drop it if
  not worth the time, and it isn't: no content-index engine of its own,
  plausibly irrelevant to a content-search report entirely. Left the existing
  one-paragraph hedge in place and did not chase it further.
- No **[measured-by-me]** numbers exist anywhere in this file — I was explicitly
  told not to install/run anything, and I didn't. Every performance/size number
  here is `[upstream-documented]` (Recoll's own perf page) or
  `[community-anecdote]` (forum reports of Baloo CPU/RAM). **There is no
  independent, controlled benchmark comparing Recoll vs. Tracker vs. Baloo vs. a
  hypothetical Tantivy-based tool at the 1M-file/10-200GB target scale that I
  could find anywhere.** This is a real gap in the public record, not just a gap
  in this research pass — if the overall report needs a head-to-head number at
  that scale, someone has to generate it, because it doesn't appear to exist.

**Contradictions found between sources:**

- "DocFetcher is abandoned" is a widely repeated claim (and was in my own first
  search-result summary) that is **factually wrong as of Jan 2026** — the
  community fork cut a release that month. State the more precise claim
  ("original author inactive, community fork still shipping occasional
  releases") rather than "dead," which several third-party review sites assert
  flatly.
- Baloo's own wiki frames it as architecturally "decentralized... no central
  database," which turned out on inspection to be a claim about process
  architecture, not storage engine — the actual storage is a single well-defined
  LMDB environment with ~10 named sub-databases, which is not what "no central
  database" suggests to a reader. Not a factual error in KDE's docs so much as a
  framing that undersells how conventional the storage layer actually is; the
  "decentralized" framing is really about the three cooperating _processes_
  (file miner, extractor, index), not the disk format.
- The multi-year stream of Baloo CPU/RAM complaints turns out, per the
  self-throttling section below, to sit alongside real and fairly sophisticated
  self-throttling code (SCHED_IDLE, ioprio idle, cgroup weights,
  battery-awareness) — i.e. this is not a case of nobody having tried. The
  throttling measures visibly do not prevent the complaints, which is the more
  interesting and more useful finding than "Baloo doesn't try to throttle
  itself."

**What the overall report must not miss:**

1. **Xapian (Glass) and Lucene are both mature B-tree/segment-based
   inverted-index designs from the 2000s–2010s; nothing here uses an LSM-tree.**
   If the eventual Rust build is weighing Tantivy (LSM-like segment-merge
   design, closer to Lucene than to Xapian) against a custom structure, the fact
   that **every single successful long-lived Linux desktop search tool in this
   survey is Xapian- or Lucene-family** is a strong prior worth stating plainly,
   not just implying.
2. **The daemon-resource-consumption failure mode is the single most repeated
   cause of user abandonment across this entire slice** — it killed or crippled
   the reputation of Beagle (2006-2009) and Baloo (2020s), and is the explicit
   reason Recoll (no forced daemon) and Catfish (no index at all, just shells to
   grep) get recommended as "the ones that don't hurt." Any new Rust tool's
   single most important design decision, per this evidence, is bounding
   background CPU/RAM/IO deterministically — not maximizing indexing speed or
   query features.
3. **Nobody in this survey does content-search with regex except
   Elasticsearch-backed tools (sist2, Nextcloud FTS via ES) and Obsidian's own
   `/regex/` core feature.** Xapian, Lucene/DocFetcher, Tracker/SPARQL, and
   Baloo all lack it. If regex search is a design goal for the new tool, there
   is no embedded (non-ES) prior art in this survey to copy from directly —
   that's a genuine gap, not an oversight on my part.
4. The ecosystem's own launcher extensions (Ulauncher wrapping
   Recoll/Tracker/DocFetcher/locate) effectively vote **Recoll and Tracker** as
   the two backends worth integrating with; nobody bothers wrapping Baloo
   despite it shipping on every KDE desktop, which is worth reading as an
   implicit reputation/API-friction signal on top of the explicit CPU/RAM
   complaints.
5. I would flag the brief's scope as reasonable as drawn, with one suggestion: a
   companion note comparing this list's index-size ratios (Recoll: ~4% for a PDF
   corpus) against Tantivy/Lucene's typical published index-to-corpus ratios
   (usually cited in the 10–30% range for general text in Lucene's own
   literature) would sharpen the "what does 200GB of mixed source+office+email
   actually cost me on disk" question the reader almost certainly cares about —
   I did not chase that number down for Tantivy specifically since it's arguably
   another slice's territory, but flag it here so it doesn't fall through the
   cracks between slices.
