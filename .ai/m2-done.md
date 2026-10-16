# Grep M2 done-note

Started 2026-10-17 in grep-m2, branch wt/grep-m2 at f4a5571.
Scope: -i -o -l -v -F -e -c, shared iteration/planning, bounded parallel file
output using existing scan workers, and real host cancellation.

Implementation, owned cases, full accounting, cancellation/cache regressions
and gates are complete. D79 remains open: the recommended per-line treatment
of empty absolute anchors is provisional. No benchmark runs here.

## Commit 1 — shared byte-span iteration (f0c172b)

Regex and Search expose one iterator for nonoverlapping spans. Assertions keep
the whole haystack; empty spans advance one byte, including inside UTF-8,
and an empty at the previous consuming match’s end is suppressed. Three
owned unit tests cover byte/Unicode empties, consuming Unicode, adjacent-empty
suppression, terminal empties, anchors, word boundaries and termination.

Gates: fmt and all-target clippy green with zero compiler warnings; workspace
**1135 passed / 13 ignored**, up three from M1. No extra sandbox/TCP skips.
Full log: `/tmp/grep-m2-iteration-gates.log`. Initial checks caught and corrected
a test range-literal lint and an escaped word-boundary fixture typo.

## Scheduler seam audit

The existing verifier pool is a bounded batch/ring scheduler whose current
payload is document truth and first-match offset. Its row callback cannot emit
line groups without rereading. Extend that same scheduler with a group payload
and one-file batches, retaining its spawn, cancellation, ring and byte-budget
mechanisms. Search’s existing payload/order must stay covered by its tests.
This is an extension of the existing pool, not a second concurrent pool.

## Work sequence

The following commits completed the profile, planning, grouped scheduling,
cancellation and cache work. The private harness remained read-only throughout;
adapters and raw reports stayed under /tmp. No corpus rows enter this repo.

## Commit 2 — resolved M2 profile and line output (2fe3df0)

Adds -i/-o/-l/-v/-F/-e/-c, long aliases, attached values, clustered -e,
repeated pattern OR, and rg’s supported fixed/invert negations. Compilation,
escaping and trigram derivation share the resolved pattern. Name/count modes
override one another in argv order; quiet wins independently. Counts with -o
count spans, inversion counts/emits selected nonmatching lines. Names and quiet
stop scanning a file at its first selected line.

Universe::Candidates covers positive lines, names and nonzero counts; inversion
requires Universe::Searchable. Even -cv reads posting-negative files. Two
owned unit tests cover resolved-profile soundness over original BOM/invalid/
Unicode bytes and output-universe overrides.

48 new owned differential cases assert status as well as byte/group agreement:
37 positive (exit 0), 8 no-hit (exit 1), 3 usage/pattern errors (exit 2).
Together with M1’s 70 named differential cases (47/16/7), that is **118 named
cumulative cases: 84 positive, 24 no-hit, 10 errors**. Additional M1/R1–R3
integration regressions remain. Fixed the owned comparer to recognize newline-
terminated name records as separate file groups while retaining line order.

Profile gates: fmt and all-target clippy green, zero compiler warnings;
workspace **1185 passed / 13 ignored**. No extra skips. Log:
`/tmp/grep-m2-profile-gates.log`.

## Full accounting at the profile checkpoint

Read-only private harness, pinned rg 15.2.0. Scratch adapter, generated tree,
index, candidate executable and raw reports all live in `/tmp/grep-m2-compat`,
not either repo. Adapter adds the M2 target absent from the private stub and
preserves the harness sandbox. No private source rows or corpus content are
copied into owned fixtures.

All **3332** commands were exercised: **1626 exact agrees + 512 accepted D74
visibility differences = S 2138**; **2138 + 1194 named refusals = 3332**.
Zero execution failures, zero refusal failures, no unclassified supported
differences. **E 2138 - 0 within-E deferrals + 0 support outside E = S 2138.**
Every difference is stdout only, exit 0 on both sides, and consists solely of
a ferret-visible file excluded by rg’s .ignore. No missing oracle records.
Owned reproducer remains `ignore_visibility_is_the_documented_d74_difference`,
using an owned .ignore rule and matching extra.cache file.

[Per-command accounting](m2-accounting.tsv) records all 3332 ids, E/S membership,
exact/accepted/deferred result, each out-of-surface flag’s planned slice (or
permanent refusal), and differing channels. It contains ids and classifications
only, no mined argv/content. Final scheduler/cache wiring will repeat this audit.

## New D-brief / provisional behavior

D79 in DECISIONS.md records observed rg cross-line artefacts for zero-width
absolute anchors under -o/-co. Existing GREP.md promised per-line spans; M2
uses recommended option A provisionally and asked Dave asynchronously. The
own isolated probe uses red lines; no private mined rows are reproduced. This
case is outside the observed 2138 supported audit rows and is not counted as
an approved difference there. All other probed M2 output interactions matched.

