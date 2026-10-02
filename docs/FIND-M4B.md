# Default find: correctness and performance

The default source uses catalog visibility and kinds and lazy live stat. It
shares ordered traversal with the unrestricted source. Kernel name listings
retain live readdir order; catalog name rows are sorted and cannot supply it.
Explicit ignored starts/suffixes walk live, ignoring nested rules. Unreadable
opaque directories also walk live. Nested roots supply an edge from their root
record where the outer crawl stops (D34). Ignored recursive children never emit.

Missing/incompatible indexes, unresolved starts and new uncatalogued names
observed in listings fail with status 1, with re-index/`-I` guidance. Deleted
names are skipped. Changes to metadata are read live. Changing ignore policy
requires re-indexing. `-empty` uses raw child counts, including ignored names;
`-links` uses live lstat.

Config: `$XDG_CONFIG_HOME/ferret/config`, default `~/.config/ferret/config`,
contains `find_no_ignore = true|false`. Missing/empty means false; blank lines
and `#` comments are accepted, unknown/duplicate/malformed keys fail. Explicit
`-I` and help/version bypass config and the index. Neither mode logs queries.

A pasted `find … -delete` skips ignored files and still exits 0 when selected
deletions succeed. A visible directory still containing ignored files can fail
with ENOTEMPTY and exit 1.

## Correctness checks

The committed CLI self-check shares every expression with the GNU differential
suite and compares **exact** output order, exit status and stderr emptiness
against `-I` for seven start spellings, with no ignore rules. It also compares
`-exec +`, `-execdir +` and printf. Dedicated cases cover ignored starts and
references, re-inclusion/prune, changed size/link counts, deleted names,
permission denial, config, unresolved/stale starts and nested indexed roots.

Seed corpus, both targets, clean plus all four ignore trees, jobs 4:

- default: 243 agree, 9 skipped, 4 reported differ, zero error;
- unrestricted: 315 agree, 5 skipped, zero differ/error.

All four default differences are command `ec752f4d3d48`:
`find @ROOT@ -name ref -exec sh -c 'echo {}' ';'`. The oracle result is
**skipped**, with `sh -c command not allowlisted:` followed by the allowlisted
absolute coreutils echo path, while ferret is `ok`, exit 0. The harness compares
those statuses as a difference; this is an oracle allowlist artifact, not a
GNU result mismatch. Every supported seed comparison agrees. The harness is
unchanged; its old control-3 catalog grammar is also unchanged.

Artifacts: `/home/dave/w/find-compat/.scratch/ferret-impl/m4b/`:
`seed-progress-initial.jsonl`, `seed-summary-initial.json`, `seed-initial.log`.
The runner wrapper only redirects temporary/output directories into this slice.
The full corpus remains Dave's acceptance run; its old unrestricted baseline
67,394 agree / 20 differ / 6 harness-failure was not re-run here.

## Serial timing on the 300k tree

Initial median of 7 warm repeats, each preceded by a counting/warmup run.
No competing `ferret_timing|synthetic|ferret-bench` process. Start load:
1.32 / 1.54 / 1.36. No compilation ran during measurements. Each cell gives
milliseconds and the median sample's starting 1/5/15-minute load; every
invocation's before/after loads are preserved in `timing-loads.json`.

| Expression after `.` | default ms (load) | fd ms (load) |
| --- | ---: | ---: |
| `-name *.c` | 185.2 (1.44/1.56/1.37) | 25.4 (1.39/1.55/1.36) |
| `-type f` | 162.8 (1.56/1.58/1.38) | 34.9 (1.56/1.58/1.38) |
| `-maxdepth 2 -mindepth 1` | 10.9 (4.32/2.15/1.56) | 7.0 (1.56/1.58/1.38) |
| `-type f -size +1024c` | 450.8 (4.25/2.21/1.59) | 51.3 (4.53/2.23/1.59) |
| `-print0` | 161.9 (3.90/2.20/1.59) | 39.4 (3.90/2.20/1.59) |
| `-mtime 0` | 453.3 (3.47/2.19/1.60) | 51.2 (3.69/2.21/1.60) |
| `-name *.c -o -name *.h` | 209.2 (3.09/2.15/1.59) | unsupported |
| `-path */d1*/* -name *.rs` | 219.7 (3.07/2.18/1.61) | unsupported |
| `-type d` | 161.1 (2.90/2.16/1.60) | 22.7 (2.90/2.16/1.60) |
| `-empty` | 450.7 (3.32/2.31/1.67) | 44.4 (3.65/2.34/1.67) |
| `-name *.py -newer ./README` | 226.2 (3.92/2.48/1.73) | unsupported |
| `-regex .*\.\(c\\|h\)` | 185.0 (4.21/2.59/1.77) | unsupported |

