# Design

How Super Ferret is put together. Decisions referenced as D*n* are in
[DECISIONS.md](DECISIONS.md); the goals are in [GOALS.md](GOALS.md) and the
order of work in [ROADMAP.md](ROADMAP.md). This document describes the design as
intended; each section says which slice first builds it, and sections are
revised as the slices land.

## Shape in one paragraph

A **catalog** records every file and directory under the configured roots as a
tree of names over inodes, and every distinct file content as a **document**
with a dense ordinal id (D4, D5). An **index** over documents holds structures
that each turn a query atom into a set of _candidate_ documents — postings,
filters, trigram filters, possibly positions (D6). A **verifier** scans the
candidates' bytes so every answer is exact. A **planner** composes candidate
sources without knowing their formats, and results come out per path (D15).
Renames, moves and duplicates change the catalog only; the index changes only
when content changes.

## Crates

One cargo workspace (D1). Each line lists a crate's dependencies; there are no
cycles. This block is enforced: `crates/ferret/tests/layering.rs` fails when a
crate's `Cargo.toml` disagrees with it.

```text
ferret         → ferret-query, ferret-crawl, ferret-catalog, ferret-index, ferret-verify, ferret-policy
ferret-query   → ferret-index (the CandidateSource trait only), ferret-catalog, ferret-verify, ferret-text, rustix
ferret-crawl   → ferret-policy, ferret-catalog, rustix, blake3
ferret-index   → ferret-text
ferret-catalog → blake3 (checkpoint integrity), intpack (local path for M3; permanent form open — D59)
ferret-verify  → regex
ferret-policy  → (std only)
ferret-text    → (std only)
ferret-bench   → anything; nothing depends on it
ferret-daemon  → later
```

| Crate            | Owns                                                                                                                           | Knows nothing about            |
| ---------------- | ------------------------------------------------------------------------------------------------------------------------------ | ------------------------------ |
| `ferret-policy`  | `DirRules::decide(path, entry) -> Decision`, `sniff`; `.ferretignore` / `.gitignore` / global (D13); the defaults setup writes | the catalog, the index         |
| `ferret-crawl`   | walking roots, `statx`, change detection against the catalog, hashing                                                          | query, index formats           |
| `ferret-catalog` | names, inodes, documents, storage, name dictionary and catalog row postings (D54, S1b)                                        | content tokens and document postings |
| `ferret-text`    | the tokenizer and identifier splitting (D9); versioned                                                                         | files, ids                     |
| `ferret-index`   | segments over doc ids; each structure implements `CandidateSource`                                                             | files, paths, inodes           |
| `ferret-verify`  | re-reading a file and matching a query atom against its bytes                                                                  | how candidates were found      |
| `ferret-query`   | query syntax, planning, execution, result rows                                                                                 | any structure's on-disk format |
| `ferret`         | CLI, config, XDG, JSON output, query log; shared engine coordination and batch/daemon hosts (S1b)                              | —                              |

Two boundaries carry the design, and both are where D1 said the thought goes:

- **`ferret-index` knows doc ids and byte strings, not files.** It is the part
  reusable for a VictoriaLogs-style embeddable store (D6). Anything with a path
  in it lives in the catalog.
- **The planner sees `CandidateSource`, never a format.** Adding a structure (a
  trigram filter, positions) is a new implementor plus a registration line; the
  planner does not change. That is the orthogonality test for this crate: a new
  structure must not require reading `ferret-query`.

```rust
/// One query atom's candidate documents, from one structure.
pub trait CandidateSource {
    /// Which atoms this source can answer, and at what cost.
    fn estimate(&self, atom: &Atom) -> Option<Estimate>;
    /// Doc ids that may match, ascending. `Estimate::exact` says whether a
    /// verifier must still check them.
    fn candidates(&self, atom: &Atom) -> Box<dyn DocCursor + '_>;
}

pub struct Estimate {
    pub docs: u64,     // upper bound on candidates
    pub cost: Cost,    // bytes to read, roughly
    pub exact: bool,   // true: every candidate matches
}

/// intpack's cursor shape: next_geq drives leapfrog intersection.
pub trait DocCursor {
    fn next_geq(&mut self, target: DocId) -> Option<DocId>;
}
```

