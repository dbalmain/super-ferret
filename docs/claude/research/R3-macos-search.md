# R3 — macOS Desktop Search

Scope: Spotlight internals plus the strongest third-party tools. Written
entirely from public sources — I am on Linux and cannot run any of this. Every
non-obvious claim below is labeled and cited; where the record format is
genuinely unknown, I say so rather than guessing.

## Summary table

| Tool                         | Own index or Spotlight                                           | Index structure                                                                                                            | Regex/proximity?                                                                                              | Formats                                                                                                               | Price                               |
| ---------------------------- | ---------------------------------------------------------------- | -------------------------------------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------- | --------------------------------------------------------------------------------------------------------------------- | ----------------------------------- |
| Spotlight (`mds`/`mdworker`) | Own (system-wide, per-volume)                                    | Undocumented inverted-index-like store, reverse-engineered only partially (see below)                                      | No regex, no phrase/proximity in `mdfind`/`NSPredicate` syntax [upstream-documented]                          | Whatever `mdimporter` bundles cover (~dozens built in)                                                                | Free, built into macOS              |
| HoudahSpot                   | Rides Spotlight                                                  | N/A — pure front-end over `mdfind`/Spotlight APIs                                                                          | Inherits Spotlight's limits; adds a structured query builder, not new query power [community-anecdote/vendor] | Same as Spotlight                                                                                                     | ~$34 one-time [vendor]              |
| Alfred                       | Rides Spotlight for file search                                  | N/A                                                                                                                        | Inherits Spotlight's limits                                                                                   | Same as Spotlight                                                                                                     | Free / Powerpack ~£34 [vendor]      |
| Raycast                      | Own index for filenames/metadata; content search rides Spotlight | Undocumented, described only as a "local file index" [vendor-doc]                                                          | No regex documented; content search inherits Spotlight limits                                                 | Filenames+metadata always; content only where Spotlight indexes                                                       | Free / Pro subscription [vendor]    |
| DEVONthink                   | Own, per-database                                                | Its own document store + full-text index + a statistical "AI" layer (concordance, classification, similarity) [vendor-doc] | Supports Boolean and some advanced operators in its search syntax [vendor-doc]; regex not confirmed           | Broad — PDF, HTML, RTF, email, Markdown, images (OCR), and more, imported into its own store                          | ~$99+ one-time per edition [vendor] |
| Foxtrot Professional Search  | Own, independent full-text index                                 | Independent full-text index files built per project/volume, not documented in detail publicly                              | Boolean, wildcard, exclusion, proximity (slider-based word-distance window), and regex supported [vendor-doc] | PDF, HTML, Apple Mail, word processor/spreadsheet/presentation formats, extractable data from other apps [vendor-doc] | Paid, tiered (Home/Pro) [vendor]    |
| Find Any File                | None — direct filesystem scan                                    | N/A                                                                                                                        | Name/date/size matching; no full-text                                                                         | Filesystem metadata only                                                                                              | Low one-time price [vendor]         |
| EasyFind                     | None — direct filesystem scan                                    | N/A                                                                                                                        | Boolean, wildcard, regex on names; content search reads files at scan time                                    | Text-based content search, plus name/attribute search, into packages/bundles                                          | Free [vendor]                       |

---

## Spotlight internals

### Process architecture

Spotlight is a pipeline of cooperating daemons, not a single indexer:

- **`mds`** (metadata server) is the long-running daemon that owns the on-disk
  index and coordinates indexing work; it is the process users see spike in
  Activity Monitor as "mds" or "mds_stores." [community-anecdote,
  corroborated by multiple sources]
  https://eclecticlight.co/2021/01/28/spotlight-on-search-how-spotlight-works/
- **`mdworker`** processes are spawned by `mds` to do the actual per-file
  extraction work; several run in parallel (sandboxed, one per importer/task) so
  a hung importer on one file type doesn't block the whole pipeline.
  https://theevilbit.github.io/posts/macos_persistence_spotlight_importers/
  https://eclecticlight.co/2022/12/08/spotlight-problems-mds_stores-and-mdworker-in-trouble/
