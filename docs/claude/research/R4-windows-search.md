# R4 — Windows Desktop Search

**Framing note**: this is competitive intel for a Rust engine already being
built (Super Ferret), not a buyer's guide — no tool below is being evaluated for
purchase. Target scale is a personal workstation: ~1M files today, headroom to
~5M, tens to a few hundred GB, source/text/config prioritized over PDF/Office
over media metadata, email/OCR deferred to later plugins. Stated design
preference is **denser index over raw query speed** — where a source below
trades index size against latency, both numbers are given and the tradeoff is
named explicitly rather than assuming faster wins. Licence is noted for each
tool; it's moot for the closed-source Windows-native ones but matters for
anything with readable source (dtSearch's engine internals are not public
despite being licensable; DocFetcher/Recoll are the only genuinely open designs
in this slice).

## Summary table

| Tool                                             | Index structure                                                                                               | Filename / content                             | Regex?                                                                   | Formats                                                                                                                                      | Licence                                                                            | Status (2026)                                                                             |
| ------------------------------------------------ | ------------------------------------------------------------------------------------------------------------- | ---------------------------------------------- | ------------------------------------------------------------------------ | -------------------------------------------------------------------------------------------------------------------------------------------- | ---------------------------------------------------------------------------------- | ----------------------------------------------------------------------------------------- |
| **Everything** (voidtools)                       | Proprietary in-memory structure built from NTFS MFT + live USN journal; optional on-disk content index (v1.5) | Filename/path always; content optional (v1.5+) | Yes, via `regex:` — PCRE-style engine (see below)                        | N/A for filenames; content index is format-agnostic (best on plain text; needs external filters for binary formats — unclear, see done-note) | Closed-source, free binary (donation-ware)                                         | Actively developed, v1.5 stable branch, v1.5 alpha ongoing                                |
| **Windows Search** (built-in)                    | ESE (JET Blue) database, `Windows.edb`, inverted-index-like property store                                    | Both (filename always; content via IFilter)    | No native regex in AQS                                                   | ~50+ via IFilter/property-handler plugins (Office, PDF via Adobe/Foxit plugin, email, media props)                                           | Closed-source, part of Windows                                                     | Ships in Win10/11; largely unchanged core since Vista, some UI/backend tweaks in Win11    |
| **dtSearch**                                     | Proprietary compressed inverted index, own format                                                             | Content-first, also filename/metadata          | Yes, native regex (`##` prefix) plus fuzzy/proximity/stemming            | 25+ document filters (Office, PDF, email/PST, archives, etc.)                                                                                | Closed-source, commercial (source escrow / SDK licensable, engine itself not open) | Actively sold and updated (legal/eDiscovery market — e.g. Relativity)                     |
| **FileLocator Pro / Agent Ransack** (Mythicsoft) | Optional index (Pro); Lite version does live unindexed scanning                                               | Both                                           | Yes, full regex on both filename and content boxes, independent of index | Broad text/Office extraction; OCR in Pro                                                                                                     | Closed-source, freeware (Lite) / commercial (Pro)                                  | Actively maintained, v9.x line                                                            |
| **X1 Search**                                    | Proprietary index, historically federates Outlook/Exchange/PST + filesystem                                   | Both                                           | Limited (mostly boolean/proximity, not full regex)                       | Broad (email-centric: PST/OST, Office, attachments)                                                                                          | Closed-source, commercial                                                          | Still sold, enterprise/legal focus                                                        |
| **Copernic Desktop Search**                      | Proprietary index                                                                                             | Both                                           | No advertised regex                                                      | 170+ file types claimed                                                                                                                      | Closed-source, commercial                                                          | Active, "whole-system indexer" branding, home license had a 75,000-file cap in some tiers |
| **Lookeen**                                      | Proprietary index, Outlook/Exchange-centric                                                                   | Both (Outlook-heavy)                           | No                                                                       | Outlook/PST/Exchange/Public Folders + desktop                                                                                                | Closed-source, commercial                                                          | Active, Outlook-search niche                                                              |
| **DocFetcher** (Windows port)                    | Lucene-based index (Java)                                                                                     | Content + filename                             | Lucene query regex to a limited extent                                   | Office, PDF, HTML, plain text, archives                                                                                                      | **Open source, EPL** (Eclipse Public License) — readable design                    | Low/no active development; still downloadable                                             |
| **Recoll** (Windows port)                        | Xapian index                                                                                                  | Content + filename                             | Yes (Xapian supports regex-ish extensions; primarily boolean/phrase)     | Very broad via helper filters (same engine as Linux Recoll)                                                                                  | **Open source, GPLv2** (Recoll) over **Xapian, MIT/GPL-dual** — readable design    | Windows port exists but is the minor platform for this Linux-first tool                   |

---

## Where each tool sits on the size/speed curve

Given the stated preference for a denser index over faster search, here is every
hard number this pass found, plotted on both axes rather than treated as a
single "how fast" score:

- **Everything**: ~100 MB RAM per 1M files [community-anecdote,
  converged estimate, range 75-200 MB/million across threads] for an **all-in-memory**
  structure with no on-disk cost beyond the ~45 MB/million persisted snapshot [community-anecdote].
  This is the extreme "spend RAM, buy instant" corner — it is not memory-efficient
  (it holds the whole namespace resident), but it is _index-format_-efficient: no
  compression, no on-disk B-tree overhead, because the whole thing lives as one process's
  heap. For a Rust engine targeting a denser _on-disk_ index this is close to an
  anti-pattern to copy — Everything's efficiency story is "small enough to hold entirely
  in RAM," not "compact on disk," and at 5M files (upper end of the stated target)
  that's ~500 MB resident just for filenames/paths, before any content index.
