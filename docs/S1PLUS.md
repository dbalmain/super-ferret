# S1+ — Incremental catalog

M0 design, 2026-10-03. Implementation baseline: `4e38e77`, format v3.
M1–M3 implement the checked version-4 checkpoint, durable log and effective
reader. M4/M4b implement the batch recrawl producer; M5 implements typed coverage
retention. M6–M7 remain a design.
The build is split into slices below. D51 is open; its recommended choice is
the provisional compaction schedule. D52 interprets D27 C as epoch-scoped ids:
proceeding on the recommendation; Dave may veto. Neither is recorded as an
answer from Dave.

A refresh appends one atomic transaction containing changed rows to a packed
snapshot. A resident writer keeps its lookup structures between refreshes.
A query pins the snapshot and a committed log prefix for its whole lifetime.
Compaction renumbers inode/name ids and starts a new checkpoint epoch.
The recrawl still costs a walk; only publication becomes proportional to the change. S1b's small bursts
use the same reconciliation and commit path without walking every root.

D26 B and D27 C remain the right direction. Today's snapshot is about 569 MB
at exactly 10M names, rather than S1's 1.18 GB, but rewriting even that for each
inotify burst is the wrong cost. The log saves roughly a million times the
logical writes for one changed file. The price is bounded
replay and occasional full compaction. An unchanged full recrawl still visits
every entry; M4b sorts directory listings and only the changed/alias residue.
S1+ cannot turn filesystem enumeration into a small update.

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
- `WriterSession::open` validates/warms the old view once, then caches sorted
  inode ids, immutable document-row ordinals and rooted directory identities.
  Sparse maps track lookup changes. Name references reuse the reader's cached
  base inverse and effective LifePut rows. `Transaction::begin` is the checkpoint
  fallback;
  its carry identity lookup is lazy and hash sorting happens at checkpoint commit.
  `keep` copies roots only for initial/checkpoint fallback publication. Ordinary
  recrawls leave kept roots in the view and append a final sparse change set.
- `walk.rs` already has `IoOp` and `FaultContext::{Root, Dir, Child}`. The
  original D33/D26 operation-tag addition has landed. `Entered.entries` is
  absent on a partial listing. Entries read before a listing fault can still
  generate events. `index.rs` treats non-root Lstat/NotFound as deletion
  and resolves other typed faults after all workers finish. Existing unchanged
  old scopes are retained without copying; new unreadable child directories may
  be opaque. Directory OpenDir/List/Reopen EACCES is instead a covered opaque
  observation, including at a new or initial root (D26). Other root faults
  without an old unchanged root, unresolvable contexts and global policy/sniffer
  transitions under protection block publication.
- Ignored edges have one of seven high child tags and no inode. Failed
  re-inclusion traversal collapses to an opaque ignored edge. Specials have
  stat rows and a sparse kind table. Directory entry counts include ignored
  children and are distinct from the indexed child count.

M4/M4b implement the recrawl producer and resident `WriterSession`; M5 resolves
coverage faults into checked directory/edge scopes and publishes retention with
trustworthy updates in one log transaction. Initial indexing uses a checkpoint;
without an old root to protect, a root fault other than directory EACCES blocks
it. Directory EACCES publishes an opaque row with no visible children, carrying
the D26 amendment unchanged under the log. Never treat an unclassified fault as
deletion.

## Files and publication

An index directory contains `lock`, `current`, `snapshot.<base>` and
`changes.<base>`. File suffixes are monotonically allocated checkpoint
numbers, never paths supplied by a caller. `current.tmp` and checkpoint temp
files are private to the writer. The advisory single-writer lock remains.

`current` is a fixed 128 B little-endian manifest: magic/version, checkpoint
number, generation sequence, committed log end, checkpoint sequence, sniffer
version, next InoId/NameId/DocId, live inode/name/directory/document counts,
reserved zeros, and a 128-bit checksum over the preceding bytes. The exact
field offsets become a format fixture in M1. A generation is the tuple
`(incarnation, checkpoint, sequence)`, not sequence alone. The checkpoint
number is the inode/name id epoch; even a compaction with no logical change
invalidates old-epoch handles. A 128-bit catalog incarnation occupies 16 of
the reserved bytes and also appears in the snapshot and log headers; an
explicit fresh index creates a new incarnation. Handles include their epoch,
never just a bare u32. The snapshot embeds its initial allocation counters
and checkpoint sequence. A log file starts with a 64 B
checksummed header identifying the format and checkpoint.

The new snapshot retains v3's packed columns and section dependency order.
Its head adds allocation counters and a document reference-count section,
and adds a 128-bit checksum per section plus one over the head/table.
Every section size and descriptor still has an exact checked interpretation.
Document reference counts are separate from inode fields and are not loaded
by a name query. There are no persisted inode/name id maps. A nullable blocked
RetainedAt column per directory stores its last trustworthy subtree sequence; none means no retained-fault subtree.
Together with Entries and Traversed it preserves DirPut coverage flags across
checkpoints. Its all-none cost on this 10M distribution is about 0.22 MB;
a heavily faulted tree can cost up to about 8 B/directory. A writer-only Policy
section stores a BLAKE3-128 fingerprint of global rule bytes and eligibility
configuration, so a session can detect a global policy transition after restart.
Namespace PolicyPut replaces that fingerprint after a completely covered
transition, even if no entry changed. Sniffer version remains explicit. This is a new format version, with v3 queries refused as today.
M1 provides an explicit v3-to-new checkpoint import so the initial migration
can preserve existing DocIds and configured roots; otherwise an explicit
fresh index is allowed and is an id-namespace reset. A reset is never silently
substituted for compaction. No content postings exist yet to migrate.
Imported dense v3 InoIds/NameIds become the initial epoch ids, with next ids
equal to their counts. A v3 reader already open keeps its original file.
The import keeps that file available until the new manifest is durable.

