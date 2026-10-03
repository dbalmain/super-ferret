# Parallel find implementation plan

M5b implements F10 B / F11 A: parent evaluation completes before publishing
children; depth-first parents wait for all descendant tasks. Workers evaluate
whole expressions, including commands. Starts are independent. Pruning is a
local descent decision and quit cancels shared work.

Reuse the live/catalog DFS engine, donating sibling ranges to a bounded
standard-library worker pool. Keep path-based operations and existing cached
metadata, symlink, xdev and directory-batch semantics. Replace thread-local
shared output/handle ownership with Arc and short mutex scopes. Capture child
stdout as one command record; serialize interactive prompts.

Measure live and catalog walks separately, including single-entry startup.
Start with min(16, cores), compare worker counts before choosing. Keep catalog
parallelism only if measured faster. Run structural/action stress tests,
sorted GNU differential/self-check, seed then final full harness, and workspace
gates. Record load, every remaining difference and final binary in the live
M5b done-note. No dependency, unsafe, manifest/lockfile changes or push.

## Checked implementation checkpoint

The bounded pool donates already-observed sibling ranges. A completion count
keeps each parent behind its donated descendants; waiting parents return to
the queue. Only owned levels may donate, and a recorded suspension survives
a racing completion. Directory handles and output files use Arc ownership.
Workers own expression control and batches, capture child stdout whole, and
serialize interactive prompts. Parent records flush before descendant work
is published. Quit cancels further traversal and evaluation; collected batches
still flush and started children are reaped.

Use min(16, cores) for live work, capped at 8 for shallow live and catalog
walks. Five-sample measurements favor 16 for live stat-heavy rows and 8 for
most catalog scans. Shallow catalog work stays on the caller while live
fallback levels still donate. No pool starts for a file or maxdepth zero.
The paired shallow comparison is M5a 6.982 ms / M5b 6.927 ms, load
2.76/2.55/2.41, 25 samples each.

Workspace: 390 passed, four pre-existing ignored; formatter and workspace
clippy -D warnings pass. Parallel GNU differential: 105 expressions, green.
Default/live self-check: 105 expressions x seven starts, green. Seed harness:
247 default agreements / 315 live agreements, no differences or errors.
The full 135693-row final run is complete: zero real differences and zero
errors for both targets after manual classification. Raw remaining differences
are 61 default / 129 live; all 190 rows are individually explained in the
M5b done-note. Three default / six live both-timeouts are inconclusive.
The fd stretch goal meets two of eight comparable rows.

Five warm samples, milliseconds; each cell includes host load 1/5/15 at its median sample. fd 10.4.2. Start load 2.16/2.36/2.35. No competing benchmark or compiler.

