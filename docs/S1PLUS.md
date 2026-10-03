# S1+ — Incremental catalog

M0 design, 2026-10-03. Implementation baseline: `4e38e77`, format v3.
This document specifies the next format; it does not describe shipped code.
The build is split into slices below. D51 is open; its recommended choice is
the provisional compaction schedule, not a settled answer from Dave.

A refresh appends one atomic transaction containing changed rows to a packed
snapshot. A resident writer keeps its lookup structures between refreshes.
A query pins the snapshot and a committed log prefix for its whole lifetime.
Compaction rewrites physical rows, not stable ids. The recrawl still costs a
walk; only publication becomes proportional to the change. S1b's small bursts
use the same reconciliation and commit path without walking every root.

D26 B and D27 C remain the right direction. Today's snapshot is about 569 MB
at exactly 10M names, rather than S1's 1.18 GB, but rewriting even that for each
inotify burst is the wrong cost. The log saves roughly a million times the
logical writes for one changed file. The price is id translation, bounded
replay and occasional full compaction. An unchanged full recrawl remains
linear in entries; S1+ cannot turn filesystem enumeration into a small update.

## What the code does today

These are constraints found in the code, rather than inferred from DESIGN:

- `format.rs` writes v3: a 40 B header, 23 section entries of 16 B and 17
  column descriptors of 16 B, then contiguous sections. The head is 680 B.
  The three name columns and most inode fields have 128-row blocks; dev,
  mode and owner use dictionaries. Document ids are a sequence column.
- `read.rs::Catalog::open` reads the head only. `load` reads a section
  positionally into its own buffer, checks dependencies first and validates
  before installing it in a `OnceLock`. It keeps the open file, not its path.
  D38 B is implemented. D30's answer is **C**, with persisted directory names;
  the B constraint in this brief is the lazy section-read contract, not a
  requirement to derive every inverse at open.
- `build.rs::plan` numbers directories breadth first, then file inodes by
  first name, and names in `(physical parent, basename)` order. It checks
  duplicate names, reachability and conflicting hard-link observations.
  `number_files` sorts identities; `assign_docs` sorts hashes and reuses live
  documents. Kept observations yield to fresh ones.
- `transaction.rs::begin` loads the old catalog in full and builds sorted
  identity and hash lookups on **every run**. `keep` copies whole roots into
  batches. `commit` streams a complete file, frees batches, reads it back
  whole for validation, then renames and syncs the directory. None of these
  three paths is suitable for a resident one-file update unchanged.
- `walk.rs` already has `IoOp` and `FaultContext::{Root, Dir, Child}`. The
  original D33/D26 operation-tag addition has landed. `Entered.entries` is
  absent on a partial listing. Entries read before a listing fault can still
  generate events. `index.rs` treats non-root Lstat/NotFound as deletion,
  permits OpenDir/List EACCES and aborts on other coverage faults. The EACCES
  exception currently writes the directory without its old children.
- Ignored edges have one of seven high child tags and no inode. Failed
  re-inclusion traversal collapses to an opaque ignored edge. Specials have
  stat rows and a sparse kind table. Directory entry counts include ignored
  children and are distinct from the indexed child count.

The proposed fault reconciliation replaces the last two coverage rules only
in its own slice. Until that slice lands, keep the current amended A′ rule.
Do not temporarily treat an unclassified fault as a deletion.

## Files and publication

An index directory contains `lock`, `current`, `snapshot.<base>` and
`changes.<base>`. File suffixes are monotonically allocated checkpoint
numbers, never paths supplied by a caller. `current.tmp` and checkpoint temp
files are private to the writer. The advisory single-writer lock remains.

`current` is a fixed 128 B little-endian manifest: magic/version, checkpoint
number, generation sequence, committed log end, checkpoint sequence, sniffer
version, next InoId/NameId/DocId, live inode/name/directory/document counts,
reserved zeros, and a 128-bit checksum over the preceding bytes. The exact
field offsets become a format fixture in M1. The manifest's generation is a
commit sequence, not an id namespace. The first snapshot embeds the same
allocation counters and checkpoint sequence. A log file starts with a 64 B
checksummed header identifying the format and checkpoint.

The new snapshot retains v3's physical packed columns and section dependency
order. Its head adds allocation counters, stable-id maps and document reference
counts, and adds a 128-bit checksum per section plus one over the head/table.
Every section size and descriptor still has an exact checked interpretation.
Maps and reference counts are separate sections, not inode fields loaded by
a name query. This is a new format version, with v3 queries refused as today.
M1 provides an explicit v3-to-new checkpoint import so the initial migration
can preserve existing DocIds and configured roots; otherwise an explicit
fresh index is allowed and is an id-namespace reset. A reset is never silently
substituted for compaction. No content postings exist yet to migrate.

### Transaction envelope

A transaction is a 64 B header, an array of 48 B block descriptors, contiguous
block payloads and a 32 B footer. Everything is little-endian and aligned to
8 B. No native Rust layouts go on disk.

The header contains magic/version, total length, sequence and previous
sequence, resulting allocation counters, block count and
reserved zeros. Each descriptor contains family/flags, record count, relative
offset, byte length, a 128-bit payload checksum and reserved zeros (48 B).
The footer repeats sequence and total length and holds a 128-bit checksum of
the header and descriptor array. A descriptor checksum commits the payload
without making a reader fetch that payload at open. Length arithmetic is
checked; descriptors must tile the payload, families cannot repeat and the
footer must be exactly at the declared end. Unknown versions, flags and
opcodes are errors. A footer alone does not publish anything: `current` does.

Four families keep the reader's dependencies explicit:

| Family | Records | A load that uses it |
| --- | --- | --- |
| Namespace | inode birth/death, names, directories, root membership | Names, directory paths, children and resolution |
| Inodes | full inode observations: stat, kind, content state and doc binding | Any requested inode field; no other base inode column is forced |
| Aux | symlink targets and work-tree records | Links or WorkTrees, projected independently |
| Docs | live document hash and indexed-inode reference count, death | Docs and writer content lookup |

