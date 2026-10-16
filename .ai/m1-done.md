# M1 done-note

2026-10-16. M1 implemented and verified. No new D-brief or provisional product
choice was needed. No benchmark ran on the geekom; no other agent/agent CLI or
push was used. No Cargo manifest/lockfile or crate dependency edge changed.
The private harness was read-only; its worktree remains clean.

## Step 0

Committed separately as `b0e04f5` (within the first 20 minutes). Every correction
in GREP.md is dated 2026-10-16:

- D72 A: automatic scans; optional trigram index; search retains refusal.
- D73 A+D: snapshot membership/absence, opt-in policy-preserving live in M6.
- **The brief misread D74. A means D47 visibility**, just as GREP.md already
  recommended. The provisional status was stale. `.ignore`/`.rgignore` remain
  a possible later feature, not scheduled.
- D75 **D**, replacing raw A: strip initial UTF-8 BOMs consistently, defer
  UTF-16 with an approved encoding difference. Added the M1 BOM/soundness scope.
- D76 C and SCAN.md supersede the old serial scan latency assumptions: shared
  parallel verification, bracketed streaming and required-run screening exist.
  Recorded measured ranges without promising them for grep line output.
- D77 C stores physical-core thread count at index creation, caps it by current
  parallelism, retains the environment override and fallback. D78 A retains
  resident-name construction on open.
- Search grep is already case-sensitive (D67 A2); the old fold-by-default claim
  was wrong. Removed obsolete external-brief/review blockers and recorded the
  authorized scratch-adapter workflow and full docs-commit gates.

## Implementation, per commit

- `b0e04f5`: the contract correction and initial done-note only.
- `87feec7`: expression-free find CatalogSource/LiveWalk constructors and
  catalog entry identity; neutral checked-reader outcomes with crawl fault
  adaptation; stable-byte retry/failed-byte regressions; extracted the existing
  streaming loop. Search retains its optional read-result interface through
  this same adapter. No copied filesystem bracket or new regex executor.
- M1 host commit (this commit): rg argv parsing for `-n -g -q`, aliases,
  clusters/attached values, `--`, `--color=never`, scan aliases, help/version;
  query-owned necessary candidates through GrepAtom and uncovered coverage;
  local Engine pin plus read-only content index; direct files, file symlinks
  and stdin; initial UTF-8 BOM removal; per-line anchors; policy override globs
  and hidden traversal; typed retry/error handling; bounded grouped plain
  output using find’s spill buffer; exact 0/1/2 precedence. Unsupported slices
  refuse before output. No grep query logging/find configuration or rg fallback.
  The dev shell now supplies pinned rg 15.2.0 via FERRET_RG from the same public
  nixpkgs revision as the private harness, without a private path dependency.

The planned seam needed one adjustment: the existing document verifier pool
carries one truth/offset per path, not line groups. Sending line output through
its row callback would reread files and lose line-scope semantics. M1 shares
its **matcher, derivation, candidate cursors, crawl reader and streaming core**;
line records have their own query profile. File groups currently execute
sequentially. M2 should generalize the existing bounded scheduling to line
groups and use the D77 setting, rather than introducing another pool. This
limitation is now explicit in GREP.md; no claim of SCAN.md’s throughput for M1.

## Owned differential cases

`crates/ferret/tests/grep.rs` contains **70 separately named differential cases:
47 positive (oracle exit 0), 23 negative (16 exit 1; 7 exit 2)**. These counts
are for the fixed macro table only, not inflated by loop iterations or private
refusals. Every case invokes the real CLI and pinned rg, compares exit status,
diagnostic presence and complete file groups. Intra-file order and duplicate
lines remain significant; only file groups may reorder.

They cover positive/negative n/g/q and interactions, long/attached/clustered
options, regex alternation/classes/intersection/Unicode/scoped flags/anchors,
empty patterns and inputs, LF/CRLF/final unterminated lines, BOM and invalid
bytes, explicit hidden/symlink/glob bypass, glob slash/globstar/braces/classes/
last-match/parent pruning, implicit/explicit stdin plus files/directories,
quiet/error precedence and syntax errors.