The speed gate is **not met**. This implementation retains exact live order;
fd performs parallel traversal/stat work. The catalog filtering adds overhead
to the sequential directory listing floor.

## Stored-stat measurement, unselected

A temporary two-file trial used the catalog size/whole-second mtime columns
for `-size` and integer `-mtime`, leaving the same evaluator, traversal and
output. It eagerly decoded both columns for every catalog entry, so its name
and type controls include that overhead. Nine paired warm samples, alternating
binary order, with one discarded counting/warmup each. Output byte counts
matched for all four queries. No competing benchmark; start load
0.93 / 1.36 / 1.46. Each median sample's start load appears below; all samples
and before/after loads are in `stat-trial.json`.

| Query | live stat ms (load) | stored stat ms (load) |
| --- | ---: | ---: |
| name | 189.43 (1.99/1.55/1.52) | 194.98 (1.99/1.55/1.52) |
| type | 163.38 (1.99/1.56/1.52) | 168.94 (1.99/1.56/1.52) |
| size | 443.29 (1.99/1.56/1.52) | 169.68 (1.91/1.55/1.52) |
| mtime | 459.33 (1.84/1.54/1.52) | 169.89 (1.84/1.54/1.52) |

Existing catalog-only `search` is a separate control, **not** a replacement
find evaluator: it uses catalog order, stored metadata and absolute output
paths. Its stdout byte counts differ accordingly. Warm median of 9:

- `search *.c`: 12.11 ms, load 1.85/1.55/1.52.
- `search type:f size:>1024`: 11.64 ms, load 1.85/1.55/1.52.
- `search mtime:<1d`: 21.49 ms, load 1.85/1.55/1.52.

Trial code is [find-m4b-stored-stat.patch](find-m4b-stored-stat.patch), preserved
but **unapplied**. Trial/live binaries and formatted source copies are in
`/tmp/find-m4b-measure/`. The patch changes only the measured size/mtime subset;
it is not a complete or production-ready alternate metadata policy.

## Decision briefs

**Stat freshness.** A: keep lazy live lstat (selected by the settled handoff),
correct for current metadata but pays one syscall per matching entry. B: use
snapshot size/mtime, saving roughly 274–289 ms here, but silently answering
from stale data and missing deletions. C: a watch-backed daemon stat cache,
keeping correctness within the watched set but requiring the later daemon.
Recommendation: retain A; the measurement supports C as a later optimization.
Dave should choose any change to B. The fact that changes it: a defined and
acceptable freshness contract for snapshot metadata. B alone still misses fd
by over 3x because the ordered traversal remains.

**Live traversal order versus catalog speed.** A: current live listings, exact
current order but 160–220 ms even for cheap predicates. B: persist traversal
order at indexing and keep it current via watches, adding storage/crawler work
and the daemon but allowing catalog traversal. C: permit catalog order for
plans whose effects cannot observe order, keeping live order for quit/command
plans; fast name queries but changes the requested exact-order contract and
plain output order. Recommendation: investigate B (measure its storage cost
and freshness checks), or explicitly approve C if output order need not match.
Neither is silently selected in this slice. A faster directory-name map alone
cannot remove the live traversal floor demonstrated by the unrestricted rows.

The functional slice is implemented. It is **not a completed milestone** under
the speed gate; the report records the measured constraint and proposed shapes.