### M1 checkpoint wire layout (implemented)

M1 uses version 4. Its 96 B header keeps v3's fields at 0..40, adds
incarnation at 40..56, checkpoint at 56, checkpoint sequence at 64,
next InoId at 72 and next NameId at 76, and reserves 80..96 as zeros.
Twenty-six 32 B section entries hold offset, length and BLAKE3-128 checksum.
Eighteen 16 B column descriptors follow, including nullable-blocked RetainedAt.
A 16 B checksum over all preceding head bytes ends the 1,232 B head.
DocRefs stores one u32 per live Docs row, counting indexed inode bindings;
multiple hard links to one inode count once. Policy holds 16 B. M4 fingerprints
its domain/version tag, size cap, global-rule presence and global-rule text with
BLAKE3-128. A change refreshes every configured root before PolicyPut. Local
rule bytes are read by the actual refreshed walk. Zero remains unknown for
legacy imports and callers that supply no fingerprint.

`current`'s fixed layout is:

| Offset | Field | Bytes |
| ---: | --- | ---: |
| 0 | `FERRETCR`, version u32, reserved zeros | 16 |
| 16 | incarnation | 16 |
| 32 | checkpoint, sequence, committed log end, checkpoint sequence | 32 |
| 64 | sniffer version | 4 |
| 68 | next InoId, NameId, DocId | 12 |
| 80 | live inodes, names, directories, documents | 16 |
| 96 | reserved zeros | 16 |
| 112 | BLAKE3-128 over bytes 0..112 | 16 |

M1 has no log: committed log end is zero, sequence equals checkpoint sequence,
and inode/name counters equal dense base counts. Epoch-local holes arrive with
M2/M3; DocId holes already persist. The existing full-rebuild writer advances
both checkpoint and sequence on each explicit checkpoint publication. M4's
ordinary recrawl increments only sequence, and an empty diff changes neither. It streams and syncs a private snapshot, validates it, renames to
`snapshot.<n>`, syncs that directory entry, then writes/syncs/renames `current`
and syncs the directory. Retired snapshots are unlinked after that last sync;
pinned descriptors retain their bytes. Abandoned suffixes are skipped rather
than reused. The writer computes checksums with a bounded reread after
streaming columns, since independent positional column writes are interleaved.

`ferret import-v3` is the explicit import path. It copies packed columns without
renumbering base ids or DocIds, fills all-none RetainedAt and computed DocRefs,
and validates through the v4 decoder. A v3 file has no integrity digests, so
import can detect structural damage but cannot detect arbitrary changed values
that v3 never checksummed. The old file remains until the manifest is durable.
Ordinary queries continue to refuse v3; explicit indexing with roots can reset
that namespace as before. Raw InoId/NameId accessors are internal to a pinned
query view; exported `Handle` requests check their complete generation before
interpreting a numeric id.

### Transaction envelope

A transaction is a 64 B header, an array of 48 B block descriptors, contiguous
block payloads and a 32 B footer. Everything is little-endian and aligned to
8 B. No native Rust layouts go on disk.

The header contains magic/version, total length, sequence and previous
sequence, resulting allocation counters, block count and
reserved zeros. Its exact budget is 8 B magic, 4 B version, 4 B block count,
three u64s (length/sequence/previous), three u32 counters, 4 B flags and
8 B reserved. Live counts are in the manifest and are cross-checked when the
corresponding families load. Each descriptor contains family/flags, record
count, relative
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

A completely covered sniffer transition updates the manifest's sniffer version
in the same transaction, including a PolicyPut when all entry rows are equal.
The snapshot retains the sniffer under which it was built; after a nonempty log,
the effective reader uses the published manifest version. Requiring those two
versions to remain equal would force an unnecessary checkpoint on every sniffer
transition.

RootPut/RootDelete key by root InoId and include the configured absolute path
for a put. LinkPut/LinkDelete key by InoId, with target bytes for a put.
WorkTreePut/WorkTreeDelete key by directory InoId, with kind, common identity
and path for a put. These use the same record header and counted-string rule.
M2 uses opcodes 1..15 in this order: LifePut, InodeDelete, NamePut,
NameDelete, DirPut, RootPut, RootDelete, PolicyPut, InodePut, LinkPut,
LinkDelete, WorkTreePut, WorkTreeDelete, DocPut, DocDelete. Family tags are
u16 values 0..3 in the table order; descriptor flags are u16 at byte 2,
record count is u32 at byte 4, offset/length are u64 at 8/16, digest is
24..40 and 40..48 is reserved. RootPut and LinkPut have id at byte 8,
string count at 12, bytes at 16 (`align8(16 + L + 1)`). WorkTreePut has
id at 8, kind at 12, zeros at 13..16, common dev/ino at 16/24, string
count at 32 and bytes at 36 (`align8(36 + L + 1)`). Their deletes are
16 B id/reserved records. PolicyPut is 24 B, its hash at 8..24.
DirPut flags use bits 0..3 for traversed, search suppressed, complete,
retained fault; retained-at is u64::MAX for none. LifePut flags are currently
zero; a root directory may have zero indexed names, while other inode births
require at least one. Kind uses the `Kind` discriminants 0..6. All other flags are zero.
The log header is magic `FERRETCL` at 0, version at 8, zeros at 12..16,
incarnation at 16, checkpoint at 32, checkpoint sequence at 40 and checksum
of 0..48 at 48. Transaction magic is `FERRETTX`.

Accepted M1 manifests with log end zero identify a missing, empty log;
only checkpoint sequence equal to published sequence is legal in that case.
M2 checkpoints create the header and publish end 64. A log writer upgrades
an M1 empty prefix under the writer lock before appending. `Published::open`
pins the checked pair and provides explicit base checkpoint and lazy log
family access. M3 supplies the effective view through ordinary `Catalog::open`; M2 initially
refused a nonempty log with `OverlayRequired`, avoiding stale query answers.
Live-count cross-checking needs overlay liveness and is therefore M3 work;
M2 checks counter limits/monotonicity and the final envelope counters against
current, plus each loaded record's wire invariants and references' bounds.
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

