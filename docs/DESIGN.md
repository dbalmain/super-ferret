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
ferret-crawl   → ferret-policy, ferret-catalog, rustix
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
| `ferret-catalog` | names, inodes, documents, the snapshot + log store, name scan (D14)                                                            | tokens, postings               |
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

Six tables. `names`, `inodes` and `docs` have dense `u32` ids assigned by the
catalog; `roots`, `links` and `worktrees` hang off an existing `InoId`. Raw
inode numbers are data, never keys.

| Table       | Id       | Row                                                                                                                             |
| ----------- | -------- | ------------------------------------------------------------------------------------------------------------------------------- |
| `names`     | `NameId` | parent directory `InoId`, name bytes (in the name heap), child `InoId`                                                          |
| `inodes`    | `InoId`  | `(dev, ino)`, kind, size, mtime, ctime, mode, uid, gid, `DocId` or none                                                         |
| `docs`      | `DocId`  | content hash (BLAKE3, 128 bits kept), live flag                                                                                 |
| `roots`     | —        | configured root paths and the `InoId` of each                                                                                   |
| `links`     | —        | a symlink's `InoId`, its target as `readlink` returned it (in the heap)                                                         |
| `worktrees` | —        | a work tree's top directory `InoId`, kind (main / linked / submodule), repository id (the common directory's path, in the heap) |

Derived at load: `hash → DocId`, `DocId → [InoId]`, `InoId → [NameId]`
(directories have exactly one name), and each directory's work tree from the
nearest `worktrees` row above it. A path is the walk from a name through its
parent's name to a root.

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

**Storage.** A snapshot of fixed-width rows plus a contiguous name heap, and an
append log of row operations since the snapshot. A reader maps the snapshot and
replays the log; the writer compacts the log into a new snapshot and swaps it in
with the write-fsync-rename-fsync-dir sequence. The name heap is contiguous on
purpose: it is what filename search scans (D14). Rough size: ~48 B per inode,
~12 B per name plus name bytes — about 70–80 MB at 1M files (estimate; the first
slice measures it). The memory budget is a config value, defaulted from
measurement (D5).

**Change detection.** A re-crawl compares `(size, mtime, ctime)` with the
catalog row; unchanged means no read and no hash. A reused `(dev, ino)` after a
delete carries a new ctime, so it is re-read and re-hashed like any change.

## Policy and crawl (D10, D13)

`ferret-policy` is pure: the crawler carries a `DirRules` per directory (`root`,
then `enter` with that directory's ignore-file contents, or `traverse`), asks it
to `decide` each entry using a borrowed root-relative path, and `sniff`s file
heads; it is tested against a golden corpus of trees and expected decisions.
Precedence, most specific first: a `.ferretignore` in the directory or an
ancestor; `.gitignore` and `.git/info/exclude` inside a work tree (a `.git` file
contributes exclude from its gitdir, or from that gitdir's `commondir` when it
has one; a symlinked `.git` contributes none); the user's global ignore file
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
its last component into the list only where it can match. A layer above the root
(D22) is stepped down to the root before the walk. The same cursors answer
whether a `.ferretignore` `!` pattern reaches below an excluded directory. No
whole path is ever matched.

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
under `~`). Both are dropped with the root's last `DirRules`.

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

A configured root is always walked, even when the enclosing work tree's rules
exclude it or one of its ancestors: naming a root overrides `.gitignore` (D25),
as it overrides the global ignore file. Those rules still apply below the root.
The walk crosses into file systems mounted below a root (D20).

Two levels of inclusion: **catalogued** (name searchable, metadata filterable)
and **content-indexed** (also hashed and tokenized). Binary files and files over
the size cap are catalogued, not content-indexed.

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
