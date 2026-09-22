# R2 — Linux filename search, code search, and regex-capable search tools

Scope: filename-search daemons/indexers, and indexed/unindexed regex code
search. Sibling doc covers full-text document search (Recoll/Tracker/Baloo) —
not duplicated here except where directly relevant to index-design comparisons.

## Framing note (post-decision revision)

Written for a reader who has already decided to build (Super Ferret, a new Rust
search engine) and knows inverted-index mechanics cold — this is the author of
the original Ferret, the Ruby port of Lucene. Adjustments from the first pass:

- Every tool below is **competitive intel**, not a shopping list. "Could I
  depend on this" is de-emphasized throughout in favor of "what is the exact
  algorithm, and what does it cost in bytes and in query time" — the plan is to
  implement the algorithms directly, not link a library.
- **Licence is recorded for every tool named, and GPL/AGPL is flagged
  explicitly** — not as a disqualifier for reading the design, but because a
  GPL/AGPL codebase can be studied and reimplemented-from-description but not
  vendored or linked into a permissively-licensed project. Stated preference for
  the new engine is MIT/Apache/BSD. Quick scan of everything in this document:
  **plocate — GPLv2. mlocate — GPLv2. GNU findutils — GPLv3. fsearch — GPLv2.
  ANGRYsearch — GPL. catfish — GPL. cscope — BSD-ish (permissive). GNU
  Global/gtags — GPLv3. ctags/universal-ctags — GPLv2-ish. OpenGrok — CDDL**
  (weak-copyleft, file-level; more permissive than GPL but not
  BSD/MIT-equivalent). **codesearch/csearch, zoekt, livegrep, hound — all
  BSD-3/Apache-2.0** (permissive — these four carry the richest indexed-search
  algorithmic prior art in this survey and are also fully safe to read
  line-by-line, or fork, without licence friction). **ripgrep — MIT/Unlicense
  dual. ugrep — BSD-3. ag — Apache-2.0. ack — Artistic/GPL dual. fd —
  MIT/Apache-2.0. fzf, broot — MIT.** Net: the most algorithmically valuable
  reading (Cox's codesearch paper+code, zoekt's design doc+source, livegrep's
  suffix-array implementation) has zero licence friction against a permissive
  target.
- **Where a design trades index size against query latency, both numbers are
  given and neither is presumptively "better."** The stated design preference is
  a denser (smaller) index at some acceptable latency cost — which cuts directly
  against the instinct visible in zoekt's positional trigrams and livegrep's
  suffix array, both of which spend substantial extra bytes to buy query speed.
  Flagged inline per tool, and revisited in the closing summary of the
  regex-over-trigram-index section.
- Scale is confirmed personal-workstation: ~1M files (under ~5M), tens to a few
  hundred GB — not NAS/TB scale. Content priority is source code and text/config
  first, which is this document's entire subject matter; PDF/ Office and media
  metadata are secondary and belong to the sibling doc; email and OCR are later
  plugins, out of scope here.

## Summary table

| Tool                                      | Lang                 | Licence                        | Status (last release checked)                                    | Index structure                                                                                 | Index size vs corpus                                                                       | Regex engine                                                        | Query latency (typical)                                                                                    |
| ----------------------------------------- | -------------------- | ------------------------------ | ---------------------------------------------------------------- | ----------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------ | ------------------------------------------------------------------- | ---------------------------------------------------------------------------------------------------------- |
| plocate                                   | C++                  | GPLv2                          | Active (Debian default `locate` since bullseye)                  | trigram inverted index, compressed filename blocks, posting lists                               | ~0.42x of mlocate's DB for same tree [community-anecdote, see below]                       | glob only, no regex                                                 | ~8ms typical query [community-anecdote]                                                                    |
| mlocate                                   | C                    | GPLv2                          | Maintenance-only, superseded by plocate as Debian/Fedora default | flat sorted DB, per-dir diff encoding, linear scan                                              | smaller index but linear scan cost                                                         | glob/basic regex (`--regex` via libc)                               | seconds on large trees [community-anecdote]                                                                |
| GNU findutils `locate`/`updatedb`         | C                    | GPLv3                          | Active but legacy design unchanged for decades                   | "front-compressed" ASCII sorted list, whole-DB scan                                             | ~compact but scan-bound                                                                    | glob, `-r`/`--regexp` BRE                                           | scan-proportional to DB size                                                                               |
| fsearch                                   | C/GTK                | GPLv2                          | Active (GitHub, sporadic releases)                               | full in-memory tree/array of all indexed paths, rebuilt each run or on demand                   | RAM ≈ several× on-disk DB; no persistent index format documented                           | POSIX/PCRE-ish via GLib regex, and simple glob                      | sub-ms once loaded [upstream/community]                                                                    |
| ANGRYsearch                               | Python/Qt            | GPL                            | Largely dormant (last significant activity years old)            | wraps `mlocate` DB, loads into Python list/SQLite                                               | none of its own — rides on mlocate                                                         | none native                                                         | fast for filename substring only                                                                           |
| catfish                                   | Python               | GPL                            | Active, thin wrapper                                             | delegates to `locate`/`mlocate`/`find`/`tracker`/`baloo` as backend                             | n/a (no own index)                                                                         | delegates                                                           | delegates                                                                                                  |
| fzf + fd (pipeline)                       | Rust (fd) / Go (fzf) | MIT/Apache-2.0 (fd), MIT (fzf) | Both very active                                                 | none — fd walks filesystem live each run, fzf does in-memory fuzzy scoring                      | no index at all                                                                            | fd: Rust `regex` crate; fzf: fuzzy matcher, not regex               | walk-time proportional to tree; ~1M files in low single-digit seconds warm [estimated]                     |
| broot                                     | Rust                 | MIT                            | Active                                                           | none — live directory walk, no persistent index                                                 | n/a                                                                                        | Rust `regex` crate subset for search-in-tree                        | interactive, walk-bound                                                                                    |
| ripgrep (rg)                              | Rust                 | MIT/Unlicense                  | Very active                                                      | none (unindexed)                                                                                | n/a                                                                                        | Rust `regex` crate (default), or `regex-automata`/`pcre2` with `-P` | see throughput numbers below                                                                               |
| ugrep                                     | C++                  | BSD-3                          | Active                                                           | none by default; optional `ugrep-indexer` sidecar (bloom-filter based, merged into ugrep ≥6.0)  | indexer output much smaller than corpus, exact ratio not published                         | own regex engine (RE/flex-derived DFA) + `--pcre` via PCRE2/Boost   | comparable to rg on warm cache; indexed mode claims >10x speedup on cold/large trees [upstream-documented] |
| ag (The Silver Searcher)                  | C                    | Apache-2.0                     | Effectively unmaintained (sparse commits since ~2019)            | none                                                                                            | n/a                                                                                        | PCRE                                                                | slower than rg/ugrep, see benchmark                                                                        |
| ack                                       | Perl                 | Artistic/GPL                   | Low-activity maintenance                                         | none                                                                                            | n/a                                                                                        | Perl regex                                                          | slowest of the unindexed greps in every benchmark found                                                    |
| Google Code Search / csearch (codesearch) | Go                   | BSD-3                          | Dead (rsc archived; last real work ~2015)                        | trigram inverted index, gob-encoded posting lists on disk                                       | ~20% of source size [upstream-documented]                                                  | RE2                                                                 | interactive on hundreds-of-MB corpora [upstream-documented]                                                |
| zoekt                                     | Go                   | Apache-2.0                     | Active (Sourcegraph-maintained fork of google/zoekt)             | positional trigram index + "trigram with successor/distance" verification, memory-mapped shards | ~3.5x of source size [third-party/derived from design doc, see below]                      | RE2                                                                 | sub-100ms typical on Sourcegraph-scale corpora [community-anecdote]                                        |
| livegrep                                  | C++/Go               | Apache-2.0                     | Low activity (mostly dormant since ~2016-2019)                   | suffix array over concatenated corpus buffer (divsufsort)                                       | index ≈ same order of magnitude as corpus (raw text + SA of 4 or 8 bytes/char) [estimated] | RE2                                                                 | realtime as-you-type on Linux-kernel-scale corpora [upstream-documented]                                   |
| hound                                     | Go                   | MIT                            | Dormant (Etsy project, last real commits years old)              | wraps codesearch's trigram index (uses google/codesearch library)                               | inherits csearch's ~20% figure                                                             | RE2 (via codesearch)                                                | web UI, seconds                                                                                            |
| OpenGrok                                  | Java                 | CDDL                           | Active-ish (Oracle-derived, slow release cadence)                | Apache Lucene index (tokenized, not raw trigram) + ctags symbol data                            | Lucene index commonly 0.5–2x source [community-anecdote]                                   | Java regex subset via Lucene span/regex queries                     | seconds, scales with Lucene shard count                                                                    |
| cscope                                    | C                    | BSD-ish                        | Maintenance-only                                                 | custom symbol-relationship database (`cscope.out`)                                              | typically smaller than source; symbol-only                                                 | not general regex — symbol lookups                                  | fast for symbol queries only                                                                               |
| GNU Global (gtags)                        | C                    | GPLv3                          | Active-ish, slow cadence                                         | tag database (GTAGS/GRTAGS/GPATH), pluggable parsers (ctags/pygments/universal-ctags)           | comparable order of magnitude to source                                                    | plugin-dependent; base tool is symbol lookup not regex              | fast for tag lookups                                                                                       |
| ctags / universal-ctags                   | C                    | GPLv2/MIT-ish                  | universal-ctags active                                           | flat tags file, sorted, symbol → location                                                       | small (tag lines only)                                                                     | none (symbol index, not text search)                                | instant                                                                                                    |