Each record starts with `opcode:u8, flags:u8, reserved:u16, length:u32` (8 B,
length includes zero padding). Fixed-width records have an exact length;
variable records include a checked byte count. Byte strings are raw Linux
bytes, never Unicode-normalised. Names exclude NUL, slash, dot and dot-dot;
names and path strings carry a trailing NUL, counted in the byte length.
Records are complete replacements or tombstones, not arithmetic deltas. This
makes replay repeatable and allows one transaction to coalesce repeated edits
to the same row into its final value.

| Record | Payload | Encoded bytes |
| --- | --- | ---: |
| InodePut | id u32; kind/state u8 each; reserved u16; doc u32 or none; stat fields in `batch.rs::Stat` order, 68 B on wire | 88 |
| LifePut | id u32; kind/flags u8 each; reserved u16; indexed-name count u32; reserved u32 | 24 |
| InodeDelete | id u32; reserved u32 | 16 |
| NamePut | id, parent and tagged child u32; string byte count u32; basename plus NUL | `align8(24 + L + 1)` |
| NameDelete | id u32; reserved u32 | 16 |
| DirPut | id, own NameId or none, raw entry count or unknown, flags u32; retained-at sequence u64 | 32 |
| DocPut | DocId u32; indexed-inode references u32; hash 16 B | 32 |
| DocDelete | DocId u32; reserved u32 | 16 |

RootPut/RootDelete key by root InoId and include the configured absolute path
for a put. LinkPut/LinkDelete key by InoId, with target bytes for a put.
WorkTreePut/WorkTreeDelete key by directory InoId, with kind, common identity
and path for a put. These use the same record header and counted-string rule.
M1 fixes their offsets and lengths alongside the fixed records above.
LifePut carries kind without loading stat columns and counts **indexed names**,
not `st_nlink`; ignored names have no LifePut. Directory flags include
traversed/search-suppressed and complete/retained-fault coverage.

A chmod writes InodePut only. A content change writes InodePut and the affected
DocPut/DocDelete records. A rename writes NamePut under its existing NameId;
it updates old/new parent observations and raw counts when known. Creating
an edge writes NamePut and updates its inode's name count; creating an inode
also writes LifePut and InodePut. Removing the last indexed name deletes its
inode and any aux rows and decrements the document's inode count. Adding a
hard link does not add a document reference. A directory move never rewrites
descendant names or content. Directory deletion does enumerate and tombstone
its catalogued subtree; its cost is the removed subtree, not one record.

### Sync order and recovery

For a normal commit:

1. Validate the change set against the pinned previous generation. Append its
   transaction to the existing log with positional writes; sync the log file.
2. Write the complete new manifest to `current.tmp`, sync it, rename it over
   `current`, then sync the index directory.
3. Return the committed generation and let S1b publish it to new queries.

There are three sync barriers: log, manifest file, directory. They batch the
whole burst, not each record. An empty diff writes **zero** catalog bytes,
advances no sequence and does no catalog fsync. A successful API result means
durable. A manifest rename followed by failed directory sync returns the
equivalent of today's `Undurable`: the generation may already be visible,
so callers must not retry the same intent blindly. Before rename, an error
publishes nothing. A failed log sync or partial write poisons that writer
session until it reopens and recovers; it cannot append after an uncertain tail.

A reader opens `current` once, validates its fixed bytes, opens the named
snapshot and log, checks matching checkpoint headers and pins the exact log
end and sequence. It never follows a later manifest or the growing EOF.
It reads only transaction framing through that end initially. A concurrent
append cannot change any byte in its prefix. Loaded sections and delta runs
are immutable and shared by `Arc`; a query keeps that generation even while
new queries receive the next one. No query is held behind the writer lock.

Under the writer lock, recovery accepts exactly the prefix named by the valid
manifest. Bytes after that end are unpublished, whether they hold half a
record, a complete footer or a fully synced transaction. The writer truncates
that suffix and syncs before appending. It never promotes it by guessing that
a footer means success. A truncation, checksum failure or sequence gap
**inside the published prefix** is corruption: refuse the generation and
report which file/block needs a re-index. Do not silently roll back acknowledged
updates. A bad manifest is also corruption, not a cue to adopt a temp file.
Readers neither recover nor truncate. Power-loss tests must establish that
the old or new valid manifest survives each rename/sync boundary; derived
data with storage damage beyond that guarantee requires re-indexing.

Checkpoint publication writes and syncs a new snapshot and a new empty log,
syncs the directory entries naming both, then publishes/syncs the new manifest.
An unsuccessful checkpoint leaves `current` naming the old pair. Retired pairs
are unlinked only after the new manifest is durable. Already-open descriptors
keep their bytes after unlink. A reader racing file opening with unlink retries
from a freshly opened manifest on ENOENT; if it has opened both files it keeps
them. Header mismatches on an already pinned pair are errors, not retries.
One slow reader can pin one old checkpoint, so disk/RSS reporting includes
retired-but-open generations. Directory creation keeps today's ancestor-sync
discipline. No live or retired snapshot is truncated or rewritten.

## Ids and physical rows

InoId and NameId become stable logical u32 ids. Each allocation uses and
increments a persisted high-water counter. No deleted id is reused, including
after compaction. A failed, unpublished transaction need not reserve ids for
external callers: allocated tokens do not escape before commit. Crossing the
reserved top 16 inode values or the name limit fails before publication; no
wrap or automatic reset. These limits concern historical allocations, not
live entries. DocIds retain their current independent high-water rule.