Parallel host scheduling, cancellation, cache regressions and measurement
commands remain for subsequent commits.

## Commit 3 — shared bounded scheduling for file groups (11044f8)

The existing verification Pool, Batch and Slot now carry either document rows
or completed file groups. Search keeps its original batch sizing, ordered
ledger/cache behavior, ring, byte budget, spawn path and stop guards. Groups
use that same ring and budget with one-file batches. Workers claim from a
streaming producer under a short producer lock, avoiding an unbounded path
queue; EOF and cancellation occupy positions so a waiting emitter always wakes.
Only the calling thread publishes a group. This extends the existing pool; it
does not start a second pool alongside it.

Three owned scheduler tests force simultaneous workers with a barrier, verify
whole payloads/order, cancel work in progress on emission Break, force the
finished ring to its exact bound without sleeps, and exercise spawn-failure
sequential fallback. All use the actual scheduler. Existing search verifier
and cache/cancellation tests remain unchanged and pass.

Gates: fmt/all-target clippy green, zero compiler warnings; workspace
**1188 passed / 13 ignored**, no extra skips.
Log: `/tmp/grep-m2-pool-gates.log`. Host wiring/cache/CLI cancellation tests
remain, as does the final full compatibility audit.

## Commit 4 — parallel host, cancellation and catalog-version records

The grep host now streams file tasks through the same query scheduler. Workers
own catalog candidate selection/live fallback, a fork of the existing checked
reader, matching and bounded per-file rendering. Only the caller publishes each
whole group. This keeps ordered emission (allowed by the unspecified-order
contract), bounded input claims and the existing output spill buffer. Stdin is
an operand barrier so no worker can block waiting on its producer. Help includes
the M2 switches. D77 resolves the worker count; explicit-only inputs open no
index and cap the override/default by operand count.

The plan’s Searchable universe bypasses positive postings entirely under -v,
including -cv and inverted names. No posting-negative content is accepted as an
inverted line without reading it. Candidate planning, checked/direct readers,
retry acceptance, line/span scanning, rendering and spill replay all receive
real cancellation predicates. A worker’s stable quiet hit cancels an earlier
active read without waiting for ordered emission. Fatal spill/output faults
retain their original error, stop the producer/workers and exit 2. Writers use
ConnectionAborted for cancellation: Interrupted would make write_all retry
forever. Emission/rendering checks cancellation at most every 64 KiB.

Only duplicated catalogued documents capture prefix-free line/span records.
Each alias must independently stat as Catalogued before reuse; Current results
are read and kept local, never promoted to DocId facts. The shared record cache
has a 16 MiB accounting budget, entries stop capture at 64 KiB, and large
records simply use the ordinary read/spill path. Container overhead is bounded
in addition to that accounting. Cache locks never cover I/O or matching;
concurrent misses may independently read the same bytes. Input reservations use
the existing 64 MiB scheduler budget; each worker drops an input buffer above
1 MiB after work. As before, current growth and uncapped direct long lines can
exceed their initial-size reservation. Output/spill handle counts are bounded
by workers plus the 16-slots-per-worker ring; spill bytes follow required output.

Eight new tests: a real group-host quiet hit against a deterministically blocked
reader; real scanner cancellation before its next read; real spill replay
cancellation after one chunk; duplicate-cache/fresh-stat/current-byte behavior
and oversized cache exclusion; actual CLI whole spilled groups at 1/4 threads;
three generations of duplicate/current aliases with line/count/inverted/name
comparisons at 1/4 threads; real CLI quiet/closed-stdout cancellation while a
stdin producer stays open; and quiet-before-error diagnostics. Existing M1 and
all three review rounds’ regular-file/procfs/fallback regressions remain green.
No sleeps in the new cancellation/scheduler synchronization; timeouts only guard
and clean up a regressed cancellation path.

A newly owned positive case pins a cheap M1 difference found here: on a quiet
hit followed by a missing explicit file, rg still reports the missing operand.
Ferret now reports resolved metadata failures before searching, while the hit
still wins exit 0. Reproduced first in `/tmp/grep-m2-quiet-error-repro.log`.
Named differential totals are now **119: 85 positive, 24 no-hit, 10 errors**.
The additional integration matrices are not inflated into that denominator.

Focused revert checks all failed in the real code as required:

- Scanner predicate replaced by false: the scan continued and hit a cancelled
  renderer, instead of stopping before another read.
- Spill cancellation wrapper removed: replay completed instead of stopping
  after one 64 KiB chunk.
- Quiet-hit signal removed: the earlier reader stayed blocked until the test’s
  timeout released it for cleanup.
- Cache-hit stat removed: the changed alias incorrectly received cached red
  records instead of its own blue bytes.

Each change was restored before gates. Logs: `/tmp/grep-m2-revert-scan.log`,
`/tmp/grep-m2-revert-spill.log`, `/tmp/grep-m2-revert-quiet.log`,
`/tmp/grep-m2-revert-cache-stat.log`; summary `/tmp/grep-m2-reverts.log`.