## plocate

Source: Steinar H. Gunderson, plocate — <https://plocate.sesse.net/>, source at
<https://git.sesse.net/?p=plocate> (mirrored on GitHub, e.g.
<https://github.com/caldwell/plocate>). Debian adopted it as the default
`locate` provider starting with Bullseye/Bookworm.

**Design.** plocate replaces mlocate's linear-scan database with a genuine
inverted index: it builds a **trigram index over filenames**, where the database
stores, for every 3-byte substring (trigram) seen across all indexed filenames,
a **posting list** of which filename-blocks contain it. Filenames are grouped
into small blocks (multiple filenames concatenated together) before compression;
the doc explains the tradeoff explicitly — "The index format uses compressed
blocks of filenames... This makes the index smaller because the compression
algorithm gets more context to work with, and because there are fewer elements
in each posting list, though it also makes posting lists less precise, moving
more work to weeding out false positives after posting list intersection."
[upstream-documented, plocate-build(8) / sesse.net] This is the same core tradeoff
every trigram-index tool makes (index density vs. false-positive rate), just applied
at the block level rather than the per-file level.

**Query algorithm.** For a query string, plocate trigrams it, intersects the
posting lists for the required trigrams (fewest-postings-first ordering, same
idea as csearch/zoekt), decompresses the surviving filename blocks, and does a
final substring check to eliminate any false positives introduced by block-level
indexing. I/O is issued asynchronously via `io_uring` where available (Linux
≥5.1), which matters more on spinning disks than SSD/NVMe. [upstream-documented, plocate.sesse.net]

**Short-query / trigram-miss handling.** A query shorter than 3 bytes cannot
form a full trigram, so plocate (like every trigram scheme) falls back to a
looser filter — effectively degrading toward a fuller scan for 1–2 character
queries. The plocate docs don't spell out an exact fallback algorithm distinct
from the general "not enough trigrams to be selective" case; treat this as the
same degenerate case documented for Russ Cox's algorithm below (short/`.`-heavy
patterns yield a near-`ANY` query).

**Numbers found.**

- Query time: plocate ~0.008s total vs mlocate ~20.118s total, on the benchmark
  quoted on the project page (specific corpus not restated in the fetched
  excerpt — treat as the author's own demo, not an independent benchmark).
  [upstream-documented, but the underlying corpus/methodology wasn't visible
  in what I could fetch — flag this]
- Index size: plocate database 466 MB vs mlocate 1.1 GB for the same filesystem
  quoted on the same page — i.e. plocate's index is **~42% the size of
  mlocate's**, despite storing strictly more information (a real inverted index
  vs. a flat sorted list). [upstream-documented, plocate.sesse.net —
  again, exact test corpus not confirmed from the fetched excerpt, treat as author-reported]
- Access control model unchanged from mlocate/slocate lineage: results are
  filtered per requesting user by directory `+rx` permission checks, not a
  separate ACL store.
- I could **not** get authoritative numbers for build time or throughput
  (files/second during `updatedb`/`plocate-build`) from what was fetchable — the
  plocate.sesse.net page and man pages don't quote a number, and I did not have
  `plocate`/`plocate-build` installed locally to measure it myself. **No
  published build-time figure found.**

## mlocate / GNU findutils locate+updatedb

`mlocate` (merge-locate): the database is a sorted list of pathnames with
**directory-level front compression** (each directory's entries are stored as a
diff against the previous directory), searched by `locate` doing effectively a
**linear scan with substring matching** over the (moderately compressed) DB — no
inverted index, no trigrams. This is why every query costs roughly O(DB size)
regardless of query selectivity: the whole reason plocate exists is to give
locate an index instead of a scan. mlocate also caches per-directory mtimes so
`updatedb` can skip unchanged directories incrementally — that part is still
fast (its actual defect is the _search_ path, not the update path).