- **`mdimporter` plugins** are bundles (`.mdimporter`, found under
  `/System/Library/Spotlight/`, `/Library/Spotlight/`, and inside application
  bundles at `Contents/Library/Spotlight/`) that know how to parse one or more
  UTIs/file types and emit a dictionary of metadata attributes (`kMDItem*` keys)
  plus extracted text content. Any app can ship one to make Spotlight index its
  proprietary format.
  https://theevilbit.github.io/posts/macos_persistence_spotlight_importers/
- **`mdutil`** is the admin CLI: turn indexing on/off per volume
  (`mdutil -i off|on /Volumes/X`), force an erase-and-rebuild
  (`mdutil -E /Volumes/X`), or check status (`mdutil -s`).
- **`mdfind`** is the CLI query tool (the shell equivalent of a Spotlight search
  box); **`mdls`** dumps all indexed metadata attributes for one file.
- **Core Spotlight (`CSSearchableIndex`)** is the modern app-facing API
  (introduced iOS 9 / macOS, still current) that lets an app hand structured
  `CSSearchableItem`s (with attribute sets) to a _private, app-scoped_ index
  that Spotlight also surfaces in system search — distinct from the
  filesystem-content pipeline above. Apple's own docs describe this as the
  supported way to make in-app content (not files) searchable.
  https://developer.apple.com/documentation/corespotlight (referenced via
  Apple's Core Spotlight framework docs, current as of 2024 SDKs)

### Change notification: FSEvents

Spotlight does not poll the filesystem. It relies on **FSEvents**, a
kernel-level API (`/dev/fsevents`) that batches directory-level change
notifications and hands them to a userspace daemon, `fseventsd`, which persists
a rolling log of events per volume so that a reboot or an app that was offline
can "catch up" without a full re-scan.

- Log location moved with Big Sur's read-only system volume split: pre-Big Sur
  it's at `/.fseventsd`; Big Sur and later it's at
  `/System/Volumes/Data/.fseventsd/`.
  https://medium.com/@boutnaru/the-macos-forensic-journey-fsevents-file-system-events-directory-location-d842fc6707d1
