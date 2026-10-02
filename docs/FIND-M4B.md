# Milestone 4b: default find over the catalog

Implementation plan (2026-10-03):

- Add CatalogSource behind EntrySource; share traversal with LiveWalk, retain
  readdir order, prune, depth and directory-local batch boundaries.
- Resolve explicit starts against the catalog; ignored/opaque starts walk live.
  Visible recursion excludes ignored entries. Metadata remains a lazy live lstat.
- Refuse missing/incompatible catalogs and unresolved starts with status 1 and
  actionable advice; do not silently drop into unrestricted traversal.
- Add a dependency-free config file at $XDG_CONFIG_HOME/ferret/config, with
  find_no_ignore = true/false. Explicit -I bypasses config and the index.
- Document ignored deletion success; test default/live equivalence, ignored
  starts and references, opaque handoff, emptiness and live metadata.
- Run the formatter, workspace clippy/tests, seed harness and serial timing
  measurements; finish with a release build.

The catalog sorts children by name, so it cannot supply live filesystem order.
Directory name listings are needed to retain -quit and -exec batching behavior.
This cost will be measured before claiming the speed gate.
