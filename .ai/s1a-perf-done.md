# S1a perf fixes: done-note

Branch `wt/s1a-compact`, started at `08ac432`. In progress.
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

## Numbers
(pending)

## Not fixed
(pending)

## Divergences from the brief
- Build "9.4 -> 19.4 s" is mostly load: 7.6 -> 11.2 s on a quiet machine.