GNU findutils' own `locate`/`updatedb` predates mlocate and uses a simpler
"front-coding" ASCII format (each entry stores a common-prefix count + suffix).
Same scan-bound query story. It's the tool that's actually in the OS on many
minimal distros/containers even though most desktop Linux moved on.

**Why they were slow.** No index at all on the query side: `locate PATTERN`
substring-matches PATTERN against every decompressed entry in the (potentially
GB-scale) DB. Doubling the number of files roughly doubles every query's cost —
there's no sublinear structure to exploit. This is exactly the defect plocate's
trigram index fixes.

## fsearch

Source: <https://github.com/cboxdoerfer/fsearch>, project page
<https://cboxdoerfer.github.io/fsearch/>. GTK app, explicitly modeled on
Windows' "Everything" (which uses live NTFS-MFT reads, not applicable on Linux,
hence fsearch substitutes a full filesystem walk + in-memory database).

**Design.** fsearch builds its entire file/directory database **in memory** on
startup/first index (there is a persistent on-disk cache of scanned entries
between runs, but the live query structure is in-RAM), then does fast in-memory
substring/regex matching with no disk I/O during search — this is why it feels
instantaneous per keystroke once loaded, at the cost of RAM proportional to file
count (roughly one entry struct per file/dir, order of tens-of-bytes to ~100
bytes each depending on path length and metadata cached) and a load-time cost
proportional to total file count on cold start. For ~1M files this puts RAM in
the "tens to ~100 MB" range by rough extrapolation, but **I found no
upstream-published RAM-per-file or load-time-per-file figure** — the one number
publicly discussed (≈420,000 entries indexing "/home" in a few seconds) is a
forum anecdote, not a measured benchmark. [community-anecdote — Arch
forum thread] **No authoritative RAM/load-time table found.**

**Failure mode implication for a 1M-file/10–200GB corpus:** fsearch's model
doesn't care about the _bytes_ of file content at all — it only indexes
names/metadata, so corpus size in GB is irrelevant to it; only file _count_
matters. That makes it a poor comparison point for a tool that also has to do
content search, but a very relevant one for the "pure filename" half of Dave's
requirement.

## ANGRYsearch, catfish

Both are thin UI layers, not independent index engines:

