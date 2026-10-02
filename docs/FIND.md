# Find sources and freshness

`ferret find` answers from the index. Names, kinds, visibility, size, permissions,
ownership, link counts, inode/device identity, mtime and ctime (including
nanoseconds) describe the last indexing observation. New names are absent;
deleted names and old metadata remain queryable until re-indexing. Re-index
when changing ignore policy. The daemon will maintain freshness in a later slice.

Each start operand is an independent walk; starts and siblings may interleave.
A parent's expression completes before its children start; `-depth` and
`-delete` complete children first. `-prune` stops descent. There is no
order-sensitive plan class. Live traversal, including default-mode live
fallbacks, uses a bounded worker pool. Catalog work also uses workers where
measurements show a gain; shallow catalog walks stay on the caller thread.

Workers evaluate whole expressions and run actions concurrently. `-exec`'s
exit status gates its remaining expression. Each child's stdout is captured
and emitted whole; print/printf records and file output records are atomic.
Interactive prompts are serialized. Batch argument order and boundaries are
free. `-quit` cancels all further expression and traversal work; collected
batches flush at exit. Errors are reported on stderr and set exit status 1.

The catalog's raw directory entry count includes ignored names, so `-empty`
answers exact **indexed** emptiness. Successful `-delete` actions subtract this walk's removals from
that observation, including across worker tasks. Nested indexed roots supply their missing boundary edges from
root records. Re-included ancestors are ordinary visible directories.

Explicit ignored starts, suffixes below opaque markers, and unreadable opaque
directories walk live without nested ignore rules. Ignored reference operands
use live metadata; indexed references use stored metadata. Fields not stored
(access time, birth time, allocated blocks, and device major/minor numbers) need
live observations only when the corresponding primary or format asks for them.
Atime and block-column measurements are complete; their storage/fallback
tradeoff remains open under milestone 5a's decision rule. Logical symlink
following uses stored targets where indexed and live targets where no row
exists.

`ferret find -I` / `--no-ignore` is the unrestricted live mode. It opens no index
and reads no configuration. Set `find_no_ignore = true` in
`$XDG_CONFIG_HOME/ferret/config` (default `~/.config/ferret/config`) to use it by
default. Missing/empty config means false; comments and blank lines are accepted;
unknown, duplicate or malformed settings fail. Neither mode logs queries.

Missing/incompatible indexes and unresolved explicit starts fail with status 1
and re-index/`-I` guidance. Successful selected deletions exit 0, including when
ignored files were skipped. A visible directory containing ignored files can
fail with ENOTEMPTY and exit 1.

Effectful plans (`-exec`, `-execdir`, `-ok`, `-delete`) validate live starts,
observe which catalog names still exist when entering each directory, and open
directories for descent errors and execdir handles. A later sibling removal
does not hide a name already observed; a removed directory fails descent.
Names created by actions remain absent from the snapshot. Stored predicates
retain their indexed values. In default mode, `-empty` starts with the raw
indexed child count, including ignored children, and subtracts successful
removals made by this walk's `-delete`. It does not see removals made by
`-exec` commands. Use `-delete` for this accounting or `-I` for live emptiness.

Validation, full-corpus classification, missing-field costs and the complete
warm timing table are recorded in
`/home/dave/w/super-ferret/.ai/find-m5a-done.md`. The checked checkpoint passes
all workspace gates and all eight fd-comparable timing rows. Milestone 5a is
not complete until its two decision conflicts above are resolved.