The interface is the first thing written in the index slice and the thing most
worth reviewing. The style guide prefers a plain `enum` + `match` over trait
objects until a second caller needs them, so the likely shape is an enum of the
known structures with one arm each — "adding a structure" is then a variant plus
its arms, still without reading the planner. Settled there, against intpack's
cursor, along with the cost units.

## The catalog (D4, D5)

Six tables. `names` and `inodes` have dense `u32` ids renumbered by each
snapshot (D27); `docs` holds live documents only, keyed by a `DocId` that is
never reused (D36); `roots`, `links` and `worktrees` hang off an existing
`InoId`. Raw inode numbers are data, never keys.

| Table       | Id       | Row                                                                                                                                                                     |
| ----------- | -------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `names`     | `NameId` | parent directory `InoId`, name bytes (in the name heap), child `InoId` or an ignored-type tag (no inode row)                                                            |
| `inodes`    | `InoId`  | `(dev, ino)`, size, mtime, ctime, mode, uid, gid, nlink, `DocId` or none; a 2-bit content state beside it (D37); a directory's raw entry count (D47)                    |
| `docs`      | `DocId`  | content hash (BLAKE3, 128 bits kept); rows sorted by id, with holes where content died, the ids a sequence column (implicit when there are no holes)                    |
| `roots`     | —        | configured root paths and the `InoId` of each; nested roots are separate trees, and adding or removing a root inside a kept root requires refreshing the kept one (D34) |
| `links`     | —        | a symlink's `InoId`, its target as `readlink` returned it (in the strings heap)                                                                                         |
| `worktrees` | —        | a work tree's top directory `InoId`, kind (main / linked / submodule), repository id (the common directory's `(dev, ino)`) and its path for display                     |

Directories are numbered first, breadth-first from the roots in path order, so
their `InoId`s are `0..dirs` and a parent's id is always below its child's; the
snapshot persists each directory's own `NameId` (none for a root) and a bitset
of traversed directories, retained to exclude structural ancestors from
`ferret search` (D29). For find these ancestors are ordinary visible directories
when traversal found a re-included descendant. Traversal that finds none is
collapsed to one ignored directory name, with no inode or children. A path is
the walk from a name through its parent's name to a root. Nothing else is derived at open (D30): `hash → DocId` is built by the
writer only, and `DocId → [InoId]`, the full `InoId → [NameId]` and a
directory's work tree are built when a query first needs them.

Default `ferret find` traverses those catalog edges and reads stored stat
columns with snapshot freshness. It guarantees parent/child ordering, reverses
it for depth/delete, and honours prune; sibling GNU order is not promised.
Explicit ignored starts and opaque subtrees walk live. `-I` is unrestricted
live traversal. [FIND.md](FIND.md) describes field fallbacks and effectful
observations.

A symlink is catalogued as itself — an `inodes` row of kind symlink, named like
any file — and never followed. Its target text is stored now so that D18's
reverse map (target path → links) and content matches through links can be
derived at load later, without a re-crawl.

A work tree is recorded at its top directory so results can show a match once
across a repository's linked work trees (D23): by default a hit shows once with
a count of identical copies in other work trees, a copy that differs shows
separately, and hiding linked work trees entirely is a query flag. A submodule
is its own repository, not a duplicate.

What each change costs:

| Event                        | Catalog                                                    | Index                         |
| ---------------------------- | ---------------------------------------------------------- | ----------------------------- |
| rename / move (file or dir)  | one `names` row                                            | nothing                       |
| new file, known content      | `names` + `inodes` rows, inode → existing doc              | nothing                       |
| new file, new content        | `names` + `inodes` + `docs` row (next ordinal)             | the doc is added              |
| edit in place                | inode → new or existing doc; old doc dead if no inode left | new doc added; old tombstoned |
| delete                       | rows removed; doc dead if no inode left                    | tombstoned                    |
| metadata only (chmod, touch) | `inodes` row                                               | nothing                       |

