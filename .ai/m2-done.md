# Grep M2 done-note

Started 2026-10-17 in grep-m2, branch wt/grep-m2 at f4a5571.
Scope: -i -o -l -v -F -e -c, shared iteration/planning, bounded parallel file
output using existing scan workers, and real host cancellation.

Contract/harness reading and seam audit in progress. No benchmark runs here.

## Commit 1 — shared byte-span iteration

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

## Remaining

M2 argv/profile and owned differential cases; output-universe planning;
parallel groups, cancellation and duplicate/current-byte caching; full 3332
accounting and measurement commands. Private harness remains read-only; any
adapter and reports will be under /tmp. No corpus rows enter this repository.

## Commit 2 — resolved M2 profile and line output

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

## Commit 3 — shared bounded scheduling for file groups

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
