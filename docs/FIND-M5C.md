# Parallel find idioms

M5c keeps sibling order free and concurrent `-exec … ;`, while making each
start operand complete before the next begins. Each ordinary `-exec … {} +`
action owns one shared argument batch for the run. Directory-local batches
retain their directory boundary semantics.

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
The one final full corpus is in progress; details and timing loads remain in
the done-note. The code and binary stay frozen during that run.

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

## Corpus checkpoint

The single final run uses M5b's five trees and nine scored matrix rows, jobs 4,
and the supplied release binary. Start load 2.36/1.98/2.30, with no competing
compiler or benchmark. At 86,704/135,693 rows, 26 raw differences are classified:
24 order-only and two explicitly allowed single-walk alias races; zero harness
errors. The nine both-timeouts are inconclusive. Final totals and every row's
current explanation will replace this checkpoint when the run completes.

Shared echo/cat aggregate rows can still differ by legal argument order.
Mixed outputs with newline-free delimiters can still differ when the scorer
splits lines, although each entry group is now adjacent. Those are current
order-only explanations, rather than the old worker-batch and action-interleave
explanations. No wc/grep/ls/file partition, overlapping-start deletion or
multiple-print quit race has appeared.
