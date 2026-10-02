# Find sources and freshness

`ferret find` answers from the index. Names, kinds, visibility, size, permissions,
ownership, link counts, inode/device identity, mtime and ctime (including
nanoseconds) describe the last indexing observation. New names are absent;
deleted names and old metadata remain queryable until re-indexing. Re-index
when changing ignore policy. The daemon will maintain freshness in a later slice.

Default output follows catalog order, including `-quit`. A parent precedes its
children; `-depth` and `-delete` visit children first; `-prune` stops descent.
Sibling order need not match GNU find. There is no order-sensitive plan class.

The catalog's raw directory entry count includes ignored names, so `-empty`
answers exact **indexed** emptiness. Actions that remove children do not rewrite
that observation. Nested indexed roots supply their missing boundary edges from
root records. Re-included ancestors are ordinary visible directories.

Explicit ignored starts, suffixes below opaque markers, and unreadable opaque
directories walk live without nested ignore rules. Ignored reference operands
use live metadata; indexed references use stored metadata. Fields not stored
(access time, birth time, allocated blocks, and device major/minor numbers) need
live observations only when the corresponding primary or format asks for them.
Their storage/performance choice is being measured in milestone 5a.

`ferret find -I` / `--no-ignore` is the unrestricted live mode. It opens no index
and reads no configuration. Set `find_no_ignore = true` in
`$XDG_CONFIG_HOME/ferret/config` (default `~/.config/ferret/config`) to use it by
default. Missing/empty config means false; comments and blank lines are accepted;
unknown, duplicate or malformed settings fail. Neither mode logs queries.

Missing/incompatible indexes and unresolved explicit starts fail with status 1
and re-index/`-I` guidance. Successful selected deletions exit 0, including when
ignored files were skipped. A visible directory containing ignored files can
fail with ENOTEMPTY and exit 1.

Validation and warm timing results are recorded in
`/home/dave/w/super-ferret/.ai/find-m5a-done.md`. Action disappearance behavior,
full-corpus classification and the speed gate are still being validated.