- The logs are **directory-granularity, not file-granularity**: an FSEvent tells
  you "something changed under this directory," with flags (created, removed,
  renamed, modified, etc.) and a monotonically increasing global event ID — the
  consumer (here, `mds`) still has to `stat`/re-`readdir` to find out exactly
  what changed. This is the single biggest architectural difference from Linux's
  `inotify`/`fanotify`, which report the specific path and specific event
  inline. FSEvents trades precision for durability and coalescing: a burst of
  writes to the same file collapses to one notification, and the log format
  means a _consumer that was not running_ can replay everything since its last
  known event ID, which inotify cannot do at all (inotify has no persistent log;
  a watcher that isn't running misses events entirely).
  https://www.hexordia.com/blog/mac-forensics-analysis
  https://hackmd.io/@M4shl3/FSEvents
- The on-disk log files are stored in a **proprietary, gzip-compressed binary
  format** and require specialized parsers (`FSEventsParser`, `mac_apt`, the
  Rust `FSEventsParser-rs`) to read outside the OS — Apple has not published the
  format. https://github.com/Houwenda/FSEventsParser-rs
  https://insiderthreatmatrix.org/detections/DT108
- Forensic value: because the log persists (subject to size-based rotation, not
  necessarily reflecting current disk state), FSEvents logs can reveal file
  paths and activity for files that have since been deleted — a property DFIR
  investigators exploit and that has no real analog on typical Linux desktop
  setups.
  https://www.forensicon.com/resources/articles/exploring-fseventsd-forensics-techniques/

**Transferable lesson**: a durable, replayable, coalescing change log decoupled
from the consumer's uptime is a genuinely good design point that
inotify/fanotify do not provide out of the box on Linux — a Linux indexer that
wants "catch up from where I left off, even directory-level, even if I crashed"
would need to build something FSEvents-shaped itself (or lean on fanotify's
newer permission/mark logging plus its own persisted checkpoint), because
fanotify also has no built-in durable log.

### On-disk store format — what's public and what isn't

**Apple has never published the Spotlight store's on-disk format.** Everything
known publicly comes from digital-forensics reverse-engineering, principally
Yogesh Khatri's work (`spotlight_parser`) and derivative tooling in `mac_apt`
and `plaso`/log2timeline.

Locations (multiple, by scope):

- **Per-volume, system-wide index**: `/.Spotlight-V100/Store-V2/<UUID>/`,
  holding `store.db` / `.store.db` and related files. This is the classic index
  covering ordinary filesystem content.
  https://www.swiftforensics.com/2018/08/parsing-spotlight-database.html
- **Per-user index** (introduced around macOS 10.13 High Sierra, when Apple
  moved various app data — Safari history, Notes, Mail metadata, Maps, News —
  out of scattered plist/webhistory files and into Spotlight itself):
  `~/Library/Metadata/CoreSpotlight/index.spotlightV3/`, again holding
  `store.db` and `.store.db`. Khatri notes this consolidation replaced, e.g.,
  Safari's old per-file `*.webhistory` plists under
  `~/Library/Caches/Metadata/Safari/`.
  http://www.swiftforensics.com/2018/10/the-user-spotlight-database.html
- **Live/journal files**: entries named like `live.N.M` (referenced across DFIR
  discussions of the store directory) appear to serve as write-ahead/incremental
  journals so the main `store.db` doesn't need to be rewritten on every change —
  consistent with a design that batches updates rather than doing synchronous
  single-record writes, but the exact journal semantics (commit protocol, when
  journals get folded into the main store) are **not documented publicly**
  anywhere I found; this is inferred from filenames and forum/DFIR chatter, not
  from a published spec. [forensics-literature, low confidence — flagging
  as genuinely unknown]

What Khatri's reverse-engineering _does_ establish (his own account of the
effort, `spotlight_parser`):

- The format is proprietary and undocumented by Apple; he "studied the file
  format of these databases over several months" to build a parser.
  https://www.swiftforensics.com/2018/08/parsing-spotlight-database.html
- The parser extracts per-item metadata records including attributes not
  surfaced anywhere else on disk — notably **last-opened timestamps and open/use
  counts** for files and apps, which is forensically valuable precisely because
  it isn't in any other artifact (not in the filesystem's own mtime/atime, since
  macOS doesn't reliably maintain atime for this purpose).
  https://www.swiftforensics.com/2018/08/parsing-spotlight-database.html
