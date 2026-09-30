# S1a perf fixes: done-note

Branch `wt/s1a-compact`, started at `08ac432`. Done.
Scratch: `.../scratchpad/perf/` (perf data, baseline worktree `base` at
`d7a120f` built into `base-target`).

## Profile (perf 7.2.8 from nixpkgs, user space only: perf_event_paranoid 2)

Quiet machine (load 1.6-2.8), same catalogs as `s1a-measure-done.md`.

- **Rare needle** `flamegraph`: wall 0.45 s = user 0.18 + sys 0.26. The sys
  half is reading 630 MB into fresh buffers: 347 MB of name sections plus
  283 MB of inode columns the plain-path output never reads. User: 57%
  `check_names`, 14% `check_dir_names`, 14% memcmp (name order, as v1), 6.5%
  `check_dictionary` (Mode/Owner dictionaries scanned over 10M rows: inode
  load). Hypothesis 4 holds, and it is the larger half (~0.1 s sys + ~12 ms).
- **`*`**: user 3.39 s. Inode decode for the row's `meta` is ~25% (Catalog::column
  not inlined 10.5%, inode 7%, mode 3%, doc 2.7%, owner 1.2%): ~0.85 s. The
  glob's regex 20% and `Catalog::name` 19% (three views, NUL search) are as v1.
- **Build**: quiet-machine rerun, v1 7.6 s vs v2 11.2 s wall (not 9.4 -> 19.4:
  the measured 19.4 s ran at 56% CPU under load 8). Commit phase 3.9 s -> 6.55 s.
  `inode_rows` (`Index::file`/`Index::dir` partition_point per row per column,
  12 passes) is ~22% of samples, ~2.3 s. Hypothesis 1 holds.

## Changes (commits on `wt/s1a-compact`)
- `d15d070` Rows carry no decoded inode (`Row::meta` removed). Callers that
  print metadata load its sections and read by `Row::inode`; `find --json`
  loads Size, Mtime, Doc (3 columns, not 12). Chose this over a run option
  or `Option<Inode>`: the catalog's contract is already "load, then
  infallible accessors", and no flag threads through `Query::run`.
- `74e5874` Name-section checks: `Packed::decode` fills runs of 64 values;
  `check_names` is one pass (was two, decoding offsets and parents twice);
  the converse dir-names check is a count (named dirs vs names with a
  directory child; equal counts plus the forward check imply every such
  name is its directory's edge) instead of a random lookup per name.
  A bit-buffer iterator (tried first) was no faster than `get`; a block
  iterator was slower (not inlined).
- `5c19743` Build: each inode row's `&Stat` resolved once (8 B/row, after
  names are freed; peak RSS unchanged at 1.88 GB); doc column no longer
  looks rows up; `packed::Writer` keeps its mask and a u64 buffer. Output
  byte-identical to the measured 10M catalog (`cmp`).
- `107959c` `Catalog::name` ends a name at the next offset instead of a NUL
  search: -0.21 s on `*` at 10M.

- `3720fc3` ROADMAP S1a Measured tables updated (before column untouched;
  "after" is now the fixed code; load noted; `index` rows not re-run).

## Numbers
Committed harness, same catalogs, load 2.1-4.3, nothing compiling.
10M `find`, fresh ms (evicted after fixes in brackets):

| query        | v1 before | S1a after | after fixes   |
| ------------ | --------: | --------: | ------------: |
| `flamegraph` | 243       | 440       | 295 (406)     |
| `test`       | 643       | 557       | 362 (492)     |
| `size:>100M` | 612       | 562       | 453 (613)     |
| `*`          | 2,488     | 3,696     | 2,315 (2,417) |

- 10M name-section load: v1 207 ms, S1a 280, now 254 (371 evicted).
  `load_all` 439 / 784 ms.
- 10M `find` RSS: v1 365-991 MB, S1a 604, now 333 (name) / 372-376 MB
  (metadata atom); bytes read 347 MB (was 630). `--json` 0.33 s / 452 MB.
- 10M build: v1 9.4 s, S1a 19.4 s measured (11.2 s quiet), now 9.6-9.8 s
  wall, 1.88 GB; CPU 9.5 s vs S1a 10.9 s and v1 7.3 s (same sitting).
- `$HOME`: flamegraph 14.2 (v1 12.3, S1a 21.6), test 15.6, size 21.2,
  `*` 97.2 (v1 110, S1a 156.5) ms fresh; RSS 17 MB, 14.4 MB read.
- Gates at `107959c` (code unchanged since): fmt, clippy -D warnings,
  265 passed / 1 ignored.

## Not fixed
- **Rare needle ~50 ms behind v1** at 10M: validation decodes every packed
  offset/parent/child (0.14 s user vs v1 0.09 s). Removing it means skipping
  or deferring the D38 B load check, or unpacked offsets on disk; both are
  format/contract changes the profile doesn't justify for 50 ms, and a
  resident engine (D46/D49) pays it once.
- **Build CPU 2.2 s over v1**: 0.4 s is the synthetic's own fill (entry
  counts, nlink); the rest is one pass per stat column plus the sizing pass.
  Fusing them means holding encoded columns in memory, which D40 rules out.
  A dictionary last-index cache was tried and bought nothing (reverted).
- `Catalog::column` isn't inlined into `inode()`; only matters to callers
  decoding a whole inode, which no query path does now. Left alone rather
  than add an `#[inline]` ledger.

## Divergences from the brief
- Build "9.4 -> 19.4 s" is mostly load and `fsync` (0.5-8 s run to run):
  7.6 -> 11.2 s on a quiet machine. Hypothesis 1 still held on CPU.
- Name load "+73 ms" is decode in the load checks, not bytes; a sequential
  bit-buffer iterator did not help, blocked `decode` did.
- `*` "+1.2 s": ~1.1 s inode decode, confirmed; plus 0.2 s NUL search.
- findbench's rare-needle set includes `case:Flamegraph`, so "rare needle"
  numbers are per the harness, not one query.
- `.ai/` is gitignored; this note is force-added (`git add -f`).
- ROADMAP `index` rows were not re-run (no index-path code changed).
- Scratch baseline worktree `base` (at `d7a120f`) is still registered; remove
  with `git worktree remove <scratch>/perf/base` when done.