Physical snapshot row numbers are private `InodeRow`/`NameRow` types. At each
checkpoint they are dense again: directory rows first and breadth first,
file rows by first name, name rows by `(physical parent, basename)`. Snapshot
references use these private physical rows. The decoder can therefore retain
its small, strong proof that a physical parent is below its child. Log records
use stable ids; the reader translates at the boundary. Callers never see
physical rows or assume stable-id order encodes ancestry.

Each table has a forward `row -> stable id` array of u32 and a persisted
inverse `stable id -> row`. The inverse has an 8 B page-offset directory up
to `ceil(high_water / 4096)` and allocated 4096-entry pages of u32 row numbers;
none marks a hole. An all-hole page is absent. Section lengths bound every
offset. At 10M ids the directory is about 20 kB per table, the pages about
40 MB and the forward array about 40 MB: about 160 MB for both tables.
Start with these plain arrays, not a second packing scheme for id maps.
They give constant-time translation and reuse checked positional section
loads; sequential scans carry physical rows and avoid translating each
field separately. A later measured map compression is a separate slice.

Between checkpoints a tombstone is a hole in the effective id-indexed view:
the old base row still exists, but cannot be returned as live. A new id has
only a log row. No stat column is extended or repacked when it is appended.
The base inverse answers only base residency; the overlay decides latest
liveness first. At compaction, dead physical rows, old name bytes, deleted
docs and superseded records disappear, and nonempty inverse pages are rebuilt.
Logical holes and allocation counters remain. “Holes until compaction” in D27
means reclaiming **physical storage**, not recycling ids; changing a live id
would defeat the daemon's stable handle. The page directory alone remains
bounded by the u32 id space (under 8.4 MB per inverse at exhaustion), even if
most historical pages have become empty. Scattered sparse pages can still be
expensive; measure 50% and 90% churn as well as the normal 2% case.

An InoId identifies an indexed inode lifetime, not a content or filesystem
inode number forever. Reconcile continuing names and hard links by `(dev, ino)`
and type. An observed last-name deletion retires the id; later reuse of the
same kernel inode gets a new one. A full recrawl cannot prove whether an unseen
delete/recreate reused the same kernel number between observations. If the
identity still matches, retain the id, but a changed version always revalidates
content. Do not advertise stronger lifetime detection than the filesystem
provides. A directory is keyed by its rooted namespace occurrence, as today's
directory rows are; bind-mounted occurrences are not collapsed into one tree.

A NameId identifies an edge lifetime. Preserve it for an unchanged
`(parent id, raw basename)`, including an inode replacement at that name.
A proved rename transfers it to the new edge. A recrawl can infer a rename
only for an unambiguous old/new singleton within a continuing inode; otherwise
delete and allocate edges, while preserving the inode and content ids.
An overwritten destination retires its old edge id. A delete followed by a
later create at the same spelling is a new edge if the deletion was observed.
Watcher rename cookies are hints checked against final observations, not
authority to resurrect a retired id. Ignored edges keep NameIds too.

### Reader and query invariants

| Today's reliance | Survival in the incremental view |
| --- | --- |
| `read.rs` uses an id as a column offset; `dir_count` partitions kinds | Stable-id lookup yields a live physical or overlay row. Kind comes from physical directory membership or LifePut. Counts report live counts, never loop bounds; expose live iterators separately. |
| `format.rs::check_names` proves heap tiling, monotone physical parents, in-range children and strict sibling ordering | Keep all checks for the base. Namespace blocks check raw strings, tags, assigned/live stable references and unique effective `(parent, basename)` keys. Tombstoned names never survive iteration. |
| `check_dir_names` proves a single incoming edge, lower parent and root termination | Keep the base proof; for each committed namespace transaction validate its final changed edges, exact directory incoming edges and roots. Follow effective parents for each changed directory with a visited set; reject a cycle or missing root. Unchanged base chains were already checked. Changing several edges is validated as a set, not in record order. |
| `children`/`lookup` binary-search a contiguous parent/name range | Base ranges remain sorted. Merge the changed-key range with that directory's base range, suppress overridden/deleted NameIds and keys, and return sorted unique basenames. Moving a directory changes its own edge, not its children's parent ids. |
| `dir_path`, `resolve`, `keep` and fault reporting walk to a root | Use the validated effective graph. No live-parent numeric inequality is assumed. Root membership, own-name links and cycle checking load before paths are infallible. |
| `Kinds` gallops over rising file ids | Keep galloping on physical base rows; overlay kinds are direct LifePut values. A falling logical id must never reach that physical cursor. |
| `ferret-query/src/run.rs` locates heap hits by monotone offsets and uses `next NameId` as the next span | Scan base and effective delta heaps separately; locate by private heap row and return stable NameId through the forward map. Skip superseded base rows, and emit the latest delta once. NUL termination and no-cross-name literal matches remain. |
| `run.rs::all_names` and `inode_scan` enumerate ids; metadata passes make an InoId-sized bitset and decode 64-row runs | Runs and bitsets become private physical-row candidate sets, plus an overlay candidate set. Patch changed fields before testing; clear deleted rows. Emit stable ids. A changed inode is tested once and still fans out to all live names. |
| Search suppresses traversed directories and ignored/special targets, and caches sibling paths | Preserve the result domain and suppression flags. Base scan grouping stays; delta scans can group by effective parent. The sibling cache is an optimisation, never a termination proof. |
| Find's `CatalogSource` gets `entries`, `contents`, counts and `resolve`; DFS frames establish order | These consume the effective view. Parent-before-child, reverse order for depth/delete and prune follow graph traversal, not ids. Sibling order remains free under D50 F10. The per-query delete-count map uses stable parent ids in its pinned generation. |
| Nested roots are sorted by parent id in `find/walk.rs` | Sort their derived attachment list by stable parent and binary-search equality as today; no assumption that the parent was allocated first. |
| `stats.rs` computes depths in one numeric-id pass and loops over dense names/files | Use an explicit root traversal for depth and live row iterators for census/refcounts. Numeric-id topological order is removed. |
| `index.rs::content_faults` and `Transaction::begin/keep` loop over dense ranges | Live iterators and effective graph traversal replace these loops. Kept roots produce no copied batches and no sweep. |