The integration target has **85 tests** in total. Additional real-code
regressions cover snapshot new/deleted/changed names and posting absence,
no-content-index scans, UTF-16 and binary approved differences, all deferred
switches, help/version/direct input without an index, changed candidate size
errors, config/log isolation, fatal unindexed starts, opaque-fallback pruning,
D74 visibility, large spill output, raw argv glob corner cases, read denial,
special operands and non-UTF-8 filename bytes.

Query tests exercise the actual line scanner’s stronger soundness invariant
against original indexed bytes, including BOM removal, scoped folding, invalid
bytes, classes, empty matches and line anchors. They also test tiny reads, long
lines and cancellation. Production Host::checked tests inject partial reads,
one/repeated changes, read failures, disappearance and cancellation: only a
stable successful attempt can publish output. A real Host output failure stops
work. The prerequisite tests cover neutral retries and actual crawl fault
mapping (operation, ENOENT, size/cap, change and cancellation).

## Private oracle accounting

Adapter: `/tmp/grep-m1-compat/adapter.py`. It imports the harness read-only,
redirects tools/sandbox scratch state and disables bytecode writes. It extends
bubblewrap mounts with the candidate executable, its runtime libraries and a
scratch index; the fixture tree remains read-only. Ferret’s environment omits
rg config selection, while rg retains the harness’s empty pinned config.
Reports and all private commands/content remain in `/tmp/grep-m1-compat`.
Nothing mined was copied into super-ferret. This adapter is ephemeral, not a
committed harness change.

Complete repeated audit of **all 3,332 actual argv**:

| Outcome | Commands |
| --- | ---: |
| Exact agreement | 1,006 |
| Individually explained supported difference | 13 |
| Supported S | **1,019** |
| Named deferred refusal | 2,313 |
| Total | **3,332** |
| Harness/refusal failures | **0** |
| Unexplained supported differences | **0** |

E = 1,002: 981 agrees + 12 approved differences + 9 actual `-e` rows deferred
until M2. S = E − 9 + 26 outside-E supported argv = 1,019. Outside E: 25 exact
agrees and 1 approved difference. Supported oracle exits: 30 success, 981 no
match, 8 usage errors; the owned positives address that corpus imbalance.
Each deferred row was actually run and checked for exit 2, empty stdout and a
diagnostic; the adapter records its id and every out-of-surface option.

All 13 differences are **D74 visibility, stdout only, both exit 0, no
stderr**: rg honors `.ignore`; ferret does not. No extra/missing record remains
when that one excluded name is accounted for. IDs:

`20b9d50c8f92`, `3c7f73069774`, `56e0c60fa44e`, `5a65d1136143`,
`6ce3bf0f9dc7`, `846d3365aa7f`, `888c68bebcdc`, `9072265e4cd7`,
`98993d3f97da`, `d2536096855c`, `d5c96e25e0b4`, `de3131b9c369`,
`fb7bf28b3414`.

An owned reproducer is the visibility test: `.ignore` contains `*.cache`, an
`extra.cache` file contains `lantern`, and both tools search the directory.
Ferret includes that file; rg excludes it. This is documented, not “matched.”

The named refusal families are M2 (case/fixed/OR/span/count/inversion/name
modes), M3 (files/hidden), M4 (limits/qualifiers/smart-case/pattern files/count
variants/case-sensitive), M5 (filename/line/context/heading/NUL/pretty), M6
(ignore bypass/text/depth), M7 (multiline/dotall), M8 (replace), M9 (JSON).
Permanent/current contract refusals include PCRE2, compressed search, types,
follow, encoding, sorting, custom separators, null-data, trim/iglob, engine
controls, binary, columns and column previews, and granular ignore switches.
`-p` is rg’s pretty alias (M5), despite the harness parser’s unknown-short-p
label. Per-command named flags and counts are in the scratch JSON; refused
rows are never counted as agreement.

## Differences settled during owned work