- His blog explicitly deferred a detailed record-format writeup ("The format of
  the database will be discussed in a later post") — I could not confirm whether
  a follow-up post with block/property/category-level detail was ever published;
  the most authoritative surviving reference to the actual block-level structure
  is the `spotlight_parser.py` source itself (Python, actively maintained
  through at least late 2025), which is the closest thing to a specification
  that exists publicly.
  https://github.com/ydkhatri/spotlight_parser/blob/master/spotlight_parser.py
- Downstream tools (`mac_apt`'s Spotlight plugin, and a `plaso`/log2timeline
  parser added via PR #3125) consume Khatri's parsing logic and export to
  spreadsheet/SQLite/flat-text, which is itself evidence that the format is
  stable enough to reverse-engineer reliably but not that Apple has ever
  confirmed the layout. https://github.com/log2timeline/plaso/pull/3125

**Is it an inverted index?** Functionally, yes at the conceptual level —
multiple independent descriptions of Spotlight's design characterize it as
maintaining an inverted index mapping terms/attributes back to files, refreshed
incrementally as FSEvents arrive.
https://eclecticlight.co/2021/01/28/spotlight-on-search-how-spotlight-works/ But
**no public source documents the actual posting-list encoding, term dictionary
structure, or compression scheme** used inside `store.db` at the byte level
beyond what `spotlight_parser.py`'s code implies (record framing,
category/property enumeration, and per-value type tags). I could not find a
credible public claim about which general-purpose compression (if any, e.g.
LZFSE-style) is applied to postings versus stored values — treat any specific
compression-algorithm claim about Spotlight's store as **unverified** unless
sourced directly to the parser code, which I was not able to fully render here
(blocked by fetch size limits on the one paper that seemed most likely to cover
it, put.as's "Shedding Light on the macOS Spotlight Desktop Search Service" —
worth a follow-up read if this matters to the final report:
https://papers.put.as/papers/macosx/2019/summit_archive_1564171500.pdf).

**Metadata attributes vs. content terms**: architecturally these are treated
uniformly as "attributes" (`kMDItemFSName`, `kMDItemContentType`,
`kMDItemTextContent`, etc.) — an `mdimporter` returns one dictionary per item
mixing filesystem metadata (owner, size, kind) and content-derived fields
(document text, EXIF data, ID3 tags) and Spotlight indexes all of it through the
same pipeline, which is why `mdfind`/`NSMetadataQuery` predicates can mix
`kMDItemKind == 'PDF'` with `kMDItemTextContent == '*foo*'` in one expression.
https://developer.apple.com/library/mac/documentation/Carbon/Conceptual/SpotlightQuery/Concepts/QueryingMetadata.html

### Query language: `mdfind` / `NSMetadataQuery` predicates

Apple's own comparison document is the best primary source on what the query
language can and cannot do:
https://developer.apple.com/library/archive/documentation/Cocoa/Conceptual/Predicates/Articles/pSpotlightComparison.html

Key documented facts [upstream-documented]:

- Comparisons must be `KEY operator VALUE`, never the reverse.
- Substring match is written `myAttribute == '*foo*'` (glob-style wildcards),
  not a `LIKE`/regex construct.
- Case/diacritic sensitivity is toggled with trailing modifier letters on the
  literal (`'foo'cd`), a different mechanism from NSPredicate's `[cd]` prefix on
  the operator.
- There is **no regex support** and **no explicit phrase-search or proximity
  operator** in the Spotlight query grammar — multi-word literals are matched as
  a substring/wildcard pattern, not as an exact adjacent-phrase query with its
  own operator, per Apple's own predicate-comparison reference above and
  corroborated by community documentation of `mdfind` usage. This absence of
  phrase/proximity/regex operators is exactly the gap that independent tools
  like Foxtrot Professional Search built their whole value proposition around
  (see below).
- The Eclectic Light Company's practical testing series on Spotlight search
  behavior documents various "quirks" (e.g., surprising misses on content
  search) as of 2025 — a useful secondary source for user-observed limits,
  though it is analysis/blog content, not Apple documentation.
  https://eclecticlight.co/2025/09/04/quirks-of-spotlight-local-search/
  https://eclecticlight.co/2021/01/29/spotlight-on-search-search-and-you-might-be-lucky/

### Format coverage via `mdimporter`

- macOS ships built-in importers under `/System/Library/Spotlight/` for the
  obvious system formats (Mail, Contacts, iCal/Calendar, images with EXIF, PDF,
  plain text/RTF, source code, fonts, etc.) — Apple does not publish an
  authoritative list, but the mechanism itself (a Spotlight-importer bundle
  target in Xcode, `Info.plist`-declared UTI mappings, a
  `GetMetadataForFile`/`GetMetadataForURL` entry point) is documented for
  developers.
  https://theevilbit.github.io/posts/macos_persistence_spotlight_importers/
- Any third-party app can ship `Contents/Library/Spotlight/YourApp.mdimporter`
  inside its `.app` bundle; `mds` discovers and loads these automatically, which
  is also a known **persistence/security concern** — a malicious or compromised
  importer runs (sandboxed, but still) whenever matching files are touched,
  which is why importer bundles have shown up as a persistence technique in
  offensive-security writeups, and why Apple's TCC (privacy/permission) boundary
  around Spotlight has itself had exploitable gaps (the 2025 "Sploitlight" TCC
  bypass via Spotlight importers/plugins is a concrete recent example).
  https://theevilbit.github.io/posts/macos_persistence_spotlight_importers/
  https://thewindowsupdate.com/2025/07/28/sploitlight-analyzing-a-spotlight-based-macos-tcc-vulnerability/

### Known limitations and complaints

- **CPU/reliability**: `mds`/`mds_stores`/`mdworker` runaway CPU usage is a
  long-running, recurring user complaint across macOS versions, usually
  triggered by a bad importer, a large untracked change set (e.g. after a
  migration or a big git operation), or index corruption.
  https://eclecticlight.co/2022/12/08/spotlight-problems-mds_stores-and-mdworker-in-trouble/
  https://mole.fit/blog/mds-mdworker-high-cpu-mac
- **Index corruption / silent staleness**: macOS provides no proactive
  corruption detection or "your index is stale, rebuild?" prompt; users must
  notice symptoms (missing results, indexing stuck) and manually run
  `mdutil -i off`, delete `.Spotlight-V100`/`.Spotlight-V200`, then
  `mdutil -i on` + `mdutil -E` to force a full rebuild — an expensive,
  all-or-nothing operation with no incremental-repair path exposed to users.
  https://eclecticlight.co/2024/11/19/when-and-how-to-rebuild-spotlight-indexes/
  https://www.macrumors.com/how-to/rebuild-spotlight-search-index-on-mac/
- Sonoma-era regressions were bad enough to generate sustained threads on
  Apple's own developer forums (app indexing broken, Mail search stuck
  "indexing" indefinitely even after rebuild).
  https://developer.apple.com/forums/thread/738074?page=3
- **Content search reliability** is inconsistent enough that Eclectic Light
  Company still publishes systematic "quirks of Spotlight" investigations in
  2025 — this is not a solved problem even after two decades of the feature
  existing.
  https://eclecticlight.co/2025/09/04/quirks-of-spotlight-local-search/
- **Network volumes**: Spotlight indexing of SMB/AFP network shares is
  inconsistent and often disabled/limited by default, which is exactly the gap
  that direct-scan tools (EasyFind, Find Any File) and independent full-text
  engines (Foxtrot) exist to fill.

---

## Third-party tools

### HoudahSpot — the strongest Spotlight _front-end_

HoudahSpot deliberately does not maintain its own index; it is a UI layer over
`mdfind`/the Spotlight query APIs. Its value is entirely in query construction
and result presentation, not in query power:

- A visual, criterion-by-criterion query builder (name/kind/path/tags/dates/
  content) combined with AND/OR/NOT logic and include/exclude location scopes,
  updating results live as criteria change.
- **Saved search templates**: a named, reusable bundle of criteria + scope +
  result columns, so a recurring question ("all PSD files touched this quarter
  under this client folder") becomes one click instead of a hand-typed predicate
  each time.
- Hundreds of selectable result columns drawn from the full `kMDItem*` attribute
  space that Spotlight already indexes but the stock Spotlight UI never exposes.

https://www.houdah.com/houdahSpot/
https://eclecticlight.co/2021/02/04/spotlight-on-search-better-and-different-3rd-party-apps/

Because it rides Spotlight, HoudahSpot inherits every one of Spotlight's limits
verbatim: no regex, no phrase/proximity operators, same content-search
reliability gaps, same blind spots on unindexed volumes. Its whole contribution
is turning Spotlight's _existing_ attribute space into something usable via
structured UI rather than hand-written predicate strings — a "good front-end
can't fix a weak engine" case study.

### Alfred and Raycast — launcher-layer, mostly riding Spotlight

- **Alfred** queries Spotlight's metadata index directly for file search; it
  does not maintain a separate on-disk file index of its own. Its value-add is
  entirely in the launcher UX (workflows, snippets, clipboard history), not in
  indexing.
- **Raycast** is a hybrid: it maintains **its own local index of filenames and
  metadata** (undocumented internal structure — Raycast has not published it),
  but for searching _inside_ file contents its own manual states it falls back
  to "your operating system's built-in search index" (Spotlight), and that
  content search is disabled wherever Spotlight isn't covering a folder.
  https://manual.raycast.com/file-search
  https://medium.com/@andriizolkin/spotlight-vs-alfred-vs-raycast-31bd942ac3b6

Both are lexical search, same as Spotlight — no semantic/embedding-based
retrieval is documented for either's core file search as of this writing
[community-anecdote based on manual + comparison pieces; not confirmed
against Raycast's own architecture docs, which don't exist publicly at
this level of detail].

### DEVONthink — genuinely its own engine

DEVONthink is the most architecturally distinct tool in this set because it does
not search the live filesystem at all in its normal mode — it _imports_
documents into its own database/store and indexes and organizes them there.
Public documentation only goes so far (DEVONthink has never published its
retrieval internals as an engineering spec), but the documented feature set is
genuinely more than "front-end over Spotlight":

- **Concordance**: an explicit word-frequency/word-list view over a document or
  the whole database — a classic IR concept (term index browsable by the user)
  surfaced directly in the UI, not hidden inside a query box.
  https://download.devontechnologies.com/download/devonthink/3.8.2/DEVONthink.help/Contents/Resources/pgs/inspectors-seealso.html
- **Classify**: for each group (folder-like container) DEVONthink tries to find
  the pattern of contextual/statistical relationships in that group's documents
  that distinguishes it from sibling groups, then suggests which group a new or
  existing document best fits; "Auto Classify" applies this automatically to
  place documents without user confirmation.
  https://myproductivemac.com/blog/devonthink-part-5-classification21102015
- **See Also**: surfaces documents judged related by the same content/context
  analysis engine that powers Classify — explicitly advertised as finding
  connections "you'd never have spotted yourself," i.e.,
  similarity/nearest-neighbor-style retrieval over the whole database rather
  than a query the user types.
  https://discourse.devontechnologies.com/t/see-also-classify-what-do-they-look-at/55816

DEVONthink does not publish the underlying algorithm (whether it's TF-IDF-style
vector similarity, a Bayesian classifier, or something else) — community
discourse threads exist precisely because users are trying to reverse-engineer
why See Also/Classify behave the way they do from the outside, which itself
signals the internals are opaque even to power users. [vendor-doc

+ community-anecdote — no engineering-level source found]

### Foxtrot Professional Search — the independent full-text engine with real query power

Foxtrot is the clearest existence proof that Spotlight's query-language gap (no
regex, no proximity, no real phrase search) is a solvable, shippable feature
rather than a platform ceiling:

- Builds and maintains its **own full-text index files**, separate from
  Spotlight, across local disks, external drives, and network/NAS volumes —
  addressing Spotlight's weak network-volume coverage directly.
- Query grammar supports **Boolean operators, wildcards, term exclusion (leading
  hyphen), and — distinctively — proximity search via a drag-adjustable
  word-distance slider** ("find documents where two concepts appear within N
  words of each other"), explicitly marketed as something no other macOS search
  tool offers.
- **Regular expressions** are supported for narrowing results, plus
  document-level secondary/refinement searches within an initial result set.
- Coverage: PDF, HTML, Apple Mail, word-processor/spreadsheet/presentation
  formats, and "any extractable data from other applications" via its own
  importer mechanism (separate from `mdimporter`).
- Lets users partition indices per project/archive/client and search them
  independently or together — explicit user control over index scope that
  Spotlight doesn't expose (Spotlight indexing is essentially per-volume on/off,
  not per-folder-set).

http://foxtrot-search.com/foxtrot-professional.html
https://www.macdrifter.com/2015/01/searching-without-spotlight.html
https://myownsys.com/2025/01/30/maximize-mac-efficiency-with-foxtrot-search/

Foxtrot does not publish its index's internal format either — no engineering
docs found — but its feature list is the single best evidence in this whole
survey that a small independent team can out-query Spotlight on a Mac using an
inverted index of their own design, with real proximity and regex, at a consumer
price point.

### Find Any File and EasyFind — the direct-scan alternative

Both represent the opposite architectural bet from everything else here: no
index at all, scan the filesystem live on every search.

- **Find Any File**: name/date/size matching with a hierarchical results view;
  its explicit selling point is a _guarantee_ of completeness — because it reads
  the filesystem directly at search time, it cannot miss a file due to stale or
  corrupted index state, unindexed volumes, or excluded locations, the way
  Spotlight (and everything riding on it) can.
- **EasyFind** (DEVONtechnologies): same no-index philosophy, but goes further
  into content — supports Boolean operators, wildcards, and **regex** on
  filenames, plus real-time text-content search inside files, and can look
  inside `.app` bundles and other packages that Spotlight treats as opaque.

https://sourceforge.net/app/easyfind/mac/
https://danielbahl.com/tech/easyfind-on-macos-when-spotlight-fails-and-terminal-is-too-much
https://macfilesearch.com/easyfind-find-any-file.html

**The tradeoff is exactly the one you'd expect**: correctness/completeness and
zero index-maintenance cost, at the price of O(tree size) latency per query,
worse on large volumes or when content search is enabled (has to open and read
every candidate file). These tools are popular specifically as a _fallback_ for
the cases where Spotlight's index has failed, is excluded, or is untrustworthy —
direct evidence that a meaningful fraction of the Mac power-user base does not
trust the built-in index to be complete.

### Newer/other tools worth naming (2024–2026)

- Search-aggregator/comparison pieces from 2026 (`dhito.io`, `filect.io`) group
  current-generation Mac search tools into "launchers" (Alfred, Raycast,
  LaunchBar) vs. "deep search" (HoudahSpot, Foxtrot, EasyFind, Find Any File) —
  no new independent-index full-text engine emerged in this period that the
  search surfaced beyond the incumbents above; Raycast's own file-search feature
  (as opposed to the launcher itself) is the most actively-developed newer
  entrant, per its manual.
  https://dhito.io/blog/best-spotlight-alternatives-mac-2026/
  https://filect.io/blog/spotlight-alternative-mac/

---

## Design lessons for a Linux builder

**Copy:**

1. **Separate "what changed" from "what to do about it" with a durable,
   replayable log, not a live-only notification stream.** FSEvents' key property
   — a consumer that was offline can catch up from a persisted event-ID
   checkpoint — is something inotify/fanotify do not give you for free. A Linux
   indexer that wants correctness across crashes/reboots without full rescans
   should build its own durable, coalescing change journal on top of
   fanotify/inotify rather than assuming the kernel API alone is sufficient.
2. **Treat metadata and content as one attribute space, indexed uniformly.**
   Spotlight's `kMDItem*` model — mixing filesystem attributes and
   content-derived fields in one queryable namespace — is why cross-cutting
   queries ("PDFs modified this month containing X") are natural on macOS. Don't
   build two separate systems (a metadata DB and a full-text index) that can't
   be joined in one query.
3. **Make the importer/extractor interface a stable public plugin API.**
   `mdimporter`'s success (dozens of built-in formats, easy third-party
   extension) shows the value of decoupling "how do I get text out of this
   format" from the indexer core — but see the security lesson below.
