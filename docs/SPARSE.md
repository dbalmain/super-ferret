# Sparse grams: regex candidate experiment

Status: design committed; implementation and measurement pending. Bench-only
indexes; product formats and planning remain unchanged. No network sources used.

## Design and soundness

Question: do variable-length grams reject M5's false regex candidates cheaply
enough to justify a larger index?

A gram has at least three bytes. Its two endpoint bigrams must each outrank all
interior bigrams. Every trigram qualifies vacuously. Use ASCII-folded raw bytes
before weighting and keying, exactly as `ferret_text::trigram_key` does. Compare
fixed bigram hash and inverse corpus occurrence frequency, caps 8 and 32. Ties
use a local, deterministic ordering; absolute document offsets cannot enter a
weight because they would invalidate the substring argument.

A required literal is necessary for the regex, never sufficient. Reuse Cox's
existing HIR fold, preserving its AND/OR structure, with longer exact and edge
facts for the experiment. Extract grams only inside each literal. Every gram
then reappears in any containing document: its predicate inspects only its own
folded bytes. Outside endpoints cannot make a gram required. OR retains every
branch; weakening AND by selecting rare children preserves completeness.

Retain trigrams: deleting a covered trigram from the index would lose queries
for that shorter literal. Query-side omission is safe when a retained longer
gram implies it. Include fixed-length literal grams as a cheaper control.

## Measurement plan

Prefer the refreshed D65C population and its frozen bytes over S3's old corpus.
Reuse M5 class seeds, including its snapshot-derived identifier patterns, and
also measure the other `screen::regex_classes` workloads. Compare actual shipped
four-child trigram candidates against each variant. Encode whole-corpus posting
lists with the segment codec, reporting singleton inlining separately. Report
distinct keys, build CPU, candidate and oracle document/byte median and p90,
and the same file-size bands. Check every oracle match against every variant.
Only aggregates are recorded; no corpus names or text.

Initial free disk: about 11 GB. Stop below 5 GB. Run only one foreground
benchmark at a time after process preflight. Keep checkpoints at 20–30 minutes.

## Decision brief (pending results)

- Shipped trigrams: no new storage; high false-candidate read volume.
- Sparse grams: extra extraction and storage; potentially selective long keys.
- Fixed longer grams: simpler extraction; potentially higher postings volume.

Recommendation and the fact that would change it await measurements.

## Implementation checkpoint

`ferret-bench sparse <catalog> <paired-index> <snapshot>` constructs all seven
layouts serially: trigrams through longer literal facts, hash caps 8/32,
frequency caps 8/32, and fixed 4/8 plus trigrams. The shipped planner supplies
a separate real four-child baseline. The optimized product trigram inverter
also supplies build CPU and full-merge bytes, separate from the generic map
builder used for experimental layouts.

The shared Cox fold now has two fact consumers in `ferret-verify`: existing
trigrams retain every original cap (including two-byte edges); the bench API
returns folded literal AND/OR facts, retaining 31-byte edges and exact strings
up to 32 bytes before release. No product index or planner code changes.
Unicode case equivalences stay HIR alternatives, rather than being normalized
by the byte index. Nine new tests drive extraction and derivation together.

Hash weights use a fixed SplitMix64 avalanche of the folded pair number.
Frequency weights use corpus occurrence counts, with rarer pairs ranked higher.
Both rank by `(weight, folded pair number)`; repeated identical pairs have equal
rank. Equal endpoints are permitted, but every interior must be strictly lower.
The stack removes equal ranks after emitting their pair, so repeated ties do
not create arbitrary long grams. Each pair is pushed once and popped once;
at most two grams are emitted per bigram. Caps filter emissions, not the stack.

Indexing keeps every emitted distinct key per document. Queries flatten AND,
rank children by posting frequency, retain four, and keep every OR branch.
Whole-index accounting runs the real segment writer to a sink, including its
front-coded dictionary, relative DocIds, PFor blocks, singleton inlining and
checksums. The benchmark keeps one index in RAM at a time and writes no corpus
index files. Generic map build CPU is a prototype cost, not a production builder
projection. Frequency training is a separate one-time corpus pass.

Measurement and final recommendation remain pending. The implementation
checkpoint passes pinned formatting, workspace clippy with warnings denied,
and **937 tests / 13 ignored**, with zero Rust warnings (baseline 928 passed;
nine added). No Cargo manifest or lockfile changes.