Liveness is catalog state: a doc is live while some inode points at it. The
index reads a live-docs bitset from the catalog rather than keeping its own
tombstones, so there is one source of truth.

**Storage.** S1+ now publishes a checked v4 snapshot plus a bounded transaction
log under a manifest, with epoch-scoped inode/name ids and stable DocIds.
[S1PLUS.md](S1PLUS.md) defines the implemented publication, recovery and
effective-reader contract. The column-layout description below records the
S1a/find baseline; its per-run snapshot replacement is superseded by S1+.
The snapshot contains a versioned header (magic, format version, sniffer version, next
`DocId`, the directory, inode, name and document counts), a table of 23 sections by
offset and length, a descriptor per packed column, and the sections themselves.
Every id and inode field is a bit-packed column (S1a): `count` values of
`width` bits, least significant bit first, ending in 8 bytes of padding
(written as zeros, not checked; reads never depend on it) so a read is one
unaligned 8-byte load, a shift and a mask, plus a ninth byte at widths 58 to 63
when the value straddles. A descriptor holds the column's
base, width and dictionary length. Every coding is a frame of reference
(value minus the minimum; times order-mapped from `i64` first). A directory's
name is one frame for the column that reserves all ones for none; `dev`,
`mode` and the `(uid, gid)` pair are indexes into a sorted dictionary at the
head of their section. The three name columns and every other inode field
(ino, size, the four time columns, `nlink`) are blocked: a frame of reference
per 128 rows, through a table of 16 B entries (base, offset and width) ahead
of the values, so a read stays O(1) and one outlier widens only its block. An
inode's `DocId` and a directory's entry count are nullable blocked: each block
is framed by its real values alone, and one that holds a none flags it beside
its width and reserves that width's all ones; a block of nothing but nones is
width 0. Neighbouring rows are close: offsets only grow, parents only rise, a
directory's children are numbered together, files numbered by name sit beside
siblings that share sizes, times and nearby inode numbers, and directories and
unhashed files leave document ids out in runs. At 10M names blocking took
offsets from 35.9 to 16.3 MB, parents from 26.9 to 6.1 MB (smaller than a
per-directory child-range table, 7.2 MB, and still O(1) from a name to its
parent), children from 30.7 to 19.6 MB, size, mtime and ctime from 115.2 to
47.3 MB together, `nlink` from 12.8 to 1.8 MB, ino from 36.6 to 17.9 MiB, the
nanoseconds from 73.2 to 60.4 MiB, `DocId` from 28.1 to 8.3 MiB and entry
counts from 2.6 to 1.0 MiB; 64 and 256 rows measured within 1 MB either way on
offsets. Directory names stay one frame: blocked, they were 3 MiB smaller and a
scan of every name, which reads one per directory it enters, took 1.5-2%
longer. A scan reads
the name columns a block at a time, each name's end carried from the next
one's start, so a pass costs no table read per row.
Document ids are a sequence column, row `i` holding `id - i`: with no holes it
is width 0 and a row is found from its id by subtraction, and holes widen it
only to the bits of their total. Each inode field is its own section,
so a query loads only the fields it tests; the three name columns share one
section, since every name read needs all three. The name heap holds
NUL-terminated names in `(parent, name)` order (D28 A) and is contiguous on
purpose: it is what filename search scans (D14). The strings heap holds root
paths, link targets and work-tree paths; roots, links, work trees and document
hashes stay fixed-width rows. A reader opens the file by reading its head alone (680
B), which fixes every section's and column's exact length, and then reads each
section positionally when a query first needs it (D38 B), together with the
sections it is checked against (names need the heap; directory names need names;
roots need directory names and strings; links and work trees need strings).
Each section is validated as it loads: every offset, index, dictionary index
and ordering a reader will follow is checked, so a corrupt or truncated file is
an error from the load that reads the bad section, never a panic or a loop, and
a query that does not read a section is not failed by it. A file in another
format version is not read at all: a query says to re-index, and `ferret index
DIR...` replaces it as though there were no previous generation. The writer
holds an advisory lock on `lock` for the whole run, writes `catalog.tmp`, fsyncs
it, renames it over `catalog` and fsyncs the directory; a reader holding the old
generation keeps it (D32). Measured on `$HOME` before S1a (D28, D30): 51.2 MB
for 435k names, of which inode rows (64 B) were 27.8 MB, name rows (12 B) 5.2
MB, the name heap 10.6 MB and doc rows (20 B) 7.1 MB. A sanity check after S1a
on an 88k-name source tree: 51.0 B per name against 97.8 B, the inode fields
23 B against 64 B and the name columns 6.4 B against 12 B; the sub-second times
(30 bits each) are the largest inode fields.
The memory budget is a config value, defaulted from measurement (D5).

