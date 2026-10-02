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