- **Matched:** malformed globs error before reads; cwd-relative slash globs;
  explicit directory-start exceptions; hidden-file versus hidden-directory
  overrides; pruned parents; braces/class syntax; comment and trailing-space
  globs; non-POSIX override classes and reversed ranges. Policy reuses the
  existing component matcher with an override syntax profile, preserving
  ordinary ignore behavior.
- **Matched:** mixed stdin’s plain prefix is `<stdin>`, implicit directory
  prefixes omit `./`, explicit `.` retains it; CR and filename bytes survive.
- **Matched:** BOM anchors, empty/no-final-line semantics, case/invalid-byte
  regexes, quiet’s read-error precedence, and no-index direct routing.
- **Documented:** UTF-16 decoding remains D75 D’s approved miss; searched NUL
  input uses the contract’s stdout binary message rather than rg’s variable
  warning bytes. Owned tests pin status/channel and the promised message.
- **Refused:** every unbuilt flag and special-file operand; invalid regex/LF
  profiles. No silently dropped or delegated flags.

## Gates

All non-WIP commits run the three exact required gates; no added test skips and
no local-TCP tests were skipped. Nix’s dirty-tree notice is tooling output;
compiler, clippy and tests have no warnings.

| Commit stage | fmt | clippy all targets, -D warnings | Tests passed | Ignored |
| --- | --- | --- | ---: | ---: |
| Docs step 0 | pass | pass | 1,031 | 13 |
| Source prerequisite | pass | pass | 1,035 | 13 |
| M1 host | pass | pass | **1,125** | **13** |

Baseline was 1,031/13: net +94 tests, no drop. Logs are
`/tmp/grep-m1-step0-tests.log`, `/tmp/grep-m1-prereq-tests.log` and
`/tmp/grep-m1-final-tests.log`. Nix needed elevated tool access to its daemon
socket. An initial step-0 run reached 1,031 unit/integration passes but rustdoc
saw source edits made during the run; restored the source and reran all gates.
The prerequisite refactor’s existing cancellation tests caught extra stop
checks and passed after their original boundaries were restored. One existing
daemon idle-exit timing assertion failed on an initial prerequisite run; full
rerun passed without skips.

## M2 and measurements elsewhere

M2 remains `-i -o -l -v -F -e -c`, byte/Unicode empty-span iteration, negative
output universes, count/quiet interactions, duplicate/current-byte caching,
shared scan planning and bounded parallel line-group execution. M3–M9 remain
as the corrected GREP.md table; PCRE2 remains a refusal.

No benchmark ran here. On the benchmark machine, build once:

```sh
nix develop --command cargo build --release -p ferret
B=target/release/ferret
# Set IDX and TREE to the frozen indexed generation and matching readable tree.
# Set PATTERN and GLOB from the private measurement workload, not this repo.
FERRET_NO_DAEMON=1 /usr/bin/time -v "$B" --index "$IDX" grep -n -g "$GLOB" "$PATTERN" "$TREE" > /dev/null
/usr/bin/time -v rg --color=never -n -g "$GLOB" "$PATTERN" "$TREE" > /dev/null
FERRET_NO_DAEMON=1 /usr/bin/time -v "$B" --index "$IDX" grep -q "$PATTERN" "$TREE"
/usr/bin/time -v rg --color=never -q "$PATTERN" "$TREE"
```

Measure rare literal/alternation, short/All/dense patterns and zero-result
queries; with/without n/g/q, explicit files, stdin, BOM and long lines; high
output volume (including spill) versus quiet. Compare equivalent universes
first. Collect warm/cold median/p90, first output/total time, process CPU, RSS,
candidate/read bytes and metadata calls, postings/enumeration/setup/matching/
output shares. No-trigram comparison uses a separately prepared catalog-only
index, not destructive edits to the measured index. Capture output to verify
counts/groups before timing to /dev/null. Include resident versus standalone
when a grep resident host exists; M1 is local. Keep cold-cache control on that
machine’s existing benchmark protocol; do not claim warm hits as cold runs.
At synthetic 10M names, separate residency (D78), candidate/name enumeration
and actual content/output work. Report paths/patterns/content only privately.
