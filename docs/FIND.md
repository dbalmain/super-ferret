# `ferret find`

The contract for `ferret find [-I] [-H|-L|-P] [PATH...] [EXPRESSION]`: GNU find
syntax, answered from the index by default. Anything this page does not promise
is not promised. Why each choice was made is in [DECISIONS.md](DECISIONS.md)
(D47 and D50); what was built when, and what it measured, is in
[ROADMAP.md § S1c](ROADMAP.md#s1c--ferret-find-in-find1-syntax).

"GNU" throughout means GNU findutils 4.11.0, which the differential tests run
against.

## Modes

| Mode    | Selected by                                      | Answers from                                                     |
| ------- | ------------------------------------------------ | ---------------------------------------------------------------- |
| default | nothing                                          | the index: names, kinds, visibility and metadata as last indexed |
| live    | leading `-I` or `--no-ignore`, or the config key | the disk, now, with no ignore rules                              |

Default mode opens the index named by `ferret --index DIR find …`, else
`$FERRET_INDEX`, else `$XDG_DATA_HOME/ferret` (default `~/.local/share/ferret`),
and reads only the index sections the expression needs.

Live mode opens no index and reads no configuration. It matches GNU apart from
the order and concurrency rules and the known differences below. Help and
version requests also skip the index and the configuration. Neither mode writes
a query log.

Both modes run the same parser and evaluator over the same depth-first walk;
only the source of names and metadata differs.

## Freshness (default mode)

Default mode is an index query, like `locate`. Names, kinds, visibility and
stored metadata describe the last `ferret index` run:

- a name created since then is absent, including one created by the command's
  own actions;
- a name deleted since then is still listed, and changed metadata still answers
  with its old value, until the next index run.

An expression with an effect on the tree or on files checks that names still
exist. The effects are every form of `-exec`, `-execdir`, `-ok` and `-okdir`,
`-delete`, and the file outputs `-fprint`, `-fprint0`, `-fprintf` and `-fls`.
Such an expression lstats each start operand. On entering a directory it opens
it and lstats each child. Non-directory names that have gone are dropped.
Directories removed by this invocation's own `-delete` are also dropped;
externally removed directories still report ENOENT. A later sibling's
removal does not hide a name already observed. Stored predicates such as
`-mtime` and `-size` still answer from the index.

Re-index after changing ignore rules: the index does not watch policy files, or
anything inside an ignored tree. The daemon (S1b) will keep the index current;
until then, `-I` asks about the disk as it is now.

## Ignore rules (default mode)

Paths excluded by the ignore rules (D13: the global rules, `.gitignore` and
`.ferretignore`) do not exist to default mode, with these exceptions:

- A start operand that names an ignored path, or a path beneath an ignored
  directory, is walked live, without nested ignore rules. So is an indexed
  directory that could not be read when it was indexed.
- A re-included path's ancestors are ordinary visible directories.
- An ignored reference operand (`-newer REF`, `-samefile REF` and the like)
  resolves from the disk.
- A directory's `-links`, `-size` and `-empty` count its ignored children, as
  GNU sees them.

A start or reference operand outside every indexed root fails, exit 1, with
advice to re-index or use `-I`.

A pasted `find … -delete` skips ignored files, and exits 0 when the deletions it
selected succeed. Deleting a visible directory that still holds ignored files
fails with ENOTEMPTY and exits 1, as a live `rmdir` would.

## Stored and live metadata (default mode)

The index stores, and default mode answers from: kind, size, mode, owner and
group ids, link count, device and inode numbers, mtime and ctime with
nanoseconds, symlink targets, and each directory's raw entry count, ignored
entries included (F8 B).

These are not stored (F13 A). They are read from the disk only when the
expression asks for them:

| Field                  | Asked for by                                                  |
| ---------------------- | ------------------------------------------------------------- |
| access time            | `-atime`, `-amin`, `-anewer`, `-used`, `-newera*`, `%a`, `%A` |
| allocated blocks       | `-ls`, `%b`, `%k`                                             |
| birth time             | `-newerB*`, `%B`                                              |
| device major and minor | `-ls` on a device                                             |
| access permission      | `-readable`, `-writable`, `-executable`                       |
| filesystem type        | `-fstype` (the mount table, against the stored device)        |

Entries walked live, under an ignored start or an unreadable directory, read
everything from the disk. With `-H` or `-L`, a symlink is followed through the
index when its target is indexed, and through the disk when it is not.

## `-empty` (default mode)

A file is empty when its stored size is zero. A directory is empty when its
stored raw entry count, ignored entries included, minus the children this walk's
own `-delete` has removed, is zero (F12 D). A directory walked live reads its
listing.

So `find . -depth -type d -empty -delete` removes the directories it empties, as
GNU does. Removals made by a command are not seen: `-exec rmdir {} \;` leaves an
emptied parent behind in default mode. Use `-delete`, or `-I`.

## Order

Three rules, in both modes:

- a parent's expression completes before its children start;
- under `-depth` or `-delete`, children complete before their parent;
- `-prune` stops descent below the entry it is true for.

No other order is promised (F10 B): not sibling order, not the order of start
operands that overlap, not argument order within an `-exec … +` batch, and not
which entry reaches `-quit` first. Output with no record separator, such as
`-printf '%s'`, concatenates in whatever order entries finish.

## Start operands

Start operands run one after another, in operand order, when the expression has
an effect (listed under Freshness) or `-quit`. Otherwise they may overlap.
Printing to stdout with `-print`, `-printf` or `-ls` alone does not sequence
them.

`-quit` sequences starts because GNU stops inside the first start that reaches
it; a later start running alongside could otherwise report a missing path and
exit 1 first.

## Concurrency and output

The walk runs on a pool of up to min(16, CPUs) workers, capped at 8 for catalog
walks and for `-maxdepth` 2 or less. A catalog walk with `-maxdepth` 2 or less
stays on the calling thread, apart from directories it walks live and start
operands that may overlap. `-maxdepth 0`, or a single CPU, starts no pool.

Each worker evaluates whole expressions and runs their commands itself (F11 A).
An `-exec … ;` is a test: its exit status decides whether the rest of the
expression runs for that entry. Commands for different entries run concurrently,
so commands with clashing side effects can leave a different tree from run to
run.

**An entry's output commits whole.** Everything one entry writes — its prints,
`-printf` and `-ls` records, its commands' stdout, and its records for `-fprint`
files — commits whole and never interleaves with another entry's. Completed
records may stage together in a bounded transaction; commands and `-quit`
commit immediately. The commit lock covers destination buffers and their final
flush. A failed capture or rendering discards that entry's record. A command's stdout is a pipe, drained while it runs. Up to 64
KiB per stream is held in memory; beyond that the stream spills to a private,
unlinked file in `$TMPDIR`, so memory stays bounded while temporary storage
grows with the entry's output. A command's stderr and stdin are inherited, not
captured. `-ok` and `-okdir` prompts are serialised, and their commands run with
stdin closed, as in GNU.

**`-exec … {} +` uses one shared batch per action**, across all workers and
start operands. A batch runs when the next argument would pass the argument
limit, and once at exit. Batch boundaries follow the limit GNU uses; argument
order within a batch is free. Full batches run outside the batch's lock while
workers keep collecting, so two batches of one action may run at once. Workers
stage up to 32 paths, or 4 KiB of path bytes, before merging into the shared
batch. A batch command's stdout is written whole. `-execdir … {} +` batches stay
per worker and per directory, and run when that worker's directory changes.

**`-quit` commits exactly one winning entry.** The first entry to reach `-quit`
commits its output and latches the quit; entries that finish later discard
theirs, and no new entry starts. Commands and prompts already running finish and
are waited for (DECISIONS, Find M5b, option A). Collected batches, shared,
staged and per-directory, run at exit.

## Configuration

`$XDG_CONFIG_HOME/ferret/config` (default `~/.config/ferret/config`) holds one
setting:

```text
# make -I the default
find_no_ignore = true
```

The value is `true` or `false`. Blank lines and `#` comments, including at the
end of a line, are allowed. A missing or empty file means `false`. Any other
key, a duplicate, or a malformed value fails with exit 1. Explicit `-I` does not
read the file.

## Exit status

`0` when every operation succeeded, whether or not anything matched. `1` for any
error:

- a usage or parse error, or a recognised feature that is not implemented;
- in default mode, a missing or incompatible index, a start or reference operand
  outside every indexed root, or a bad config file;
- a traversal or metadata error, which is reported while the walk continues;
- a failed `-delete`, or an `-exec … +` or `-execdir … +` batch whose command
  fails;
- a failure writing output, which stops the walk.

An `-exec … ;` or `-ok … ;` whose command fails, or cannot be launched, is a
false test and leaves the status unchanged; a launch failure is reported on
stderr.

## Known differences from GNU find

Beyond the order rules:

- **Default mode:** the ignore rules, freshness and `-empty` behaviour above.
- **Concurrent commands** (F11 A). Under `-L`, two links to one directory can
  send concurrent `rm` commands at the same files, and the outcome varies from
  run to run.
- **Command stdout is a pipe**, not the terminal, because it is captured.
- **The walk is path-based** (F5 A). Deletion uses the observed parent
  directory handle, including for explicit starts, so replacing an ancestor
  cannot redirect deletion outside that directory. `-execdir`/`-okdir` and
  live symlink reads (`%l`, `%Y`, `-lname`, `-ilname`, `-xtype`) use the same
  observed parent. Explicit parents require search permission, without read
  permission. Under `-P`, descent does
  not follow a replacement symlink; an explicit trailing slash still follows
  the operand's link. A tree deeper than PATH_MAX stops at the
  first path that is too long, and an ancestor renamed mid-walk reports ENOENT;
  both exit 1 where GNU carries on.
- **Not implemented**, failing with exit 1: `-context`, `-files0-from`,
  `-printf %Z`, the GNU regex word assertions `\<` and `\>`, and multi-byte
  collating symbols.
- **Regex backreferences** use a bounded search. A pattern that exhausts it is
  an error for that entry, with exit 1, rather than an unbounded search.
- **Dates and names:** `-printf` and `-ls` format times in UTC, whatever `TZ`
  says, in the C locale. User and group names come from `/etc/passwd` and
  `/etc/group` only.

## Resource and expression limits

Output keeps at most 64 KiB per captured stream in memory, then uses temporary
storage. Spill creation, writes and reads report the operation and temporary
path; an output failure stops the walk.

Freshness checks and file output alone retain no directory descriptors across
ancestor levels. Actions or live link reads needing an observed parent retain
one descriptor per active ancestor, plus active command batches; such walks
can reach the process's descriptor limit and report EMFILE. Increase that limit
for deep `-delete`/`-execdir` walks. Path length remains limited by PATH_MAX.

Expressions accept at most 2,000 explicit or implicit `-a`, `-o`, `-not` and
comma operators, and at most 128 nested parentheses or negations in total.
Exceeding either limit gives a parse error with exit 1, before evaluation.
