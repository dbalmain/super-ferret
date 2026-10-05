# S1b — One resident engine, batch mode and the daemon

M0 design, 2026-10-05. Code baseline: `75dcd49`, after the S1+, find and main
merges into `wt/s1b`. This document specifies work; it does not describe a
daemon that already exists. The S1+ producer and find evaluator are built.

D46 C plus batch and D49 A bind the hosts. D51 A binds writer pauses; D52 B
binds handles; D53 A keeps semantic validation; D54 B adds interned names,
row postings and a term index while keeping BFS. D55 remains open. Its
recommendation is not an answer and this slice does not adopt it.

## Engine

The library target of `ferret` owns `Engine`, the coordinator of catalog,
query execution and optional writer refresh. Both hosts use it. This fits the
existing dependency graph and `crates/ferret/tests/layering.rs`: `ferret`
already depends on query, crawl and catalog. The daemon is a second binary in
that package, not a new `ferret-daemon` crate. Query semantics remain in
`ferret-query`; filesystem observation remains in `ferret-crawl`; publication
remains in `ferret-catalog`. No new crate or dependency edge is needed for the
engine.

Open and validate names and inode columns once, using the same resident open
in batch and daemon. Catalog sections are checked owned buffers today, not
mmaps. Future content indexes are mapped by their owning crate when S2 builds
them; `ferret-index` currently has no implemented index to map. The engine
does not add a catalog mmap or a trusted-reader shortcut.

A query pins one immutable effective `Catalog`: its snapshot plus committed
overlay prefix. A publication installs another view for new queries. Queries
do not hold the writer lock. Same-epoch refresh adopts the returned checked
view without replay; checkpoint publication replaces the epoch and rebuilds
derived caches. Every numeric scope checks the complete generation before
dereferencing an id. Queued work retains locators, not unqualified numbers.

## Batch host

`ferret batch` opens one engine, consumes JSON lines and returns one tagged
block per query. Requests distinguish search atoms from untouched find argv;
results include begin, streamed rows/output and end with the native command's
exit status. Exact byte strings have a base64 representation. The protocol
and client action boundary are specified below as the design is completed.

## Daemon host

`ferretd` serves a user-owned Unix socket, spawns on first use, and keeps the
engine resident. `FERRET_NO_DAEMON` and prohibited background operation use
the same engine in process. Event ingestion stays responsive while a single
writer coalesces hints and calls `ferret_crawl::refresh`. Lost events request a
complete backstop; cookies never prove a rename.

Compaction remains at an idle writer boundary under the lock. Queries keep
their pinned generation. S1+ R1 measured a 22.451 s median whole pause, with a
16.949–23.131 s range. Queue service and end-to-end freshness have not been
measured. The oversized-change rebuild reached 2.57 GiB faultless and
2.71–2.78 GiB faulted; those are workload peaks, not memory ceilings.

## Cost model and build slices

The reference fixture has 10,448,739 names, not exactly 10M. S1+ M7 measured
name-only cold opens at 324.22 / 560.63 / 806.94 ms for base / 1% / 2% overlays.
Those are not full engine opens. M6 measured writer setup at 3.27 s clean and
4.30 s near 1%; clean setup current RSS was 761.71 MiB. D54's separate BFS
prototype measured 18.6 B/row on `$HOME` and 9.3 B/row on nix, not the complete
engine. A complete estimate and measurement plan follow in the final design.

Land the engine and batch host first, then D54, the socket host/client,
watching and backstops, and scheduling/budget validation. Each slice names its
files, real API tests and measurements in the completed milestone table.

## Open questions

D55 affects timestamp storage, time-query block skipping, resident bytes and
carry-over semantics. Until answered, preserve full stored timestamps and the
current carry-over key. Find's client action boundary needs a costed brief:
using the resident daemon for selective action queries is faster than another
full load, but client execution is simpler to keep correct with cwd, tty,
environment, observed parent handles and concurrent actions.