Directory coverage must be distinct from raw counts: a retained-fault directory
has an old stored subtree and an unknown **current** raw count. `contents`
returns unreadable for that directory, so default find walks live at its
boundary and reports the fault, rather than pretending the retained listing
is fresh. Search can continue answering from retained indexed data, with the
generation's coverage diagnostics available to the host. Re-inclusion
ancestors keep search suppression. Explicit ignore changes are authoritative
policy, not faults: collapse newly ignored trees and retire their live rows;
re-including them assigns new ids unless those rows were retained for a fault.

## Combining snapshot and log

`Catalog::load(sections)` keeps its explicit, fallible contract. Open checks
the manifest, snapshot head, log head and the committed transaction framing;
it does not load name, stat, hash or aux payloads. Loading Names reads its base
dependencies, id maps needed for translation, and Namespace blocks. Loading
a stat field reads that base column, its inode maps and Inodes blocks and
projects just that field into the effective view. Loading Docs reads Docs,
its reference counts and Docs blocks. Aux projections never force all inode
columns. Cross-family bindings are checked when the relevant families load;
anything used as an index must be checked before access. Unused payload
corruption does not fail an unrelated query. S1b's resident host calls
`load_all` once, through the same API, not a second reader.

Validate framing once, payload checksums and structural checks on first load.
Build immutable sparse overlays for changed stable ids and changed child keys,
with row references into retained block buffers. Replay final replacements in
sequence order. Coalesce a transaction by id/key before committing. Derived
`hash -> DocId`, full inode/name inverses and content inverses are writer-only
or first-use structures, as D30 requires. They are not reconstructed by every
name query.

For resident generations, share the base buffers and unchanged overlay runs.
Represent overlays as immutable sorted runs at geometric sizes: merging two
equal-size runs keeps the newest value/tombstone per key; a generation pins
its run list. Merge work is amortised over updates, with at most logarithmically
many runs; an old query retains old runs. Load-time replay can sort/coalesce
one whole family instead. Range iteration merges sorted runs, while sparse
lookup searches newest applicable runs before the base. Build the suppression
stream in **physical base-row order** for base scans; do not hash-probe every
one of 10M names. Scan delta name bytes in a contiguous heap of latest live
delta names, built when Names loads or its run set merges. This never recopies
the base heap. A namespace rename into an old directory replaces its lookup
key and heap span, so both stale base hits and stale keys must be suppressed.

This has occasional merge latency, rather than a worst-case constant update
bound. M3 measures one-file updates immediately before and after a run carry,
and all-name scans with 0, 1% and 2% overrides. If sparse overlays cost more
than the budget below, reduce the checkpoint threshold before introducing a
second persistent tree implementation. Neither creating a new generation nor
opening a writer may copy the whole catalog for a one-file change.

## Producing changes

### Full or selected-root recrawl

Keep `index`/`index_change`'s root-set reconciliation under the single writer
lock, including D34's widening for nested-root boundary changes. A long-lived
writer session caches its identity lookup, live hash lookup, reference counts
and current view. The batch CLI creates one session for its run; S1b keeps one
between bursts. `Transaction::begin` must stop rebuilding these per burst.
Build base identity lookup as sorted u32 physical row ordinals, not copied
stats, and hash lookup as sorted u32 doc row ordinals into already-loaded
hashes. Overlay maps hold only changes. No second 24 B copy of every hash.

Walk workers still use directory tokens and stat-bracketed, handle-relative
content observations. Resolve tokens to continuing or allocated stable
directory ids once observations are merged. Do not use `build::plan` to
renumber the entire candidate snapshot and then diff that numbering.
Compare children by parent identity and basename; compare all stat fields,
target bytes, coverage flags, entry counts, content state and doc binding.
Emit nothing for equal rows. Changed metadata retains InoId; a rename retains
it too. Unchanged versions carry content by today's conservative
`(dev, ino, size, mtime, ctime)` test. Conflicting fresh hard-link observations
publish Fault/no-doc for the inode as today; a fresh observation overrides a
carried one only if its scope is trustworthy.

Track seen old names/inodes in bitsets and complete/protected directory scopes.
Sweep only refreshed, completely covered scopes for unseen edges; never sweep
kept roots or protected subtrees. Subtract root removals explicitly. Process
edge deletions, replacement observations and refcount changes as one final
set, so moving a last hard link within a transaction does not transiently kill
and recreate its inode/document. Collapse unsuccessful traversal with the
existing reachability rule. Sort final changed rows deterministically; current
per-worker arrival order is not a persistence contract.

M4 may retain whole-walk batches to bound its first review slice, but not build
a second encoded snapshot. At 10M that is still about 1.2 GB of observation
storage. M6 streams completed directory observations into reconciliation,
retaining only changed rows, seen bits and the existing bounded alias backlog.
That is an explicit RSS milestone, not a claim that the first diff removes
every allocation. A zero-change walk does no publication; telemetry may record
its time outside the catalog. Content-fault reporting uses changed/fault inode
sets and live aliases, avoiding today's full fault-state pass per small burst.

### S1b's interface

Two seams keep filesystem knowledge in crawl and persistence in catalog:

```text
ferret-crawl::RefreshRequest {
    expected_generation,
    scopes: [Entry { parent: InoId, basename }, Directory(InoId), Root(path)],
    rename_hints: [(old_parent, old_name, new_parent, new_name)],
    reason: Burst | Overflow | PolicyChange | Backstop
}
ferret-crawl::refresh(&mut WriterSession, request, options) -> RefreshReport

ferret-catalog::ChangeSet {
    base_generation, final row replacements/tombstones,
    allocation counters, root edits, coverage diagnostics
}
WriterSession::commit(changes) -> Unchanged | Committed { generation, changes }
```