- **Windows.edb**: the opposite corner — large **and** slow-ish, i.e. dominated
  by neither axis cleanly. Reported real-world blowouts of 92-107 GB on 300-500
  GB source volumes [community-anecdote] put it at roughly **20-35% of
  source-corpus size**, which is worse than dtSearch's worst-case published
  ratio (1/3) and far worse than dtSearch's large-collection figure (~15%), for
  a system with no reputation for being especially fast either. This is the
  cautionary example, not a design point worth emulating on either axis: the
  size comes from unbounded per-item content snippets
  (`System_Search_AutoSummary`) and whole-file reindexing of appended logs, i.e.
  accidental bloat rather than a chosen size/speed tradeoff.
- **dtSearch**: the only vendor with a published, defensible ratio — **~15% of
  corpus size at large scale, up to 1/3 at small scale** [vendor-claimed]. No
  independent latency numbers were found in this pass, but dtSearch is generally
  regarded (by the legal/eDiscovery market it serves) as fast enough for
  interactive use at multi-GB-to-TB scale, so 15% is a reasonable data point for
  "compact and still fast," and the single best external benchmark to hold a
  Rust index-size claim against.
- **FileLocator Pro / Agent Ransack**: no ratio published; the tool's own design
  statement is the tradeoff — Lite mode chooses **zero index, all latency**
  (scans live), Pro chooses **an index, presumably trading some size for
  speed**, with no published number for how much. Structurally useful as a
  design precedent (offer both modes) rather than a numeric one.
- **Recoll / DocFetcher (Xapian / Lucene)**: no Windows-specific figures were
  found, but both engines are documented elsewhere (outside this Windows slice)
  as producing indexes in a broadly similar range to dtSearch's (roughly 10-30%
  of corpus depending on options) — flagged as [community-anecdote] carried over
  from general Xapian/Lucene knowledge rather than a Windows-specific citation,
  and worth confirming against whichever slice of this survey covers
  Recoll/Xapian directly rather than re-deriving it here.

The takeaway for Super Ferret: **dtSearch's ~15% is the number to design
against**, not Everything's all-RAM model and not Windows Search's unbounded
property store. Everything is worth studying for its _update_ mechanism
(MFT+USN), not its _index_ efficiency — it never had to solve the size-density
problem because it never put the index on disk in a compact form at all.

## Everything (voidtools)

### The core trick: MFT + USN journal, not a filesystem crawl

Everything's headline capability — indexing tens of millions of files in seconds
and returning keystroke-latency search results — comes from refusing to use the
normal Win32 directory-enumeration APIs (`FindFirstFile`/`FindNextFile`) at all.
Instead:

1. **Initial index build**: Everything opens the raw NTFS volume (`\\.\C:`) and
   reads the **Master File Table (MFT)** directly, in one sequential pass. The
   MFT is the record NTFS itself already maintains — one entry per
   file/directory, holding name, parent reference, size, and timestamps — so
   this is reading the filesystem's own metadata structure rather than asking
   the filesystem driver to walk it name-by-name and reference-count every
   directory. This is what makes first-index time proportional to volume size /
   MFT size rather than proportional to the number of `FindNextFile` round-trips
   a crawl would need. [community-anecdote / consistent with voidtools'
   own long-standing forum explanations,
   e.g. https://www.voidtools.com/forum/viewtopic.php?f=7&t=5433 (fetch blocked
   by 403 during this research — see done-note) — corroborated by third-party
   summary: https://voidtools.com/forum/viewtopic.php?t=12779]
2. **Ongoing updates**: after the initial scan, Everything stops reading the MFT
   and instead polls the **USN (Update Sequence Number) change journal** —
   NTFS's append-only log of every create/delete/rename/attribute-change on the
   volume — roughly every second, applying deltas to its in-memory index. The
   USN journal is bounded (Microsoft docs describe it as a capped, circular log;
   community accounts describe roughly "about a week" of history under typical
   activity, though the actual retention is governed by the configured journal
   size, not wall-clock time) [community-anecdote,
   https://voidtools.com/forum/viewtopic.php?t=12779]. If Everything (or its background
   service) has been off long enough that the USN journal has wrapped past the last
   position it read, it must fall back to a fresh full MFT re-read for that volume.
