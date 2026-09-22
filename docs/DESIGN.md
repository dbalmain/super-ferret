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
cycles.

```text
ferret         → ferret-query, ferret-crawl, ferret-catalog, ferret-index, ferret-verify
ferret-query   → ferret-index (the CandidateSource trait only), ferret-catalog, ferret-verify, ferret-text
ferret-crawl   → ferret-policy, ferret-catalog
ferret-index   → ferret-text, intpack (git dependency, may be vendored — D11)
ferret-catalog → (std only)
ferret-verify  → regex
ferret-policy  → ignore
ferret-text    → (std only)
ferret-bench   → anything; nothing depends on it
ferret-daemon  → later
```

| Crate            | Owns                                                                                             | Knows nothing about            |
| ---------------- | ------------------------------------------------------------------------------------------------ | ------------------------------ |
| `ferret-policy`  | `should_index(path, meta) -> Decision`; `.ferretignore` / `.gitignore` / global / built-in (D13) | the catalog, the index         |
| `ferret-crawl`   | walking roots, `statx`, change detection against the catalog, hashing                            | query, index formats           |
| `ferret-catalog` | names, inodes, documents, the snapshot + log store, name scan (D14)                              | tokens, postings               |
| `ferret-text`    | the tokenizer and identifier splitting (D9); versioned                                           | files, ids                     |
| `ferret-index`   | segments over doc ids; each structure implements `CandidateSource`                               | files, paths, inodes           |
| `ferret-verify`  | re-reading a file and matching a query atom against its bytes                                    | how candidates were found      |
| `ferret-query`   | query syntax, planning, execution, result rows                                                   | any structure's on-disk format |
| `ferret`         | the CLI, config, JSON lines output, the query log                                                | —                              |

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

The trait is the first thing written in the index slice and the thing most worth
reviewing; its exact shape (boxed cursors versus an enum of known cursors, cost
units) is settled there, against intpack's cursor.

## The catalog (D4, D5)

Four tables, all with dense `u32` ids assigned by the catalog. Raw inode numbers
are data, never keys.

| Table    | Id       | Row                                                                     |
| -------- | -------- | ----------------------------------------------------------------------- |
| `names`  | `NameId` | parent directory `InoId`, name bytes (in the name heap), child `InoId`  |
| `inodes` | `InoId`  | `(dev, ino)`, kind, size, mtime, ctime, mode, uid, gid, `DocId` or none |
| `docs`   | `DocId`  | content hash (BLAKE3, 128 bits kept), live flag                         |
| `roots`  | —        | configured root paths and the `InoId` of each                           |

Derived at load: `hash → DocId`, `DocId → [InoId]`, `InoId → [NameId]`
(directories have exactly one name). A path is the walk from a name through its
parent's name to a root.

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

`should_index` is a pure function over path and metadata, tested against a
golden corpus of trees and expected decisions. Precedence, most specific first:
a `.ferretignore` in the directory or an ancestor; `.gitignore` and
`.git/info/exclude` inside a work tree; the user's global ignore file; built-in
defaults (`node_modules/`, `target/`, `.venv/`, …, a size cap, a binary check).
`!pat` in a `.ferretignore` overrides an ancestor `.ferretignore` or any
`.gitignore`, and can re-include below an excluded directory; the walker
descends an excluded directory only when some `!` pattern could match inside it.
The first implementation wraps the `ignore` crate; the re-inclusion case is the
wrapper's job.

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