Compaction without a diff keeps logical sequence S, renumbers inode/name ids
and increments the checkpoint epoch; the new log begins after S. A checkpoint
that incorporates a diff advances the sequence too. DocIds and their allocation
counter do not change merely because the epoch changes. Checkpoint publication
writes and syncs a new snapshot and a new empty log,
syncs the directory entries naming both, then publishes/syncs the new manifest.
An unsuccessful checkpoint leaves `current` naming the old pair. Retired pairs
are unlinked only after the new manifest is durable. Already-open descriptors
keep their bytes after unlink. A reader racing file opening with unlink retries
from a freshly opened manifest on ENOENT; if it has opened both files it keeps
them. Header mismatches on an already pinned pair are errors, not retries.
One slow reader can pin one old checkpoint, so disk/RSS reporting includes
retired-but-open generations. Directory creation keeps today's ancestor-sync
discipline. Recovery removes abandoned temp/orphan checkpoint files under the
writer lock after establishing the manifest's pair; it never adopts them.
If obsolete files exist, recovery first syncs the directory containing the
selected manifest: a prior `Undurable` writer may have stopped without an OS
restart, so a readable rename alone is not permission to retire its old pair.
No live or retired snapshot is truncated or rewritten.

## Epoch-scoped ids

D27 C is interpreted as stable, never reused **within a checkpoint epoch**;
compaction removes the holes by renumbering. [D52](DECISIONS.md#d52--d27-c-ids-across-compaction)
records both interpretations and their costs. The earlier M0 interpretation
required lifetime-stable logical ids and private physical rows. It had no
identified consumer and added about 159.7 MB at 10M, so this design replaces it.
Epoch ids are both faster and simpler for the planned workload.

The consumer check covers current code and the content/daemon plans:

| Consumer | What it keeps and how it survives compaction |
| --- | --- |
| Current search/find, path and metadata candidates | A pinned catalog generation. InoIds, NameIds, sibling caches and find's delete-count map belong to that view; D32 keeps its files and buffers alive. |
| S1b watch table and queued scopes | Watch locations use paths or `(root occurrence, dev, ino)` with checked directory handles/identity. Cached catalog ids carry a generation. A stale request retries/re-resolves before dereferencing an id. A bare `(dev, ino)` alone is not enough for bind-mounted directory occurrences or kernel reuse. |
| S2 content segments, postings, liveness and later filters | [DESIGN's crate graph and CandidateSource/DocCursor](DESIGN.md#crates) and [content structures](DESIGN.md#content-documents-tokens-structures-d6-d8-d9), plus ROADMAP S2/S3, use DocIds. `ferret-index/src/lib.rs` explicitly knows no files, paths or inodes. DocIds remain stable across epochs. |
| DocId → inodes and inode → names | Catalog-owned, first-use inverses in DESIGN's catalog section; rebuild within the new view. They are not references stored in a content segment. |
| Future name/metadata indexes and asynchronous work | D4 places filename terms and metadata in the catalog; no separate persistent inode-keyed consumer is specified in S1b/S2. Catalog-owned candidates can be rebuilt/remapped with a checkpoint; work can pin its source generation or retain a locator and retry. An actual external consumer unable to do either would change D52. |

A base InoId **is** its checkpoint row number. Directory rows are dense and
breadth first before files; D29's parent < child proof holds for base directory
edges. Base NameId is its row number in `(parent id, basename)` order, matching
heap order. There is no `InodeRow`/`NameRow` translation layer, forward map,
paged inverse or sparse page encoding.

Log-created ids start at the respective checkpoint row count and increase
monotonically. Persist next InoId and NameId in each transaction/manifest;
never reuse a deleted id in that epoch. A tombstone leaves a hole in the
effective id-indexed columns: the immutable base row still exists but is dead,
or a log-only row is removed. New rows live only in the overlay; appending does
not extend or repack a base stat column. Base counts, epoch high water and
live counts are distinct. Allocation tokens do not escape before commit, so
an unpublished transaction need not reserve its ids.

New directories can have ids above base files; moves can put a lower-id child
under a higher-id parent. Directory classification uses the base directory
prefix only for unmodified base rows, and LifePut for new rows. Effective
ancestry is a validated graph, never an id inequality. Neither `0..live_dirs`
nor `0..live_inodes` enumerates the effective view; expose live iterators.

At compaction, traverse the effective graph and assign fresh dense InoIds,
directories in BFS order then files by first name, and parent/name-sorted NameIds. Rewrite every inode/name reference:
name parents/children, directory own-name links, roots, links, specials and
worktrees; rebuild catalog inverses and writer lookups for the new epoch.
Remove dead rows, obsolete strings and superseded log records. Reset next
InoId/NameId to the new checkpoint's live row counts. Compaction itself never changes
DocIds, hashes, content bindings or next DocId; an incorporated diff can change
them under the usual content/liveness rules.
RetainedAt sequences and coverage flags also survive.

The compactor uses transient old-id → new-id u32 arrays to rewrite references,
about `4 * (9.959M + 10M)` = **79.84 MB** at this fixture's dense high water,
plus bounded log allocations (estimate). This is planning scratch only, freed
before self-check; it is neither stored nor loaded by normal readers. Dead
old slots map to none. Thresholds bound epoch growth, so 50%/90% cumulative
churn across repeated checkpoints does not grow these arrays or the new file
with historical inode/name allocations. Concurrent compaction would retain
these maps until its suffix has been rebased; D51 accounts for that difference.

Every outward inode/name id handle names its `(incarnation, checkpoint)`; requests carry
the full expected generation. An epoch mismatch returns RetryFromCurrent
before interpreting numeric ids, including when the sequence is unchanged.
Re-resolve scopes from a pinned old view's path/identity or a retained watch
locator, not by applying an old number to the new file. New queries receive
the new view and empty epoch-specific candidate/path caches; old queries keep
the old view. Watch descriptors need not be recreated just because catalog
ids changed. The commit result distinguishes a checkpoint view from a
same-epoch delta so a host cannot apply renumbered ids as ordinary replacements.

Before an epoch counter exceeds the inode limit `u32::MAX - 16`, or reaches
NameId's `u32::MAX` sentinel, checkpoint. This retains the existing v3 reader's
conservative bound: since the counter is a count rather than the last id,
**17** top inode values are unavailable, including none and ignored type tags.
The earlier “top 16” wording confused those two quantities. If the live
checkpoint plus proposed births cannot fit,
fail before publication; no wrap. Historical inode/name allocation exhaustion
is no longer a format concern. Live-count limits still exist, and DocId and
sequence/checkpoint counters retain their independent exhaustion checks.
An explicit incarnation reset is distinct from routine compaction.

Within an epoch an InoId identifies an indexed inode lifetime, not a content
or filesystem inode number forever. Reconcile continuing names and hard links
by `(dev, ino)` and type. An observed last-name deletion retires the id; later
reuse of the same kernel inode gets a new one. A full recrawl cannot prove
whether an unseen delete/recreate reused the kernel number between observations.
If identity still matches, retain the epoch id, but a changed version always
revalidates content. A directory is keyed by its rooted namespace occurrence,
as today; bind-mounted occurrences are not collapsed into one tree.

Within an epoch a NameId identifies an edge lifetime. Preserve it for an
unchanged `(parent id, raw basename, child lifetime)`. An inode replacement at
that name retires the edge and allocates a new NameId, matching M4's explicit
delete/recreate contract. A proved rename transfers it to the new edge. A recrawl infers a rename
only for an unambiguous old/new singleton within a continuing inode; otherwise
delete and allocate edges while preserving the inode and content ids. An
overwritten destination retires its old edge id. An observed delete followed
by a create at the same spelling is a new edge. Watcher cookies are hints
checked against final observations, not authority to resurrect an id. Ignored
edges keep epoch NameIds too.

### Reader and query invariants

| Today's reliance | Survival in the incremental view |
| --- | --- |
| `read.rs` uses an id as a column offset; `dir_count` partitions kinds | A live base id indexes its column directly; overlay replacements/deaths take precedence and new ids resolve in the overlay. Base directory membership or LifePut supplies kind. Base counts, high water and live counts are separate; effective enumeration uses live iterators. |
| `format.rs::check_names` proves heap tiling, monotone physical parents, in-range children and strict sibling ordering | Keep all checks for the base. Namespace blocks check raw strings, tags, assigned/live epoch references and unique effective `(parent, basename)` keys. Tombstoned names never survive iteration. |
| `check_dir_names` proves a single incoming edge, lower parent and root termination | Keep the base proof; for each committed namespace transaction validate its final changed edges, exact directory incoming edges and roots. Follow effective parents for each changed directory with a visited set; reject a cycle or missing root. Unchanged base chains were already checked. Changing several edges is validated as a set, not in record order. |
| `children`/`lookup` binary-search a contiguous parent/name range | Base ranges remain sorted; a new directory has no base range. Merge the changed-key range with that directory's base range, suppress overridden/deleted NameIds and keys, and return sorted unique basenames. Moving a directory changes its own edge, not its children's parent ids. |
| `dir_path`, `resolve`, `keep` and fault reporting walk to a root | Use the validated effective graph. No live-parent numeric inequality is assumed. Root membership, own-name links and cycle checking load before paths are infallible. |
| `Kinds` gallops over rising file ids | Keep galloping over base ids in a base scan; overlay kinds are direct LifePut values, including new directories above the file prefix. Mixed/falling id lookups use the checked random-access path, never that rising cursor. |
| `ferret-query/src/run.rs` locates heap hits by monotone offsets and uses `next NameId` as the next span | Scan base and effective delta heaps separately. A base heap row is its NameId and next base row still bounds its span; a delta span carries an explicit epoch NameId. Skip superseded base rows and emit the latest delta once. NUL termination and no-cross-name matches remain. |
| `run.rs::all_names` and `inode_scan` enumerate ids; metadata passes make an InoId-sized bitset and decode 64-row runs | Keep base-id bitsets and base column runs, with a separate overlay-new candidate set. Patch changed fields before testing and clear deleted ids. High water/live counts are not base scan bounds. Emit epoch ids directly; each changed inode is tested once and fans out to all live names. |
| Search suppresses traversed directories and ignored/special targets, and caches sibling paths | Preserve the result domain and suppression flags. Base scan grouping stays; delta scans can group by effective parent. The sibling cache is an optimisation, never a termination proof. |
| Find's `CatalogSource` gets `entries`, `contents`, counts and `resolve`; DFS frames establish order | These consume the effective view. Parent-before-child, reverse order for depth/delete and prune follow graph traversal, not ids. Sibling order remains free under D50 F10. The per-query delete-count map uses epoch parent ids in its pinned generation. |
| Nested roots are sorted by parent id in `find/walk.rs` | Sort their derived attachment list by epoch parent and binary-search equality as today; no assumption that the parent was allocated first. |
| `stats.rs` computes depths in one numeric-id pass and loops over dense names/files | Use an explicit root traversal for depth and live row iterators for census/refcounts. Numeric-id topological order is removed. |
| `ferret-crawl/src/index.rs::content_faults` and `Transaction::begin/keep` loop over dense ranges | Live iterators and effective graph traversal replace these loops. The full-checkpoint `Transaction::keep` still copies a kept root into its output batch; M4's diff producer leaves untouched roots in the effective view without copying or sweeping them. |
| Cached ids and writer request scopes | Pin the source generation or carry its epoch. Compaction changes the generation even at the same sequence; mismatches retry before dereference. DocId candidates alone remain valid across epochs, subject to current liveness. |

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
dependencies and Namespace blocks. Loading a stat field reads that base
column, the Life projection of Namespace and Inodes blocks, and projects just that field into the effective
view. The Life projection checks birth/death/id/count records independently;
it does not require base Names or NameHeap. The Namespace payload checksum
is verified in full once, but graph/child-key checks wait for the Names load.
This keeps a metadata pass that rejects everything from loading names merely
to discover inode tombstones. Loading Docs reads Docs,
its reference counts and Docs blocks. Aux projections never force all inode
columns. Cross-family bindings are checked when the relevant families load;
anything used as an index must be checked before access. Unused payload
corruption does not fail an unrelated query. S1b's resident host calls
`load_all` once, through the same API, not a second reader.

Validate framing once, payload checksums and structural checks on first load.
Build immutable sparse overlays for changed epoch ids and changed child keys,
with row references into retained block buffers. Replay final replacements in
sequence order. Coalesce a transaction by id/key before committing. Derived
`hash -> DocId`, full inode/name inverses and content inverses are writer-only
or first-use structures, as D30 requires. They are not reconstructed by every
name query.
Derive old-key tombstones from the previous effective edge before coalescing
a NamePut that changes parent/basename. Keep those tombstones in the child-key
overlay until a checkpoint; an id-only replacement index is insufficient to
hide its old lookup key. Validate sibling uniqueness and incoming-directory
edges on the final transaction state. InoId type/liveness and DocId bindings
are cross-checked before `load_all` succeeds and before any writer publishes.

For resident generations, share the base buffers and unchanged overlay runs.
Represent overlays as immutable sorted runs at geometric sizes: merging two
equal-size runs keeps the newest value/tombstone per key; a generation pins
its run list. Merge work is amortised over updates, with at most logarithmically
many runs; an old query retains old runs. Load-time replay can sort/coalesce
one whole family instead. Range iteration merges sorted runs, while sparse
lookup searches newest applicable runs before the base. Build the suppression
stream in **base-id order** for base scans; do not hash-probe every
one of 10M names. Scan delta name bytes in a contiguous heap of latest live
delta names. In M3, Names load or a namespace change rematerialises this sparse
latest-name heap and its suppression stream; metadata-only generations share
both. Record runs carry geometrically, but this derived namespace work is
O(dirty names): one rename measured about 15/32 ms at 1/2% mixed overlays
(ROADMAP S1+). It never recopies the base heap. M4 batches namespace changes;
M6 accounts for their burst cost. A heap per immutable run is a possible later
optimisation requiring multiple-heap span/ownership and query merging. A namespace rename into an old directory replaces its lookup
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
The session owns the writer lock; S1b routes root/index edits through that
session, rather than opening a competing CLI writer alongside the daemon.
Build base identity lookup as sorted u32 base InoIds, not copied
stats, and hash lookup as sorted u32 doc row ordinals into already-loaded
hashes. Overlay maps hold only changes. No second 24 B copy of every hash.

Walk workers still use directory tokens and stat-bracketed, handle-relative
content observations. Resolve tokens to continuing or allocated epoch
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
and recreate its inode/document. When deleting the last refreshed alias leaves
names only in kept roots, M4 promotes those alias roots to refreshed roots and
walks them before publication: otherwise no observation updates the shared
inode's nlink/ctime/content. M6 can narrow that promotion to checked alias scopes;
untouched roots remain unswept. Collapse unsuccessful traversal with the
existing reachability rule. Sort final changed rows deterministically; current
per-worker arrival order is not a persistence contract.

M4b reduces each worker's local file listing by basename against the sorted
old children before retaining it. Fully equal single-name files retain a compact
old-name reference, checked against a pinned generation and a same-path parent
hint. Equality includes all stat fields, content state/hash and symlink target;
policy/sniffer changes cannot bypass those checks. Files with multiple filesystem
links or multiple indexed names keep full observations. Changed/unmatched rows
and alias candidates alone enter the identity-sorted residue. An unmatched alias
can name a compacted inode even with st_nlink == 1 (bind mounts); expand that
inode's compact observations into the residue so conflict handling and canonical
ordering still see the full group. Final edge and document lifetimes are unchanged.

This moves directory-local observation reduction ahead of M6 to meet M4b's CPU
and RSS budget. It does not yet stream the directory graph or complete coverage
scopes. The observation batches still retain directories and compact name
references for the run. Seen bits are sized by epoch high-water ids, not live
counts. Directory tokens carry checked parent hints and use dense per-batch
lookup arrays, rather than a tree lookup per observed file. The checkpoint's
children are already sorted; only effective overlays need a sparse merge.
Checkpoint fallback expands compact references into ordinary observations.
A zero-change walk does no publication; telemetry may record its time outside
the catalog. ROADMAP S1+ reports setup, replay, diff and publication separately.

M6 streams completed directory observations into reconciliation, retaining only
changed file rows, seen bits, the directory token/coverage table and the existing
bounded alias backlog. The directory table still costs O(directories); resolve
continuing directory ids as tokens are minted and retain provisional mappings
for newly discovered/ambiguous moves. File observations need not survive once
compared to their parent's old children. Content-fault reporting uses
changed/fault inode sets and live aliases, avoiding a full fault-state pass per
small burst. Scoped alias lookup and bounded directory buffering remain M6 work.

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
                              | Checkpointed { generation, view }
```

These are proposed interfaces, not Rust declarations. Expected-generation
mismatch returns RetryFromCurrent before dereferencing any request id or
doing writes; expected_generation includes incarnation, checkpoint and sequence.
A refreshed scope must resolve to the same root and inode identity under the lock; a changed or
vanished parent promotes the request to a containing directory/root refresh.
No external caller can submit an unobserved “delete this inode” from inotify.
Retain a path or checked watch locator, or pin the source view, to re-resolve
old scopes after an epoch change. A same-epoch committed delta lets the resident
engine adopt the next view without reopening/replaying its entire log. A
Checkpointed result replaces the view and invalidates epoch-specific caches.
The returned view keeps base sections lazy; the resident host loads it before switching new queries.

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
not path-prefix string guesses. M5 aggregates typed events after the joined walk;
`Entered.entries` plus the complete set of typed faults supplies the final
coverage result. No walker completion event is needed while the crawl joins
all workers before reconciliation; M6 must preserve that completion proof when
it releases directory observations incrementally.

| Fault | Publication rule |
| --- | --- |
| Child Lstat/NotFound | Confirmed vanished edge; delete it and its old subtree if it was a directory. It is the only operation/error pair exempted as a disappearance. |
| Directory OpenDir/List/Reopen EACCES | Covered opaque directory, including a new or initial root: publish its row with unknown entry count, discard any listed prefix and retire old children/subtree in the same final set. No retained-at marker; repeated identical denials publish no generation. |
| OpenDir/List/Reopen other than EACCES, including ENOENT and identity mismatch | Protect that directory's old namespace subtree. Discard all new observations below it, including a partially listed prefix; retain old children and aux state, mark coverage stale and current raw count unknown. |
| Child Lstat other than NotFound, or Readlink | Retain the old edge and subtree when identifiable. If the edge is new or its type is unknown, protect the old parent directory instead. |
| Local ReadIgnore or ProbeGit, including EACCES and ENOENT for a broken gitdir | Protect the directory whose rules are uncertain and its whole subtree; fresh decisions below it cannot be trusted. Missing optional ignore files that the walker accepts are not faults. |
| Root open/stat/list fault other than directory OpenDir/List/Reopen EACCES | Protect the whole existing unchanged root. A missing root is not an implicit root removal. No old root to protect, or a root-set edit requiring its new boundary, blocks the transaction. |
| Global ignore/config read fault; unknown operation/context; unresolvable protection scope | Block publication of the entire transaction. |
| Content open/stat/read fault, moving stat bracket or alias conflict | Publish the valid namespace/stat observation as Fault/no-doc; retry on a later refresh. No new DocId is minted for unknown content. |
| Bad pattern (`Event::Pattern`) | Keep today's diagnostic and remaining rules; not an I/O coverage failure. |

Directory EACCES is permanent user-chosen state, not a protected scope. This
carries D26's amendment unchanged: a chmod-hidden directory must not keep its
old searchable children. Faults strictly below its discarded prefix cannot
retain those children; rule uncertainty at the denied directory itself or an
ancestor still follows the protection/blocking rules above. An ignore-file
EACCES never becomes a covered directory denial. Initial checkpoint publication
likewise discards partially listed prefixes across workers. Policy remains in
force: when denial prevents re-inclusion, D29 collapses a Traverse-only chain
to its opaque ignored edge, as the full checkpoint builder does.

Directory stat and raw count describe the observations, not an atomic
filesystem snapshot. A child that vanishes after listing can leave those fields
from before its disappearance. Stable injected-fault runs compare against the
real full builder with the same fault; a real mutation race checks durable edge
retirement first and the full-index oracle after a stable retry.

Protection requires the old directory occurrence still belongs to the
refreshed root and was not proved replaced. If its identity was replaced, there
is no valid old subtree to attach: protect a proven unchanged ancestor instead,
or abort. A relocated old directory whose old incoming path is no longer
anchored protects its checked owner root, so retaining its old edge cannot leave
a dead parent after the sweep. New unreadable directories can be recorded as opaque, with no invented
children and unknown counts; uncertain new roots and policy scopes block if
there is no valid anchor. Overlapping protection scopes reduce to the outermost
ones before reconciliation, and may override observations from other workers.
A protected scope wins over any inferred absence or rename inside it. Observed
hard-link changes outside it may update the shared inode; retained names still
refer to that inode, with fresh trustworthy observations winning as in D31.

Retain an existing subtree by **not emitting deletes**, not by copying it.
Only the faulted directory's coverage row/diagnostic changes. For a retained
non-directory edge, the parent carries that diagnostic/unknown count while
trustworthy siblings may still update; files have no directory coverage column. The retained-at
sequence identifies its last trustworthy subtree, not the latest failed
attempt; repeated identical faults do not force a new generation. This is
D26 A carried by B's log. When a later listing succeeds, reconcile against that
retained subtree and clear its coverage marker in the same transaction.
Protection under a changed global policy/sniffer cannot be represented under
one advanced version; abort that version transition until every root is
reclassified. Report typed faults and the number of outermost protected scopes to the caller;
the CLI warns about retained data and stale counts even when other scopes commit
successfully. Root-set edits block only when their changed boundary lies within
an anchored protected directory (or an opaque new directory), rather than
blocking unrelated root removals.

## Documents and checksum

A metadata change never assigns a new DocId while its content binding remains
live. Loss of eligibility or a content fault clears the binding without
minting an id; reappearance after that document becomes dead follows D36 B,
not a promise to remember retired content forever. After revalidation, equal content
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
Document counts add 32.52 MB and RetainedAt about 0.22 MB with no faults;
inode indexed-name counts derived once by the writer take 39.84 MB RAM.
Thus the estimated new checkpoint is **602.0 MB**, about **60.2 B/name** before
checksum/head overhead (under 0.1 MB). The removed forward/inverse maps were
`8 * (9.959M + 10M)` ≈ **159.7 MB**, about **21%** of the former 761.7 MB
estimate. No inode/name translation or map validation remains in a scan/load.
A cold name query loads about **290.6 MB**, rather than roughly 450 MB.

This assumes the current dense DocId sequence. Retiring documents can add up
to 4 B per live doc to that sequence column, at most another 32.5 MB at this
fixture's document fraction; budget roughly **0.64 GB** for a steady churn
checkpoint. Heavy retained faults can add more. Inode/name historical churn
no longer grows checkpoints: compaction resets their counters and repacks
only live rows. Live distribution, strings, aux data and stable DocId holes
still affect size. Resident full catalog is about **574 MiB** of encoded
buffers, plus overlays/allocator/query scratch, saving about **152 MiB**.
D48's 1 GB resident goal is plausible, not yet measured. Writer lookup ordinals
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
**1% distinct new or overwritten rows** in either names or inodes, or
**5% dead base rows**. These are initial measured-work targets, not format constants.
Repeated updates of one file hit log/record limits even with only one dirty
row. New/overwritten and dead fractions use checkpoint live counts, not
lifetime high water; a deleted row is in the dead fraction, not both.
The initial 2% dirty-row target was reduced to 1% after M3: mixed 100k/200k
name and inode-field replacements on the 10M fixture increased broad-query
resident RSS from 603.48 to 699.64/796.30 MiB, and full-open peak from 634.57
to 729.37/823.96 MiB (ROADMAP S1+). This leaves more room for pinned generations
and transient carries. M7 implements and tunes the trigger; it is not a format
limit, and the reader still accepts larger overlays. The earlier 20–80 MB
inode/doc-only estimate did not include this mixed namespace representation.
Geometric runs and queries retaining old runs can increase that; measure them.

Preflight the final transaction against these bounds and epoch id limits.
If a burst crosses a bound, or the incoming diff itself exceeds it, build the
next checkpoint directly from the old view plus that validated diff and
publish atomically. Do not compact first and append an old-epoch change set
unchanged, or write a huge log only to rewrite it. Checkpointed returns the
new epoch view; the host cannot treat it as a same-epoch delta. An unrelated
checkpoint racing a queued request causes RetryFromCurrent. This adds a
full-write cost for large diffs and threshold boundaries; report it separately
from small-update latency. Limits bound the published log, not input size.

Provisionally compact at an idle writer boundary, holding the writer lock;
queries retain old views and keep running. Traverse the effective graph,
renumber live rows densely, rewrite all epoch references and verify document
reference counts. Stream sections with bounded buffers as today; do not
re-materialise 120 B walk batches for every row. Reuse the effective catalog
as the row source, freeing plan arrays when their sections finish. Reserve
about **0.7 GB additional disk** (checkpoint plus bounded temp/log space,
including DocId holes) beyond the old pair, and about **0.2–0.7 GB transient
RAM** for ordering, transient remaps and validation (estimates). The dense
old-to-new remaps cost about **80 MB** at 10M plus bounded epoch births.
Free planning arrays before read-back; retaining the whole new checkpoint
for validation alone adds about 0.6–0.64 GB. Resident engine plus writer and
compaction scratch can exceed 1 GB transiently; D48's steady query-resident
goal is reported apart from that peak. Retired readers pin old buffers/files.

Estimated compaction at exactly 10M writes **602.0 MB** plus a log header and
manifest, reads about **602.0 MB** of source and another **602.0 MB** for finished
section checksum/self-check, and takes **9–20 s** with barriers, graph planning,
packing and remapping. This is a conservative planning range, not a measured
speedup. Removing 159.7 MB from each of those three passes saves about
`3 * 0.1597 / 1 + 0.1597 / 2` = **0.56 s** under the byte/checksum model,
before savings from removing persisted-map checks; renumbering still needs
transient reference remaps as the former physical-row planner did. This does
not establish a subsecond writer pause. For comparison, measured v3 build at
10.45M was **10.71 s** and **1,640.7 MiB** peak (ROADMAP S1c/M4a; load 7.86).
Earlier v2 commit wall varied **7.7–14.4 s**, explicitly dominated by sync,
in ROADMAP S1a's rerun series. Those are whole-build baselines, not an
incremental compactor benchmark. Resident compaction may read its source from
buffers; do not count that as guaranteed cold I/O saved. A full cold load of
602.0 MB adds about **0.60 s read + 0.30 s checksum**, plus validation: about
0.24 s less model byte work than the former 761.7 MB checkpoint.

Epoch renumbering leaves D51's recommendation at idle-boundary compaction.
Concurrent construction would need transient old-epoch → new-epoch maps through
cutover, assign new-epoch ids for suffix births, and rebase every inode/name
reference before rewriting suffix records, counters, framing and checksums.
DocIds need no translation. Prepare the new writer lookup/candidate structures
while building the checkpoint, then patch them with the rebased suffix; a full
10M-row lookup rebuild must not be hidden in the claimed short cutover. Queued
old-epoch scopes retry and current queries adopt the new view. Raw suffix copy
is invalid. D51 gives the bounded-suffix numbers; M7 measures remapping as well
as packing. The smaller file helps both schedules, but concurrent checkpointing
now has an additional id-rebasing protocol to maintain.

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
points, not measurements of v3 or M0's checksummed epoch reader. Estimated
new full engine start is **0.8–2.3 s with a modest log**, dominated by base
load, checks and replay. The range subtracts roughly 0.24 s of model byte work
from the earlier planning estimate; it is not an observed start time. M2 records
header-only, each lazy load and resident start separately. A daemon pays them at start; a batch host pays them once per batch.

At a record-triggered checkpoint, 500k unique content edits amount to about
167k files and 22.7 MB payload if batched; checkpoint amplification is then
about **27x** catalog bytes, compared with a snapshot per edit at roughly
**1.32 million x** for the 456 B tiny transaction. A 64 MB trigger gives roughly
9.4x. This is the tradeoff for bounded start time and sparse overlays, not a
claim that compaction makes all refreshes O(change) in the worst case.

M0's headline measure for M6: on one fixed 10M checkpoint, report metadata,
content, create/delete and rename bytes, p50/p95/max commit time, observer time,
checksum time, sync time and RSS. Measure empty/fresh and near-threshold logs,
including a geometric-run carry. Measure resident scoped requests and full
recrawls separately, with first-process writer setup separated too. Repeat the
1% and compaction rows with unique and shared docs, faults, hard links, repeated
epoch churn with DocId holes and concurrent pinned readers. Run one benchmark
at a time;
record commit, machine/load, storage, fixture count, warm/evicted method and
whether the directory/inode cache was evictable. No new timing in this document
should be promoted from estimate without that source.

## Build slices

Each slice leaves the full fmt/clippy/test gates green and updates this design
and ROADMAP with its own measurements. New filenames below are proposed;
existing paths are relative to the repository root. No watcher is built here.

| Slice | Change and files touched | Tests and measurement gate |
| --- | --- | --- |
| **M1 — Checked checkpoint and epoch ids** | `crates/ferret-catalog/src/{lib,batch,format,read,build,transaction}.rs`, new `generation.rs`, `src/tests/{decode,round_trip,carry,roots}.rs`, new epoch/migration fixtures; catalog manifest and lockfile for reusing crawl's existing BLAKE3 version; `crates/ferret/tests/layering.rs`, DESIGN's dependency graph and decisions | v3 import preserves roots/DocIds; dense base ids, tagged generation/epoch mismatch, epoch-local holes and reserved limits, every truncation/value flip, lazy checksum failures; measured bytes per section at 10M and the 602 MB checkpoint estimate, checksum throughput, no-log name/metadata/full open and RSS |
| **M2 — Durable log transactions** | new catalog `src/log.rs`, `src/tests/log.rs`; `transaction.rs`, `read.rs`, `format.rs`, `src/tests/commit.rs`; `crates/ferret-bench/src/main.rs` | every append truncation and sync/rename crash point; published-prefix corruption refused, unpublished tail ignored; lock races, old-reader lazy loads after append/checkpoint unlink; measured tiny/batched writes, three barriers, header-only opens versus T/N |
| **M3 — Effective reader and queries** | new catalog `src/overlay.rs`, log/read/generation modules and tests; `crates/ferret-query/src/run.rs`, its tests and `src/find/{walk,test}.rs` as needed; `crates/ferret/src/stats.rs`, census/CLI tests | snapshot-plus-log matches a materialised oracle for create/delete/replace/rename/move, directory cycles rejected, ignored/special/traversed/root cases, hard links and docs, all candidate strategies; find prune/depth/delete semantics; measured 0/1/2% overlays, merge-carry latency, resident queries/RSS; no changes to free sibling-order contract |
| **M4 — Recrawl diff producer** | new crawl `src/reconcile.rs`, `src/tests/reconcile.rs` and `examples/recrawl.rs`; `index.rs`, CLI index reporting; new catalog `src/session.rs` plus batch/transaction/read/log/generation seams; shared M3 materialised-checkpoint oracle and bench driver | unchanged pass writes zero; metadata equal-content DocId stable; ambiguous rename/reused identity, hard links across kept/refreshed roots, policy/sniffer changes and root boundaries; retain amended A′ fault rule; measure no-change, one-file and 1% full-recrawl writes/time/RSS including session setup |
| **M5 — Coverage reconciliation** | new crawl `src/coverage.rs` and `src/tests/coverage.rs`; `index.rs`, `reconcile.rs`, walker/content I/O test seams; shared materialised-checkpoint oracle; CLI partial reporting and retained find tests; existing catalog coverage flags/RetainedAt format | inject each IoOp/error/context, partial listing on several workers, new/replaced directories, overlapping protection, stale counts, recovery, global/sniffer transitions; verify old subtree retained and find's live fallback; measured faulted-subtree writes independent of subtree size |
| **M6 — Resident refresh seam and bounded observations** | crawl `lib.rs`, `index.rs`, `reconcile.rs`, new `src/refresh.rs` and `src/tests/refresh.rs`; catalog WriterSession/change-set API; synthetic example and bench driver | simulated bursts call the real crawl API: final-state deletes, move hints, stale sequences and epochs (including unchanged-sequence compaction), overflow, ignore changes, count refresh and conflicting aliases; no watcher; stream directory reconciliation and cap temporary storage; headline one-file/1% 10M measurements with near-threshold logs, resident setup amortised |
| **M7 — Compaction and budgets** | new catalog `src/compact.rs`, `src/tests/compact.rs`; build/transaction/log/read/generation seams; bench/synthetic driver, stats budget reporting | pinned readers and crashes at every checkpoint boundary, dense BFS ids after each compaction, every reference remapped, old-reader ids still valid and stale requests rejected before dereference, counters reset without resetting DocId, refcounts recomputed, retained coverage, deterministic packed rows; measured disk/RSS peak including transient remaps, writes/time at 10M, repeated 50/90% cumulative churn without historical checkpoint growth, trigger/replay/overlay budgets; apply D51's answer |

M1 may retain base column cursors internally while retaining the old query
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
proof, but blocks incoming bursts for an estimated 9–20 s at 10M. Concurrent
checkpoint construction keeps bursts flowing but needs suffix id rebasing/replay,
a second publication proof and a peak-memory budget. The initial recommendation is the
idle-boundary version, measured before S1b adopts it; the deciding fact is the
permitted worst-case freshness lag under sustained churn. The decision brief
has named options and numerical costs. [D52](DECISIONS.md#d52--d27-c-ids-across-compaction)
records the epoch interpretation with its competing option and numbers:
proceeding on the recommendation; Dave may veto. No consumer requires the
lifetime-map option, so this is not a fastest-versus-simplest disagreement.
Checksum choice, batch durability, typed retention and content identity do
not ask Dave to re-answer their settled direction.
