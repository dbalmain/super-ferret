# Historical milestone 4b measurements

These are the measurements referenced by the decisions page. The current
contract and validation are in [FIND.md](FIND.md); milestone 5a supersedes
M4b's live order and live freshness. The numbers below describe M4b only.

## Serial timing on the 300k tree

These measurements precede the full-corpus correctness fixes below; they were
not repeated for that follow-up. Initial median of 7 warm repeats, each preceded
by a counting/warmup run.
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

The unapplied size/mtime trial was superseded by milestone 5a and its patch
removed. It measured only that subset, not a complete metadata policy.
