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
