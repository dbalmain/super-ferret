# REVIEW.md — super-ferret review notes

Repo-specific review guidance, accumulated by the review-craft skill. The skill
reads **Standing checks** before every review and appends to the **Findings
log** when a review uncovers a durable lesson. Keep entries terse.

## Standing checks

Mandatory extra criteria every review applies here (promoted from recurring
findings). Each should name the guard that will eventually retire it.

- (none yet)

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
  `crates/` keeps it from coming back. Not applied yet.

### 2026-10-03 — two copies of the per-entry evaluation loop

- **What:** `Plan::run` (find/mod.rs) and `Task::step` plus the `run_parallel`
  prologue (find/parallel.rs) repeat the same per-entry evaluation, and they
  have already drifted. A failed `-execdir` directory change breaks out of one
  loop but only sets quit in the other.
- **Guard:** structural. Keep one `evaluate_entry` and one `prepare` (checks for
  unsupported features, warnings, reference resolution), called by both paths,
  or drive the sequential path through `Task`. Not applied yet.

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
