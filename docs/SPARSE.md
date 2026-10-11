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
