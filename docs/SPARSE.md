# Sparse grams: regex candidate experiment

Status: implementation committed and gates green; full-corpus measurement
running. Bench-only indexes; product formats and planning remain unchanged. No network sources used.

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

The run uses the refreshed D65C catalog at `/tmp/d65c/catalog`, its codec-1
fully merged control at `/tmp/d65c/control`, and frozen bytes at
`/tmp/d65c/corpus`. `Snapshot::open` verifies incarnation and DocId bound.
The checked-out S3 document describes an isolated `$HOME` catalog but has no
concrete catalog/snapshot paths; the supplied fresh D65C paths supersede it.
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
by the byte index. Eleven new tests drive extraction, derivation and bounded encoding together.

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
checksums. The benchmark writes no corpus index files. Trigrams and hash cap 8 use
a whole map; other arms use bounded first-byte ranges. Generic map build CPU is a prototype cost, not a production builder
projection. Frequency training is a separate one-time corpus pass.

Measurement and final recommendation remain pending. The implementation
checkpoint passes pinned formatting, workspace clippy with warnings denied,
and **939 tests / 13 ignored**, with zero Rust warnings (baseline 928 passed;
eleven added). No Cargo manifest or lockfile changes.

## Baseline measurement checkpoint

D65C compatibility checks passed: **112,382 live**, **112,376 readable**,
**6 unreadable**, **3,978,812,718 content bytes**, **250 queries** (50 per class).
The complete oracle costs 506.34 CPU s / 513.06 wall s. Shipped candidates
include every oracle match. Rare-literal candidate bytes median/p90 are
**1,540,658,366 / 1,861,211,679**, against oracle **4,033,515 / 22,288,165**.
The rare class's readable-byte false share is **99.52%** (sum-based, not the
ratio of independent medians). Identifier candidate bytes median/p90 are
**2,783,209,101 / 3,097,586,912**, against oracle **36,785,558 / 238,940,235**.

Product trigram full-merge accounting: **10,158,272 distinct keys**,
**259,877,638 postings**, **228,353,108 postings bytes**, **56,655,667 dictionary
bytes**, **286,112,775 total bytes**, **22.31 CPU s / 22.60 wall s**. Frequency
training costs **3.93 CPU s / 4.09 wall s** over **3,978,700,343 bigram
occurrences**, **52,900 distinct bigrams**. Experimental variants remain running.

The unbounded hash cap-32 attempt reached 208.9M keys and caused heavy swapping
(about 38 GB RSS, machine swap 20 GB against 8.7 GB initially). It was stopped;
its incomplete timing is discarded. All remaining arms now use 32 disjoint,
contiguous first-byte ranges and one persistent segment writer in global key
order, retaining only query postings after each range. A fixture calls the real
document inverter and compares whole encoded segments byte for byte for all
seven variants. Exact whole-index bytes are preserved.

Bounded sparse extraction scans forward only from starts in its current range,
until an endpoint at least as heavy as the first would block further grams.
Its worst case is `cap` pairs per selected start. It agrees exhaustively with
the monotonic stack and with the definition, including ties and length caps.
Extra corpus passes are included in build CPU. The 32-way builder is a memory
tradeoff, so build times compare complete prototype layouts, rather than solely
weight functions. A comma-separated variant label can resume individual arms.

The bounded implementation passes all gates: **939 passed / 13 ignored**, zero
Rust warnings. The remaining five variants are being restarted serially on the
same snapshot and class seeds; their oracle is recomputed, not inferred.

First sparse result (hash cap 8): **116,535,553 keys**, **799,093,308 postings**,
**882,968,921 postings bytes**, **1,664,977,098 total bytes**, **634.51 CPU s**.
Rare candidate bytes median/p90 fall to **28,488,102 / 1,391,555,674**;
identifier to **2,673,273,947 / 3,056,592,693**. Every oracle match survives.
The 54.1× rare median reduction does not remove the tail; total storage rises
5.82× and the sum-based rare false-readable share remains 97.62%.

The resumed oracle reproduces all 20 shipped rows exactly. Its CPU/wall cost is
518.00/672.81 s; the wall difference follows cold reads after the aborted build.
The optimized trigram replay is byte-identical (22.72 CPU s), and frequency
training is 3.99 CPU s. The bounded cap-32 build has completed 12/32 ranges
without the former memory pressure.

An additional cheaper control is being added: all trigrams plus fixed 4-grams
whose bytes form valid UTF-8 alphanumeric/underscore characters. Both index
and query use that substring-local predicate, and every rejected long gram
falls back to trigrams. It is therefore sound for punctuation, partial UTF-8,
and Unicode case alternatives. It measures whether concentrating long keys
on word literals removes much of the full raw-byte dictionary cost. The new
control's regression and measurements are pending; checkpoint is WIP.

Hash cap 32 is complete and passes all 250 oracle checks. It uses
**268,757,928 keys**, **3,722,459,430 total bytes**, and **1,154.94 CPU s**.
Its headline candidate medians/p90s equal cap 8 in every class; doubling the
length cap has not bought headline selectivity. Frequency cap 8 is running.

The word-only control also includes a 4+5-gram arm. Every M5 rare literal has
at least five bytes, so this tests fixed interior coverage where sparse
endpoint choices can leave a short literal with only three- or four-byte keys.
Word-only 4-grams remain the smaller control and cover four-byte identifier
ends. Both retain all raw trigrams and apply the same local predicate to query
keys, including fallback for punctuation and partial UTF-8. No whole-word
boundary is required: regex substrings inside an identifier remain candidates.

Frequency cap 8 passes all 250 oracle checks. It has **114,965,290 keys**,
**1,527.20 MiB total index**, **794.63 MiB postings**, and **726.48 CPU s**.
Rare candidate MiB median/p90 are **300.14 / 1,464.51** (worse than hash), while
identifier is **2,319.50 / 2,867.31** and code class **1,723.12 / 2,739.59**.
Rare false-readable share remains **98.24%**. Frequency weighting has traded
some rare-literal selectivity for modest gains in the other literal classes.

A further regression records a limitation of the strict tie rule: the required
literal `aaaaa` emits only `aaa` under either sparse weight or cap, admitting
a document containing just `aaa`. The word 4+5 control rejects that false
candidate through the real extractor and required-literal derivation. Repeated
pairs cannot use absolute-offset tie breaks without invalidating soundness.
The new word/tie tests await final gates (expected 941 passed / 13 ignored).