4. **Ship real query power: proximity and regex are not exotic asks.** Foxtrot
   proves a small team can add proximity search and regex to a consumer
   full-text tool. Spotlight's decision to omit both from its query language,
   twenty-plus years in, is a real and repeatedly criticized gap — a Linux
   engine that supports true phrase, proximity, and regex out of the box beats
   the platform search on day one.
5. **Give power users a legible query surface and let a front-end sit on top.**
   HoudahSpot's whole existence — a paid product that adds nothing but UI over
   an existing engine — is proof that a well-specified, attribute-rich query
   capability is itself valuable independent of UI; design the query
   language/API first, expect UIs to be built on it later (by you or others).
6. **Offer a no-index escape hatch.** The sustained popularity of EasyFind and
   Find Any File shows real demand for "just scan the disk, guarantee
   completeness" when the index is stale, excluded, or untrusted — a Linux tool
   should have a documented, easy fallback to direct scanning for the same
   reason, and should be honest with users about when the index might be behind
   reality.

**Avoid / don't repeat:**

1. **Don't hide index health from the user.** Spotlight gives no proactive
   signal that the index is corrupt or badly stale; users find out from symptoms
   and then have to run manual, all-or-nothing rebuild commands (`mdutil -E`, or
   full delete-and-rebuild). Expose index freshness/health as a first-class,
   checkable state, and support incremental repair, not just nuke-and-rebuild.