| Query | GNU | bfs | fd | ferret -I | ferret |
| --- | --- | --- | --- | --- | --- |
| `-name *.c` | 198.7 (2.07/2.33/2.35) | 84.9 (2.07/2.33/2.35) | 27.2 (2.07/2.33/2.35) | 30.0 (2.07/2.33/2.35) | 10.7 (2.07/2.33/2.35) |
| `-type f` | 155.3 (2.07/2.33/2.35) | 59.4 (1.98/2.31/2.34) | 27.5 (1.98/2.31/2.34) | 31.9 (1.98/2.31/2.34) | 21.8 (1.98/2.31/2.34) |
| `-maxdepth 2 -mindepth 1` | 11.6 (1.98/2.31/2.34) | 3.7 (1.98/2.31/2.34) | 13.2 (1.98/2.31/2.34) | 3.0 (1.98/2.31/2.34) | 6.9 (1.98/2.31/2.34) |
| `-type f -size +1024c` | 408.2 (1.90/2.29/2.33) | 207.8 (1.90/2.29/2.33) | 50.6 (1.91/2.29/2.33) | 50.5 (1.91/2.29/2.33) | 22.7 (1.91/2.29/2.33) |
| `-print0` | 150.0 (1.91/2.29/2.33) | 48.5 (1.91/2.29/2.33) | 29.3 (1.91/2.29/2.33) | 34.8 (1.91/2.29/2.33) | 22.5 (1.84/2.26/2.32) |
| `-mtime 0` | 425.1 (1.84/2.26/2.32) | 209.9 (1.84/2.26/2.32) | 49.0 (1.77/2.24/2.31) | 49.7 (1.77/2.24/2.31) | 23.7 (1.77/2.24/2.31) |
| `-name *.c -o -name *.h` | 241.4 (1.77/2.24/2.31) | 118.8 (1.77/2.24/2.31) | unsupported | 30.3 (1.71/2.22/2.31) | 11.9 (1.71/2.22/2.31) |
| `-path */d1*/* -name *.rs` | 203.2 (1.71/2.22/2.31) | 97.2 (1.71/2.22/2.31) | unsupported | 35.3 (1.65/2.20/2.30) | 22.6 (1.65/2.20/2.30) |
| `-type d` | 146.5 (1.65/2.20/2.30) | 40.2 (1.65/2.20/2.30) | 23.7 (1.84/2.23/2.31) | 32.5 (1.84/2.23/2.31) | 9.2 (1.84/2.23/2.31) |
| `-empty` | 474.2 (1.84/2.23/2.31) | 302.2 (1.84/2.23/2.31) | 46.1 (1.77/2.21/2.30) | 57.5 (1.77/2.21/2.30) | 22.0 (1.77/2.21/2.30) |
| `-name *.py -newer ./README` | 235.7 (1.77/2.21/2.30) | 250.5 (1.71/2.19/2.29) | unsupported | 30.9 (1.71/2.19/2.29) | 11.7 (1.71/2.19/2.29) |
| `-regex .*\.\(c\|h\)` | 308.1 (1.65/2.17/2.29) | 90.5 (1.65/2.17/2.29) | unsupported | 40.6 (1.84/2.20/2.30) | 29.3 (1.84/2.20/2.30) |

All 12 live-versus-bfs gates pass; all 12 default rows beat M5a. bfs regex emits zero bytes versus 1558848 for GNU/ferret, so its regex comparison has different result work. Output-byte policy differences with fd/default are retained in the JSON.

## Final result and remaining policy

The final corpus ran once, with no competing benchmark or compiler. It took
1773.929 seconds at start load 1.43/2.07/2.25. Default: 52575 agree,
301 agree-unordered, 61 manually classified differences, 7368 skipped and
3 both-timeouts. Live: 66964 agree, 321 agree-unordered, 129 manually
classified differences, 7965 skipped and 6 both-timeouts. There are zero
real differences and zero errors after inspection. No harness code changed.

The full row explanations and raw artifact paths are in
`/home/dave/w/super-ferret/.ai/find-m5b-done.md`. A two-command rm binary
probe reproduced the empty-parent outcome from overlapping recursive batches.
A type-d syscall profile, load 2.74/4.82/5.31, sees 7734 statx calls for
ferret versus 8 for fd 10.4.2, and the same 15456 getdents64 calls.
Ferret spawns 15 threads versus fd's 33 clone3 calls. These observations
explain remaining metadata and concurrency costs; they are not traced timing
comparisons. No dependency, unsafe or directory-fd rewrite was needed.

At the M5b checkpoint, quit cancelled new work and waited for started actions;
the A/B termination choice remained open. M5c answers A: finish started actions
and discard later entry output under a quit latch. Forced termination and finite
quit latency are not claimed.

Release binary: `/home/dave/w/super-ferret-wt/find-m5b/target/release/ferret`.
All workspace gates and development oracles pass; 390 tests pass and four
pre-existing tests remain ignored. No manifest/lockfile changes or push.

## M5c contract update

FIND-M5C.md supersedes M5b's independent concurrent starts, worker-local ordinary
batches, action-level output records and open quit policy. Effectful starts now run in
sequence; read-only starts may overlap. Ordinary batches are shared, entry output is committed as one unit
with bounded-memory spill, and quit uses an output latch while finishing
started commands (option A). Sibling order and concurrent semicolon commands
remain as measured in M5b.