Final host gates: fmt and all-target clippy green, zero compiler warnings;
workspace **1196 passed / 13 ignored**, up 64 from M1. No additional sandbox or
TCP skips. Known watch idle-exit/find_json pipe tests passed this run. Log:
`/tmp/grep-m2-host-gates.log`.

## Final compatibility accounting

Repeated every command against the final parallel host: **3332 = 1626 exact +
512 accepted D74 visibility + 1194 named deferred/refused**. **S = 2138**;
**E = 2138 - 0 within-E deferrals + 0 outside-E support = S 2138**.
No execution failures, refusal failures or unexplained supported differences.
Every accepted difference remains stdout only, both exits 0, solely a file
excluded by rg’s .ignore; there are no missing oracle records. The scratch
adapter now stores normalized difference records rather than full duplicate
captures, without changing the harness comparator. Final scratch log:
`/tmp/grep-m2-compat/final-run.log`; report `summary.json` in that directory.
[All 3332 command ids/classifications](m2-accounting.tsv) were regenerated from
this final audit. The named refusal flags and milestone/permanent classification
are per row, not a deferred-combinations bucket.

## Measurements for the benchmark machine (not run here)

Inside `nix develop`, build once. Set IDX/TREE to the frozen indexed generation
and matching readable tree, and set patterns from the private workload. First
compare whole output groups/counts on equal searchable universes; prepare any
D74 policy adjustment outside the timed region. These commands compare 1 and N
workers; N is supplied explicitly and FERRET_SEARCH_THREADS is the same D77
override as the product uses. rg is the dev shell’s pinned FERRET_RG.

```sh
cargo build --release -p ferret --bin ferret
FERRET_BIN=target/release/ferret
GREP_THREADS_N=${GREP_THREADS_N:?set the N-thread comparison count}
for threads in 1 "$GREP_THREADS_N"; do
  for mode in lines spans names counts span_counts inverted quiet; do
    case "$mode" in
      lines) set -- -n ;;
      spans) set -- -n -o ;;
      names) set -- -l ;;
      counts) set -- -c ;;
      span_counts) set -- -c -o ;;
      inverted) set -- -v ;;
      quiet) set -- -q ;;
    esac
    FERRET_NO_DAEMON=1 FERRET_SEARCH_THREADS="$threads"       /usr/bin/time -v "$FERRET_BIN" --index "$IDX" grep "$@" -- "$PATTERN" "$TREE" > /dev/null
    /usr/bin/time -v "$FERRET_RG" --threads "$threads" --color=never "$@" -- "$PATTERN" "$TREE" > /dev/null
  done
  FERRET_NO_DAEMON=1 FERRET_SEARCH_THREADS="$threads"     /usr/bin/time -v "$FERRET_BIN" --index "$IDX" grep -Fi -- "$LITERAL" "$TREE" > /dev/null
  /usr/bin/time -v "$FERRET_RG" --threads "$threads" --color=never -Fi -- "$LITERAL" "$TREE" > /dev/null
  FERRET_NO_DAEMON=1 FERRET_SEARCH_THREADS="$threads"     /usr/bin/time -v "$FERRET_BIN" --index "$IDX" grep -e "$PATTERN" -e "$PATTERN2" -- "$TREE" > /dev/null
  /usr/bin/time -v "$FERRET_RG" --threads "$threads" --color=never -e "$PATTERN" -e "$PATTERN2" -- "$TREE" > /dev/null
done
```

Run the existing benchmark machine’s warm/cold protocol, alternating tool order;
report median/p90, first-group and total time, CPU, RSS, bytes read, metadata
calls, selected line/span/name counts, and output/spill volume. Cover rare,
short/All/dense and zero-result patterns; inversion; Unicode folding and byte/
Unicode empties; duplicates and copies changed since indexing; large spilled
output, stdin/explicit files, quiet hits and closed-consumer cancellation. Also
compare stored search_threads with no override; record affinity/available cores.
Drain real pipes as well as /dev/null. Preserve snapshot/policy/BOM equivalence
before attributing speed differences. No resident grep host was added.

## Corrections / remaining decisions

The row/first-offset callback was not a line-output seam; extending the existing
ring’s payload and task claim mode fit better than rereading through that
callback. GREP.md now records that shape and precise output override rules.
The test comparer needed name-record grouping as well as line-record grouping.
D79’s observed empty-absolute-anchor artefact contradicts the written per-line
rule; recommended A remains provisional pending Dave’s answer. It affected no
rows in the full supported audit and is not counted as an approved exception.
No other M2 implementation work remains; later flags remain the named slices
in GREP.md. No manifest/dependency edits, pushes, other agent CLI launches or
benchmarks. Work was confined to grep-m2; grep-m1 was left untouched.