2. **Don't let third-party extractor plugins run with ambient trust.** The
   `mdimporter` security history (persistence technique, 2025 "Sploitlight" TCC
   bypass) shows what happens when arbitrary bundles get auto-loaded and run
   against untrusted file content with real system privilege. Sandbox extractors
   hard, and treat "parses attacker-controlled bytes" as the threat model for
   every format plugin from day one.
3. **Don't leave the query grammar underpowered because "most users don't need
   it."** Two decades of complaint threads and an entire cottage industry of
   paid front-ends and independent engines exist because Spotlight's grammar
   stayed thin. Ship the power users' features (regex, proximity, boolean,
   phrase) even if the default UI hides them behind an "advanced" toggle.
4. **Don't couple index scope tightly to volume boundaries.** Spotlight's
   indexing granularity is fundamentally per-volume; Foxtrot's per-project index
   partitioning is a feature users explicitly want and Spotlight can't give
   them. Design indices as user-scoped collections of paths, not as an implicit
   property of "which disk is this file on."
5. **Don't let CPU cost be unbounded and unexplained.** Runaway `mds`/`mdworker`
   CPU is one of the most persistent, decades-long user complaints about
   Spotlight. Whatever scheduling/priority model you use for background
   indexing, make its resource ceiling explicit and visible, and make a single
   bad file/format degrade gracefully rather than pegging a core.