These are proposed interfaces, not Rust declarations. Expected-generation
mismatch returns RetryFromCurrent before doing writes. A refreshed scope must
resolve to the same root and inode identity under the lock; a changed or
vanished parent promotes the request to a containing directory/root refresh.
No external caller can submit an unobserved “delete this inode” from inotify.
The returned committed delta lets the resident engine adopt the next view
without reopening/replaying its entire log. The returned view keeps base
sections lazy; the resident host loads it before switching new queries.

The watcher coalesces names and parents and calls `refresh` for a burst. Crawl
checks final disk state, opens parent descriptors and hashes with today's
bracket. A disappeared entry is removed only after scoped Lstat/NotFound or a
complete parent listing. Unmatched move cookies refresh both involved scopes;
cross-root moves follow configured boundaries. Changed children require parent
raw entry counts to be refreshed by a complete listing, or marked unknown;
notification arithmetic alone is not a complete raw census. Ignore-file and
git-exclude changes expand to the affected subtree, global rules/size-cap or
sniffer changes expand to the required roots, and overflow invalidates hints
and requests a complete backstop recrawl. A sniffer-version change cannot
advance the global header while roots retain old classifications (D37).
Watcher setup, watch limits, burst timers and socket protocol remain S1b work.

A ctime-only change is not proof that bytes stayed the same. A recrawl chmod
therefore may reread/hash the file; equal hashes retain the DocId and produce
no content-index work. Do not infer a metadata-only edit from equal size/mtime:
a writer can restore mtime. Notification hints likewise cannot override the
conservative rule after overflow or uncertain coverage. The cost model
separates this content-read cost from catalog commit cost.

### Typed faults and subtree retention

Keep `IoOp` and `FaultContext`; add a final directory coverage result delivered
after its jobs finish: Complete, Protected { operation, context, error } or
Boundary. `Entered` starts descendants and cannot serve as their completion
marker. The completion result can be aggregated by crawl from existing typed
events; M5 tests whether an explicit walker completion event reduces state.
Fault scopes are transaction-local directory tokens or checked old edge ids,
not path-prefix string guesses.

| Fault | Publication rule |
| --- | --- |
| Child Lstat/NotFound | Confirmed vanished edge; delete it and its old subtree if it was a directory. It is the only operation/error pair exempted as a disappearance. |
| OpenDir/List/Reopen, including ENOENT, identity mismatch and EACCES | Protect that directory's old namespace subtree. Discard all new observations below it, including a partially listed prefix; retain old children and aux state, mark coverage stale and current raw count unknown. |
| Child Lstat other than NotFound, or Readlink | Retain the old edge and subtree when identifiable. If the edge is new or its type is unknown, protect the old parent directory instead. |
| Local ReadIgnore or ProbeGit, including ENOENT for a broken gitdir | Protect the directory whose rules are uncertain and its whole subtree; fresh decisions below it cannot be trusted. Missing optional ignore files that the walker accepts are not faults. |
| Root open/stat/list fault | Protect the whole existing unchanged root. A missing root is not an implicit root removal. No old root to protect, or a root-set edit requiring its new boundary, blocks the transaction. |
| Global ignore/config read fault; unknown operation/context; unresolvable protection scope | Block publication of the entire transaction. |
| Content open/stat/read fault, moving stat bracket or alias conflict | Publish the valid namespace/stat observation as Fault/no-doc; retry on a later refresh. No new DocId is minted for unknown content. |
| Bad pattern (`Event::Pattern`) | Keep today's diagnostic and remaining rules; not an I/O coverage failure. |

Protection requires the old directory occurrence still belongs to the
refreshed root and was not proved replaced. If its identity was replaced, there
is no valid old subtree to attach: protect a proven unchanged ancestor instead,
or abort. New unreadable directories can be recorded as opaque, with no invented
children and unknown counts; uncertain new roots and policy scopes block if
there is no valid anchor. Overlapping protection scopes reduce to the outermost
ones before reconciliation, and may override observations from other workers.
A protected scope wins over any inferred absence or rename inside it. Observed
hard-link changes outside it may update the shared inode; retained names still
refer to that inode, with fresh trustworthy observations winning as in D31.

Retain an existing subtree by **not emitting deletes**, not by copying it.
Only the faulted directory's coverage row/diagnostic changes. This is D26 A
carried by B's log. When a later listing succeeds, reconcile against that
retained subtree and clear its coverage marker in the same transaction.
Protection under a changed global policy/sniffer cannot be represented under
one advanced version; abort that version transition until every root is
reclassified. Report protected scopes and counts to the caller; partial
coverage is visible even when other scopes commit successfully.

## Documents and checksum

A metadata change never assigns a new DocId. After revalidation, equal content
uses the existing live hash row, including content shared by another inode.
Different content uses its already-live DocId or a new high-water id.
Store a u32 indexed-inode reference count beside each live document in a
separate packed-snapshot section (start with plain u32). The writer updates
the affected counts only; dropping to zero writes DocDelete in the same commit.
Counts are per inode, not per name. Tombstones remove a dead hash from content
lookup immediately, retaining only the high-water counter under D36 B. A revert
after that gets a new id unless another live inode still holds the old hash.
S2 may later choose to retain postings/history; S1+ does not choose that for it.
Overflow and inconsistent counts fail validation. Compaction recomputes counts
as a cross-check, not as the normal discovery mechanism for dead documents.

Revisit D39 with **per-section and per-log-block BLAKE3-128 checksums**, plus
the framing/head/manifest checksums above. Use the BLAKE3 implementation already
present in crawl, shared as a workspace dependency in M1; do not own a second
checksum algorithm or choose a whole-file checksum. Content hashes and storage
checksums use distinct domain tags containing format, section/family identity
and length. Checksums are accidental-corruption detection, not authentication.
They detect changed names, stat values and hashes that structural checks allow.

