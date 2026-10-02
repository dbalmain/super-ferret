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