---

## Done-note

**What I could not verify:**

- The actual byte-level record format inside `store.db`/`.store.db` —
  block/category/property layout, term-dictionary structure, and any compression
  scheme applied to postings or stored values. The best public source is the
  `spotlight_parser.py` source code itself
  (https://github.com/ydkhatri/spotlight_parser/blob/master/spotlight_parser.py),
  which I did not fully render/parse in this pass. If the final report needs
  byte-level specifics (e.g. "is it a B-tree, is it LZFSE-compressed"), that
  source file needs a direct read, not a web search — flagging as a gap rather
  than guessing.
- Yogesh Khatri's swiftforensics.com blog explicitly promised a follow-up post
  with detailed format documentation ("The format of the database will be
  discussed in a later post," Aug 2018); I could not confirm whether that
  follow-up was ever published, or find it if so.
- A paper that looked like it would be the single best source for
  Spotlight-internals detail — Vico Marizale's "Shedding Light on the macOS
  Spotlight Desktop Search Service" (put.as, 2019) — exceeded this tool's fetch
  size limit and I was not able to read its contents. This is a real gap: it is
  the kind of source (conference-talk writeup, security-research depth) most
  likely to contain the record-format detail the parser code implies but that I
  could not confirm from search snippets alone.
  https://papers.put.as/papers/macosx/2019/summit_archive_1564171500.pdf
- The live/journal file naming pattern (`live.N.M`) and its exact
  write-ahead-log semantics are inferred from context, not confirmed against a
  primary source — flagged inline above as low confidence.
- DEVONthink's and Foxtrot's actual retrieval algorithms (vector similarity?
  Bayesian? classic tf-idf?) are not published by either vendor; everything said
  about them here is feature-level, from vendor docs and user community
  discussion, not engineering documentation.

**Contradictions found:** none material — sources were broadly consistent on
architecture (mds/mdworker/mdimporter pipeline, FSEvents-driven updates,
per-volume + per-user store split, no regex/proximity in Spotlight's query
grammar). The one soft tension is that some secondary sources casually call
Spotlight's store "an inverted index" as settled fact, while the only
primary-ish evidence (the forensics parser work) documents record/category/
property structure without ever using or confirming that specific data-structure
term — I've flagged the inverted-index characterization as the consensus
description, not a confirmed structural fact.