3. **Why this needs admin rights**: opening a volume for raw/low-level access
   (`\\.\C:`) and reading USN journal records both require elevated privileges
   under Windows' security model — a normal user-mode process cannot open a raw
   volume handle. This is why standalone Everything historically prompted for
   "Run as Administrator" to index NTFS volumes at all [community-anecdote:
   https://www.voidtools.com/forum/viewtopic.php?t=9398]. voidtools' answer to this
   friction is the **Everything Service**: install it once (as an admin, or via elevated
   install), and it holds the elevated volume handle persistently; the ordinary (non-admin)
   Everything.exe client then talks to the service rather than opening the volume
   itself, so day-to-day use doesn't need an elevated GUI session [community-anecdote:
   https://www.voidtools.com/support/everything/everything_service/,
   https://www.voidtools.com/forum/viewtopic.php?t=6606]. Community reports also
   note a nuance that as a standard user (with the service installed) the raw-device
   path changes character — `\\.\X:` vs a `\\?\Volume{guid}` style handle — which
   is consistent with the service being the actual privileged holder of the volume
   handle rather than the client [community-anecdote].
4. **Non-NTFS volumes**: FAT/FAT32, exFAT, network shares, optical media, and
   anything without an MFT/USN journal cannot be indexed this way — there is no
   MFT to read and no USN journal to subscribe to. For these, Everything falls
   back to conventional **folder indexing** — an ordinary recursive directory
   scan using the standard file APIs, refreshed periodically or via filesystem
   change notifications rather than the journal trick — which is slower to build
   and cannot track changes made while Everything isn't watching, as precisely
   as the NTFS path can [upstream-documented:
   https://www.voidtools.com/support/everything/indexes/, which
   explicitly separates "NTFS index", "ReFS index", "Folder index", and "File List"].
5. **ReFS**: voidtools' own documentation lists ReFS as a first-class supported
   index type alongside NTFS ("Everything automatically indexes fixed NTFS and
   ReFS volumes") [upstream-documented:
   https://www.voidtools.com/support/everything/indexes/]. ReFS also maintains a
   form of USN-journal-equivalent change tracking, so the mechanism generalizes,
   though voidtools does not publish the low-level details of how the ReFS path differs
   internally from the NTFS path, and this research did not find a public breakdown
   of ReFS-specific internals — treat the ReFS support as confirmed at the "it works"
   level, not at the mechanism level.

### In-memory structure: mostly closed / inferred

voidtools does not publish source code or a data-structure spec for Everything.
What can be said with actual sourcing:

- The in-memory index holds **name and path information** always; size and
  modified-date are indexed by default; folder size, creation date, access date,
  and attributes are **optional** extras that cost additional memory when
  enabled [upstream-documented: https://www.voidtools.com/support/everything/indexes/].
- **Memory footprint**: community and FAQ-adjacent figures converge around
  **~100 MB of RAM per 1,000,000 indexed files**, with on-disk index size around
  **~45 MB per 1,000,000 files** [community-anecdote / possibly
  FAQ-adjacent, precise original source not confirmed in this pass — treat as
  a converged community estimate rather than a single authoritative citation:
  https://www.voidtools.com/faq/,
  https://www.voidtools.com/forum/viewtopic.php?t=8318,
  https://www.voidtools.com/forum/viewtopic.php?t=9024]. A fresh Windows 11 install
  (~250,000 files) reportedly uses roughly 35 MB RAM / <14 MB on disk for its index
  — roughly consistent with (a little better than) the per-million figure scaled
  down [community-anecdote]. Other forum threads report higher steady-state numbers
  (150–200 MB per million during active indexing, before settling) [community-anecdote:
  https://www.voidtools.com/forum/viewtopic.php?t=11234,
  https://www.voidtools.com/forum/viewtopic.php?t=9014]. **No official, single vendor-published
  number was found**; the 100 MB/million figure is the value that recurs most consistently
  across independent forum threads and is the best available estimate, but it is
  a converged community figure, not a benchmark you should cite as vendor-measured.
- The name lookup structure is widely assumed by the community to be some form
  of sorted/compact string table enabling very fast prefix and substring
  matching (Everything's search-as-you-type feel implies an index that supports
  incremental substring filtering over millions of rows in milliseconds), but
  **voidtools has not published the actual data structure**, and no credible
  reverse-engineering writeup was located in this pass. Do not present a
  specific structure (trie, suffix array, etc.) as fact — it is unknown /
  closed-source.

### Startup / index-build time

No rigorous third-party benchmark with a controlled volume size and disk type
was found in this pass. Community anecdotes describe multi-million-file NTFS
volumes indexing "in seconds" on the initial MFT read, which is the entire point
of the MFT-read strategy vs. a directory crawl (crawls of similar volumes with
tools like `dir /s` or Everything's own "folder index" fallback take much longer
— minutes, by comparison, in various forum reports) [community-anecdote]. Given
the lack of a controlled benchmark, **no number is asserted here as measured** —
flagged as a gap in the done-note.

### Everything 1.5: content indexing and richer filtering

Everything 1.5 (still, as of this research, partly in alpha/beta release
channels alongside the 1.4 stable line) adds:

- **Optional content indexing**: users can enable indexing of file _contents_ in
  addition to name/path/metadata. This is opt-in per Everything's indexing
  options (content indexing is expensive in both index size and CPU, consistent
  with dtSearch/Windows Search's own tradeoffs) [upstream-documented:
  https://www.voidtools.com/support/everything/indexes/, forum
  context: https://www.voidtools.com/forum/viewtopic.php?t=9996].
- **`regex:` function** in the query language, applying a regular expression to
  filenames or (combined with `content:`) to file contents. Community/forum
  discussion around Everything 1.5's regex content search references PCRE-style
  syntax features — named groups, lookaheads, the `(?m)` multiline flag, and a
  dedicated `multiline:` search modifier that changes whether `^`/`$` anchor to
  the whole text or per-line [community-anecdote / forum:
  https://www.voidtools.com/forum/viewtopic.php?t=12803,
  https://www.voidtools.com/forum/viewtopic.php?t=14829].
  **voidtools has not published which exact regex engine/library backs this**
  (e.g., whether it's a bundled PCRE, PCRE2, or a hand-rolled engine) — this is
  inferred from the syntax surface it exposes, not confirmed from source or an
  explicit statement in the pages fetched during this research. Flagged as
  unverified in the done-note.
- **Functions/modifiers** (query-language building blocks), documented at
  voidtools' "Advanced Search Functions" reference page
  (https://ftp.voidtools.com/support/everything/advanced_search_functions/) and
  forum syntax threads, include (non-exhaustive, but representative of the
  breadth a Linux competitor would need to match):
  - `size:` (e.g. `size:1kb`, ranges, `metric:size:1kb` for decimal/metric
    interpretation) [upstream-documented, from forum: https://www.voidtools.com/forum/viewtopic.php?t=10860]
  - `dm:` (date modified, e.g. `dm:today`, `dm:april`, ranges) and sibling date
    functions for created/accessed dates
  - `content:` (content search, combinable with `regex:`)
  - `ext:`, `type:`, `folder:`, `file:`, `empty:`, `dupe:`/`dupe` —
    duplicate-detection style modifiers, `attrib:`, `parent:`, `child:`,
    `sizerank:`, and various date/attribute predicates
  - Boolean operators: `AND`/`&&`/space (implicit AND), `OR`/`|`, `NOT`/`!`,
    parenthesized grouping
  - Wildcards: `*` and `?` glob-style wildcards, distinct from and combinable
    with `regex:`
  - Combined example from a forum post: `*.exe|dm:today` — OR-combining a
    wildcard filename match with a date-modified predicate
    [community-anecdote/forum].

This breadth — a compact but expressive query micro-language covering booleans,
wildcards, ranges, dates, sizes, attributes, content, and full regex, all
evaluated live against an in-memory index as the user types — is arguably as
much of a differentiator as the raw indexing speed, and is the part most
directly worth reverse-specifying for a competing implementation.

---

## Windows Search (built-in)

### Architecture

Windows Search decomposes into several cooperating processes, per Microsoft's
own documentation and long-standing community architecture write-ups:

- **`SearchIndexer.exe`** — the core Windows Search **service** process. It owns
  the index (the ESE database) and the list of URIs/items queued for
  (re)indexing, and it exposes the query APIs (OLE DB provider,
  `ISearchQueryHelper`, the `search-ms:` protocol) that Explorer, Outlook, Start
  menu search, etc. use [upstream-documented:
  https://learn.microsoft.com/en-us/windows/win32/search/-search-ifilter-about
  and
  related Win32 search docs].
- **`SearchProtocolHost.exe`** — hosts **protocol handlers**, which know how to
  enumerate items in a given namespace (the filesystem, a mapped mailbox, a
  custom data source registered by a third-party app). The indexer calls into
  the protocol handler and asks it to hand back items needing indexing.
- **`SearchFilterHost.exe`** — hosts **IFilters** and **property handlers**,
  running as a low-integrity, sandboxed process specifically because the code
  that parses arbitrary untrusted file formats (PDF parsers, Office parsers,
  third-party plugin filters) is the highest-risk code in the pipeline;
  isolating it limits the blast radius of a parsing exploit
  [upstream-documented: https://learn.microsoft.com/en-us/windows/win32/search/-search-ifilter-about].
- **IFilter interface**: the plugin contract third parties implement to teach
  Windows Search a new file format. An IFilter's job is to walk a document and
  emit (a) chunks of extracted text (with position information, so
  phrase/proximity queries are possible) and (b) chunks of property values
  (author, title, custom metadata). Windows Search tokenizes the returned text
  (word-breaking, normalization — casing, accents) before inserting it into the
  index [upstream-documented: https://learn.microsoft.com/en-us/windows/win32/search/-search-ifilter-about].
- **Property handlers** are the complementary interface for structured metadata
  (EXIF, ID3, Office document properties) independent of full-text content.
- The **gathering/queue** concept: the indexer maintains a work queue (referred
  to in forensic literature via the `SystemIndex_Gthr` / `SystemIndex_GthrPth`
  tables — see below) of items discovered by protocol handlers that are pending
  a filter pass; this is how a bulk file-copy or a resumed-after-sleep indexer
  catches up incrementally rather than rescanning everything.

### On-disk: `Windows.edb`, an ESE/JET Blue database

- Windows Search stores its index in a single **ESE (Extensible Storage Engine,
  aka "JET Blue")** database file, historically at
  `C:\ProgramData\Microsoft\Search\Data\Applications\Windows\Windows.edb`
  [forensics-literature:
  https://www.levelblue.com/blogs/spiderlabs-blog/windows-search-index-the-forensic-artifact-youve-been-searching-for/,
  https://forensafe.com/blogs/winsearchindex.html].
  ESE is the same embedded-database engine that backs Active Directory
  (NTDS.dit), Exchange's mail stores (historically), and Windows'
  `Extensible Storage Engine` APIs generally — a B-tree-based ISAM engine, not a
  relational SQL database, though tools built atop it (like the Windows Search
  query interface) present a SQL-like `OLE DB` surface over it.
- **Schema shape**, per forensic tooling/write-ups that parse the file directly
  (Microsoft does not publish the schema as an API contract, since applications
  are meant to go through the query API, not the raw file):
  - `SystemIndex_Gthr` and `SystemIndex_GthrPth` — the **gathering** tables,
    tracking which items (by `ScopeID`/`DocumentID`) are queued/known and
    reconstructing full paths from the scope+document ID pair
    [forensics-literature: LevelBlue SpiderLabs blog above].
  - `SystemIndex_PropertyStore` (and, in some Windows versions, a
    `SystemIndex_1_PropertyStore` variant) — described as the "forensic
    cornerstone": this is where per-item metadata properties **and extracted
    content are actually stored**, including a `System_Search_AutoSummary` field
    holding a text summary/snippet of file contents extracted by the filter for
    that item [forensics-literature: LevelBlue SpiderLabs blog]. This is notable:
    rather than a classical separate "inverted index" table structurally distinct
    from a "documents" table, forensic literature describes the property store itself
    as carrying both structured properties and indexed/extractable text-derived fields,
    with the actual word-level inverted index encoded in ESE's internal structures
    rather than exposed as an obviously named SQL-like table.
  - Deleted/tombstoned records are recoverable in some cases because ESE's
    page-based storage doesn't necessarily zero out payload immediately on
    logical delete — this is the basis for forensic recovery tools' claims of
    recovering deleted-file evidence from `Windows.edb` [forensics-literature:
    multiple sources above].
  - Direct table/schema inspection is normally done via **`esentutl`**
    (Microsoft's built-in ESE utility) or third-party tools like
    `esedbinfo`/`esedbexport` (from libesedb) or dedicated forensic tools
    (WinSearchDBAnalyzer, and others) rather than any documented Microsoft
    schema reference — Microsoft explicitly does not document `Windows.edb`'s
    internal table layout as a stable contract, which is exactly why forensic
    literature (reverse-engineered) is the best source, and also why it
    "frequently restructures" across Windows versions per at least one source
    [forensics-literature: https://forensafe.com/blogs/winsearchindex.html].
- **Windows 11 changes**: this research did not find a specific, well-documented
  structural change to the ESE schema itself for Windows 11 (as opposed to
  feature/UI changes to Windows Search, e.g. cloud content search, Bing
  integration in Start menu search). Flagged as unverified/not found rather than
  asserting no change occurred.

### Index size vs. corpus size

- Microsoft's own support article acknowledges the problem class directly:
  **"The Windows.edb file grows very large in Windows 8 or Windows Server
  2012"** [upstream-documented:
  https://support.microsoft.com/en-us/topic/the-windows-edb-file-grows-very-large-in-windows-8-or-windows-server-2012-ab43f14c-7e22-34c5-e704-e4ad1e39871c],
  describing cases where the file can exceed **50 GB**.
- Community-reported extreme cases: a 300 GB (278 GiB) drive with the index
  growing to **107 GiB** (≈38–50% of the drive, depending on how you compute the
  percentage) [community-anecdote], and a 500 GB drive with a **92 GB**
  `Windows.edb` [community-anecdote]. Common root causes cited: indexing mail
  stores (PST files) with content indexing on, and indexing
  continuously-appended log files, which causes the property store to accumulate
  huge amounts of re-extracted text content over the file's lifetime rather than
  a bounded per-file cost [community-anecdote / Microsoft support article].
- **No official "expected ratio" is published by Microsoft** the way dtSearch
  publishes one (see below) — the size is emergent from what's indexed
  (filename-only items are cheap; content-indexed items, especially
  frequently-modified large text files or archives, are not), and Microsoft's
  own guidance in these cases is essentially "exclude the offending locations
  and rebuild," not "here is the expected ratio."

### Query: Advanced Query Syntax (AQS)

- AQS is the default query language layered over the ESE-backed index, exposed
  through Explorer's search box, `search-ms:` URIs, and the OLE DB provider for
  programmatic queries [upstream-documented:
  https://learn.microsoft.com/en-us/windows/win32/lwef/-search-2x-wds-aqsreference,
  https://learn.microsoft.com/en-us/windows/win32/search/-search-3x-advancedquerysyntax].
- Supports: property-scoped queries (`kind:`, `datemodified:`, `size:`,
  `author:`, arbitrary indexed properties), **boolean operators**
  `AND`/`OR`/`NOT` (must be uppercase — the one case-sensitive part of an
  otherwise case-insensitive syntax), **exact phrase matching** via double
  quotes, and wildcard-ish "starts with" behavior on word stems (AQS does
  word-breaking/stemming rather than true glob wildcards).
- **No native regular-expression support** in AQS — this research found no
  Microsoft documentation exposing a regex mode for the built-in query language,
  consistent with the general reputation that Windows Search's query power is
  far behind dedicated content-search tools.
- **Proximity search**: not found in Microsoft's own AQS documentation as a
  supported operator. Flagged as "not found" rather than asserting it
  categorically does not exist anywhere in the stack, but there is no
  first-party doc describing a proximity operator analogous to dtSearch's `w/n`.

### Reputation and what actually goes wrong

The commonly cited failure modes, cross-referenced against the technical picture
above:

1. **Unbounded index growth** relative to corpus, as documented by Microsoft's
   own KB article and widely reported for mail-heavy or log-heavy indexed
   locations — a direct consequence of the property-store design storing
   extracted content/snippets per item rather than a size-capped structure.
2. **Indexer service resource contention** — `SearchIndexer.exe` and its
   filter-host children doing IFilter parsing work compete for CPU/disk I/O,
   especially right after a large file operation or a forced rebuild; this is a
   longstanding, widely reported (if less rigorously documented) user complaint.
3. **Silent index corruption/staleness** requiring a manual rebuild (Control
   Panel → Indexing Options → Advanced → Rebuild) — an operational sign that the
   ESE store or its gathering queue can get into states Windows itself can't
   self-heal from, which is part of why forensic/parsing tools exist for a file
   most users never look inside.
4. **Coverage gaps** — items outside indexed locations, or files whose IFilter
   isn't installed/registered (a very common complaint with third-party
   formats), simply don't appear in results at all, with no fallback slow-scan
   the way Everything or a `grep`-style tool would provide.

---

## dtSearch

- Positioned as a serious commercial full-text search **engine/SDK** (not just
  an end-user app) used inside legal/eDiscovery platforms such as Relativity, as
  well as its own desktop products.
- **Index size ratio — vendor-published**: "generally about 1/8 to 1/3 the size
  of the original documents," with large collections trending toward **~15%**
  [vendor-claimed: https://support.dtsearch.com/faq/dts0142.htm]. This is dtSearch's
  own stated figure and is the one hard, citable "index-to-corpus ratio" number found
  in this entire survey — worth using as the benchmark a Linux competitor's own index-size
  claims should be checked against.
- **Regex**: native, invoked with a `##` prefix in query syntax
  [vendor/integrator-documented:
  https://help.relativity.com/10.3/Content/Relativity/Regular_expressions/Using_regular_expressions_with_dtSearch.htm],
  and combinable with fuzzy search, stemming, and proximity operators in the same
  query.
- **Fuzzy search**: matches spelling variants of a term (edit-distance style).
- **Proximity search**: supported (`w/n` style "within n words" operators are
  the conventional dtSearch syntax, per integrator docs).
- **Format coverage**: dtSearch's own marketing states 25+ (often cited higher,
  up to "hundreds of file types" in some materials) document filters covering
  Office formats, PDF, email containers (PST/OST/MBOX), and compressed archives.
- Pricing is per-developer-license/per-seat commercial, not published as a
  simple consumer price point; this research did not pull current price sheets
  (out of scope of what a citation-worthy source would confirm quickly) —
  flagged as not verified with a number.

## FileLocator Pro / Agent Ransack (Mythicsoft)

- Same underlying engine/codebase; **Agent Ransack** is the free-tier ("Lite")
  branding, **FileLocator Pro** is the paid, more-featured branding — Mythicsoft
  explicitly describes them as the same product line
  [https://www.mythicsoft.com/agentransack/information/, https://www.mythicsoft.com/filelocatorpro/information/].
- **Two distinct modes**, unusually explicit compared to most competitors:
  - **Non-indexed / live search** (what the free Lite tier does): scans matching
    files on demand at search time, with no persistent index. Slower on huge
    trees but always fresh and requires no storage overhead.
  - **Indexed mode** (Pro): builds and maintains a persistent index with a
    real-time-update option, trading storage/build time for query speed, closer
    to Everything/Windows Search/dtSearch's model.
- **Regex**: full regex support, and notably it's offered **independently on the
  filename box and the content box** — i.e. you can regex-match the path/name
  and separately regex-match the content in the same query, which is a genuinely
  distinguishing feature versus tools that only regex one axis.
- Format coverage and OCR are Pro-tier features; the Lite tier is closer to a
  fast, well-UI'd `grep`/`findstr` wrapper than a full content-search engine.

## X1 Search, Copernic Desktop Search, Lookeen, DocFetcher, Recoll (Windows)

- **X1 Search**: enterprise/legal-market tool, historically strong at federating
  **Outlook/Exchange/PST** search with filesystem search under one query
  surface; pricing has reportedly risen significantly, pushing users toward
  cheaper alternatives [community-anecdote: forum threads discussing X1
  pricing complaints]. No public regex support was found documented; its
  differentiator is breadth of connected sources (mail, filesystem, sometimes
  SharePoint) rather than query-language sophistication.
- **Copernic Desktop Search**: consumer/prosumer whole-system indexer, claims
  170+ indexed file types, $29–$96/year tiers [vendor-claimed, from aggregator
  source — not independently verified against Copernic's own site in this pass],
  with at least one lower tier historically capped at a fixed file count (75,000
  files cited for a "home" tier in one source) — flagged as needing verification
  against Copernic's current site before being treated as current.
- **Lookeen**: Outlook/Exchange-centric add-in-style search, from $69
  [vendor-claimed, aggregator-sourced].
- **DocFetcher**: open-source, Lucene-backed (Java) content search tool. It is
  essentially unmaintained/low-activity at this point — worth noting as an
  open-source prior-art example of a Lucene-on-desktop architecture, but not a
  currently thriving competitor.
- **Recoll** on Windows: same Xapian-based engine as the Linux-native tool (in
  scope of a different slice of this survey presumably), ported to Windows;
  treated by the market as the minor platform for a Linux-first tool rather than
  a first-class Windows offering. A Windows build is sold/bundled for a small
  fee (~$5 cited) apparently to fund packaging/maintenance rather than as a
  meaningfully different product from the open-source core.

---

## Design lessons for a Linux builder

**The single most transferable idea is: don't ask the filesystem for its own
metadata one file at a time — read the filesystem's own bulk metadata structure,
and subscribe to its change log instead of polling.** Everything's entire speed
advantage is that `FindFirstFile`/`FindNextFile`-style enumeration does
per-entry work (a syscall, a directory-entry parse) multiplied by file count,
while an MFT read is one large sequential I/O over a structure NTFS already
keeps compact and complete. The USN journal replaces "periodically re-crawl to
notice deletes/renames" with "read an append-only log of exactly what changed."
At the confirmed target scale (~1M files, headroom to ~5M) this is the whole
game: at 1M entries, even a naive walk that's 50-100x slower per-entry than an
MFT read is a few seconds to low tens of seconds of wall-clock, not minutes —
the two designs only diverge sharply at NTFS's own home scale (tens of millions
of files, corporate fileservers), which is well past Super Ferret's stated
target. Don't let "Everything is instant" set an unreachable cold-start bar; set
the bar at the actual corpus size.

**Honest verdict on matching Everything's cold-start behaviour: cannot be
matched at the mechanism level, can plausibly be matched at the scale that
matters.** There is no Linux structure that plays the role of "one sequential
read of a compact, complete, already-existing metadata table" — see below. So a
from-scratch index build on Linux will always be a tree walk, with syscall and
directory-entry-parse overhead per file that NTFS's design lets Everything skip
entirely. That gap is real and structural, not a tuning problem. What's
plausibly closeable is not the mechanism but the _outcome_ at ~1M files:
`getdents64` in large batches plus parallel `statx` across cores can walk a
warm-cache 1M-file tree in low single-digit seconds on decent NVMe — call it one
to two orders of magnitude slower per-entry than an MFT scan, but on a corpus
small enough that the constant-factor gap doesn't translate into a user-visible
difference. **No controlled benchmark for either side at this exact scale was
found in this research pass** — the "low single-digit seconds" figure is
[estimated] from the general shape of parallel `getdents64`

- `statx` walks reported elsewhere, not a measurement of this codebase, and
  should be replaced with a real number from Super Ferret's own prototype before
  it appears in anything client-facing.

**Idle-CPU and I/O priority — this is the requirement Everything's own design
mostly sidesteps and Linux tooling has to solve explicitly.** Everything's
steady state is cheap by construction: once the initial MFT read is done,
"watching for changes" is reading a small, already-computed delta log once a
second — there's no scanning to throttle. A Linux design without a USN-journal
equivalent (below) has no such free lunch: if the update path is periodic
re-walks rather than an event stream, keeping idle CPU low is now the indexer's
own problem, not something inherited for free from the OS. This argues for
`fanotify`-driven event delivery (below) as the primary update path specifically
_because_ it avoids re-walk-and-diff, with a slow, `ionice -c3` (idle I/O
class) + `nice -n19` background full re-scan only as a periodic correctness
backstop (to catch fanotify overflow/missed events, per the durability gap
below) rather than the primary mechanism — the backstop should be rare and
throttled hard, not a scheduled equivalent of Windows Search's gatherer queue
running at normal priority.

**Linux has no single directly equivalent structure, and this is the real
architectural gap a competitor must solve explicitly, not gloss over:**

- There is no cross-filesystem "MFT" — ext4, XFS, Btrfs, and (where present) ZFS
  each keep inode/metadata information in incompatible on-disk formats, and none
  of them expose a supported, documented "give me every inode's metadata in one
  bulk read" interface analogous to reading NTFS's `$MFT` as a file. Reading
  ext4's inode tables or Btrfs's B-trees directly (bypassing VFS) is possible in
  principle (there is prior art in forensic and recovery tooling, and in
  filesystem-specific debug tools like `debugfs` for ext4), but it means **one
  bespoke, fragile, format-version-sensitive reader per filesystem** rather than
  one MFT reader that covers "the vast majority of desktop Windows disks are
  NTFS." This asymmetry (one dominant format on Windows vs. several
  actively-used formats on Linux, each evolving) is probably the single biggest
  reason nothing that plays Everything's exact trick exists on Linux.
- Btrfs does expose `btrfs subvolume find-new` and there's a generation-number
  mechanism that gives a rough analog of "what changed since generation N" for
  snapshot-aware incremental use cases, but it is not a general-purpose,
  always-on change journal comparable to USN, and it's Btrfs-specific.
- The realistic Linux equivalent for the _change-tracking_ half (not the _bulk
  initial read_ half) is **`fanotify`** with `FAN_MARK_FILESYSTEM` /
  superblock-wide watches (Linux 5.1+/5.9+ depending on features needed) or, at
  lower scale, `inotify`. fanotify with filesystem-wide marks can watch an
  entire mounted filesystem for a bounded set of event types without one watch
  descriptor per directory (inotify's classic scaling problem), which is the
  closest thing to "USN journal but real-time push instead of pollable log." But
  it fundamentally differs from USN in two ways worth calling out explicitly:
  (1) it's a live event stream with no persistent backing log — if your daemon
  isn't running (or drops an event under buffer pressure) you lose that change
  forever, unlike USN's durable, re-readable journal with several days of
  retention; and (2) it requires `CAP_SYS_ADMIN` for the filesystem-wide mark,
  which is its own privilege story, not obviously better than the "admin once,
  then a service holds the privilege" pattern Everything uses.
- For the **initial bulk index build**, the practical Linux answer is closer to
  "the fastest possible tree walk" — `openat2`/`statx` with
  `AT_STATX_DONT_SYNC`, `getdents64` in large batches, multiple threads walking
  disjoint subtrees in parallel — rather than an MFT-equivalent bulk read,
  because no bulk read exists. This means first-index time on Linux will
  structurally be closer to "a well-optimized parallel crawl" than to "one
  sequential multi-GB read," and a Linux implementation's speed claims should be
  benchmarked against that reality rather than assumed to match Everything's
  numbers by architectural analogy.
- The **query-language lesson** is filesystem-agnostic and directly copyable:
  Everything's compact functional query micro-language (`size:`, `dm:`,
  `regex:`, `content:`, boolean/wildcard combinators, all live-evaluated against
  an in-memory structure as the user types) is a bar any competing tool needs to
  clear on UX grounds regardless of the indexing backend, and none of the
  underlying primitives require anything NTFS-specific.
- The **privilege-separation lesson** from both Everything (a persistent service
  holds the elevated handle; the UI client doesn't need to be elevated) and
  Windows Search (untrusted format-parsing code runs in a separate,
  low-integrity process) both map cleanly onto Linux: a privileged (or
  `CAP_DAC_READ_SEARCH`/fanotify-capable) daemon doing discovery/indexing, with
  content-extraction (the part that parses untrusted PDFs, Office docs, etc. —
  the highest-CVE-density code in any of these systems) sandboxed separately (a
  seccomp'd worker process, or per-format subprocess) is directly transferable
  and arguably more important on Linux, where format-parsing libraries (poppler,
  libreoffice's filters, etc.) are exactly the kind of code you don't want in a
  root-ish daemon.
- The **ESE/property-store lesson** is a cautionary one, not a design to copy:
  Windows Search's reputation problem stems from unbounded per-item content
  storage (snippets/summaries) inside the index with no visible cap, plus
  reindexing entire large files on every modification (logs). A Linux design
  should budget and cap per-document stored content explicitly (dtSearch's ~15%
  published ratio is a good target to benchmark against) and treat
  frequently-appended files (logs) as a special case requiring
  incremental/tail-aware reindexing rather than whole-file reprocessing.

---

## Done-note

**Could not verify / gaps:**

- voidtools' own forum post explaining the MFT rationale
  (https://www.voidtools.com/forum/viewtopic.php?f=7&t=5433, "Why Don't Other
  Softwares Use the MFT?") returned **HTTP 403** when fetched directly during
  this research; the MFT/USN mechanism above is corroborated instead via a
  second-hand community summary (voidtools.com/forum/viewtopic.php?t=12779) and
  is consistent with years of stable, widely-repeated community description, but
  the primary-source page itself was not read directly in this pass. Recommend
  re-fetching with a different method if a from-the-author quote is needed.
- **No specific in-memory data structure for Everything's name index was found
  or should be asserted.** It is closed-source; only the _inputs_ (MFT read, USN
  journal) and _rough memory cost_ are documented/estimated. Any claim about a
  trie, hash table, sorted array, etc. would be speculation dressed as fact — I
  did not find such a claim from a credible source and have deliberately avoided
  asserting one.
- **Which regex engine backs Everything's `regex:` function is not confirmed.**
  The syntax surface (named groups, `(?m)`, lookaheads per forum discussion) is
  consistent with a PCRE-family engine, but voidtools does not appear to publish
  this explicitly in the pages retrieved. Flagged, not guessed.
- **No controlled, reproducible benchmark for Everything's initial index build
  time on a large volume** was found — only "seconds," qualitatively, from forum
  anecdotes. If Dave wants a real number, this would need an actual test on
  Windows hardware, which is out of reach from this research pass (and out of
  reach in general, since it's closed-source and testing requires Windows +
  NTFS).
- **RAM-per-million-files for Everything (~100 MB) is a converged community
  estimate, not a single vendor-published benchmark** — multiple independent
  forum threads land in a similar range (75–200 MB/million depending on options
  and phase), but no authoritative single source with a controlled methodology
  was located.
- Copernic's pricing and the "75,000 file" tier cap came from an aggregator/blog
  source, not Copernic's own current pricing page — should be re-verified
  against copernic.com directly before being treated as current fact in the
  final report.
- Whether Windows 11 changed the `Windows.edb` schema specifically (vs. just
  Search UI/cloud features) was not confirmed either way — reported here as "not
  found," not as "no change occurred."
- Proximity-search support in AQS specifically (vs. in Windows Search generally,
  e.g. via richer OLE DB query providers) was not confirmed to exist or not
  exist from a first-party Microsoft doc; flagged rather than guessed in either
  direction.
- I did not find a source describing exactly what happens, mechanically, when
  Everything's service has been offline long enough for the USN journal to have
  wrapped/expired past its last read position — inferred (a full MFT re-scan for
  that volume) from how USN journals generally behave, not confirmed as
  Everything's documented behavior specifically.

**Contradictions found:** community RAM-per-million figures for Everything range
from ~75 MB to ~200 MB depending on thread and phase (idle vs. actively
indexing) — reported as a range rather than resolved to one number, since no
single source settles it.
