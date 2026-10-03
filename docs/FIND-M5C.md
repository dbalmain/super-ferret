# Parallel find idioms

M5c keeps sibling order free and concurrent `-exec … ;`, while making each
effectful start complete before the next begins. Read-only starts may overlap. Each ordinary `-exec … {} +`
action owns one shared argument batch for the run. Directory-local batches
retain their directory boundary semantics. Full shared batches detach under
the lock and run outside it; collection proceeds while commands run.

An entry commits all its output together. Child stdout is drained while the
command runs; large records spill to an unlinked temporary file rather than
growing memory without bound. Quit commits the winning entry under the output
lock and discards entries completing later. Already started actions finish,
and collected batches flush at exit (DECISIONS option A).

Acceptance includes real-walker action and quit stress tests, overlapping-start
deletion against GNU, workspace gates, twelve timing rows against bfs and M5b,
a many-start comparison, and one final full ferret-harness corpus run. The
current results are in `/home/dave/w/super-ferret/.ai/find-m5c-done.md`.

## Checked functional checkpoint

Seven new CLI tests drive both catalog/live walkers, including repeated header
and contents grouping, a single wc total, a single quit winner, started-child
completion, large spill output, matching file/stdout quit winners, and GNU's
overlapping-start deletion result. All workspace gates pass: 397 passed and
four pre-existing ignored, zero warnings. The GNU oracle and catalog/live
expression self-check pass.

The shared mutex and sequential starts have real measured costs. DECISIONS.md
contains the timing brief and bounded staging proposal; that extension remains
open under Dave's stop/write-up instruction. The fifteen-sample read-only table
passes all twelve inherited live-versus-default-bfs gates, while seven default
medians exceed paired M5b by 0.37–2.30%. A speed-complete milestone is not
claimed. Default's shallow catalog row was already slower than bfs in M5b.
The one final full corpus is complete; details and timing loads remain in
the done-note. The code and binary stayed frozen during that run.

## Cheapest next batching experiment

If the measured cost is accepted as a reason for the extra state, stage at most
32 paths or 4 KiB per task and action, then append that chunk under the shared
batch lock. The shared batch alone partitions arguments and spawns commands.
Keep its existing argument-limit and process-serialization rules. Merge every
staged chunk when its task finishes, including after quit, then perform one
shared exit flush after all workers finish. This preserves minimal invocations,
bounded staging and collected-argument quit semantics; no worker spawning batch
is reintroduced. Measure it against both name-selected and all-file probes
before keeping it. This proposal is not implemented in the frozen candidate.

For 368 small starts, retaining one pool avoids repeated thread startup, but
each root still drains its descendant barriers before the next starts. Fewer
workers or caller-thread handling for narrow roots is a separate measurement;
reintroducing concurrent starts would break the deletion idiom again.

## Final corpus

One final run, 135,693 rows in 2,026.076 seconds, same five trees and nine scored
matrix rows as M5b, jobs 4. Start load 2.36/1.98/2.30; no competing compiler or
benchmark. The supplied release binary remained frozen; no harness source edit.

| Target | agree | agree-unordered | raw differ | skipped | both-timeout | order-only | concurrency-only | real differ | error |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| default | 52618 | 301 | 18 | 7368 | 3 | 17 | 1 | 0 | 0 |
| -I | 67086 | 300 | 28 | 7965 | 6 | 27 | 1 | 0 | 0 |

Raw differences fell from 190 to 46 across 18 command IDs (formerly 53).
146 old row scopes disappeared; two new legal-order scopes involve unseparated
size output and single-batch binary cat argument order. All 46 rows have current
individual explanations in the done-note and m5c/classifications.json, using
M5b's ID-to-[class, explanation] format. Nine both-timeouts are inconclusive.

The common-idiom partition/interleaving/quit/start-race explanations are gone.
Remaining shared echo/cat aggregates differ by free argument order; mixed
outputs with newline-free delimiters differ under legal whole-entry order;
unseparated printf/tail records exceed the bounded witness; quit can choose a
different legal entry or reach a symlink error branch first. The two remaining
concurrency rows are the explicitly allowed single-walk followed-alias rm/delete
races. No raw difference is claimed as harness-proved agreement.

The four functional fixes are checked. Speed completion remains open under the
measured-cost brief; this milestone is not claimed fully complete. All gates
pass, 397 tests pass and four pre-existing tests remain ignored. Binary SHA256:
`b75f73d455f816a42d04e66e8a9353d63af07a7f16e2a82eaba78744c4230a5a`.

## Revised follow-up in progress

Dave accepted the read-only timing noise and corrected the measured-cost brief:
full shared batches must run outside the lock, and only effectful starts need
sequencing. Both corrections are implemented with targeted tests. The existing
effectful classifier now includes all file outputs. New measurements, seed and
one fresh full corpus will replace the initial candidate's results above.
