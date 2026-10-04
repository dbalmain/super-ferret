# REVIEW.md — super-ferret review notes

Repo-specific review guidance, accumulated by the review-craft skill. The skill
reads **Standing checks** before every review and appends to the **Findings
log** when a review uncovers a durable lesson. Keep entries terse.

## Standing checks

Mandatory extra criteria every review applies here (promoted from recurring
findings). Each should name the guard that will eventually retire it.

- **Live access after observation goes through the observed handle.** Any
  syscall on an entry the walk has already observed (delete, stat, readlink,
  access, `-empty`, descent, `-execdir`'s chdir) must be relative to the
  retained directory handle, never a rebuilt pathname. Ask: if an earlier
  `-exec` renames or replaces an ancestor here, what does this call reach?
  Seen in four separate places across three review rounds (2026-10-03/04).
  Guard: the `find_review.rs` mutation tests; a lint-style test that rejects
  pathname `std::fs` calls in `find/walk.rs` and `find/action.rs` would retire
  this check. Not applied yet.
- **All output commits through one transaction.** A new output primitive,
  sink, batch kind or size class must use the shared commit, through to the
  final flush. Two rounds found records split by a path that bypassed it
  (batches, then short records). Guard: the deterministic CLI interleave tests
  in `find_review.rs`.

## Findings log

### 2026-10-03 — gate depends on a GNU find the flake does not provide

- **What:** four test files hardcode
  `/nix/store/i9wgqa0l88aprvpwfaq5hkfa6pklhlv0-findutils-4.11.0/bin/find`.
  `flake.nix` does not provide findutils, so `cargo test --workspace` passes
  only while find-compat's devshell keeps that path alive. A garbage collection
  or another machine breaks the gate with no code change.
- **Guard:** add findutils to the flake devshell, export its path as an
  environment variable, and read it in one shared test helper. A
  `layering.rs`-style test that fails on any literal `/nix/store/` path under
  `crates/` keeps it from coming back. Applied in R1: `FERRET_GNU_FIND` and
  `tests/support/gnu_find.rs`. The literal-path test is not applied yet.

### 2026-10-03 — two copies of the per-entry evaluation loop

- **What:** `Plan::run` (find/mod.rs) and `Task::step` plus the `run_parallel`
  prologue (find/parallel.rs) repeat the same per-entry evaluation, and they
  have already drifted. A failed `-execdir` directory change breaks out of one
  loop but only sets quit in the other.
- **Guard:** structural. Keep one `evaluate_entry` and one `prepare` (checks for
  unsupported features, warnings, reference resolution), called by both paths,
  or drive the sequential path through `Task`. Applied in R1: `Plan::prepare`,
  with `Plan::run` delegating to the `Task` loop.

### 2026-10-03 — relaxing an ordering in a brief dropped an observable

- **What:** the M5c follow-up brief let start operands run concurrently whenever
  the expression has no actions. That missed `-quit`: a later missing start then
  reported ENOENT and exit 1 before GNU's quit. The full corpus caught it as
  77e2ba5a5ff3.
- **Why missed:** the brief listed side effects on the tree and files, but not
  order-observable outputs: exit status, stderr, and which entry `-quit`
  selects.
- **Guard:** the test
  `quit_in_an_early_start_never_reports_a_later_missing_start` (applied,
  98d4b4c). When a brief relaxes an order, it should list every output that
  order can change.

### 2026-10-04 — folding callers into a shared helper changed one caller's input

- **What:** Astra round 2 asked for one observed-path policy. The shared
  `with_observed_path` rebuilt each operand from `name()`, which strips a
  trailing slash. So `find -I victim/ -delete` deleted a regular file that GNU
  refuses with ENOTDIR. The unification fixed four bugs and introduced this
  one, and none of its tests used an operand with a trailing slash.
- **Why missed:** reviewers checked that each folded path now behaved like the
  shared one, not that the shared one preserved each caller's input form.
- **Guard:** when paths are folded into one helper, test the helper with every
  input form a caller could pass. For find operands that means a trailing
  slash, a repeated slash, `.`, `..`, and a dangling symlink. Applied as
  `find_review.rs` tests for the trailing-slash case.

### 2026-10-04 — a regression test that passed on the unfixed code

- **What:** the first test for the stale first-child stat printed `%p` only.
  Nothing forced a metadata call, so the test passed on the bug. Adding `%s`
  made it fail before the fix.
- **Guard:** every fix's test is run against the pre-fix tree. Briefs already
  ask for this; keep asking, because it caught this one.

### 2026-10-04 — final-set validation rescanned its own records per inode

- **What:** M4's root-liveness test scanned all changed records once per inode
  whose name count changed, making large deletions quadratic in dirty rows.
- **Guard:** derive the final root set once, then use membership during inode
  retirement. Existing real-crawl root-retirement and materialised-checkpoint
  oracle tests cover the resulting behavior. Applied in M4.

### 2026-10-04 — an equal-row fast path can hide a later alias conflict

- **What:** M4b can compact an equal single-link observation before a new alias
  appears in another listing. Sorting only full observations would then miss
  the version disagreement and publish Hashed instead of shared Fault.
- **Guard:** expand compact observations of every inode appearing in the alias
  residue before grouping. The real-crawl test
  `compact_equal_observation_joins_a_new_alias_before_conflict_resolution` fails
  Hashed vs Fault when expansion is disabled. Applied in M4b.

### 2026-10-05 — retaining a moved scope also needs a live incoming path

- **What:** protecting an old directory ID after it moved kept its incoming edge,
  while the former parent could still be swept. Checking inode identity alone
  did not prove that the old namespace occurrence stayed anchored.
- **Guard:** check the complete old parent/name chain before selecting a scope;
  a relocated occurrence protects its checked owner root. The real-crawl test
  `a_fault_after_a_directory_move_retains_a_live_old_ancestor` covers surviving,
  removed and newly created parents, plus disk replay and successful recovery.
  Its pre-fix probe failed. Applied in S1+ M5.