The writer is built for 10M entries (D40). Workers fill columnar batches, about
120 B per entry. The build first decides every id with a few `u32` index arrays
per entry, deduplicating inodes and documents by sorting rather than through
maps, so every build error comes before a byte is written. Once the document
ids are decided it keeps each file inode's content state and frees the batches'
hashes. It then streams the columns to their places in `catalog.tmp`, each
through a bounded 64 KiB buffer that a larger write bypasses, freeing batch
names once the name sections are out, and reads the file back for the decode
check only after the batches are gone. The old generation is released before
the build. Synthetic 10M (the `$HOME` dump under 23 prefixes; 1.18 GB file at
S1): first build peak 1.66 GB, commit 5.5 s; a re-run peaks at 2.56 GB during
the walk, since the old generation (read into memory) and the new batches are
both live. 40M: 6.4 GB and 10.1 GB, commit 23 s. After S1a (580 MB file) the
first build peaks at 1,604 MiB in the commit and 1,401 MiB while filling, and a
re-run that carries every file at 2,014 MiB. The writer releases its lock with `LOCK_UN` on drop,
because a child forked by any thread shares the lock's file description until it
execs.

**Change detection.** A re-crawl compares `(size, mtime, ctime)` with the
catalog row; unchanged means no read and no hash. A reused `(dev, ino)` after a
delete carries a new ctime, so it is re-read and re-hashed like any change.

**Ignored names and find sources (4a).** Format v3 reserves child values
`u32::MAX - 1 ..= u32::MAX - 7` for ignored directory, file, symlink, FIFO,
socket, block and character types, respectively. `u32::MAX - 8` is reserved
for future tombstones and is rejected today; inode ids stop below the top 16
values. An ignored name has no inode, stat, content state or document. An
ignored directory is one opaque marker, with no names underneath. Workers keep
ignored names in a separate compact batch vector, without stat data. The writer
checks directory reachability before pruning unsuccessful re-inclusion traversal,
then propagates visible descendants upward and numbers the retained directories.
The surviving ancestors have stat rows and ordinary find visibility; the old
traversed bit still suppresses them in search to preserve existing result counts.

FIFOs, sockets and devices have ordinary names and stat rows, with Unindexed
content and no document. A sparse `Specials` section holds `(InoId, kind)` pairs
for those visible types, in inode order. `Links` loads this small table too, so
kind lookup and name search need no mode-column load. Search continues to return
only regular files, directories and symlinks; special entries are available to
find through the catalog API. Search tests special kinds after its existing
name, metadata and path filters, so rejected path matches need no kind lookup.
EACCES while opening or
listing a directory retains its ordinary directory inode, no children and an
unknown raw entry count. Other coverage faults still prevent publication.

The read API for find is `Catalog::entries(dir)` (name id, raw basename, kind and
`Target::Inode(id)` or `Target::Ignored(kind)`), `contents(target)` (catalogued,
ignored opaque, or unreadable opaque), `has_children(dir)` (raw count nonzero,
unknown if unreadable), and `resolve(absolute_bytes)`. Resolution returns a root
or name target plus any unresolved suffix below an opaque marker, for a live
source to finish; it never follows symlinks or resolves `..`. Children need Names
and Links; contents/emptiness need Entries; resolution needs Roots and Entries.
The legacy name accessors expose the raw tagged child; callers must check
`Name::target()` before using it as an inode id. Search filters ignored tags
before stat reads in every candidate strategy, and content-fault reporting and
root carry-forward do the same. The stats census counts ignored names by type
without reading a stat row and reports visible special inodes separately. Format v2 is refused with the version error and
re-indexed by the writer.

