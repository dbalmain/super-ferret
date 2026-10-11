# Token policy: base64-like runs and timestamps

2026-10-12, branch `wt/tokens` from `wt/d65c` at `65f215f`. Codec 1 and
today's tokenizer remain the default; every policy here is an opt-in
experiment. Work in progress: sections below are filled in as measured.

## Plan

- **T1**: base64-like runs leave the term index; trigrams keep their bytes.
- **T2**: recognised timestamps go to a date index; their time-of-day and
  epoch pieces leave the term index.
- A one-pass census over the frozen corpus counts terms and hapaxes under
  each policy, false suppressions, epoch false positives and date-index
  shapes at second, minute, hour and day keys.
- The D65 harness re-runs codec 1 and C4b under T1, T2 and T1+T2 against
  today's codec-1 control.

## Method

(pending)

## Results

(pending)

## Semantics

(pending)
