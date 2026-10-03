# Parallel find idioms

M5c preserves free sibling order and concurrent `-exec … ;`. Read-only starts
may overlap; effectful starts, and starts of an expression with `-quit`, complete
in operand order. Start scheduling reuses M5a's `has_actions`: all
exec/execdir/ok/okdir variants, delete and all file-output primaries (`-fprint`,
`-fprint0`, `-fprintf`, `-fls`). `-quit` also sequences starts: GNU quits inside
the first start that reaches it, so a concurrent later start must not report a
missing path and exit 1 first (`find src missing -print -quit`, corpus
`77e2ba5a5ff3`). Stdout print/printf/ls alone do not sequence starts.

Each ordinary `-exec … {} +` action owns one shared argument batch across all
workers and starts. Full batches detach under the lock, then spawn and wait
outside it while collection continues. Workers stage up to 32 paths and merge
at 4 KiB of path bytes; an oversized path merges immediately. Only the shared
batch partitions argv. Task completion merges every partial stage, including
when quit latches; the shared remainder runs after all workers join. Execdir
retains directory-local boundaries. Full-batch processes may overlap (F11 A).

An entry commits all its output together. Child stdout drains while the command
runs; output above 64 KiB per stream spills to a private unlinked temporary file.
Memory is bounded per stream and worker; temporary storage grows with output.
Quit commits its winning entry under the output lock and discards entries that
finish later. Already started commands finish, and collected batches flush at
exit (DECISIONS option A).

## Validation

Nine CLI tests drive both catalog and live walkers, including repeated header
and contents grouping, one wc total, one quit winner, started-child completion,
large output spill, the same file/stdout quit winner, GNU overlapping-start
deletion, duplicate read-only starts, and full batches plus the final remainder
above 128 KiB. Focused action tests permit collection during child execution;
real-walker donation tests cover every effectful primary.

Formatter, workspace clippy with `-D warnings`, workspace tests (401 passed,
four pre-existing ignored) and release build pass. The ignored constrained-stack
GNU batch probe, parallel GNU expression oracle and catalog/live expression
self-check also pass. No tests removed, dependency changes or unsafe.

## Measured choices

Dave corrected the initial speed brief: holding the batch lock while spawning
and waiting was a process-execution cost. After moving execution outside the
lock, batch-all remained 144.411/188.290 ms default/live against M5b's
75.811/49.490 ms. Bounded staging was therefore authorized and implemented;
its follow-up measurement recovered M5b throughput. Raw initial-correction
samples are `m5c/revised-probes-before-staging.json`.

A narrow-root donation-size guard was tried and removed. Sequential effectful
starts took 148.730 ms live with it versus 135.290 ms before it. A syscall
profile with the guard shows one pool startup (15 clone3 calls), 345 futex,
7746 statx, 7727 directory open and 15454 getdents64 calls. The pool already
persists across starts; avoiding sibling barriers leaves substantial directory
I/O serial and did not improve this probe. No further scheduler policy is kept.
The exact remaining cost is reported, rather than assigning it all to lock
contention or pool drainage. Read-only starts recover the old parallel behavior.

Dave accepts the initial 0.4–2.3% default-mode timing differences as noise.
Final fifteen-sample targeted probes and twelve timing rows are complete; seed
is green (247 default and 315 live agrees, no differences/errors). One fresh
full corpus is running on the frozen release binary. Final loads, classifications
and raw artifact links will be recorded here and in
`/home/dave/w/super-ferret/.ai/find-m5c-done.md`.

## Revised warm timing

Fifteen warm samples per cell, interleaved forward/reverse target order.
Start load 3.108/2.387/2.659, 32 logical CPUs; no competing own compiler or benchmark.
Cells are milliseconds (load 1/5/15 at the median sample); fd 10.4.2.