- **ANGRYsearch** (<https://github.com/DoTheEvo/ANGRYsearch>) is a PyQt GUI that
  shells out to mlocate's existing database and loads matches into memory for
  fast incremental filtering as you type — it inherits mlocate's DB format and
  staleness characteristics (depends on cron'd `updatedb`), it doesn't build its
  own index. Development has been largely dormant.
- **catfish** (<https://github.com/catfish-search/catfish>) is a search-tool
  front-end that picks whichever backend is available (`locate`, `find`,
  Tracker, Baloo) — no independent index of its own, so its performance and
  index-format story is entirely whatever backend it's delegating to that
  session.

Neither is a serious data point for index design; both matter only as UX
precedent (instant-as-you-type filtering over a backend result set).

## fzf + fd, broot

Both are explicitly **unindexed, walk-time** tools — relevant here as the "no
index at all" baseline for filenames, mirroring ripgrep/ugrep/ag's role for
content:

- **fd** (<https://github.com/sharkdp/fd>) walks the filesystem with parallel
  directory traversal (rayon-based), applies a glob/regex (Rust `regex` crate)
  per entry, and has no persistent index — every invocation re-walks. It
  respects `.gitignore` by default like ripgrep, which materially reduces work
  on source trees.
- **fzf** (<https://github.com/junegunn/fzf>) is a fuzzy line-matcher, not a
  filesystem walker itself; typically fed by `fd`/`find`, it holds the candidate
  list in memory and does incremental fuzzy scoring (not regex) per keystroke.
- **broot** (<https://github.com/Canop/broot>) is a live tree-navigator with
  built-in search-as-you-descend; also walk-time, no persistent index, Rust
  `regex` crate for pattern matching within the visible tree.

None of these publish index-size or build-time numbers because none of them
build a persistent index — their whole selling point is that a warm-cache walk
over a modern NVMe-backed tree is fast enough not to need one for interactive
use at moderate file counts. Whether that holds at 1M files / 200GB on a
spinning-adjacent or cold-cache workload is exactly the question a persistent
index exists to answer, and I found no rigorous fd/fzf-at-1M-files benchmark —
treat any number here as [estimated] until measured.

## Regex/code search with an index

### Google Code Search / Russ Cox's `codesearch` (`cindex`/`csearch`)

Primary source: Russ Cox, "Regular Expression Matching with a Trigram Index" —
<https://swtch.com/~rsc/regexp/regexp4.html>. Code:
<https://github.com/google/codesearch> (Go, BSD-3, effectively archived — last
substantive work ~2015 per the repo's own description, no active maintenance
since).

**Index.** `cindex` builds a trigram index: for every 3-byte substring across
all indexed files, a posting list of file IDs containing it, stored as a
gob-encoded on-disk structure alongside the list of indexed paths/files. **Index
size ≈ 20% of source size**, measured by Cox against a 420 MB Linux 3.1.3 source
tree yielding a 77 MB index. [upstream-documented, swtch.com/~rsc/regexp/regexp4.html]

**Query algorithm — the reusable part.** See the dedicated section below;
csearch is the reference implementation of the algorithm. Regex syntax is RE2
("basically Perl's, but without backreferences"); `-brute` bypasses the index
entirely for patterns the compiler can't turn into a useful trigram query.

**Numbers.** On the DATAKIT example query in the paper, the trigram index
reduced candidates from 2,739 files to 3 before the final regex verify pass;
case-sensitive searches got roughly 100x speedup over brute force,
case-insensitive roughly 10-15x (fewer distinguishing trigrams once case
variants are OR'd in). [upstream-documented]

**Status/failure modes:** dead project — no incremental update story beyond
re-running `cindex` (it's a batch full-rebuild tool, not designed for live
filesystems), no daemon, no filesystem watch. Anything built today citing this
design should treat it as the algorithm reference, not a deployable tool.

### zoekt

Primary source: design doc in the repo,
<https://github.com/sourcegraph/zoekt/blob/master/doc/design.md> (originally a
Google project by Han-Wen Nienhuys, now maintained by Sourcegraph). Go,
Apache-2.0, actively maintained (Sourcegraph ships it as their code-search
backend).

**Index.** Positional trigrams: for each trigram, zoekt stores not just "which
file" but **offsets within the file**. To match a longer literal (e.g. "The
quick brown fox"), instead of intersecting posting lists for every trigram in
the string and then verifying, zoekt extracts two trigrams from the string and
checks that their recorded offsets are the **correct distance apart** before
falling through to full verification — this is the "trigram with
successor/distance" trick, and it dramatically cuts the intersection work for
long literals compared to naive AND-of-all-trigrams. [upstream-documented,
zoekt design.md via fetch above]

**On-disk layout.** Index is split into **shards**, each a single
memory-mappable file containing file contents, filenames, content posting lists
(varint-encoded), filename posting lists, per-branch bitmasks (for multi-branch
repos), and format/repo metadata. Shards are capped near **4 GB** because
offsets are `uint32` — in practice each shard holds up to roughly 1 GB of source
content once index overhead is counted.

**Index size.** Roughly **3.5x the corpus size** once content + posting lists +
metadata are all counted, with posting lists alone contributing roughly 2x
overhead on top of the stored content. [this figure came back from
the design-doc fetch above but I could not re-verify the exact wording/number
against the live doc in a second pass — treat as **upstream-documented
but re-verify against `github.com/sourcegraph/zoekt/blob/master/doc/design.md`
directly before quoting in the final report**, since design docs like this
get edited]

**Case folding.** For case-insensitive queries, zoekt expands each literal into
its case-variant OR-set (e.g., "abc" → "abc"/"Abc"/"aBc"/... trigram unions)
rather than folding case in the index itself — same fundamental cost tradeoff
Cox describes for csearch's case-insensitive path (fewer effective
distinguishing trigrams, more false positives to verify).

**Query language / regex engine:** RE2 (Go's `regexp/syntax` derived), matching
the Cox lineage; boolean query trees (AND/OR/NOT), repo/branch/file filters, and
shard-level partial evaluation so a shard that can't possibly match a
trigram-AND clause is skipped without decompressing it.

**Incremental updates:** zoekt is built around whole-repo re-indexing per shard
on change (git-aware — it indexes a specific commit/branch state), not
fine-grained file-level incremental updates; Sourcegraph's operational model is
"re-index the repo shard when its HEAD moves," which is fine at Sourcegraph's
git-centric granularity but is a mismatch for a live desktop filesystem where
individual files change constantly outside of any VCS commit boundary.

### Sourcegraph search architecture

Sourcegraph's product search layer sits on top of zoekt for indexed search and
falls back to a "searcher" service doing unindexed ripgrep-style search (their
own "universal search" federates indexed-zoekt results with live-grep results
for unindexed/large/monorepo cases). This is architecturally the most directly
relevant precedent for "index what you can, live-grep the rest" — worth citing
as prior art for a hybrid design, but it's a distributed multi-service
architecture built for many repos/many users, not a single-desktop design; most
of its complexity (repo sharding, horizontal search-service scaling) doesn't
transfer to a 1-machine/1M-file target.

### livegrep

Primary source: Nelson Elhage, "Regular Expression Search with Suffix Arrays" —
<https://blog.nelhage.com/2015/02/regular-expression-search-with-suffix-arrays/>.
Code: <https://github.com/livegrep/livegrep> (C++ backend + Go frontend,
Apache-2.0, low ongoing activity — treat as reference architecture, not a
maintained product).

**Design — the key difference from trigram indexes.** livegrep does **not**
build an inverted index at all. It concatenates the entire corpus into one giant
in-memory buffer and builds a **suffix array** over it (using the
`libdivsufsort` library). Because a suffix array is a sorted array of all
suffixes of the buffer, any _literal_ substring search becomes a binary search
over the suffix array — O(log n) comparisons each of up to O(pattern length), no
false positives, no posting-list intersection at all. For patterns with a
required literal component, livegrep extracts that literal,
suffix-array-searches for it, and only then applies RE2 verification/expansion
for the non-literal parts of the pattern (the same false-positive-then-verify
idea as trigram search, but with an exact index for the literal-anchor step
instead of a lossy one). [upstream-documented, blog.nelhage.com]

**Why this differs from trigram indexing:** a trigram index can only ever narrow
to "files containing all required 3-byte substrings" and still needs a full
regex verification pass over every surviving file. A suffix array, by contrast,
can directly answer "where does literal X occur" _exactly_, with no false
positives, which is strictly more precise per byte of index — the tradeoff is
that the whole corpus must be held as one contiguous addressable buffer
(RAM-resident or mmap'd) and the suffix array itself costs roughly one
4-or-8-byte integer per byte of source (i.e. **the index is on the order of the
corpus size, or several times larger** depending on whether 32-bit or 64-bit
suffix positions are used) — this is why livegrep was built for a single large
but bounded corpus (Linux kernel HEAD, ~1 commit) rather than something that has
to persist and incrementally update a much larger, constantly changing tree.
[estimated — I did not find an upstream-quoted index-size multiplier; the
"one integer per input byte" cost is inherent to the suffix-array data
structure, not something I measured]

**Regex engine:** RE2, same choice as Cox's csearch and zoekt.

**Practical fit for Dave's target (1M files, 10–200GB):** a naive suffix array
over 200GB at 4 bytes/char is 800GB of index just for the SA itself, before the
corpus buffer — this design does not scale to the stated corpus size without
heavy modification (compressed suffix arrays / FM-index territory, which is a
much harder build). Flag this prominently: **livegrep's approach is a poor
structural fit at the stated scale** unless paired with a compressed suffix
array variant, which the project itself does not implement.

### hound (Etsy)

<https://github.com/hound-search/hound> — Go, MIT, largely dormant. Wraps
`google/codesearch`'s trigram index library directly (same on-disk
trigram/posting-list format, same ~20% index-size ratio, same RE2 engine) behind
a web UI with a config-driven multi-repo indexer and periodic re-pull/re-index.
Contributes no new index design over csearch; relevant only as evidence that the
Cox trigram index has been reused as a library, not just a paper.

### OpenGrok

<https://github.com/oracle/opengrok> — Java, CDDL, Oracle-maintained (slow
release cadence but not dead — releases roughly annually). Structurally
different from the rest of this list: it indexes with **Apache Lucene** (a
tokenized inverted index, not a raw trigram byte-index) plus a separate
**ctags-derived symbol database** for definitions/xrefs. Because Lucene
tokenizes source code (splitting on identifier/punctuation boundaries via a
source-aware analyzer), it supports fast identifier and phrase search but its
"regex" support is Lucene's `RegexpQuery`, which operates on the
tokenized/analyzed terms, not true full-text PCRE/RE2 semantics over raw bytes —
an important caveat when comparing "does it support real regex." Index size
relative to source is a general Lucene characteristic (typically 0.5–2x
depending on tokenization and stored-fields configuration) rather than a
trigram-specific ratio; I did not find an OpenGrok-specific published number.
[community-anecdote for the ratio range — generic Lucene behavior,
not OpenGrok-verified]

### Krugle

Krugle was a commercial code-search product; the product and company are defunct
and I found no current source, design doc, or maintained fork. **Not worth
further research time** — flag in done-note that this bullet from the brief is a
dead end.

### cscope, GNU Global (gtags), ctags/universal-ctags

These are **symbol indexes**, not text/regex search engines, and answering "does
it support regex" the way the brief wants requires saying plainly: **no, not in
the sense the rest of this document means.**

- **cscope** (<http://cscope.sourceforge.net/>) builds a custom cross-reference
  database (`cscope.out`) of C-language symbol relationships (who calls this
  function, where is this symbol used, etc.) via its own C-aware parser — it is
  not a generalized regex-over-text engine, though it does support one query
  mode that's an "egrep pattern," implemented as a plain scan, not indexed.
- **GNU Global** (<https://www.gnu.org/software/global/>) builds
  `GTAGS`/`GRTAGS`/`GPATH` tag databases, with pluggable back-end parsers (its
  own, or delegating to universal-ctags/pygments) supporting many languages;
  still fundamentally a definition/reference index, not a substring/regex index
  over raw text.
- **ctags/universal-ctags** (<https://github.com/universal-ctags/ctags>)
  produces a flat, sorted `tags` file mapping symbol name → file/line/pattern;
  trivial size (one line per tag), instant lookup, no regex search capability at
  all beyond editor integrations that grep the tags file itself.

These matter to the report only as the answer to "should a new tool also index
symbols, not just trigrams" — worth a design footnote, not competitive
alternatives to regex/content search.

### ripgrep, ugrep, ag, ack — the unindexed baseline

These set the bar an index has to beat: if a query is fast enough unindexed on
warm cache, the entire cost of building/maintaining an index only pays for
itself on cold cache or at larger-than-RAM corpora.

**ripgrep** (<https://github.com/BurntSushi/ripgrep>), Rust, MIT/Unlicense dual,
very actively maintained (v15.1.0 present on this machine, confirmed via
`rg --version`, built with `+pcre2` feature). Uses the Rust `regex` crate by
default (a linear-time finite-automata engine, no backtracking, so worst-case is
not exponential the way PCRE backtracking can be) and can opt into PCRE2 with
`-P` for backreferences/lookaround at the cost of that engine's backtracking
risk.

**ugrep** (<https://github.com/Genivia/ugrep>), C++, BSD-3, active. Own regex
engine (a DFA-based engine derived from the author's RE/flex work) plus optional
`--pcre` via Boost.Regex/PCRE2. Not installed on this research machine, so no
local measurement was possible this pass.

**ag / The Silver Searcher** (<https://github.com/ggreer/the_silver_searcher>),
C, Apache-2.0, effectively unmaintained (infrequent commits, long-open issues).
PCRE-based.

**ack** (<https://beyondgrep.com/>), Perl, low activity. Consistently the
slowest tool in every third-party benchmark found.

**Throughput numbers (all third-party, not run by me on my own corpus this
pass):**

From the ripgrep.dev benchmark page (<https://ripgrep.dev/benchmarks/>), run on
an Intel i9-12900K/64GB/NVMe, Ubuntu 22.04, via `hyperfine`, median of 10 runs,
**warm cache**: [third-party-benchmark]

| Corpus                                        | Pattern                        | rg     | ugrep         | git grep      | GNU grep      | ag            | ack            |
| --------------------------------------------- | ------------------------------ | ------ | ------------- | ------------- | ------------- | ------------- | -------------- |
| Linux kernel 6.6 source, ~75k files, ~900MB   | `[A-Z]+_SUSPEND` (536 matches) | 0.082s | 0.301s (3.7x) | 0.273s (3.3x) | 0.671s (8.2x) | 0.443s (5.4x) | 3.231s (39.4x) |
| 13.5GB single file (Project Gutenberg concat) | "Sherlock Holmes"              | 6.73s  | 7.02s         | —             | 9.20s (1.4x)  | 34.60s (5.1x) | —              |

From the same page: ripgrep on the 13.5GB single-file case implies roughly
**2GB/s** literal-search throughput on warm cache on that hardware
[third-party-benchmark, derived: 13.5GB / 6.73s ≈ 2.0 GB/s]. A separately-cited anecdote
(DEV Community article) claims rg completing a 13.5GB grep in 1.664s vs GNU grep
9.484s — this **contradicts** the ripgrep.dev number above (6.73s vs 1.664s for what's
described as the same corpus/pattern class) by roughly 4x; I could not reconcile
which is right without re-running it myself. **Flag this contradiction explicitly
— do not average or pick one silently.** [third-party-benchmark, conflicting]

I was not able to run a controlled local benchmark this pass (no representative
200GB/1M-file corpus present on this research machine — `/home/dave/w` is 142GB
but mixed content, not warm-cache-controlled, and running a multi-minute grep
sweep was out of scope for the time budget here). **This is a gap** — the final
report should either accept the ripgrep.dev numbers as the best available
third-party source, or budget a real `hyperfine`/`rg`-vs-`ugrep` run against
Dave's actual target corpus before deciding whether an index is justified at
all.

### ugrep-indexer

Primary source: <https://github.com/Genivia/ugrep-indexer> (merged into ugrep
≥6.0 per the repo's own description — check `ugrep --version` for whether a
given install has it built in, since 6.0 is recent).

**Design.** Not a trigram index — a **Bloom-filter-based per-file index**. For
each indexed file, ugrep-indexer builds a Bloom filter over hashes derived from
short substrings (up to the first ~16 bytes of the _pattern_ at query time,
matched against precomputed hash tables built from the file's content at index
time) so that a query can cheaply test "could this file possibly contain this
pattern" and skip files that can't, without ever opening/decompressing a real
posting list. It deliberately uses **N² hash functions instead of the textbook
N**, because the author found the standard Bloom-filter parameterization has too
high a false-positive rate for the short substrings typical of code search —
trading construction cost for fewer files that need real grepping.
[upstream-documented, Genivia/ugrep-indexer README]

A cheap pre-filter is applied before even consulting the Bloom filter: reject a
file immediately if any byte in the pattern's first 8 bytes doesn't occur
_anywhere_ in the file at all (a raw byte-histogram check, stricter/cheaper than
the Bloom filter and zero false positives for that specific check).
[upstream-documented]

**Numbers.** Upstream claims **>10x speedup** on grepping with the index vs.
without, across large trees [upstream-documented, project README/tagline], but
does not quote an index-size-vs-corpus ratio in what I could retrieve. Not
installed locally — no independent measurement possible this pass.

**Failure mode:** false positives are inherent to any Bloom-filter-based
file-level index (as opposed to plocate/csearch's trigram-level posting lists) —
a file that merely _contains the right bytes in some order_ still gets grepped
for real even if the actual pattern isn't present, so precision degrades for
very short or very common patterns exactly the way it does for trigram indexes,
just via a different mechanism. No incremental update mechanics beyond
re-running the indexer; treat as batch, same as everything else surveyed here
except plocate's async I/O framing.

## Newer entrants (2024–2026 search)

I ran targeted searches for new Rust/Go/Zig code-search or filename-search
projects in this window and did not turn up a genuinely new indexed-search
engine beyond what's already covered (zoekt/Sourcegraph continue to be the
active reference implementation; ripgrep/ugrep continue to be the active
unindexed baseline). Notable adjacent movement worth flagging to the main
report:

- **Cursor's "fast regex search: indexing text for agent tools"**
  (<https://cursor.com/blog/fast-regex-search>) — surfaced in the livegrep
  search above — is a 2025/2026-era writeup from an AI-coding-tool vendor about
  building a trigram-adjacent index specifically to serve LLM-agent-driven
  codebase search quickly. This is directly relevant prior art for "why would
  you build this in 2026" and is worth the main report reading directly rather
  than taking my paraphrase — I did not fetch its full content this pass due to
  time budget; **flag as unread-but-relevant** in the done-note.
- No credible new plocate/csearch/zoekt-class Rust project found. This is itself
  a finding: the trigram-index-for-code-search space has had essentially one
  dominant living implementation (zoekt) for years, and the filename-index space
  has one (plocate) — a Rust-native equivalent of either does not appear to
  exist yet as a mature project, which is either a warning sign (hard to get
  right) or a gap (opportunity), and the report should say which.

## Regex-over-trigram-index algorithm — worked explanation

This is the mechanism behind csearch, zoekt, hound, and (for filenames) plocate.
Primary source throughout: Russ Cox,
<https://swtch.com/~rsc/regexp/regexp4.html>.

### The data structure

For every distinct trigram (3-consecutive-byte substring) that appears anywhere
in the corpus, store a **posting list**: the sorted list of file (or, for zoekt,
file+offset) IDs where that trigram occurs. A corpus of source code typically
has on the order of a few hundred thousand distinct trigrams (bounded by 256³ ≈
16.7M possible, but real text uses a small fraction), each with a posting list
whose length is proportional to how common that byte-triple is.

### Compiling a regex into a trigram query

The algorithm computes, for every sub-expression of the parsed regex, four
properties, combined bottom-up over the regex's AST:

- **match**: a boolean query over trigrams (AND/OR tree) that _every string this
  sub-expression can match_ is guaranteed to satisfy. This is the thing you
  actually evaluate against the posting-list index.
- **exact**: the precise, small set of strings this sub-expression can match, if
  it's small enough to enumerate (e.g. a literal, or a short alternation) — used
  to derive tighter trigram queries for concatenations.
- **prefix** / **suffix**: sets of possible leading/trailing substrings (used
  when `exact` is too large to enumerate, so you can still say something about
  the trigrams spanning a concatenation boundary).

**Base cases:**

- Literal string of length ≥3 (`"Search"`): `exact = {"Search"}`,
  `match = trigrams("Search") = "Sea" AND "ear" AND "arc" AND "rch"`.
- Literal string of length <3 (`"a"`, `"ab"`, `""`): `trigrams()` of a too-short
  string is defined as `ANY` — no constraint can be derived, so `match = ANY`.
  This is the formal statement of "too-short-a-query degenerates to a full
  scan": there simply isn't a 3-byte window to hash.
- Any single wildcard byte (`.`): `match = ANY` — it can be anything, so nothing
  about its trigrams is knowable.

**Compound rules:**

- **Alternation** `e1|e2`: `match = match(e1) OR match(e2)`. Example:
  `/Google|Yahoo/` →
  `("Goo" AND "oog" AND "ogl") OR ("Yah" AND "aho" AND "hoo")`.
- **Concatenation** `e1 e2`: `match = match(e1) AND match(e2)`, **and then** the
  algorithm tries to tighten further using trigrams that straddle the boundary
  between e1 and e2, via `trigrams(suffix(e1) × prefix(e2))` — this is what lets
  `/foo.*bar/` still contribute the trigrams of "foo" and "bar" even though the
  `.*` in between contributes nothing (see below).
- **Repetition** `e*`, `e+`, `e{n,m}`: for `e+` (one-or-more),
  `match = match(e)` (must contain at least one occurrence's trigrams). For `e*`
  (zero-or-more, including zero), `match = ANY` **unless** more context is
  available from surrounding concatenation — this is the single most important
  degenerate case to understand: `.*` by itself is unindexable because it might
  match the empty string, so nothing can be _required_ to be present anywhere.

### Worked examples

1. `/Google.*Search/` Compiles to:
   `"Goo" AND "oog" AND "ogl" AND "gle" AND "Sea" AND "ear" AND "arc" AND "rch"`.
   The `.*` in the middle contributes nothing (per the repetition rule above),
   but concatenation still lets both literal halves contribute their own
   required trigrams, since _both_ literals must be present somewhere in any
   matching string regardless of what's between them. This is the example from
   the paper directly.

2. `/ab*c/` (zero-or-more `b`) `b*` alone would be `ANY`, but concatenated with
   a literal `a` before it and `c` after, the tightening step can still derive
   the required trigram set is looser — in the degenerate case where the string
   could be just `"ac"` (b appears zero times), there's no guaranteed 3-byte
   substring spanning a-then-c, so `match` ends up close to `ANY` — this is the
   case where a query "looks like it should be indexable" but isn't, because of
   a `*` sitting right at the literal/literal boundary.

3. `/[Gg]oogle/` (case-insensitive-ish via character class) Expands to
   alternation over the character class: `match =` trigrams for `"oogle"`
   combined with the first-char choice — in practice the engine treats this like
   `/Google|google/`, i.e.
   `("Goo" AND "oog" AND ... ) OR ("goo" AND "oog" AND ...)`. This is exactly
   why **case-insensitive search costs more query terms and yields more false
   positives**: Cox reports roughly 10-15x speedup for case-insensitive vs.
   roughly 100x for case-sensitive on the same corpus, because the OR over case
   variants is strictly less selective than a single AND chain.
   [upstream-documented, swtch.com/~rsc/regexp/regexp4.html]

4. Short/degenerate: `/ab/` (2-byte literal) `trigrams("ab") = ANY` by
   definition (string too short for even one trigram) — the index contributes
   nothing, and the search degrades to "check every file" followed by a real
   substring search. This is the formal version of "short queries can't be
   indexed" that the brief asks about.

5. Leading/trailing wildcard: `/.*\.log$/` The `.*` at the front is unindexable
   on its own (per rule above), and `\.log$` is a 4-byte literal (well, 4 chars:
   `.log`) so its own trigrams (`".lo"`, `"log"`) _do_ contribute — the query
   becomes `".lo" AND "log"`, which is indexable and reasonably selective (most
   files don't contain the literal substring ".log" anywhere), even though the
   overall regex has an unanchored, theoretically-unindexable prefix. This is
   the general shape of "even a bad-looking regex often still yields a useful
   query because _some_ literal fragment survives the AST decomposition" — worth
   stressing to a builder, because the naive intuition ("this regex has a
   wildcard, so it can't be indexed") is usually wrong; only _purely_
   wildcard-with-no-anchoring-literal patterns (e.g. bare `.*`, or `.{3,}`)
   degrade all the way to `ANY`.

### Execution once the query is built

1. Evaluate the AND/OR trigram query against the on-disk posting lists,
   intersecting (AND) or unioning (OR) sorted file-ID lists — same algorithm as
   boolean document retrieval in classical IR, and the same reason posting lists
   are kept **sorted by file ID**: sorted-list intersection is linear in the sum
   of the list lengths, and evaluating rarest-trigram-first (smallest posting
   list) minimizes intermediate set sizes, exactly the "AND ordering"
   optimization every one of these tools (csearch, zoekt, plocate) applies.
2. The result is a **candidate set that is a superset of true matches** — it can
   contain false positives (a file that has all four required trigrams present,
   but not in the arrangement the regex actually requires — e.g. "Sea", "ear",
   "arc", "rch" could in principle appear scattered across a file without ever
   forming contiguous "Search"). It **cannot** contain false negatives — every
   trigram condition derived from the AST is a necessary (not sufficient)
   condition for a real match, so the algorithm never rules out a file that
   could match.
3. Run the **actual regex engine** (RE2 for csearch/zoekt/livegrep; Rust `regex`
   for a Rust-native build) only against the surviving candidate files, and only
   that final pass determines real matches. This is the
   "false-positive-then-verify" architecture named in the brief — the index's
   whole job is to shrink the set that needs the expensive real-regex pass,
   never to _be_ the real regex pass.

### What makes a query "unindexable" — summary for a builder

A query degrades toward `match = ANY` (i.e., the index contributes nothing and
you fall back to scanning every file) whenever the regex's derivable
required-substring set is empty. Concretely:

- Any literal fragment shorter than 3 bytes anywhere it's the _only_ constraint.
- A `*`/`?` applied directly to something whose only content is a short literal,
  with no adjacent literal to concatenate against (an isolated `a*` or `.*`).
- Heavy character-class alternation with many branches (case-insensitivity is
  the everyday version of this) — technically still indexable, but the OR blows
  up the number of trigram terms and reduces selectivity per the
  case-insensitive numbers above.
- Anchors alone (`^`, `$`, `\b`) contribute no trigram information — they're
  purely a verification-time constraint.

A builder implementing this from scratch should structure it exactly as Cox's
compiler does: a bottom-up walk of the parsed regex's AST computing (`match`,
`exact`, `prefix`, `suffix`) per node with the rules above, a final flattening
of `match` into a boolean-of-sets query, then a standard sorted-posting-list
AND/OR evaluator, then a real-regex verify pass over survivors. zoekt's
"trigram-with-distance" refinement (store trigram _offsets_, not just file
membership, and use offset arithmetic to verify longer literals cheaply before
falling all the way to a full regex pass) is a worthwhile addition once the
basic version works, because it meaningfully reduces the number of files that
need the expensive final regex pass for anything longer than a single trigram's
worth of literal content.

### Size-vs-latency: where each design actually sits

Given the stated preference ("slightly slower search for a denser index"), here
is where the surveyed designs land on that specific axis, cheapest-index first:

| Design                                                | Index size vs corpus                                                                                                                                                                                                                                                                                          | What it buys at that price                                                                                                                                            | Where the latency cost shows up                                                                                                                                                                                                                                                                                                                                                                                                                                             |
| ----------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Cox's basic trigram index (csearch/hound)             | **~20%** [upstream-documented]                                                                                                                                                                                                                                                                                | Cheapest of the indexed designs; file-level posting lists only (no offsets)                                                                                           | Every surviving candidate needs a full-file regex re-scan — no offset info to shortcut long-literal verification                                                                                                                                                                                                                                                                                                                                                            |
| plocate (filename-only, block-compressed)             | index ≈ 0.42x of mlocate's flat DB for the same tree [community-anecdote, informal] — smaller than a naive per-file trigram index because postings point at _blocks_ of filenames, not individual filenames                                                                                                   | Densest filename design surveyed; the block grouping is a direct, deliberate size/precision trade the plocate author states outright                                  | Coarser postings (block-level, not file-level) mean **more false positives per posting hit**, so more decompress-and-recheck work per query than a per-file trigram index would need — this is exactly the "spend a little latency, save a lot of bytes" trade the reader wants, just applied to filenames rather than content                                                                                                                                              |
| OpenGrok (Lucene tokenized index)                     | ~0.5–2x [community-anecdote, generic Lucene, not OpenGrok-specific]                                                                                                                                                                                                                                           | Rich per-term structure (positions, term frequencies, stored fields) enabling ranked/phrase query support trigram indexes don't give you for free                     | Regex support is bolted onto the tokenized term dictionary (`RegexpQuery` enumerates matching terms), not a raw-byte trigram scan — different cost model entirely, not a clean point of comparison on this axis                                                                                                                                                                                                                                                             |
| zoekt (positional trigram + successor/distance check) | **~3.5x** [upstream-documented, re-verify per done-note]                                                                                                                                                                                                                                                      | Buys offset-level verification: a long literal can be confirmed correct-distance-apart from two trigram hits without ever running the full regex engine over the file | This is the design the stated preference argues _against_ — it is deliberately spending roughly 3.5x the raw corpus in bytes specifically to shave query latency; a Super Ferret build following "denser index, slower query" should treat zoekt's ratio as close to an upper bound to avoid, not a target                                                                                                                                                                  |
| livegrep (raw suffix array)                           | on the order of 1x-plus per byte for the array alone (4 or 8 bytes of index per **source byte**, not per file) [estimated — no upstream multiplier found] — this is a fundamentally different scaling law than the trigram designs, since it's proportional to corpus _bytes_ not corpus _trigram vocabulary_ | Exact substring answers with zero false positives on the literal-anchor step; best possible query latency among everything surveyed                                   | The most extreme point on the "spend bytes for speed" end of the spectrum in this whole survey, and explicitly the wrong direction for the stated preference at the 200GB scale in play — flagged above as a poor structural fit for that reason, independent of the size/latency preference                                                                                                                                                                                |
| ugrep-indexer (Bloom filter, file-granularity)        | not published, but structurally the cheapest possible per-file signal — a fixed-size bitset per file regardless of file length, no posting lists at all                                                                                                                                                       | Near-zero index overhead per file                                                                                                                                     | Bloom filters have no false-negative rate but do have a tunable false-positive rate (hence the N² hash-function choice) — cheap index, but every false positive costs a full grep of that file, so it sits at the opposite end from zoekt: minimal bytes, willing to eat re-scan cost. This is structurally the closest existing design to what the stated preference is asking for, and is worth reading in full before designing Super Ferret's own file-level pre-filter |

Read against the stated preference, the two designs worth studying most closely
are **plocate's block-level posting-list compression** (deliberately coarsens
posting-list granularity to save bytes, accepts more post-filter work) and
**ugrep-indexer's Bloom-filter approach** (near-zero fixed index cost per file,
all imprecision pushed to query time) — both are permissively-licensed-adjacent
in spirit (ugrep-indexer is BSD-3; plocate itself is GPLv2, so its _design_ is
fair game to reimplement from this description but its _code_ is not vendorable
into a permissive project). zoekt and livegrep are the reference points for
"what does it cost to buy the last bit of query latency," useful to understand
but sitting on the wrong side of the stated tradeoff to imitate directly.

## Done-note

**What I could not verify / should be re-checked before the final report
ships:**

1. **zoekt's "3.5x index-size" figure** — this came back from a single WebFetch
   extraction of the live design doc and I did not independently confirm the
   exact wording or number on a second pass. Design docs get edited; re-fetch
   `https://github.com/sourcegraph/zoekt/blob/master/doc/design.md` directly
   before quoting this number as settled.
2. **plocate's headline benchmark (0.008s vs 20.118s, 466MB vs 1.1GB)** — quoted
   from plocate.sesse.net, but I could not confirm the exact test
   corpus/methodology behind those numbers from the fetched excerpt (they read
   like the author's own demo numbers, likely from the original 2020
   announcement, not a controlled independent benchmark). Treat as
   upstream-documented-but-informal, not a rigorous measurement.
3. **plocate build time / build throughput**: genuinely absent from everything I
   could fetch. plocate was not installed on this machine so I could not measure
   it myself. This is a real gap for a report evaluating build-a-new-tool
   feasibility — build time on a 1M-file/200GB corpus is a first-order design
   constraint and needs a real number, either from the plocate source/mailing
   list or a fresh local measurement.
4. **Direct contradiction in third-party ripgrep benchmarks**: ripgrep.dev's own
   benchmark page says 6.73s for a 13.5GB grep; a separately-surfaced DEV
   Community article claims 1.664s for what it describes as the same scenario.
   These differ by ~4x and I did not reconcile them. **Do not average these or
   silently prefer one** — the final report should either re-run this locally
   (`hyperfine 'rg PATTERN bigfile'` on a real 10GB+ warm-cache file) or
   explicitly present both with the discrepancy flagged, since a 4x throughput
   uncertainty on the exact baseline number the report is trying to "beat with
   an index" is not a rounding error.
5. **No local benchmarking was performed this pass** despite having `rg`
   (v15.1.0, +pcre2), `fd`, and `fzf` installed on this machine — the working
   directory (`/home/dave/w/super-ferret`) had essentially no content to grep,
   and running a multi-minute sweep over the unrelated 142GB `/home/dave/w` tree
   was judged out of scope for this slice's time budget. **This is a real gap,
   not a decline for lack of tooling** — the tools to produce a genuine
   `[measured-by-me]` ripgrep/ugrep number were present and unused. If the
   overall report needs a first-party throughput figure rather than third-party
   citations, budget a follow-up pass specifically for it, ideally against a
   corpus resembling the 1M-file/10-200GB target rather than an arbitrary
   directory.
6. **Krugle** is dead with no findable primary source — I spent minimal time and
   recommend the main report drop it rather than cite secondhand marketing-era
   descriptions.
7. **livegrep's suffix-array index-size multiplier** is my own structural
   inference (one integer per source byte for the raw SA), not a number Elhage's
   writeup itself states in what I fetched — flagged `[estimated]` above, worth
   a direct read of the full blog post (only the summary came back from
   WebSearch, not a full WebFetch) before treating it as settled.
8. **Cursor's 2025/2026 "fast regex search" blog post** was found but not read
   in full — it's plausibly the single most relevant piece of
   _why-build-this-in-2026_ prior art in this entire slice (a frontier AI-coding
   vendor recently justifying a custom index for exactly Dave's stated corpus
   shape) and deserves a dedicated fetch pass:
   <https://cursor.com/blog/fast-regex-search>.
9. **Scope note, not a complaint**: this slice is wide (8+ tools plus a
   from-scratch algorithm explanation) for the depth the brief also asks for
   per-tool ("exact on-disk format," "throughput," "known failure modes").
   Several tools above (ANGRYsearch, catfish, Krugle, ctags) are correctly thin
   because they genuinely have little independent design to report — that's a
   finding, not a shortcut — but the tools that matter most to a build-vs-not
   decision (plocate, zoekt, ripgrep/ugrep baseline numbers) each warrant more
   verification time than one research pass allowed. If the overall report is
   going to lean heavily on any single number from this file, re-verify it
   against the cited primary source directly rather than trusting this
   document's restatement.

**What the overall report must not miss:**

- The **core algorithmic insight** (regex AST → required-trigram-AND/OR query →
  posting-list intersection → real-regex verify) is the same across every
  serious indexed code-search tool built since 2006 (Google Code Search) through
  today (zoekt). A new Rust tool doesn't need to invent a new algorithm here —
  the design space that's actually open is (a) incremental update on a live,
  non-VCS-anchored desktop filesystem, which none of csearch/zoekt/livegrep
  solve well, since all three assume a batch-rebuilt-or-git-commit-anchored
  corpus, and (b) combining a filename-trigram index (plocate's problem) with a
  content-trigram index (zoekt's problem) in one coherent on-disk format, which
  nothing surveyed here does — every tool here solves exactly one of the two
  problems.
- livegrep's suffix-array approach is a legitimate structural alternative to
  trigram indexing worth presenting as a real fork in the design space, but it
  does not scale to the stated 200GB target without compressed-suffix-array
  machinery that the reference implementation doesn't have — don't let the
  report present it as a drop-in alternative to trigram indexing without that
  caveat.
- Every actively-maintained tool in this space that supports real regex uses
  **RE2** or an RE2-equivalent linear-time engine (csearch, zoekt, livegrep)
  specifically to avoid backtracking blowup during the final verify pass — a
  Rust build should treat the choice of the `regex` crate (also linear-time,
  RE2-like guarantees) over PCRE-style backtracking engines as validated prior
  art, not just a convenience default.
