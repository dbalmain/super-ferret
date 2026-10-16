# M1 done-note

Started 2026-10-16. Work in progress.

## Step 0

Updated GREP.md on 2026-10-16: D72 A and D73 A+D confirmed; D74 A is D47 visibility, so the brief misread that choice (only the provisional status was stale). D75 D strips UTF-8 BOMs everywhere and defers UTF-16, replacing raw A. Recorded D76 C shared threaded scan, SCAN.md’s measured improvements, D77 C stored thread setting and D78 A resident-on-open. Removed obsolete external-brief/review blockers and documented scratch-adapter authorization. Also corrected the claim that search grep folds by default: D67 A2 and the real engine are case-sensitive. No new D-brief needed so far.

## Implementation and commits

Pending.

## Owned differential cases and observed differences

Pending.

## Gates

Step 0: formatter and clippy passed; workspace tests 1031 passed, 13 ignored (baseline unchanged), no skips. Nix needs elevated tool access to its daemon socket. An initial test run reached all 1031 unit/integration passes but rustdoc saw implementation edits made during the run; restored the source and reran all gates successfully before committing docs.

## M2 and benchmarks

Pending contract review. No benchmarks run on this machine.

## Brief and contract corrections

Pending decision-log verification.