| Query | GNU | bfs | fd | ferret -I | ferret | M5b default |
| --- | --- | --- | --- | --- | --- | --- |
| `-name *.c` | 204.061 (3.11/2.39/2.66) | 80.203 (3.11/2.39/2.66) | 27.234 (3.11/2.39/2.66) | 31.701 (3.11/2.39/2.66) | 10.925 (3.11/2.39/2.66) | 10.808 (3.11/2.39/2.66) |
| `-type f` | 163.122 (3.74/2.53/2.70) | 53.899 (3.74/2.53/2.70) | 28.556 (3.92/2.59/2.72) | 32.650 (3.92/2.59/2.72) | 21.073 (3.74/2.53/2.70) | 20.602 (3.92/2.59/2.72) |
| `-maxdepth 2 -mindepth 1` | 14.320 (3.92/2.59/2.72) | 3.815 (3.92/2.59/2.72) | 12.398 (3.92/2.59/2.72) | 3.357 (3.92/2.59/2.72) | 7.355 (3.92/2.59/2.72) | 7.493 (3.92/2.59/2.72) |
| `-type f -size +1024c` | 426.948 (3.92/2.59/2.72) | 142.608 (3.85/2.59/2.72) | 53.655 (4.34/2.72/2.76) | 46.979 (3.85/2.59/2.72) | 22.940 (3.85/2.59/2.72) | 22.775 (3.85/2.59/2.72) |
| `-print0` | 150.435 (4.34/2.72/2.76) | 43.480 (4.23/2.72/2.77) | 29.976 (4.23/2.72/2.77) | 32.417 (4.23/2.72/2.77) | 22.594 (4.23/2.72/2.77) | 22.427 (4.23/2.72/2.77) |
| `-mtime 0` | 438.028 (3.95/2.74/2.77) | 163.259 (3.95/2.74/2.77) | 49.482 (3.95/2.74/2.77) | 51.110 (4.13/2.73/2.77) | 23.347 (4.13/2.73/2.77) | 22.968 (4.13/2.73/2.77) |
| `-name *.c -o -name *.h` | 244.356 (3.95/2.76/2.78) | 116.896 (3.95/2.74/2.77) | unsupported | 35.230 (3.95/2.74/2.77) | 12.448 (3.95/2.74/2.77) | 12.237 (3.95/2.76/2.78) |
| `-path */d1*/* -name *.rs` | 205.189 (3.71/2.73/2.77) | 90.709 (3.71/2.73/2.77) | unsupported | 29.920 (3.71/2.73/2.77) | 22.556 (3.95/2.76/2.78) | 22.414 (3.71/2.73/2.77) |
| `-type d` | 147.352 (3.58/2.72/2.76) | 36.051 (3.58/2.72/2.76) | 23.764 (3.58/2.72/2.76) | 31.470 (3.58/2.72/2.76) | 9.629 (3.58/2.72/2.76) | 9.485 (3.58/2.72/2.76) |
| `-empty` | 497.313 (3.93/2.80/2.79) | 248.014 (3.78/2.79/2.79) | 47.467 (3.93/2.80/2.79) | 55.022 (3.55/2.76/2.78) | 22.085 (3.93/2.80/2.79) | 22.324 (3.78/2.79/2.79) |
| `-name *.py -newer ./README` | 243.451 (3.35/2.73/2.77) | 193.020 (3.55/2.76/2.78) | unsupported | 35.944 (3.55/2.76/2.78) | 11.805 (3.35/2.73/2.77) | 11.735 (3.35/2.73/2.77) |
| `-regex .*\.\(c\|h\)` | 310.941 (3.06/2.69/2.75) | 88.624 (3.06/2.69/2.75) | unsupported | 35.910 (3.06/2.69/2.75) | 28.905 (3.06/2.69/2.75) | 29.389 (3.24/2.72/2.76) |

All twelve inherited live-versus-default-bfs comparisons pass. Default medians
range from 1.84% faster to 2.29% slower than paired M5b, within the timing noise
Dave accepted. The default shallow row remains above bfs, as in M5b; this
inherited gate compares live ferret against default bfs. bfs regex emits no
matches, so that cell does different result work.

Raw samples: `/home/dave/w/find-compat/.scratch/ferret-impl/m5c/revised-timing.json`.

## Revised targeted probes

Fifteen warm samples per cell, milliseconds. Start load 2.707/1.930/2.532.

| Probe | M5b default | M5c default | M5b live | M5c live |
| --- | ---: | ---: | ---: | ---: |
| 368 read-only starts | 17.843 | 17.832 | 27.251 | 27.766 |
| 368 effectful starts, unreachable exec | 31.915 | 39.398 | 28.152 | 133.455 |
| name-selected exec+ | 24.121 | 21.998 | 46.694 | 36.760 |
| all-file exec+ | 74.055 | 76.600 | 47.793 | 54.168 |

All-file batching retains a 3.4% default / 13.3% live delta in this sample after
staging, far below the pre-staging contention cost. Read-only many-starts
recovers M5b behavior; effectful sequencing has a real remaining cost.
Raw samples and per-cell loads: `m5c/revised-probes.json` in ferret-impl scratch.