Measured at 10M (D40/D43): v2 592.6 MB, v3 594.8 MB for 43,010 additional
ignored names, with identical inode/document counts and stat-column sizes;
peak build RSS 1,630 → 1,641 MiB. The high-sentinel encoding is retained after
measuring an adjacent tag range that saved 1.18 MB (0.20% of the snapshot).
Name-search row counts stay identical; the final warm full listing costs 1.2%
more in the resumed baseline/final series.
[ROADMAP § S1c](ROADMAP.md#s1c--ferret-find-in-find1-syntax) summarises the
section bytes, build time and query timings; D47's 4a brief has the encoding
comparison.

## Policy and crawl (D10, D13)

`ferret-policy` is pure: the crawler carries a `DirRules` per directory (`root`,
then `enter` with that directory's ignore-file contents, or `traverse`), asks it
to `decide` each entry using a borrowed root-relative path, and `sniff`s file
heads; it is tested against a golden corpus of trees and expected decisions.
Precedence, most specific first: a `.ferretignore` in the directory or an
ancestor within the configured root; `.gitignore` and `.git/info/exclude` inside
a work tree that starts at or below the configured root (a `.git` file
contributes exclude from its gitdir, or from that gitdir's `commondir` when it
has one; a symlinked `.git` contributes none, and a symlinked `.gitignore` is
disregarded, as git does); the user's global ignore file
(`$XDG_CONFIG_HOME/ferret/ignore`), which setup seeds once with the defaults
(`node_modules/`, `target/`, `.venv/`, …) and which is the user's to edit from
then on. A size cap and a binary check sit beside the patterns. `!pat` in a
`.ferretignore` overrides an ancestor `.ferretignore` or any `.gitignore`, and
can re-include below an excluded directory; the walker traverses an excluded
directory (its ignore files unread) only when an anchored
`.ferretignore` `!` pattern could match inside it — never for an unanchored one
such as `!*.pdf`. A `.ferretignore` inside an excluded directory is never read;
overriding an exclusion takes a `!` pattern at that directory's level or above
(D13). D16 replaces the first implementation's `ignore` crate edge with an
in-crate matcher. The crawler owns the candidate path so policy decisions do not
allocate a joined path per entry. Re-inclusion pruning discards negations that a
later exclusion provably supersedes; uncertain overlaps still permit traversal.

Each directory's rules are one list (D19). Concatenating the files lowest
precedence first — global, `info/exclude`, `.gitignore` root to here,
`.ferretignore` root to here — and taking the last matching line gives the
precedence above. Every rule in the list matches an entry's name alone. A
pattern with no slash before its last character applies unchanged in every
directory. An anchored one (`/build/`, `docs/**/*.tmp`) is followed by cursors,
positions in the pattern stepped one component per directory entered, which put
its last component into the list only where it can match. A run of `**/`
compiles to one globstar, and a step marks positions in a bitset, so a pattern
of k components costs O(k) per directory however its globstars fall. Only ignore
files at or below the configured root contribute rules (D22). The same cursors
answer whether a `.ferretignore` `!` pattern reaches below an excluded
directory. No whole path is ever matched.

A list is identified by its rules' text and flags in order, band included: a
`.ferretignore` line differs from the same `.gitignore` line. It is compiled
once into a bucketed last-match index, shared through an `Arc` by every
directory with that list, whichever files it came from. A directory holds its
path, that handle and its cursors; a child whose cursors and files do not change
shares its parent's. The rule and list tables live per root, behind one mutex
each. A directory reaches the list table only when its list differs from its
parent's, and a miss compiles outside the lock. On `~/w` with 16 workers, 88
lists serve 6,071 directories, and 24 of 1,117 lock acquisitions wait, for under
30 µs in total. Neither table evicts. The rule table is bounded by the distinct
lines of the ignore files read under the root, and the list table by the
directories entered; in practice far fewer (611 lists for 76,771 directories
under `~`). Total retained positions are the sum of the lengths of the distinct
lists, which is quadratic in depth for a chain in which every level adds rules.
Both are dropped with the root's last `DirRules`.

The walk goes through directory handles (D21): the root is opened by path, and
everything below it with `openat(O_NOFOLLOW)` plus a `(dev, ino)` check against
the stat that `decide` saw. `walk_parallel` lists directories on N worker
threads. Each worker keeps its own stack of unfinished parent listings and hands
the oldest one over only while another worker is idle. Each worker's visitor is
built by a factory and returned at the end, so a consumer accumulates per thread
with no lock. At most 128 waiting listings keep a descriptor. The rest reopen
from the root one checked step at a time, which bounds the walker at 128 + 4N
descriptors. `walk` is the same code with one worker. By default N is the
available parallelism capped at 16 (D24).

What the walker hands the catalog, besides decisions and stats: each directory
it enters carries a small token the visitor chose, returned from that
directory's `Decided` (or from `root` for the root), and every child event
carries its parent's token, on whichever worker reports it (D29). `Entered`
marks a directory as listed, with the work tree whose top it is — main, linked
or submodule, and the common directory's path and `(dev, ino)` (D23, D33). A
file's `Decided` lends its parent's descriptor and its name, for a race-free
`openat` (D33). Faults are typed by operation and by what they are about: the
root, a directory that has its token, or a named entry of one (D26). Inner roots
are given as boundaries, matched by root-relative path and optionally checked by
`(dev, ino)`; the walk reports each and does not enter it (D34).

A configured root is always walked. Ignore configuration above it is not read
(D22); global rules and ignore files at the root and below still apply to its
contents (D25). The walk crosses into file systems mounted below a root (D20).

Two levels of inclusion: **catalogued** (name searchable, metadata filterable)
and **content-indexed** (also hashed and tokenized). Binary files and files over
the size cap are catalogued, not content-indexed.

`ferret_crawl::index` is one run: take the writer lock, widen the refresh set so
that any root with a root added or removed strictly inside it is refreshed
(D34), walk each refreshed root with its inner roots as boundaries, keep the
others, commit. On a worker, a file the policy sends to the index is first
offered to carry-over (D26: equal `(dev, ino, size, mtime, ctime)` reuses the
old hash, except an old `Unindexed`, which is read, D37); otherwise it is opened
through its parent's descriptor, its `fstat` must match the walk's `lstat`, and
it is sniffed and BLAKE3-hashed with a second `fstat` bracketing the read. A
file with more than one link goes through a per-run cache keyed by `(dev, ino)`:
the first name to claim it reads it, a later name takes the stored observation
whole if its own stat agrees and is a content fault if not, and a name that
meets the inode in flight is set aside and resolved once the inode is finished
(the backlog is drained as inodes finish, and the rest when its root's walk
ends, so no root's backlog outlives it), so no worker ever waits on another.
Faults are typed (D26 A′, amended): EACCES from opening or listing a directory
publishes its row without children and with an unknown entry count. Other
coverage faults — listing, opening or reopening a directory, reading an ignore
file (the global one included: missing is the defaults, unreadable fails the
run), probing git, `readlink`, a root's lstat — publish nothing and leave the
old generation; an entry that vanished before its `lstat` is a deletion; a content
fault — open, stat or read failing, the bracket moving, aliases disagreeing —
publishes the file with content state failed and no document, and it is re-read
next run. The build is the authority on which inodes fault, since only it sees
every name's observation; the report lists every name, under a refreshed root,
of an inode it published as a fault. Those paths are resolved from the new
catalog, not held through the walk. That pass is timed as the report's
`fault_time`, outside `commit_time`: 52 ms at 10M with no faults, 289 ms with
8,142.

`ferret_crawl::index_change` is the same run given a change to the root set
(roots to add, roots to remove) rather than the whole set. It applies the change
to the previous generation's roots under the writer lock, so two concurrent
`ferret index` commands cannot each drop the root the other added. The CLI uses
it for every command that touches roots.

## Content: documents, tokens, structures (D6, D8, D9)

**Doc ids** are assigned in the order content is first seen, so a first crawl in
directory order produces near-path-sorted ids, and a segment covers a contiguous
range of ids. Merging segments concatenates ranges.

**Tokens** (`ferret-text`): maximal alphanumeric-and-underscore runs,
lowercased, plus identifier parts at camelCase, TitleCase, snake and digit
boundaries (`parseHTTPRequest2` → `parsehttprequest2`, `parse`, `http`,
`request`, `2`). The tokenizer has a version; segments record the version that
wrote them. Query terms go through the same function.

**Structures**, each a `CandidateSource` over doc ids, and each a row in the
experiments:

| Structure                        | Atom             | Exact?                 | First built |
| -------------------------------- | ---------------- | ---------------------- | ----------- |
| term postings (intpack)          | term             | yes, for a single term | S2          |
| per-doc term filter (bloom/fuse) | term             | no                     | S3 exp.     |
| per-doc trigram filter           | regex, substring | no                     | S3          |
| trigram postings                 | regex, substring | no                     | S3 exp.     |
| positions                        | phrase           | yes                    | experiment  |

Phrase without positions: intersect the terms' postings, verify the survivors.
Regex: derive a trigram query from the regex (Cox), take candidates from a
trigram structure, verify. The research's size arithmetic for these on `~/w` is
in [architecture.html § The size budget](research/claude/architecture.html); the
experiments replace it with measurements.

**Segments.** Immutable files per structure over a doc-id range, committed by a
manifest rename. The term dictionary's structure (sorted front-coded blocks, an
FST, …) is decided in S2 and is itself an experiment row.

## Find syntax

`ferret-query::find` owns `ferret find`: the GNU argument parser, the
expression evaluator, the walk over the catalog or the live tree, the parallel
scheduler and the actions. It reaches the outside world only through its
`Effects` trait, which `ferret` implements in `src/find.rs` for output,
diagnostics and running commands; `ferret` also owns the flags, the config file
and opening the index.

The walk reads directories with `rustix` (`RawDir` getdents, for the entry
kinds) and uses it for `statx`, `access` and `statfs`. That is the
`ferret-query → rustix` edge in § Crates; the policy-driven crawler in
`ferret-crawl` stays separate.

`-regex` and `-iregex` compile through `ferret-verify`, which holds the GNU
regex dialects. The `regex` crate stays a dependency of `ferret-verify` alone.

What `find` does — modes, freshness, order, concurrency, exit status and the
differences from GNU — is specified in [FIND.md](FIND.md).

## Query (S1 for names and metadata, S2 onwards for content)

1. **Parse** into atoms combined with AND / OR / NOT: `term`, `"phrase"`,
   `/regex/`, `name:pattern`, and metadata predicates (`ext:`, `size:`,
   `mtime:`, `type:`, `path:` — growing toward `find`'s full set).
2. **Plan.** Each content atom asks the registered sources for estimates and
   takes the cheapest; name and metadata atoms go to the catalog.
3. **Execute** in doc space (leapfrog intersection over `DocCursor`s), map live
   docs to inodes to names, apply name and metadata atoms per name.
4. **Verify** non-exact atoms: re-read the file, check `(size, mtime)` against
   the catalog (a changed file is dropped and queued, never reported from stale
   data), run the matcher.
5. **Emit** one row per path (D15): path, doc id, and match offsets. Human
   output by default; JSON lines behind a flag.

A metadata-only or name-only query never touches the index.

**S1's name and metadata queries** (`ferret-query`; grammar in its crate doc).
The longest literal any name atom guarantees (a word, `ext:`'s `.EXT`, a glob's
or a regex's longest literal run) drives a scan of the name heap with
`ferret-verify`'s case-folding two-byte filter (D41); each hit is mapped to its
name by galloping over name starts, tested once, and the scan resumes at the
next name. A query with metadata atoms and no literal tests every inode row
first and then walks the name rows for the inodes that pass, loading the name
sections only if one does; the inode test is a pass over each column it reads,
loading the column as its pass begins and decoding only the runs of 64 inodes
that an earlier test left a bit set in, and a test that leaves none ends the
conjunction with the later columns unread. Everything else tests every name, read in decoded
runs. A name query loads
the name, directory, root, traversed and link/special sections, never the document rows,
and no inode columns for plain output (`--json` loads Size, Mtime and Doc); a
metadata test loads only the columns it reads. Paths are resolved once per
parent directory, and a directory's from its parent's when the directory before
it was a sibling, which breadth-first numbering makes the common case. Measured (`ferret-bench`, D43): a rare word is 5 ms warm at
`$HOME`, 206 ms at 10M and 763 ms at 40M, nearly all of it the section loads.

## Experiments and metrics

An experiment is a comparison of two or more implementations of one role —
usually two `CandidateSource`s for the same atom — on bytes, build CPU, and
query latency by atom class. It lives in `ferret-bench` and writes results to
`experiments/<name>/` (JSON rows, a `report.md`, the machine description), the
way [intpack-bench](https://github.com/dbalmain/intpack-bench) does. Codec-level
questions stay in intpack-bench; see [docs/intpack/](intpack/) for its results
and decisions to date.

Later, a structure that is not clearly better ships with an opt-in mode that
builds both and logs one against the other on the user's corpus, and an opt-in
upload of those logs and the local query log. The local query and timing log
exists from S1 and is the source of both.

The log is `$XDG_STATE_HOME/ferret/log.jsonl` (mode 0600), one JSON line per
`search` and per index run (`ferret index`, `ferret roots remove`), each with
`"v":1`. A `search` line records:

- the query atoms, the plan (`explain()`) and the strategy;
- `Stats`, the rows, the time to the first row and the total time;
- the bytes the reader read, and the catalog's name and inode counts.

An index line records:

- the outcome and the exit status;
- the walk, hash, commit and content-fault-pass times, and the peak RSS;
- the crawl's counts and the published generation's counts.

No field holds an id (D27 renumbers them), a result path or a root path. The
query atoms are logged as typed, though, so query text may itself contain a path
(`search path:/home/me/private`) or any name the user searched for (D45). The file
is set to 0600 on every append, and each line is written under an exclusive
`flock`, so concurrent processes never interleave within a line. The lock is
tried for at most 150 ms, then the line is dropped with a warning, so a stopped
lock holder cannot hang a finished command. Writing is best-effort: a line that
cannot be written is a warning, never a failed command.

The CLI's JSON lines write a path that is not UTF-8 as `path` (lossy text) plus
`path_base64` (the exact bytes). The `ferret` crate doc states this contract.

## Resident engine, batch and daemon (S1b — D46, D49, D54)

[S1B.md](S1B.md) defines one library engine in `ferret`, with `ferret batch`
and a `ferretd` binary in that package as hosts. The current dependency graph
already permits that coordination. The historical `ferret-daemon → later`
placeholder above is not a proposed new crate. D57 records the proposed
external JSON-parser edge; the enforced graph changes with its implementation.

Both hosts open checked catalog buffers resident, validate the effective
snapshot-plus-overlay view (D53 A), and pin generations per query. Future
content indexes are mapped by their owning crate in S2; none exists today.
D54 adds resident interned names and row postings in catalog, and a name-term
index/planner in query using text's tokenizer, retaining BFS. These catalog
row postings do not change the content-index boundary. D55 remains open.

Ordinary queries connect to the daemon, spawning it on first use; unavailable
background operation or `FERRET_NO_DAEMON` uses the same engine in process.
D56 leaves action-plan host routing open without changing find semantics.
The daemon consumes inotify hints through S1+'s real refresh seam, maintains
raw directory counts, and uses scoped/full recrawls for gaps. D51 A pauses the
writer at idle boundaries while queries keep old views. S1B specifies queue
bounds, freshness reporting and the research-derived politeness controller,
including the full-builder memory limit recorded in ROADMAP.

## Not yet designed

Decided inside the slice that needs them, recorded in DECISIONS.md if they
become Dave's call: term dictionary structure; segment file layout and merge
policy; catalog snapshot layout detail; multiple devices and bind mounts under
one root; extraction for PDFs and media metadata (after the agent skill); TUI
and GUI (much later).
