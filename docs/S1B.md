# S1b — One resident engine, batch mode and the daemon

M0 design, 2026-10-05. Code baseline: `75dcd49`, after the S1+, find and main
merges into `wt/s1b`. This is a build specification. M1 implements the
resident engine library; M2 implements batch and JSON find, M3 the resident name
index, and M4 the query-only socket host and ordinary clients. Watches and
retained writer ownership/routing remain M5. M1 is verified at
**571 passed / 4 ignored**, with zero Rust warnings; its 10M full-open and
resident-memory results are recorded in
[ROADMAP § S1b M1](ROADMAP.md#s1b-m1--resident-engine-library-2026-10-05).

[D46 C plus batch](DECISIONS.md#d46--is-the-daemon-the-only-mode-of-operation)
and [D49 A](DECISIONS.md#d49--a-one-shot-query-with-no-daemon-running) bind the
hosts. D51 A binds writer pauses, D52 B binds handles, D53 A keeps semantic
validation, and D54 B adds interned names, row postings and a term index while
keeping BFS. D26's EACCES amendment, D29, D31, D34, D37 and the answered find
F8/F10/F11/F12/F13 rules remain unchanged. D54 is Dave's later instruction to
build the name index even though the earlier D48 conditional passed.

D55 remains open. This design does not adopt its recommendation. D56 A and D57 A
are answered: actions execute locally through the shared engine, and batch and
socket share the JSON-lines codec. D58 B selects the bounded hand-written
request reader already built in M2a.

M1's find context captures an open cwd capability as well as its absolute
logical path and start time. A path alone fails if a command moves the cwd:
relative live lookup, reference/output files and explicit-context commands need
the descriptor. The one-shot process wrapper keeps ordinary exec's inherited
cwd; the explicit library context never changes the process cwd.

## Engine ownership and open

The existing library target of **`ferret`** owns `Engine`: coordination of a
checked catalog, query execution, derived query state and an optional writer.
`ferret batch` and `ferretd` are hosts of that library. Build `ferretd` as
`crates/ferret/src/bin/ferretd.rs` in the same package. A separate daemon crate
would add wiring and dependency edges without providing another library
boundary.

This fits [DESIGN § Crates](DESIGN.md#crates) and the actual
`crates/ferret/tests/layering.rs` check. `ferret` already depends on catalog,
query and crawl, and already has `src/lib.rs`. Engine ownership needs neither a
new crate nor a new dependency edge. It is coordination in that top-level
library; parsing/evaluation stays in `ferret-query`, observation stays in
`ferret-crawl`, and persistence stays in `ferret-catalog`. Linux watch and
index-worker scheduling code belongs with crawl, using its existing `rustix`
dependency. Any additional rustix features are reviewed in that build slice.

The JSON input codec uses the bounded hand-written request reader (D58 B), with
round-trip and mutation-fuzz tests. Reuse the current byte/base64 and JSON
output helpers; no new dependency edge is needed.

### Resident state

One opener serves every host. It validates and reads all catalog sections once:
names, inode columns, directory counts/coverage, roots, links, specials,
worktrees, document bindings/hashes/refcounts and policy. A name-only one-shot
therefore also rejects corrupt metadata before emitting rows. This intentionally
replaces the old CLI's selective-load corruption behavior; the low-level
catalog/query APIs still support selective loading. Names and inode columns
remain packed where random access benefits; “resident” does not mean expanding
every stat into a Rust struct. D30's persisted directory own-name column
remains. Do not eagerly build reader-only inverses until needed.

`Catalog::open` currently pins descriptors and reads sections into checked
`OnceLock` buffers. It does **not** map the catalog. Keep those catchable I/O
errors and D53's checksums plus semantic validation. Loading the committed
overlay validates graph, references, counts and cross-family bindings through
the production reader. The engine never trusts a writer checksum in place of
that proof.

There are no content index implementations to map yet: `ferret-index` and
`ferret-text` currently contain crate contracts. S2's immutable index segments
will be mapped by their owning crate, exposed through candidate sources, with
DocId liveness from the pinned catalog. S1b allocates no dummy mappings and adds
no catalog mmap. D54's name structures are resident derived state, described
below; they are not document postings in `ferret-index`.

A query-only engine stops after resident validation and query-state setup.
From M5 a daemon attaches one `WriterSession`, sharing that session's checked `Catalog`
buffers with the query view rather than opening another complete reader. Its
identity/hash/refcount/alias lookups are additional writer memory, reported
separately. Batch attaches a writer only when a test or explicit refresh host
needs it. The same opener and executor apply to the one-shot in-process
fallback; there is no second cold-query strategy to tune.

A missing catalog remains a typed error with indexing advice. Starting the
daemon does not implicitly authorize crawling HOME or invent roots. An explicit
initial `index` request may build the first checkpoint; the daemon can serve
status while that is running.

### Queries and generation publication

The engine holds a short-lock-protected current view. To start a query, clone
that immutable view and its derived-state bundle under the lock, then release
it. The whole query pins `(incarnation, checkpoint, sequence)`, including all
its paths and metadata. Output rows carry that generation in their enclosing
result block. No query takes the writer lock, and no output write holds the
current-view lock.

Search uses the effective namespace and effective metadata: suppress dead or
replaced base rows, patch their fields, add current delta rows, then verify all
query predicates. Hard links emit the applicable paths; ignored names,
search-suppressed ancestors and special entries keep their existing search
rules. Find uses `Plan`, the effective `Catalog` and the existing source,
ordering, live-fallback and action rules in [FIND.md](FIND.md). In particular,
retained scopes can walk live; an opaque EACCES directory has no old searchable
children. A pinned catalog is a snapshot, not a claim that the live tree is
frozen while a find runs.

The writer calls the real
`ferret_crawl::refresh(&mut WriterSession, request, options)`. Handle its result
as follows:

| Result                  | Engine and queued work                                                                                                                                                                                     |
| ----------------------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `Unchanged`             | Retain the view and caches. No generation, disk write or publication sync. Refresh status can advance a host observation timestamp.                                                                        |
| `Committed { changes }` | Check `base_generation` before using any delta id. Prepare derived changes and install the returned checked `view`; share unchanged base state. Do not reopen or replay the log.                           |
| `RetryFromCurrent`      | Discard numeric scopes and resolve retained locators in the returned current view before constructing another request. The complete generation check precedes every id dereference.                        |
| `Checkpointed`          | Install the new epoch view with rebuilt name/query/alias caches. Throw away old-epoch numeric candidates, seen words and queued scopes; resolve locators again. DocIds and their next counter stay stable. |

Compaction may keep sequence unchanged: equality of sequence alone is never a
cache or request validity check. Epoch handles use the existing generation
contract; any reused external result id includes its epoch. Old queries keep
valid old ids and open descriptors even after cleanup unlinks their files. Fault
reporting after checkpointing resolves against the returned generation, using
S1+'s fixed full-report path when needed, never saved old numeric ids.

Derived query state is immutable and generation-bound too. Namespace changes
patch its sparse part before adoption; metadata-only changes share it. If a
query accelerator cannot be prepared, the new checked view can use the exact
resident scan/walk plan. An accelerator is never authority to omit rows or a
reason to present mixed generations. On uncertain publication failure, retire
the writer and recover under the lock; readers can retain their last checked
view with an explicit failed-refresh status.

Use bounded query admission and output buffers. Initially allow up to four
active queries and at most `min(16, CPUs)` total query worker permits; tune from
measurements. Reuse the existing find executor, passing available worker counts,
rather than copying its per-entry loop. Waiting clients do not pin a generation
until admitted. Slow output holds that query's pin, not the writer;
disconnect/cancellation releases it after workers and already-started actions
finish. Report internal pinned epochs and their bytes. External readers can
retain storage too; the daemon cannot revoke them.

### D54 B: names, row postings and terms

Build this after batch establishes the shared executor, before daemon latency
claims. Keep checkpoint BFS directory numbering and `(parent, basename)` sibling
order. Within an epoch, overlay births/moves need not be in numeric BFS order;
traversal uses effective edges. Compaction restores dense BFS ids.

`ferret-catalog` owns a resident name dictionary, row-to-name keys and
name-key-to-NameId postings with stored counts. Those are catalog row postings,
not content postings. M3's implementation preflight found intpack absent from
the manifests despite its planned DESIGN edge. The approved placement is
`ferret-catalog → intpack` (D59): catalog owns name storage and row postings,
while index stays a document candidate source. intpack is a git dependency
pinned to the revision the prototype measured (`6423815`, D59 A). The resident
projection uses the codec library rather than copying its implementation.

`ferret-query` owns the term-to-name-key index and its planner, using the shared
D9 tokenizer in `ferret-text`; that tokenizer's contract is presently
unimplemented and is part of this slice. An initial sorted term dictionary
suffices for exact term lookup without a new FST library dependency.
Prefix/fuzzy automata and subtree masks need their own measured justification;
D54 does not schedule full-path fuzzy search.

The v4 snapshot/log remains authoritative. Construct the dictionary from
validated names on resident open and rebuild it on an epoch change. Resident
name storage must **replace** the repeated raw basename heap/offset storage, not
keep it as well as another complete copy. Introduce a checked resident name
representation behind catalog accessors; release conversion scratch before
exposing it. Existing checksum, tiling, sibling-order and graph checks still run
before conversion. Checkpoint encoding continues to emit v4 name bytes from that
representation through the current streaming writer; no new persistent overlay
namespace or publication artifact is introduced (D53 A). Keep raw-format
decoding for import/validation, not as a separately tuned CLI query path.
Measure the conversion peak and the interaction with writer pins.

An immutable base dictionary/posting set is shared across same-epoch views.
Overlay names have a sparse distinct-name/term/posting layer. Suppression of old
base edges is checked before output; a renamed basename contributes its latest
tokens once. Ignored edge tags remain explicit. Metadata-only updates cannot
duplicate postings. A checkpoint rebuild removes dead keys and remaps all row
references. No borrowed reference to the old epoch leaks into the new bundle.

A basename predicate scans the distinct table, obtains matching name keys and
estimates work from their **stored posting counts**. Choose postings plus
memoised effective-ancestor checks for rare hits in a large scope, or a scope
walk for common names/small scopes. Use scope cardinality estimates where
available; unknown size takes the safe walk. Account for the sparse overlay in
both estimates. In D54's prototype the wrong plan took **69 ms**, versus **7.4
ms** for the chosen plan on nixpkgs `default.nix`; `*ripgrep*` took **0.63 ms**
through postings versus **7.5 ms** walking. Do not always select postings merely
because an index exists.

A path pattern with a necessary basename suffix can use that candidate set then
verify the full path. Anchored patterns, prune/depth/quit and effectful find
retain traversal semantics; a postings plan must not reorder an observable
expression. Existing bare search words remain substrings, and GNU `-name`
remains a glob. Add explicit `name-term:TEXT` to search for an exact
D9-normalised basename token; the term index cannot replace substring matching
with token matching. Find has no new GNU primary. Unsupported accelerator shapes
use the same exact evaluator. Prototype timings are evidence for this work, not
production promises for all 10M trees.

## Batch protocol

`ferret batch [--input FILE]` loads one query engine and processes requests
sequentially. Without `--input`, requests arrive on stdin. Stdout is always JSON
lines, including errors. Each query gets one contiguous tagged block; there is
no interleaving of query blocks or buffering of a complete answer. A caller may
pipeline input but processing applies output backpressure. Use a single bounded
input slot rather than accumulate every pipelined request.

Example, with argv preserved as tokens rather than a shell command:

```json
{"id":"s1","op":"search","args":["case:Cargo.toml"],"limit":20}
{"id":"f1","op":"find","args":[".","-name","*.rs","-print0"],"cwd":"/work/project"}
```

Each request id is a nonempty caller-supplied string, echoed without meaning or
durable deduplication. Clients must not reuse a tag while it is active. A
byte-valued input is a UTF-8 string or `{"base64":"..."}`; decode exact bytes
before parsing. Do not split arguments, invoke a shell or normalise away
trailing slashes, repeated slashes, `.` or `..`. Relative operands need an
absolute `cwd`; batch defaults to its launch cwd. Preserve the operand spelling
for find `%p/%P/%H` while using that context for lookup. Parsing uses one
captured start time per query, including find relative-time tests.

The output contract is:

```json
{"id":"s1","event":"begin","generation":{"incarnation":"...","checkpoint":7,"sequence":19}}
{"id":"s1","event":"row","path":"/work/Cargo.toml","type":"file","size":912,"mtime":1791000000,"doc":42}
{"id":"s1","event":"end","exit":0,"rows":1,"cancelled":false}
{"id":"f1","event":"begin","generation":{"incarnation":"...","checkpoint":7,"sequence":19}}
{"id":"f1","event":"stdout","bytes_base64":"Li9hLnJzAA==","record":1,"part":0,"last":true}
{"id":"f1","event":"end","exit":0,"cancelled":false}
```

Search rows preserve today's `path` plus `path_base64` when needed, type, size,
mtime and stable DocId. Find's arbitrary printf, NUL and command output is exact
bytes in stdout frames, not guessed line records. Large committed entry output
splits into numbered parts; no other entry's parts interleave. `record` numbers
commit units, not entries: one unit holds one or more whole entries, so a client
must not count records to count matches. Keep find's whole-entry commit gate, 64
KiB capture threshold and unlinked spill file. A broken stream can deliver a
prefix of a record, just as a broken pipe can today; it cannot claim that record
complete. Diagnostics are tagged `diagnostic` events with a code, severity and
optional byte path. End follows all committed output and contains the native
status and timings. A parse or runtime failure still gets a begin/end block,
with null generation when no view was selected. Never manufacture end after
transport failure.

Find requests add optional `capabilities`, an array of strings, and
`child_stdin`, the string `"null"` or `"inherit"`. Unknown optional fields and
unknown capability names are ignored; neither grants a known capability.
Missing/null capabilities mean none. Missing/JSON-null child_stdin means no
explicit policy, and is accepted for read-only requests only. Effects require
`"local-effects"` and an explicit child_stdin policy. Interactive actions also
require `"interactive"` and terminal stdin separate from the request stream. For
example:

```json
{
  "id": "a1",
  "op": "find",
  "args": [".", "-exec", "echo", "{}", ";"],
  "cwd": "/work/project",
  "capabilities": ["local-effects"],
  "child_stdin": "null"
}
```

Before preparation opens any output file or evaluation runs any action, find
refusals emit `begin` then `end` with exit 1 and a typed `error` code:
`LocalEffectsRequired`, `InteractiveRequired`, `Noninteractive`,
`ChildStdinOnProtocol`, or `ChildStdinRequired`. `"inherit"` is forbidden on
JSON-lines stdin even for read-only find; `--input FILE` permits it. Protocol
stdin is never prompt input, even when attached to a terminal. No automatic
approval occurs. File-input prompts are serialized, emitted as stderr bytes, and
keep C-locale y/Y approval and closed child stdin after approval.

Child stdout uses the existing bounded entry capture and commit gate. Child
stderr streams independently as `stderr` events with the same `bytes_base64`,
`record`, `part`, `last` fields as stdout. A scoped thread drains stderr while
the worker drains stdout, avoiding deadlock when stderr fills before stdout.
Each stderr read is a bounded record (at most 64 KiB, part 0, last true). Record
ids are unique across both streams, allocated at each stdout commit or stderr
read; concurrent reads/commits can reach the transport in a different id order.
Stdout commit order and each child's stderr byte order are preserved; there is
no total order between the streams or concurrent children. Stderr can interleave
stdout parts and survives quit's discard of an entry's stdout, matching raw CLI
stderr. End follows completed readers and awaited children. Child stderr never
inherits either batch output descriptor. Transport failure ends the host without
manufacturing an end event.

Relative output-file names use the request's captured cwd handle. Batch never
changes process cwd. Delete and execdir continue through observed parent
handles, and default-mode predicates/deletion counts retain F8 B/F12 D.

Set provisional limits of **1 MiB per input line**, **16,384 argv elements** and
**64 KiB per output part**, checking encoded and decoded bytes. Reject an
oversized/malformed request with a tagged error when its id is available;
otherwise use null id and continue only at a trustworthy newline boundary. The
JSON parser's nesting limit remains enabled. Unknown operations/required fields
fail; negotiated optional fields may be ignored. Limits are public protocol
values, not kernel ARG_MAX claims. No fault or argument is silently dropped to
fit a budget.

Search statuses remain 0 for a match, 1 for no match, 2 usage and 3 runtime.
Find remains 0 for success, including no match, and 1 for errors. A batch
process exits 0 when it completed the protocol, even if individual queries
returned nonzero; exits 3 on host/I/O failure and 2 for batch invocation usage.
Agents read each tagged end status. Stdin EOF finishes the current query and
exits after flushing. File input may share a terminal stdin with local actions;
JSON-lines stdin cannot also be an inherited command-input stream.

Add `status` and `reload` control blocks for tests and agents. Reload checks
`current` and adopts a newer generation through the same checked resident
opener, without a per-query reload. Batch does not watch the tree: action
queries retain normal snapshot freshness until an explicit re-index/reload. It
does not claim daemon freshness. Simulated refresh tests use the library writer
API, not a batch-only imitation of reconciliation.

The local host's built `status` event includes `generation`, `bytes` and
`engine_opens`. `bytes` counts the loaded resident catalog payload plus the
resident name planner's byte storage. `engine_opens` is an engine counter used
to verify that sequential queries share one open; explicit reloads increment it.
Search end events include `elapsed_us`, `first_row_us`, `bytes_read` and query
stats when available. Find end events include `elapsed_us`.

The suites can issue many read-only queries against one fixture/session, compare
every block with current CLI output and pinned GNU findutils 4.11.0, and rerun
against the socket host. Find-compat's driver groups cases by fixture, index,
cwd and required host capabilities. Mutating cases get independent throwaway
trees; after effects, compare both output/status and tree state. Don't reuse a
mutated fixture merely to reduce opens. Interactive/stdin tests use file input
and a PTY, or the ordinary CLI; they are not silently skipped. Batch request
framing/exit handling belongs in the harness adapter, not in copies of find
syntax or evaluation.

M2c adds six real-binary integration tests: a 15-case action parity table
against CLI stdout/status and complete throwaway tree state; exact binary stderr
and a 131,073-byte stderr-before-stdout pipe-pressure case under timeout; every
preparation refusal; caller stdin with file input and null child stdin with a
following request; execdir/file outputs with a distinct request cwd; and real
PTY yes/no approval for both interactive actions with closed child stdin. All
workspace gates pass at **620 passed / 5 ignored** (baseline 613/5).

M4 is verified at **647 passed / 5 ignored**, zero Rust warnings. Its 21
real-binary socket tests and one engine cancellation test add 22 passes over
625/5. All endpoints are isolated, reads/processes are bounded and hosts are
cleaned up; no latency/timing experiments were run in this slice.

## Daemon, socket and lifecycle

### Endpoint and request context

Use a pathname Unix stream socket in a **0700** directory under
`$XDG_RUNTIME_DIR/ferret/`, with socket mode **0600**. A deterministic short
endpoint identifies the index directory's filesystem identity so separate
FERRET_INDEX values cannot connect to the wrong engine. Confirm that identity
and catalog incarnation in hello; paths alone are not identity. The directory is
checked for ownership/mode and is not replaced through a symlink. Socket
permissions are the user's authentication, without a token or network service
([unix(7)](https://man7.org/linux/man-pages/man7/unix.7.html)). No implicit
runtime directory under world-writable `/tmp`: absent usable XDG_RUNTIME_DIR,
use the in-process fallback.

**D57 A:** use the batch JSON-lines query/event codec on the socket, with a
hello carrying protocol major/minor, build identity, catalog format, index
identity, capabilities and limits. One active query per connection avoids a
multiplexing scheduler; concurrent clients use separate connections.
Control/cancel messages can arrive during output. Status reports loading,
refreshing, compaction, degraded watch coverage and budget/freshness state.
Connection draining and event reading stay at normal scheduling priority.

The request supplies raw argv, display cwd and query start time. The server uses
explicit per-query filesystem context and never process-wide chdir or
environment mutation. Find formatting is currently C-locale UTC; retain that
contract. Same-user groups and mount namespace must be compatible for live
fallback; when the client cannot establish compatible context, run locally. Use
the captured absolute cwd as a lookup prefix, keeping operand spellings
separate. F5's existing path-based rename/ENOENT limits still apply. Do not
claim this is a transferred directory capability. D56 B would need additional
capability/descriptor transfer for client actions; that is part of its cost.

### First use, singleton and upgrades

The CLI connects first. If no server answers and background operation is
allowed, start the installed matching `ferretd` with the selected index/config
identity and detached stdin; stdout/stderr go to a private daemon diagnostic
log, never the requesting terminal. Concurrent starters use an endpoint startup
lock held by the daemon before binding; the loser connects to the winner. Under
that lock, remove a stale socket only after failing to connect. The writer lock
remains a separate catalog lock.

Hello distinguishes missing, loading and ready from protocol/version errors. A
starting daemon can acknowledge loading without blocking the event loop. Give
first use a provisional 10 s startup wait, configurable for slow storage; on
expiry or denied spawn, fall back to a batch-of-one engine in process.
`FERRET_NO_DAEMON` bypasses connection and spawn. Runtime-directory failures,
sandboxes, incompatible filesystem context and unreachable sockets fall back
without asking an agent a question. Invalid catalog data is reported, not hidden
by constructing a fresh index.

An optional systemd **user** service runs the same binary and host; no second
service implementation or mandatory systemd socket activation. Both unit and
spawn use endpoint/index singleton locking. Install/enable only when explicitly
requested; first-use spawn needs no setup ceremony. Direct spawn exits after 15
minutes without clients, with no pending refresh, backstop or publication. The
explicitly enabled unit stays resident; disable its idle timeout to avoid
restart loops. On exit, finish a started durable publication, release watches
and locks and unlink only this host's socket under the endpoint lock. Restart
always re-arms watches and schedules a complete backstop for the downtime gap.

Protocol majors must match; minors negotiate capabilities. M4 conservatively
drains on any build or catalog-format mismatch; it does not try to certify
semantic compatibility between different builds. Request a graceful drain/restart: admit no new
queries, finish current queries and publication, release the endpoint, then
start the client's binary. Do not kill active find actions or a writer mid-sync
to accelerate an upgrade. If drain cannot complete in the startup deadline, use
the in-process engine and show the mismatch in status. Do not retry a query
after it emitted bytes or performed effects; that would duplicate output or
actions. An already accepted query keeps its original generation through a
daemon upgrade.


M4 implements `daemon.rs` with `daemon/{endpoint,client}.rs`. Endpoint names are
`<dev-hex>-<ino-hex>.sock`, with matching `.lock` and `.log` files. std's Linux
`OpenOptionsExt` no-follow/directory flags and retained handles under
`/proc/self/fd` prevent a substituted runtime or `ferret/` symlink from being
followed. Both directories require this effective uid and exact mode 0700;
lock/log files require 0600, regular-file type and one link. There is no fallback
runtime path. The startup lock is held through the host's lifetime and cleanup;
a loser connects to the winner and exits. Cleanup checks the bound socket's
dev/ino under that lock. The systemd template disables idle exit with
`--idle-ms 0`; it is not installed or enabled.

Hello is an `id:"hello", event:"hello"` JSON object carrying `major`, `minor`,
`build`, `format`, `index` dev/ino, `generation` (including incarnation),
`context`, `pid`, `capabilities` and `limits`. `loading` means the endpoint is
bound while checked resident loading runs; `ready` means the checked engine can
admit queries; `failed` includes the opening error. Build identity hashes sorted
workspace Rust sources and Cargo.lock at compile time. The client compares all
uid/gid slots, supplementary groups and mount/user namespace dev/ino from Linux
procfs. Permissions authenticate same-user access; M4 does not transfer cwd
handles, authenticate a supplied pid, or compare ACL/security-module context.
Failure to obtain or match the checked context falls back locally. A ready
hello also confirms the local header's catalog incarnation; the host refreshes
ready state before hello so an incarnation replacement can be adopted.

Control envelopes are `{"op":"cancel"}` and `{"op":"drain"}`. A bounded
reader per connection receives controls during blocking output. Cancel or
hangup latches cancellation and shuts down the socket to wake a blocked writer;
no end is fabricated for a partially transmitted frame. Drain closes admission
and finishes active queries, including one on the control connection. Query
execution catches unwinding panics (find workers wake their siblings on unwind) and emits a typed `RuntimeError` end; release
and development profiles retain unwinding, enforced by a compile-time guard. Engine pins and event writes happen
outside the selection lock, and poisoned locks recover checked immutable state.
There are at most 32 connections and `min(4, CPUs)` admitted queries, with at
most `min(16, CPUs)` total query worker permits.

`FERRET_DAEMON_STARTUP_MS` sets the ready deadline (default 10000);
`FERRET_DAEMON_IDLE_MS` or `--idle-ms` sets idle exit (default 900000, zero
means disabled). `FERRET_DAEMON_BIN` overrides the sibling `ferretd` used for
spawn. Diagnostic output goes to the private endpoint `.log`. Debug builds also
provide load-delay, build/format/context override and query-panic hooks solely
for the real-binary integration tests; release builds omit them. SIGINT uses
normal process termination: closing the client socket cancels the query, while
the spawned host has a separate process group. A cancelled connection closes
rather than promising an end after partial output. One bounded pending request
slot is allowed per connection; over-pipelining closes the transport so the
control reader never blocks behind queued requests. Clients use separate
connections for concurrent queries.

### Writer ownership

M4 originally hosted queries only. M5a retains the writer and publishes through
that same resident engine; it no longer reopens an unrelated foreground writer's
catalog before each query. On a failed manifest read or publication, queries
continue on the last checked view and status reports the refresh failure.
Uncertain writer recovery validates the durable prefix under the same held lock.

The daemon retains one WriterSession and its lock between bursts. This means an
unrelated CLI writer cannot simply open the catalog while it runs.
`ferret index` and root edits therefore send explicit writer commands to a
compatible running daemon, using the real crawl/index/root-change producer under
its existing lock. Return the same report and wait for that command's
publication; a remote index is not an inotify hint. Extend the session root-edit
seam where today's `index_change` would otherwise open a second lock. A
foreground no-daemon writer encountering a daemon-owned lock fails with
owner/status advice; it does not silently stop the daemon or wait indefinitely.
A query-only fallback needs no writer lock.

Maintain a single writer queue with root/policy commands as ordering barriers.
Queries continue on the last checked view while these commands run. After
success, update configured-root boundaries and watch coverage together with view
adoption. If a protected scope prevents a global policy/sniffer transition,
retain the old header and report the failed command (D37).

M5a's socket protocol is major 1/minor 1. `index` and `roots-remove` are
explicit writer operations; argv item zero contains the originating client's
global rules, followed by absolute byte-valued paths. Index with no paths refreshes
all roots; with paths it adds/refreshes those roots. Roots-remove is a D34 barrier.
The compatible hello advertises `writer` alongside the read-only query capability.
Replies use begin, bounded stdout/stderr parts, a byte-valued originating-host log
record, and end, after checked publication and watch adoption. Sending commits
the client to that command: transport failure never replays it. Index does not
spawn a host when none is running. `FERRET_NO_DAEMON` preserves direct indexing;
a retained-lock conflict fails promptly with owner/status advice.

### Watches and reconciliation

Watch directories, not all 10M files. Use one nonblocking inotify instance with
`IN_CREATE`, `IN_DELETE`, move endpoints, `IN_ATTRIB`, `IN_MODIFY`,
`IN_CLOSE_WRITE`, self-move/delete, unmount and ignored-watch handling. Avoid
access/open/read-close notifications from indexing itself. Directory watches are
not recursive and can miss writes through an alias outside that watched
directory; periodic reconciliation remains necessary
([inotify(7)](https://man7.org/linux/man-pages/man7/inotify.7.html)).

A descriptor maps to physical directory identity and one or more rooted
namespace occurrences, accounting for bind aliases and D34. Store compact
parent-watch locators and basename bytes, plus generation-qualified resolved
ids. A directory move updates locator ancestry and re-resolves descendants; it
does not rewrite millions of full path strings. Watch descriptors survive
catalog renumbering. Handle descriptor reuse/IN_IGNORED conservatively: if a
pending event cannot be bound to the watched lifetime, request a backstop rather
than apply its number to a new directory.

Arm a directory watch before its complete observation, then reconcile events
arriving during that observation. For a newly created/moved-in directory, arm
and refresh its subtree; enumeration closes the create-to-watch gap. During
startup, establish the watch set, schedule the complete backstop, and drain
concurrent events into the next burst. A partial or denied watch installation is
a coverage gap, never proof of unchanged children.

Use a provisional **200 ms** trailing debounce with a **1 s** maximum age to
avoid endless postponement under writes. Pair move cookies within that window,
then pass both endpoints as `RenameHint`. Check final disk state through
refresh; wrong, missing or reordered cookies do not prove lifetime. Unpaired
endpoints still trigger observation; notification deletion is not a catalog
delete command. Refresh all relevant aliases using S1+'s alias promotion,
including hard links across kept/refreshed roots. Parent identity/ctime changes
can expand an Entry into a subtree or containing root; small notifications are
not a guarantee of small refresh work.

The intake thread continuously drains the kernel queue, including during
compaction. Bound pending locators, cookie state and owned name bytes to a
provisional **100,000 scopes / 16 MiB**. Coalesce by final parent/name, collapse
overlapping Directory/Root scopes, and keep the earliest enqueue time for lag
reporting. On exhaustion, discard detailed hints into one all-roots backstop
marker and continue draining. This marker survives further arrivals; it is never
a silent dropped fault. No additional durable event journal is required: a
restart always does a backstop and roots/policy are durable already.

`IN_Q_OVERFLOW` with this shared instance cannot identify the affected root:
submit `RefreshReason::Overflow`, which currently means **all configured
roots**, and clear rename/numeric hints. A userspace queue loss or ambiguous
watch lifecycle follows that same complete backstop. A known local watch gap can
schedule a Root scope with reason Burst; existing Backstop and Overflow reasons
both expand to all roots. Do not pretend the current API has a scoped Backstop
reason. Events after a backstop's intake watermark remain queued for the next
refresh; clear a pending backstop only after success, and retain a new loss
marker if another overflow occurred while it ran.

Read actual per-user limits; other programs share them. This machine currently
reports **524,288 watches**, **524,288 instances**, **16,384 queued events**
(`/proc/sys/fs/inotify/*`, read 2026-10-05). The fixture has **1,800,947
directories**. Full coverage will not fit that watch limit. Use a configurable
watch cap, initially at most the kernel limit with some headroom for the user's
other tools, and report actual successes. Do not change sysctls. `ENOSPC`,
`EMFILE`, `ENOMEM` and permission failures mark uncovered root/scope coverage
and schedule polling; do not abandon query service or the old view. A failed
directory watch alone does not retire its children: only the crawl's real D26
observation can do that.

Provisional timers: full configured-root backstop each **hour**, uncovered or
fault-retained roots due each **five minutes**, subject to the controller and no
overlapping crawl. Retry denied watches after permission/parent events and their
poll. Report actual last complete coverage and overdue work; these intervals are
scheduling defaults, not freshness guarantees under pressure. Ignored
directories need no recursive watch, but their visible parent and policy inputs
must be monitored so re-inclusion can be discovered. Watch global config/rule
parents and `.gitignore`, `.ferretignore`, git info/exclude/worktree metadata
dependencies, including those outside the visible tree; if an input cannot be
monitored, its containing root is polling-dependent. Policy changes expand
through the real refresh seam. Root boundary changes refresh the kept enclosing
roots too (D34). Network/FUSE event gaps require polling regardless of nominal
watch installation.

M5a implements this intake with one rustix instance and an observed-handle hook
before each complete directory listing. Compact immutable parent/name locators
capture physical `(dev, ino)` and a shared root path; they have no catalog ids.
Directory endpoints observe their affected subtrees through Entry scopes; self
notifications and unknown locators conservatively refresh the containing root.
Ordinary entry notifications resolve locators in the current generation; policy
and changed root boundaries widen work. Move hints pair unique cookies; wrong,
duplicate and missing endpoints still observe disk. Explicit known watch removals
have IGNORED tombstones; unknown lifetime/reuse collapses to Overflow.

`FERRET_WATCH_CAP` defaults to seven eighths of `max_user_watches`, reserving
one eighth for other user tools. All three inotify sysctls are read, never changed.
Cap/install failures become coverage gaps. `FERRET_BACKSTOP_MS` defaults to
3600000 and `FERRET_POLL_MS` to 300000. The five-minute timer refreshes only uncovered, fault-retained, relocated,
unproven-alias or unreliable-filesystem roots, using Root scopes with reason
Burst. If none need polling, it does no refresh. M5b removes the blanket alias
fallback: each descriptor owns one physical identity and all observed rooted
parent/name occurrences. An event resolves every occurrence in the checked view;
the S1+ producer promotes shared aliases and hard links across kept roots.
Unobserved hard links remain polling-dependent until the sparse physical
parent/name proof covers `nlink`; bind occurrences of one name count once.
Unknown descriptor lifetime or changed root boundaries still widens observation.

Policy dependencies come from crawl's actual consultation callbacks, including
missing inputs, `.git` entries, gitdir/commondir metadata and the global ferret
ignore file. `ferret-policy` has no input discovery: its compiler remains pure
(D13). Missing parent directories watch the nearest existing ancestor. Followed
policy symlinks watch their targets and ancestor symlink entries. Complete
unprotected root observations retire old dependencies; partial/protected work
retains them. Global ferret rule parents and the reserved `config` slot are
watched too; there is no ferret config-file parser yet. Inputs that cannot be
watched put their owning roots in the poll set.

M5b uses `fstatfs` on tree and policy-parent handles. NFS (`0x6969`),
CIFS/SMB/SMB2 (`0xff534d42`, `0x517b`, `0xfe534d42`), 9P (`0x01021997`) and
FUSE (`0x65735546`) poll even with installed watches: remote/userspace writes
need not notify this client's inode. Failure to classify also polls. This small
conservative list leaves ordinary ext, btrfs, XFS, tmpfs and overlay trees on
watches; it is not a claim that every other filesystem guarantees events.
These timers serialize through the one writer service. Pending intake/publication
prevents idle exit; an explicitly enabled unit still uses idle zero. Restart
always arms during observation and performs a complete startup backstop.

### Counts, coverage and status

Entry refresh must obtain the parent's complete raw entry count, including
ignored names, or publish unknown. Never update a stored count by event
arithmetic: coalescing, missed events and aliases make it wrong. Directory
reopen/listing EACCES publishes the ordinary opaque row with unknown count and
retires its subtree; repeat EACCES emits no generation. Other typed faults
retain only anchored checked scopes, with their coverage markers; successful
listing clears the marker in the same transaction. Ignore-read EACCES remains
protection/blocking. No watcher rule overrides the M5/M5c fault table.

`status --json` reports generation, current operation, last successful refresh,
last completely covered backstop, protected scopes, watch installed/needed/
failed counts, oldest pending age, pending scopes/bytes, backstop reason, writer
input/log budgets, current/peak RSS and pinned internal epochs. Use monotonic
durations for waits and wall times for display. Label opaque state, protected
state, watch coverage and queued freshness separately; an incremented sequence
alone does not prove complete or current coverage. `stats --json` adds the
catalog census and D54 bytes/planner counters.

M5b exposes the complete list for an existing compatible daemon, without
spawning one. Without a host, status opens the local checked catalog and sets
`host_running:false`; unavailable freshness history and watch coverage are null,
not a claim of coverage. `stats --json` uses the same selected pin and adds the
human census (including raw/unknown counts, histogram bins, extensions, content,
hard-link and duplicate state) and D54 storage/planner counters.

| State | JSON fields and types |
| --- | --- |
| Selection | `host_running`: boolean; `generation`: epoch/sequence object or null; `current_operation`: string; `pinned_internal_epochs`: integer array |
| Checked coverage | `protected_scopes`, `opaque_directories`: integers; `fault_retained`: boolean; `refresh_error`: string or null |
| Watch coverage | `watch_installed`, `watch_needed`, `watch_failed`: integers; `watch_uncovered`: boolean or null; `polling_roots`: byte-path array or null |
| Queued freshness | `pending_scopes`, `pending_bytes`, `refreshes`: integers; `oldest_pending_ms`: integer or null; `backstop_reason`, `last_refresh_reason`: string or null |
| Writer | `writer_busy`: boolean; `writer_commands`: integer; `writer_input_budget`, `writer_log_budget`: integer-field objects; `writer_input_usage`: object or null |
| Display history | `last_successful_refresh`, `last_complete_backstop`: Unix-second integers or null |
| Resident resources | `current_rss_kb`, `peak_rss_kb`: KiB integers or null; `catalog_bytes`, `planner_bytes`, `name_postings_bytes`: integers |

Installed watches count distinct physical directory descriptors, shared between
tree and policy occurrences. Failed counts include unresolved policy watch
registrations; needed is installed plus failed. Polling dependence can exist
without a failed syscall (network/FUSE or an unobserved hard link).
Input usage is the last successful producer report; log/input budgets are current
default admission ceilings. Planner counters accumulate selections and estimated
candidates for the host lifetime; their build durations are microseconds.
Waits and pending ages are monotonic. A complete-backstop wall timestamp requires
no retained coverage faults; ordinary opaque observations remain covered.
M6 implements signal sampling, worker targets, bulk pause gates, headroom
admission and byte pacing at the serial writer boundary. `controller` is null
without a host; live status reports PSI/load as numbers or `"unavailable"`,
battery as boolean or `"unavailable"`, idle classification, worker target,
blocked reason, required/available headroom, last admission kind/result and
rate-limit byte/wait counters. Sequential advice failures are diagnostic.
No-reuse advice is disabled and unmeasured. Deferred retries preserve the
oldest pending age; a host without watches still reports a complete backstop
marker and retries it before idle exit.

### Compaction, oversized fallback and politeness

An idle writer boundary means no transaction is executing. Serve coalesced
bursts there; perform requested compaction under the writer lock (D51 A). The
writer's preflight can checkpoint a threshold-crossing burst at that same serial
boundary even during sustained arrivals. Never postpone a crossed log budget
indefinitely waiting for an empty notification queue. No suffix rebase or
concurrent compactor is built. Queries retain their old view, and intake
continues at normal priority throughout the pause.

S1+ R1 measured **22.451 s** median, **16.949–23.131 s** range, whole pause,
including cache rebuild and final retirement sync. Its sampler accumulated
**1,679–2,296** arrivals every 10 ms, before any service. S1b measures
coalescing, re-resolution, oldest/newest event-to-publication lag and backlog
drainage under real load. Pause plus debounce plus service bounds the observed
lag; there is no 200 ms freshness promise across a checkpoint. On epoch
adoption, resolve the queued locators; never feed saved numbers back to refresh.

The producer's **500,000 input records / 64 MiB charged owned bytes** guard is
not an RSS cap. Exhaustion discards the attempt and rewalks every configured
root under the same lock, including for an originally scoped burst. S1+ R2's
full builder reached **2.57 GiB** faultless and **2.71–2.78 GiB** faulted;
loaded pinned source, complete builder observations, live-document bookkeeping,
plans/readback and allocator retention still coexist. Watches, query buffers and
extra pinned epochs add to daemon footprint. No S1b promise puts this path
inside 1 GB or a 2.8 GiB ceiling.

Add bulk admission before compaction/full rewalk allocation, including the
automatic fallback inside crawl, not just before the original small burst. Use a
configurable memory/disk reserve; provisionally require about **3 GiB available
memory** for a 10M full-build attempt and **0.7 GB additional disk** for a
checkpoint, then measure with watches and pins. These are admission heuristics,
not allocation proofs. M6 scales the full-build reserve linearly with live names
from 3 GiB at 10M; checkpoint memory uses 700 MB at 10M. Both have a configurable
64 MiB floor. Disk scales from 700 MB at 10M with a 16 MiB floor (or the smaller
configured reserve). A conservative estimate of current watch, locator, alias,
policy dependency and pending-queue bytes is added to memory, alongside an
optional fixed additional reserve. Existing resident and query-pin memory is
already reflected in MemAvailable. Watch estimates run only at bulk admission,
so ordinary entry bursts do not scan the descriptor map. Under insufficient headroom, discard the unpublished
attempt, keep the selected generation and enqueue a complete backstop with a
memory-blocked status. A typed deferred-bulk outcome must restore a usable
writer/current caches under its lock. Do not checkpoint scoped batches, drop
retained faults, or launch another concurrent builder to catch up. Already
admitted durable publication completes; recovery handles failures.

The controller follows
[research architecture §7d–e](research/claude/architecture.html#s7d) and
[R8 Part C](research/claude/research/R8-extraction-and-change-detection.md#part-c--indexer-politeness-staying-invisible-on-a-live-desktop),
with two deliberate adaptations: a bounded queue plus restart backstop replaces
the research's unbounded durable queue, and query/intake threads share no idle
scheduling class with indexing. This is a catalog reconciler, not yet a tiered
PDF/OCR extraction service.

Poll cheap signals at **1 Hz**: CPU and I/O PSI `some avg10`, battery state,
load for diagnostics, and optional compositor idle time. PSI describes stalled
time, not CPU usage
([kernel PSI documentation](https://docs.kernel.org/accounting/psi.html)). When
battery pause is enabled or I/O PSI exceeds **10%**, admit no new bulk jobs;
above **20% CPU PSI**, use one index worker. Otherwise use one while input idle
is at most **30 s**, `max(1, CPUs/4)` at 30–300 s, and `max(1, CPUs/2)` beyond
that, capped by configured crawler concurrency. Raise by one worker after **10
s** calm; drop immediately. Headless means no interactive session, not just a
failed idle probe. Unknown desktop idleness uses one worker; missing PSI uses
conservative concurrency and reports the unavailable signal. Core operation must
work without a compositor library or an interactive probe.

Apply per-thread nice 19 to index workers; socket, watcher and query threads
keep normal priority. M6's revised priority decision drops idle I/O class and
SCHED_IDLE: rustix 1.1.5 does not supply those calls. Linux derives default
best-effort I/O level 7 from nice 19 on schedulers that honour priority (BFQ).
Dave's nine block devices use `none`, which ignores I/O priority; the 32 MiB/s
bulk limiter is the actual I/O protection there. SCHED_IDLE adds only a small
CFS/EEVDF weight difference over nice 19. Revisit an unsafe D11 exception only
if M7 measures foreground harm that nice 19 plus pacing does not prevent. Re-read the worker target before creating a new root/fallback worker pool.
Gate new jobs between refreshes; the current crawl does not support mid-listing cancellation or an
instantaneous worker-count change. Check between content files/bulk phases where
safe, and measure controller reaction latency rather than claim it stops fsync.
Even a single crawl index worker runs on a dedicated thread, so nice 19 cannot
leak into a calling query/intake thread. Generic visitor walks retain their
single-worker caller execution. Optional systemd resource weights/MemoryHigh are additional whole-service
protection; they also affect queries and intake, so default to normal CPU
service weight and measure before imposing stronger limits. MemoryHigh is not a
safe hard full-builder ceiling. Process-wide idle priority would violate the
responsive watcher/query design. Use existing crawl-owned safe Linux calls; do
not create an unsafe host wrapper to avoid a reviewed dependency.

M6 rate-limits bulk read/write work at **32 MiB/s**, configurable, for
backstops, whole-root observations, fallback reads and checkpoint writes.
One shared limiter serializes the bounded transfer seam across index workers;
allowance starts at completion, so slow reads cannot finish together as multiple
bursts. It waits before the next transfer, with a maximum 256 KiB read burst
and 64 KiB write chunks. Checkpoint writes use a scoped, unwind-safe context on
the index owner thread; query reads and small log appends never enter it. S1+'s
unthrottled 629 MB checkpoint already needs roughly 19 s of transfer allowance
at that rate; the idle-priority daemon pause may exceed 22 s. Record both
unthrottled regression rows and deployed politeness rows. The limiter is at actual read/write calls; there is no sleep after publication.
Query reads and small burst commits are excluded.

The research's unconditional DONTNEED advice needs care: these pages may also
belong to a foreground editor or verifier. Use sequential/streaming advice for
bulk source reads and evaluate no-reuse advice on the deployed kernel; measure
foreground cache misses before evicting shared source pages. Do not evict the
catalog/index the engine intentionally keeps resident. This is an advisory
policy within scheduling, not a change to content carry or find semantics.

M6 configuration uses environment keys: `FERRET_INDEX_WORKERS`,
`FERRET_BATTERY_PAUSE` (0 disables), `FERRET_FULL_MEMORY_BYTES`,
`FERRET_CHECKPOINT_MEMORY_BYTES`, `FERRET_MEMORY_FLOOR_BYTES`,
`FERRET_ADDITIONAL_MEMORY_BYTES`, `FERRET_CHECKPOINT_DISK_BYTES`, and
`FERRET_BULK_BYTES_PER_SECOND` (0 disables pacing). Signal mounts can be supplied
through `FERRET_SIGNAL_PROC` and `FERRET_SIGNAL_POWER`; defaults are `/proc` and
`/sys/class/power_supply`. Binary fixtures use private calm mounts through the
same production reader, avoiding dependence on ambient host pressure. Trait
injection and fake monotonic clocks exercise pressure changes and the actual
scheduler/writer without wall-clock waits.

## CLI and find effects

Ordinary `ferret search` and read-only default `ferret find` are socket clients
through the M4 host. They construct the request, consume the tagged block and
render native output. Search's existing `--json` row schema stays intact; the
wire begin/end framing does not leak into ordinary row-only search output.
`ferret --json find ...` is a host flag that must precede the `find` operand; it
is consumed by `args::parse` before the "find" check and never reaches find's
own argument parser, so a find operand or argument that itself reads `--json`
(before or after `--`) passes through untouched — a find start operand can never
begin with `-` in any case, in both GNU find and ferret, so the two `--json`s
can never collide syntactically. `ferret --json find` emits the same tagged
events as a batch find block (`begin`, `stdout`/`stderr` frames, `diagnostic`,
`end`) under the fixed id `"find"`, since a CLI process runs exactly one find
request. The child's stdin inherits the caller's, as the raw CLI does, and
`-ok`/`-okdir` still refuse outside a terminal; there is no protocol caller to
withhold `local-effects`/`interactive` capabilities from, so this host runs
every action the plan asks for without the batch capability gate. Implemented in
`crates/ferret/src/find.rs`'s `run_json`, sharing
`find_json::{emit, generation, diagnostic}` with batch's encoder. Add JSON
output to stats, status and management commands too. Raw mode reproduces exact
stdout and stderr bytes; the process returns the end block's native exit status.

`find_json::refusal`'s `child_stdin` requirement is narrowed to actions that
spawn a command (`-exec`, `-execdir`, `-ok`, `-okdir`, tested by
`Plan::runs_commands`): `-delete` and the file-writing actions spawn no child
and need no stdin policy, so a batch request needs `child_stdin` only when it
also runs a command.

A limit, broken pipe or SIGINT cancels the remote query. Search's quiet broken
pipe behaviour remains; find writing failure remains an error. On socket loss, a
client reports transport failure and does not replay an already-started query.
The daemon never reports success before output and action completion. Execution
cancellation waits for started commands just as current `-quit` does; it
prevents additional entries and releases the pin afterward.

`-I`, a configured live default and information-only find need no catalog or
daemon. They retain the existing local source; a daemon cannot accelerate a live
directory enumeration by pretending it is an index query. Pure indexed find may
still do the live fallbacks in FIND. Explicit per-request cwd lookup and the
existing observed parent handles apply there; the daemon never chdirs.

**D56 A:** find plans with `-exec`, `-execdir`, `-ok`, `-okdir`, `-delete` or
file output execute in the host receiving the request, through the same engine
and find evaluator: batch now, the local client when the socket lands. A
one-shot effectful client opens a query-only resident engine. A daemon can
continue watching changes, but does not execute, approve or proxy commands. This
is the answered host-routing exception to D49's ordinary daemon query rule.

Keep stored default predicates and the per-invocation delete-count correction
(F8 B/F12 D). Do **not** turn an action expression into all-live stat or `-I`.
Keep concurrent actions (F11 A), start sequencing/quit ordering and the shared
entry-output transaction. Delete and execdir use the observed parent handle;
cwd, environment/PATH, stdin, tty and umask are the caller's. File outputs open
in that same context. RPC of a matching path list cannot implement prune,
`-exec` as a Boolean test or an emptied-parent `-delete` correctly.

In JSON/batch mode, child stdout and diagnostics must be tagged without
corrupting framing. Add an execution-context/output adapter to the existing
shared action runner; never another evaluator. Raw CLI mode keeps inherited
stderr/stdin and the existing stdout capture. JSON mode routes child stderr as
byte events too. Batch actions require an explicit local-effects capability;
stdin JSON input requires explicit `child_stdin: "null"`. With file input,
`child_stdin: "inherit"` can use the caller's stdin. `-ok/-okdir` requires an
explicit interactive capability and a terminal; otherwise return a typed
noninteractive error **before any action**, never answer yes automatically.
Prompt serialization and closed command stdin after approval remain unchanged.
Agents can use `-exec` instead. No daemon request is permitted to borrow its own
startup tty/environment as a substitute for the client's.

Search query logging happens once at the originating host (CLI or batch), with
end-to-end timing and separate server work time. Keep D45's typed query text,
never selected result paths, roots or epoch ids; find remains unlogged unless a
later answered brief changes that. Add the promised shared config `log = false`
/ `log = "shape"` with host config parsing, without changing `find_no_ignore`.
Tests/benchmarks isolate XDG and FERRET_INDEX and disable or redirect logs.
Daemon operational status is not another copy of the query log.

M4 byte parity tests use deterministic shallow indexed walks. Existing parallel
find workers can reorder complete records between two identical local runs;
the socket retains that executor and its whole-entry commit ordering rather
than adding sorting or changing the answered F10 parallel semantics. Exact
bytes within each committed record and native statuses remain unchanged.

## Cost model at the reference 10M

All bytes/name below divide by **10,448,739 actual names**. GB means decimal;
GiB/MiB mean binary. D48's **1 GB** line is **953.67 MiB / 95.71 B/name** at
this fixture. Report query engine, attached writer, watcher userspace, kernel
watches, pinned epochs and transient peaks separately. A query-engine goal is
not a total-machine memory cap.

Sources are
[ROADMAP S1+ M3](ROADMAP.md#s1-m3--effective-reader-and-resident-overlays-2026-10-04),
[M6](ROADMAP.md#s1-m6--resident-scopes-and-bounded-file-observations-2026-10-05),
[M7](ROADMAP.md#s1-m7--compaction-and-budgets-2026-10-05),
[R1](ROADMAP.md#s1-r1--input-fallback-and-compaction-phases-2026-10-05),
[R2](ROADMAP.md#s1-r2--complete-attempt-charging-and-in-place-fault-fallback-2026-10-05)
and
[D54's BFS prototype](DECISIONS.md#d54--in-memory-names-interning-postings-row-order).
Existing “cold open” rows are fresh processes with **warm OS cache**, not
storage eviction. The fixture replays observations without a live 10M tree. None
measures a socket, watch installation or end-to-end filesystem freshness.

| Item                                    | Value and evidence                                                                                          | Interpretation for S1b                                                                                                                                                         |
| --------------------------------------- | ----------------------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ |
| Query resident, no D54, clean / 1% / 2% | **Measured:** 603.48 / 699.64 / 796.30 MiB, 60.56 / 70.21 / 79.91 B/name, `2e12d7b` M3 broad resident query | Full loaded reader, without daemon/writer. Estimated initial engine adds 5–20 MiB for bounded host state; no exact daemon RSS claim.                                           |
| Full open, no D54, clean / 1% / 2%      | **Measured:** 709.28 / 1011.98 / 1351.19 ms, peak 634.57 / 729.37 / 823.96 MiB, `2e12d7b` M3                | Query-only engine estimate **0.7–1.5 s**, warm OS cache, before extra D54 build. M7's later name-only opens are 324.22 / 560.63 / 806.94 ms, not substitutes for full open.    |
| Attached writer setup, clean / near 1%  | **Measured:** 3269.35 / 4300.94 ms; current 761.71 / 865.93 MiB, about 76.44 / 86.90 B/name, `0f876e9` M6   | Estimated daemon attach **3.3–4.5 s** before D54/watches; buffers shared. Full writer setup amortised over 1,000 bursts was 3.267 ms/burst, not per-query open cost.           |
| D54 name structures alone               | **Measured prototype:** 18.6 B/row HOME, 9.3 nix, `916825a`; raw BFS 25.7 / 17.5 in its revised README      | **Estimated saving:** roughly 7–8 B/name, 71–82 MiB here, only if duplicate raw storage is released. Corpus and codec differences prevent exact subtraction.                   |
| Query engine with D54, clean / 1%       | **Estimated:** roughly 525–550 / 620–650 MiB, about 53–55 / 62–65 B/name                                    | M3 resident minus 71–82 MiB plus 5–20 MiB host state and uncertainty. Sparse index overhead at 1% must be measured. No D55 saving credited.                                    |
| Full open with D54                      | **Estimated planning range:** 1–4 s query-only, 4–7 s with writer attachment, warm OS cache                 | Full validation plus intern/postings/token construction. The prototype did not measure this integration; time to sort/build is a major uncertainty. No faster startup promise. |
| Resident refresh core                   | **Measured:** clean one file 7.23 ms / 456 B; 100k 1097.65 ms; near 1% one file 6.38 ms, `0f876e9` M6       | Excludes event wait, kernel enumeration/stat/hash and scope construction; crossing limits can cost a checkpoint instead. D54 update cost is additional until measured.         |
| Compaction / oversized rewalk           | **Measured:** 22.451 s median pause, 1626.42 MiB compaction peak; fallback up to 2777.52 MiB, R1/R2         | Query pins and watches can add memory and pacing can add pause. Budget for the actual path, not just a 629 MB snapshot.                                                        |

For query latency, M3's in-process rare name took **8.63 ms**, common `test`
**67.42 ms**, broad listing **526.60 ms**, before D54 and without output I/O.
D54's measured 4.2M prototype reached 0.3–7 ms selective and 7.4 ms worst
scoped; 10M extrapolation is **estimated 1–20 ms selective**, depending on hits
and scope, not a scaled guarantee. Empty replies may be faster than that.

A local connected socket adds an **estimated 0.05–0.3 ms** for scheduling,
framing and a small request/reply, plus JSON/output cost. A new connection/hello
has an **estimated additional 0.1–1 ms**. Both are engineering ranges without
local measurements. Large results are bandwidth/backpressure problems: base64
makes 800 MB of find bytes at least **1.067 GB** before JSON framing. At an
illustrative measured-in-the-future 1 GB/s wire throughput, that alone is 1.067
s; it cannot inherit the 526 ms in-memory scan figure. D57 compares that with a
binary payload's 800 MB. Measure first row, final row, CPU, raw/wire bytes and
throughput, rather than quote small-message latency for listings.

Inotify memory depends on **1,800,947 directories**, not 10M file entries. R8
reports an anecdotal **160–1760 B/watch** range and uses roughly **1
KiB/watch**; these are estimates, not kernel measurements on this build. At 1
KiB, all fixture directories cost **1758.74 MiB / 1.718 GiB** kernel memory;
even 524,288 installed watches cost **512 MiB**. A compact 48 B userspace
locator per installed watch adds **24 MiB** at that cap, excluding basename
strings, root occurrences, maps and allocator overhead. Full coverage's fixed
part is **82.44 MiB**. Measure slab/kernel memory and watch-name storage
separately; directory inode/dentry pinning can raise kernel cost. The 1 GB
query-engine line therefore cannot be sold as a 1 GB always-watched desktop
service.

D55 could change timestamp columns, time-window planning, carry-over and
conversion/compaction costs. Until Dave answers, all estimates include today's
full seconds/nanoseconds and conservative carry key. Exact `-newer`/printf,
same-second writes, restored mtime and clock-skew tests must gate any later
change; S1b makes no ranks-only, ctime-only or racy-time assumption.

## Build slices and review gates

Names below are proposed new files; paths are relative to the repo root. Each
slice updates this design/ROADMAP, passes fmt, clippy with `-D warnings` and the
whole workspace suite, and keeps the real query log unchanged. Test counts never
fall. Keep real APIs/oracles; do not implement test-only planners, watch
reducers, generation handling or fault tables.

| Slice                                                | Files and change                                                                                                                                                                                         | Tests and measurements                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                          |
| ---------------------------------------------------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| **M1 — Resident engine library**                     | `crates/ferret/src/engine.rs`, `lib.rs`, `search.rs`, `find.rs`; query `find/{parse,walk,parallel}.rs` for explicit context/start time; catalog read API as required                                     | Search/find parity with current hosts, two queries share one load, real writer refresh adoption, pinned old query during append/checkpoint, unchanged-sequence stale epoch rejection before dereference, retained/EACCES live fallback. Measure clean/1/2% full open, current/peak RSS, B/name, first-row latency.                                                                                                                                                                                                                                                              |
| **M2 — Batch host and common codec**                 | ferret `src/{batch,protocol,config}.rs`, `args.rs`, `cli.rs`, `json.rs`, `log.rs`, `find.rs`, `search.rs`; `tests/batch.rs`; planned serde_json manifest/graph edge in DESIGN                            | Real indexed fixtures through JSONL: native statuses, invalid/non-UTF-8 argv/output, printf/NUL, bounded output and malformed input, effect framing/explicit stdin/interactive refusal, cwd/trailing-slash cases; GNU comparisons and find-compat adapter. Measure one versus 1,000 queries, open amortisation, codec throughput/RSS; prove no per-query catalog reopen. Local actions already run here.                                                                                                                                                                        |
| **M3 — D54 resident name projection and planner**    | catalog `src/{names,read}.rs`, new `resident_names.rs`, build/compact accessors; query new `name_index.rs`, `query.rs`, `run.rs`, find safe candidate seam; text `src/lib.rs`; bench driver              | Distinct/posting/term output equals flat reference and real full-index oracle after generated create/move/hardlink/ignore/retention/epoch sequences; explicit token versus substring distinctions; planner common/rare scoped cases, count estimates include delta, all prune/quit/depth/action tests unchanged. Measure D54 build/open peak and steady B/name at 10M, 0/1/2% query/update latency, compaction cache rebuild, scoped 10 ms prototype shapes.                                                                                                                    |
| **M4 — Socket host, ordinary clients and lifecycle** | ferret `src/bin/ferretd.rs`, `src/{daemon,client,protocol,xdg,engine}.rs`, CLI query routes; `tests/daemon.rs`; user-unit template `contrib/systemd/ferretd.service`                          | D57 answer gates socket codec; D56 answer gates effectful client routing. Actual socket tests for singleton races, XDG/index separation, missing runtime/spawn denial/F_NO_DAEMON, loading timeout, version drain, cwd/context incompatibility, cancellation/backpressure/native status and no query replay. Query-only host takes no writer lock; writer routing moves to M5. No M4 timing runs (measurements are a separate slice). Verify idle exit/restart. Socket latency, cold attach, codec cost and concurrent RSS measurements belong to a separate measurement slice.                                                                                          |
| **M5a — Writer ownership, intake and backstops** | crawl `src/watch.rs`, retained root-edit/observation seams; daemon writer service and CLI routing; `tests/watch.rs` | Retained writer lock, existing-daemon index/root routing, serial barriers, arm-before-list inotify, debounce/cookies, bounded pending intake, loss backstops, capped coverage/polling, startup/hourly/five-minute timers. Real tree and generated full-index oracle tests; no timing runs. |
| **M5b — Occurrences, policy dependencies and census** | crawl watch mapping and policy seams; daemon status | Bind/alias and D34 multi-occurrence watches; external git/config policy inputs; raw-count census; complete typed status/stat JSON; statfs network/FUSE polling. Implemented; proven occurrences use watches, unproven links and failed inputs poll. |
| **M6 — Queue, controller and compaction admission**  | ferret `src/{scheduler,politeness,daemon,engine,stats,config}.rs`; crawl worker controls and `index.rs` bulk-admission seam; catalog compaction I/O hooks only where pacing needs them; systemd template | Inject signal transitions into the real scheduler/writer: ratchet/drop/battery/unknown probes, continued intake during paused writer, bounded queue collapse, D51 unchanged-sequence retries, arrivals during backstop, memory-deferred fallback keeps generation/caches, protected global transitions abort, output does not block writer. Measure whole paced/unpaced compaction, oldest/newest freshness/backlog drainage, concurrent query latency, 10M 50/90% and faulted fallback peaks with watches/pins, foreground load/cache effects.                                 |
| **M7 — Budgets and host compatibility review**       | bench driver, host/oracle tests, `docs/{S1B,ROADMAP,DESIGN,FIND}.md`; small fixes only where evidence identifies them                                                                                    | All prior find suites unchanged; batch and socket against native CLI/GNU, find-compat output/status/effects/order classification; pure/action CLI gates per D56. Recheck 10M no-change core <=9.5 s unpaced, resident one-file/1% near threshold, D54 query/steady <1 GB goal, actual daemon/kernel/transient totals and D51 freshness. Report misses without weakening decisions. No watcher completeness claim on polling-only roots.                                                                                                                                         |

Before each timing run, check `uptime` and
`pgrep -af 'harness.run|ferret_timing|ignore_timing|synthetic|ferret-bench'`.
One benchmark at a time; isolate all XDG directories, runtime socket and
FERRET_INDEX. Record command, commit, load, cache condition, repetitions and
units. Use the existing synthetic fixture for core comparisons and a real
filesystem tree for watch/syscall/freshness claims. D54 additionally uses the
HOME/nix/nixpkgs prototype shapes; their row distributions are different.

M5a's generated default watch test compares each quiescent publication with a
fresh full production index through search paths and find path/type/size/mtime
records. A modification-only phase cannot be masked by a namespace event. The
longer version is ignored: `FERRET_WATCH_BURST_ROUNDS=1000 cargo test -p ferret
--test watch -- --ignored generated_bursts_long`. Each process/socket/poll has a
deadline; every fixture isolates HOME, XDG, runtime and both indexes.

## Open questions and dependencies

- **D55, Dave's timestamp question:** leave open. Memory/column conversion,
  time-query block pruning and carry-over tests depend on its answer. Other
  engine, host and watcher work proceeds with current timestamps. Do not change
  mtime/ctime precision merely to meet the memory estimate.
- **D56, find actions:** client-local same-engine execution is simplest and
  strongest for cwd/tty/effects, while a resident cooperative service wins small
  selective action queries on startup cost. Recommendation A, pending Dave.
  Batch local execution and read-only daemon queries are independent.
- **D57, socket encoding:** a common JSONL codec is simplest, binary frames
  avoid base64/framing on bulk output. Recommendation A, pending Dave. Its brief
  also records the required batch JSON-parser dependency edge; no new engine
  crate is proposed.

No decision here reverses D26, D29, D31, D34, D37, D46, D51, D52 or D53. D54
explicitly supersedes the old conditional “no name index below 1 GB”. D56
identifies its proposed D49 host exception instead of quietly implementing one.
The stale pre-S1+ daemon paragraph in DESIGN is replaced by this shared engine
contract; historical cold-read and snapshot numbers remain measurements, not
current host architecture.