Compute section checksums while streaming output; the current positional
writer has several column writers feeding one section, so checksum the finished
section with bounded reads unless its exact byte stream can be shared cleanly.
That extra writer read is included in compaction estimates. Validate all new
transaction payloads before publication and read back the appended transaction
for a checksum/structure check; the read-back is proportional to its size.
Lazy reader verification hashes only bytes it actually loads. Do not trust a
previous checksum check across a later read of a mutable path. Committed files
and prefixes are immutable; lazy buffers retain their checked bytes.

This adds about 16 B per section and small framing overhead. At 10M the added
hash time is material, but it does not require reading unrelated columns.
Neither a checksum nor safe indexing proves that a filesystem observation was
fresh; typed faults and stat brackets remain separate checks.

## Cost model at 10M

All MB/GB below are decimal; RSS is MiB. **Measured** means an existing run or
a direct size read identified here. Every new-format size/time is **estimated**.
No incremental implementation was timed in M0.

The measured v3 artifact is
`/tmp/find-m4a-measure/sentinel-final/catalog`, written by M4a's encoder and
reported in [ROADMAP S1c](ROADMAP.md#s1c--ferret-find-in-find1-syntax) and
`/home/dave/w/super-ferret/.ai/find-m4a-done.md`. M0 read its 680 B head with
Python `struct.unpack_from('<QQ', head, 40 + 16*i)` and its `stat` length.
The format version is 3; it has 10,448,739 names, 10,405,730 inodes,
1,800,947 directories and 8,495,924 docs. Head SHA-256:
`71f06fcd66244290d90bad63e41002533445064c68aad32cf5200d6c127562d5`.
`git diff a73937e 4e38e77 -- crates/ferret-catalog` shows reader/test changes
and removal of a lint allowance; the encoder/packing format did not change.
The synthetic replicates a real stat dump, gives copies distinct hashes and
has short links, no specials/worktrees, and lower-bound raw directory counts;
it is deliberately heavy in live documents, not a forecast of a typical tree.

| Current v3 component | Measured bytes | Measured B/name |
| --- | ---: | ---: |
| Names | 44,348,882 | 4.244 |
| NameHeap | 252,108,533 | 24.128 |
| DirNames | 5,402,849 | 0.517 |
| Entries | 1,040,735 | 0.100 |
| Traversed | 225,119 | 0.022 |
| Roots + Strings + Links + Specials + WorkTrees | 510,274 | 0.049 |
| Twelve inode sections, Dev through States | 155,265,362 | 14.860 |
| Docs | 135,934,792 | 13.010 |
| Header/table/descriptors | 680 | <0.001 |
| **Total** | **594,837,226** | **56.929** |
| Name-query section set including Links/Specials | 303,636,392 | 29.060 |

Normalising that distribution to exactly 10M names gives an **estimated**
569.29 MB base, 290.60 MB name set, 9.959M inode rows and 8.131M docs.
New id maps add at most about 159.7 MB at dense high water, document counts
32.52 MB, and inode indexed-name counts derived once by the writer take
39.84 MB RAM. Thus the estimated new checkpoint is **761.5 MB**, about
76.2 B/name before page rounding and checksum/head overhead (under 0.1 MB).
A cold name query needs about 160 MB of maps on top of its 291 MB name set
in the conservative full-map loading scheme; a metadata-only column pass can
use physical rows without loading name maps. Resident full catalog is about
727 MiB of encoded buffers, plus overlays/allocator/query scratch. D48's
1 GB resident goal is plausible, not yet measured. Writer lookup ordinals
add about 72.4 MB, name refcounts 39.8 MB and seen sets about 2.5 MB; these
are separate from the query resident figure and must be budgeted if co-hosted.

Time assumptions, all **estimated**: sequential read/write 1 GB/s, checksum
throughput 2 GB/s, overlay validation/replay 0.5–2 microseconds per record,
small commit's three barriers together 1–10 ms. These are modelling inputs,
not measured device/BLAKE3 claims. M1/M2 measure them locally. Use
`bytes/bandwidth + records*replay_cost + barriers`, rather than scaling a
content hash by catalog size. Durability tails can be much slower.

### Publication, walks and churn

| Workload at exactly 10M names | Estimated logical catalog writes | Estimated time and what it includes |
| --- | ---: | --- |
| One inode metadata update, namespace unchanged | 88 B record + 144 B framing + 128 B manifest = **360 B** | **1–10 ms** resident reconcile/commit, plus occasional overlay merge; excludes observation/hash work |
| One unique hashed file becomes new unique content; old doc dies | 88 + 32 + 16 B records + 192 B framing + 128 B manifest = **456 B** | **1–10 ms + read/hash(B)** for a known scoped file; a 4 KiB file is roughly 0.006 ms of model throughput, a 1 MB file roughly 1.5 ms, excluding opens/stats |
| Create a regular indexed file, 20 B basename | LifePut 24 + NamePut 48 + InodePut 88 + DocPut 32; parent DirPut 32 + parent InodePut 88; 240 B three-family framing + 128 B manifest = **680 B** | Small commit plus hashing and complete parent listing; listing D raw children costs O(D), not O(10M) |
| 1% of entries, 100k existing inode metadata updates, one transaction | **8.80 MB + 272 B** | About **0.06–0.22 s + barriers**, using 100k replay records; actual recrawl observation cost is separate |
| 100k unique-content changes, old docs all die, one transaction | **13.60 MB + 320 B** | About **0.17–0.63 s + read/hash(total B) + barriers**, with 300k records |
| 100k names renamed, 20 B basename | **4.80 MB** of NamePut plus affected inode/parent/count records and one manifest | O(100k + affected parents); a directory rename does not write descendants |
| Full recrawl, zero changes | **0 B**, no new generation | O(10M) listings/stats/comparisons; estimated **10–30 s warm** on a comparable tree, unbounded by this model when cold; no content reread for unchanged versions |

The 1% rows count 100k **inode** changes for 10M total entries, not 1% of
8.16M non-directories on this fixture. One percent of its non-directories is
about 82k and scales those payloads by 0.82. Equal new hashes, shared old docs,
binary/unindexed files and hard links reduce document-record work. Parent
mtime/raw counts may change too; add the actual distinct parent records.
Actual NamePut size is `align8(24 + L + 1)`, not always 48 B. At the measured
mean terminated length 24.128 B its mean lies between 48.128 and 55.128 B;
the 20 B examples use the measured S1 median, not that mean.

Logical writes exclude filesystem blocks, allocation/journal traffic and
device amplification. A tiny append, new manifest inode and directory sync
can dirty roughly **12–32 KiB** of filesystem data/metadata (estimate), despite
456 B of catalog bytes. Measure logical lengths and physical write counters
separately. A per-file transaction for 100k changes would add 32 MB of framing
and manifests and **100k barrier sets**: about 100–1,000 s under the model.
Batching is required. S1b chooses its burst interval; catalog has no second
timer or acknowledgement-before-durability mode.

The warm no-change estimate is anchored, not measured at 10M on a real tree:
[ROADMAP S1a](ROADMAP.md#s1a--catalog-compaction) measured `$HOME` 445k reruns
at 0.87 s before the final build fix, and synthetic 10M reruns at 5.25 s begin
plus 6.75 s carry, about 18.82 s total user time and 2,013 MiB peak.
The synthetic does no real `getdents`/`lstat` work. Removing the full build and
cached-session setup should help, but inode lookups and filesystem stat calls
still dominate. A standalone changed-file **recrawl** costs that walk plus
the small commit; it is not the 1–10 ms scoped-update row. M4 measures both.

### Compaction and open

Request a checkpoint at the first of: **64 MB log**, **500k log records**,
**2% distinct dirty base rows** in either names or inodes, or **5% dead base
rows**. These are initial measured-work targets, not format constants.
Repeated updates of one file hit log/record limits even with only one dirty
row. Dirty/dead fractions use checkpoint live counts, not lifetime high water.
At the 2% limit, an inode/doc overlay is roughly tens of MB on disk and
20–80 MB resident (estimate, representation dependent). Geometric runs and
queries retaining old runs can increase that; report it.

Provisionally compact at an idle writer boundary, holding the writer lock;
queries retain old views and keep running. The new snapshot traverses the
effective graph, packs only live physical rows, rebuilds id maps and verifies
reference counts. Stream sections with bounded buffers as today; do not
re-materialise 120 B walk batches for every row. Reuse the effective catalog
as the row source, freeing plan arrays when their sections finish. Reserve
about **0.85 GB additional disk** (checkpoint plus worst allowed suffix/temp
space) beyond the old pair, and about **0.2–0.5 GB transient RAM** for ordering,
maps and checks (estimates). Retired readers add pinned old buffers/files.

Estimated compaction at exactly 10M writes **761.5 MB** plus a log header and
manifest, reads about **761.5 MB** of source and another **761.5 MB** for finished
section checksum/self-check, and takes **10–20 s** with barriers, packing and
map checks. For comparison, measured v3 build at 10.45M was **10.71 s** and
**1,640.7 MiB** peak (ROADMAP S1c/M4a; load 7.86). Earlier v2 commit wall varied
**7.7–14.4 s**, explicitly dominated by sync, in ROADMAP S1a's rerun series.
Those are whole-build baselines, not an incremental compactor benchmark.
Normal resident compaction may read its source from buffers rather than disk;
do not count that as guaranteed cold I/O saved. A full cold load of 761.5 MB
adds about 0.76 s read and 0.38 s checksum under the model, plus validation.

On open let T be transaction count, N record count and L log bytes:

```text
framing-only open: head reads + O(T * (96 + 48 * families)) framing bytes
                  + positional-read overhead; no O(N) payload replay yet
load chosen family: base read/check + L_family / bandwidth
                    + L_family / checksum_bandwidth + N_family * replay_cost
resident load_all: base load/check + L / bandwidth + L / checksum_bandwidth
                   + N * replay_cost
```

Envelope reads skip payloads but require header/footer reads per transaction;
coalesce adjacent requests where possible. Therefore N alone cannot determine
open time: 300k records in one transaction and 100k three-record bursts have
the same payload but very different framing and read-call costs. At one-file
content bursts, N=300k means T=100k, about **19.2 MB framing** and **13.6 MB
payload**; estimated framing-only open **0.1–0.6 s** assuming 1–5 microseconds
per envelope's positional I/O, plus base heads. Loading all delta records adds
about **0.15–0.60 s CPU** and **0.05 s** model byte/check cost. In one transaction,
the same N has just 192 B framing and 13.6 MB payload, about **0.02 s** model
byte/check cost plus replay. These figures do not include base section load.
At N=500k, replay alone is estimated **0.25–1 s**, which motivates the cap.

The measured v2 name-set open was **247 ms warm / 354 ms evicted** and full
load **439 / 784 ms** earlier in S1a; different packing rounds/byte totals are
explicit in [its tables](ROADMAP.md#s1a--catalog-compaction). They are reference
points, not measurements of v3 checksums/maps or M0's new reader. Estimated
new full engine start is **1–2.5 s with a modest log**, dominated by base load,
checks and maps. M2 records header-only, each lazy load and resident start
separately. A daemon pays them at start; a batch host pays them once per batch.

At a record-triggered checkpoint, 500k unique content edits amount to about
167k files and 22.7 MB payload if batched; checkpoint amplification is then
about **34x** catalog bytes, compared with a snapshot per edit at roughly
**1.67 million x** for the 456 B tiny transaction. A 64 MB trigger gives roughly
12x. This is the tradeoff for bounded start time and sparse overlays, not a
claim that compaction makes all refreshes O(change) in the worst case.

M0's headline measure for M6: on one fixed 10M checkpoint, report metadata,
content, create/delete and rename bytes, p50/p95/max commit time, observer time,
checksum time, sync time and RSS. Measure empty/fresh and near-threshold logs,
including a geometric-run carry. Measure resident scoped requests and full
recrawls separately, with first-process writer setup separated too. Repeat the
1% and compaction rows with unique and shared docs, faults, hard links, sparse
historical ids and concurrent pinned readers. Run one benchmark at a time;
record commit, machine/load, storage, fixture count, warm/evicted method and
whether the directory/inode cache was evictable. No new timing in this document
should be promoted from estimate without that source.

## Build slices

Each slice leaves the full fmt/clippy/test gates green and updates this design
and ROADMAP with its own measurements. New filenames below are proposed;
existing paths are relative to the repository root. No watcher is built here.

| Slice | Change and files touched | Tests and measurement gate |
| --- | --- | --- |
| **M1 — Checked checkpoint and stable ids** | `crates/ferret-catalog/src/{lib,format,read,build,transaction}.rs`, new `ids.rs`, `src/tests/{decode,round_trip,carry,roots}.rs`, new id/migration fixtures; workspace/crawl/catalog manifests and lockfile for sharing existing BLAKE3; `crates/ferret/tests/layering.rs`, DESIGN's dependency graph and decisions | v3 import preserves roots/DocIds; stable logical ids, holes, reserved limits, bidirectional map validation, every truncation/value flip, lazy checksum failures; measured bytes per section and maps at 10M, checksum throughput, no-log name/metadata/full open and RSS |
| **M2 — Durable log transactions** | new catalog `src/log.rs`, `src/tests/log.rs`; `transaction.rs`, `read.rs`, `format.rs`, `src/tests/commit.rs`; `crates/ferret-bench/src/main.rs` | every append truncation and sync/rename crash point; published-prefix corruption refused, unpublished tail ignored; lock races, old-reader lazy loads after append/checkpoint unlink; measured tiny/batched writes, three barriers, header-only opens versus T/N |
| **M3 — Effective reader and queries** | new catalog `src/overlay.rs`, log/read/id modules and tests; `crates/ferret-query/src/run.rs`, its tests and `src/find/{walk,test}.rs` as needed; `crates/ferret/src/stats.rs`, census/CLI tests | snapshot-plus-log matches a materialised oracle for create/delete/replace/rename/move, directory cycles rejected, ignored/special/traversed/root cases, hard links and docs, all candidate strategies; find prune/depth/delete semantics; measured 0/1/2% overlays, merge-carry latency, resident queries/RSS; no changes to free sibling-order contract |
| **M4 — Recrawl diff producer** | new `crates/ferret-crawl/src/reconcile.rs`; `index.rs`, `observe.rs`, `src/tests/{index,lifecycle,parallel,race}.rs`; catalog batch/transaction seams; `crates/ferret-catalog/examples/synthetic.rs`, bench driver | unchanged pass writes zero; metadata equal-content DocId stable; ambiguous rename/reused identity, hard links across kept/refreshed roots, policy/sniffer changes and root boundaries; retain amended A′ fault rule; measure no-change, one-file and 1% full-recrawl writes/time/RSS including session setup |
| **M5 — Coverage reconciliation** | crawl `walk.rs`, `index.rs`, `reconcile.rs`, tests in `src/tests/{lifecycle,race,parallel,golden}.rs`; catalog directory flags/read/decode tests | inject each IoOp/error/context, partial listing on several workers, new/replaced directories, overlapping protection, stale counts, recovery, global/sniffer transitions; verify old subtree retained and find's live fallback; measured faulted-subtree writes independent of subtree size |
| **M6 — Resident refresh seam and bounded observations** | crawl `lib.rs`, `index.rs`, `reconcile.rs`, new `src/refresh.rs` and `src/tests/refresh.rs`; catalog WriterSession/change-set API; synthetic example and bench driver | simulated bursts call the real crawl API: final-state deletes, move hints, stale generations, overflow, ignore changes, count refresh and conflicting aliases; no watcher; stream directory reconciliation and cap temporary storage; headline one-file/1% 10M measurements with near-threshold logs, resident setup amortised |
| **M7 — Compaction and budgets** | new catalog `src/compact.rs`, `src/tests/compact.rs`; build/transaction/log/read/id seams; bench/synthetic driver, stats budget reporting | pinned readers and crashes at every checkpoint boundary, stable ids after repeated churn, all-hole pages, refcounts recomputed, retained coverage, deterministic packed rows; measured disk/RSS peak, writes/time at 10M, sparse 50/90% history, trigger/replay/overlay budgets; apply D51's answer |

M1 may expose physical row cursors internally while retaining the old query
behaviour on no-log snapshots. M2 can publish test change sets before crawl
uses them. M3 completes consumer correctness before M4 enables real diffs.
M4 does not ship broad fault recovery before M5's scope tests. M6 makes the
S1b interface useful and cheap; M7 is required before an unbounded daemon
writer is released. Update module headers on both sides of each seam.

## Open question

Only a conflict between fastest updates and simplest long-term maintenance
is raised. Record it as [D51](DECISIONS.md#d51--compaction-while-the-watcher-is-busy-open).

**D51: May a checkpoint pause the writer for seconds?** Idle-boundary,
single-writer compaction is the smaller implementation and reuses the commit
proof, but blocks incoming bursts for an estimated 10–20 s at 10M. Concurrent
checkpoint construction keeps bursts flowing but needs suffix replay, a second
publication proof and a peak-memory budget. The initial recommendation is the
idle-boundary version, measured before S1b adopts it; the deciding fact is the
permitted worst-case freshness lag under sustained churn. The decision brief
has named options and numerical costs. Checksum choice, stable-id translation,
batch durability, typed retention and content identity do not ask Dave to
re-answer their settled direction.
