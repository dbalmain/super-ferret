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
ferret-query   → ferret-index (the CandidateSource trait only), ferret-catalog, ferret-verify, ferret-text
ferret-crawl   → ferret-policy, ferret-catalog, rustix, blake3
ferret-index   → ferret-text, intpack (git dependency, may be vendored — D11)
ferret-catalog → (std only)
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
| `ferret-catalog` | names, inodes, documents, the snapshot, name scan (D14)                                                                        | tokens, postings               |
| `ferret-text`    | the tokenizer and identifier splitting (D9); versioned                                                                         | files, ids                     |
| `ferret-index`   | segments over doc ids; each structure implements `CandidateSource`                                                             | files, paths, inodes           |
| `ferret-verify`  | re-reading a file and matching a query atom against its bytes                                                                  | how candidates were found      |
| `ferret-query`   | query syntax, planning, execution, result rows                                                                                 | any structure's on-disk format |
| `ferret`         | the CLI, config, XDG directories, setup, JSON lines output, the query log                                                      | —                              |

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
| `names`     | `NameId` | parent directory `InoId`, name bytes (in the name heap), child `InoId`                                                                                                  |
| `inodes`    | `InoId`  | `(dev, ino)`, size, mtime, ctime, mode, uid, gid, `DocId` or none; a 2-bit content state beside it (D37)                                                                |
| `docs`      | `DocId`  | content hash (BLAKE3, 128 bits kept); rows sorted by id, with holes where content died                                                                                  |
| `roots`     | —        | configured root paths and the `InoId` of each; nested roots are separate trees, and adding or removing a root inside a kept root requires refreshing the kept one (D34) |
| `links`     | —        | a symlink's `InoId`, its target as `readlink` returned it (in the strings heap)                                                                                         |
| `worktrees` | —        | a work tree's top directory `InoId`, kind (main / linked / submodule), repository id (the common directory's `(dev, ino)`) and its path for display                     |

Directories are numbered first, breadth-first from the roots in path order, so
their `InoId`s are `0..dirs` and a parent's id is always below its child's; the
snapshot persists each directory's own `NameId` (none for a root) and a bitset
of traversed directories, which exist only as parents and are excluded from
search (D29). A path is the walk from a name through its parent's name to a
root. Nothing else is derived at open (D30): `hash → DocId` is built by the
writer only, and `DocId → [InoId]`, the full `InoId → [NameId]` and a
directory's work tree are built when a query first needs them.

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

**Storage.** One snapshot file per catalog directory, rebuilt by every run
(D26): a versioned header (magic, format version, sniffer version, next
`DocId`), a table of eleven sections by offset and length, and the sections
themselves, fixed-width little-endian rows plus two heaps. The name heap holds
NUL-terminated names in `(parent, name)` order (D28 A) and is contiguous on
purpose: it is what filename search scans (D14). The strings heap holds root
paths, link targets and work-tree paths. A reader opens the file by reading the
header and table alone (200 B), and then reads each section positionally when a
query first needs it (D38 B), together with the sections it is checked against
(names need the heap; directory names need names; roots need directory names and
strings; links and work trees need strings). Each section is validated as it
loads: every offset, index and ordering a reader will follow is checked, so a
corrupt or truncated file is an error from the load that reads the bad section,
never a panic or a loop, and a query that does not read a section is not failed
by it. A single inode row can be read without its section, for the few rows a
name query reports. The writer holds an advisory lock on `lock` for the whole
run, writes `catalog.tmp`, fsyncs it, renames it over `catalog` and fsyncs the
directory; a reader holding the old generation keeps it (D32). Measured on
`$HOME` (D28, D30): 51.2 MB for 435k names, of which inode rows (64 B) are 27.8
MB, name rows (12 B) 5.2 MB, the name heap 10.6 MB and doc rows (20 B) 7.1 MB.
The memory budget is a config value, defaulted from measurement (D5).

The writer is built for 10M entries (D40). Workers fill columnar batches, about
120 B per entry. The build first decides every id with a few `u32` index arrays
per entry, deduplicating inodes and documents by sorting rather than through
maps, so every build error comes before a byte is written. It then streams the
sections to `catalog.tmp` in file order, freeing batch names once the name
sections are out, and reads the file back for the decode check only after the
batches are gone. The old generation is released before the build. Synthetic 10M
(the `$HOME` dump under 23 prefixes; 1.18 GB file): first build peak 1.66 GB,
commit 5.5 s; a re-run peaks at 2.56 GB during the walk, since the old
generation (read into memory) and the new batches are both live. 40M: 6.4 GB and
10.1 GB, commit 23 s. The writer releases its lock with `LOCK_UN` on drop,
because a child forked by any thread shares the lock's file description until it
execs.

**Change detection.** A re-crawl compares `(size, mtime, ctime)` with the
catalog row; unchanged means no read and no hash. A reused `(dev, ino)` after a
delete carries a new ctime, so it is re-read and re-hashed like any change.

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
directory (uncatalogued, its ignore files unread) only when an anchored
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
meets the inode in flight is set aside and resolved after the walk, so no worker
ever waits on another. Faults are typed (D26 A′): a coverage fault — listing,
opening or reopening a directory, reading an ignore file, probing git,
`readlink`, anything on a root — publishes nothing and leaves the old
generation; an entry that vanished before its `lstat` is a deletion; a content
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
sections only if one does. Everything else tests every name. A name query loads
the name, directory, root, traversed and link sections, never the document rows,
and reads inode rows one at a time for the rows it reports until that passes a
64th of the rows, when it loads the section instead. Paths are resolved once per
parent directory. Measured (`ferret-bench`, D43): a rare word is 5 ms warm at
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
`find` and per index run (`ferret index`, `ferret roots remove`), each with
`"v":1`. A `find` line records:

- the query atoms, the plan (`explain()`) and the strategy;
- `Stats`, the rows, the time to the first row and the total time;
- the bytes the reader read, and the catalog's name and inode counts.

An index line records:

- the outcome and the exit status;
- the walk, hash, commit and content-fault-pass times, and the peak RSS;
- the crawl's counts and the published generation's counts.

No field holds an id (D27 renumbers them), a result path or a root path. The
query atoms are logged as typed, though, so query text may itself contain a path
(`find path:/home/me/private`) or any name the user searched for (D45). The file
is set to 0600 on every append, and each line is written under an exclusive
`flock`, so concurrent processes never interleave within a line. The lock is
tried for at most 150 ms, then the line is dropped with a warning, so a stopped
lock holder cannot hang a finished command. Writing is best-effort: a line that
cannot be written is a warning, never a failed command.

The CLI's JSON lines write a path that is not UTF-8 as `path` (lossy text) plus
`path_base64` (the exact bytes). The `ferret` crate doc states this contract.

## Resident daemon (later, optional — D14)

`ferretd` watches the roots (inotify, with the re-crawl as the backstop), keeps
the catalog and hot index files resident so queries never start cold, and runs
indexing at idle priority. The CLI works identically with or without it: it
opens the catalog and index read-only. The research's politeness design
([architecture.html § Change detection](research/claude/architecture.html))
applies when this slice starts.

## Not yet designed

Decided inside the slice that needs them, recorded in DECISIONS.md if they
become Dave's call: term dictionary structure; segment file layout and merge
policy; catalog snapshot layout detail; multiple devices and bind mounts under
one root; extraction for PDFs and media metadata (after the agent skill); TUI
and GUI (much later).
