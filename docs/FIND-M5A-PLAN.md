# Find 5a implementation plan

Default find traverses catalog children in catalog order, using stored stat
fields and raw entry counts. Only opaque directories and explicitly ignored
starts use the live source. `-I` retains its existing live behavior. Parent /
child ordering, depth ordering and prune remain guarantees; sibling GNU order
is no longer a guarantee, including for quit and effects.

Implement metadata access behind Entry so predicates and formatted output share
one policy. Measure missing fields and action disappearance before choosing a
shape. Update snapshot regressions and sorted-output self-checks. Validate the
seed and full corpus, classify order differences, measure serial warm timing
against fd, run all workspace gates and rebuild release. Record progress and
measurements in /home/dave/w/super-ferret/.ai/find-m5a-done.md.
