# D65 harness arms under token policies (aggregates)

2026-10-12, /tmp/d65c refreshed catalog and frozen corpus, compared with the plain codec-1 control. Steady state unless noted; CPU is follow + steady merge. Produced by d65c.py --tokens and .ai/summarize.py (private).

| arm | steady MB | saving | full MB | follow+steady CPU s | ratio | distinct | singletons | df1/df2/absent p90 | mid med (x) | common med (x) |
| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |
| control | 450.28 | 0.0% | 398.12 | 269.05 | 1.000 | 18990510 | 19412375 | 0.14/0.15/0.12 | 1.88 (1.00) | 26.26 (1.00) |
| c4b | 317.95 | 29.4% | 305.18 | 284.17 | 1.056 | 2687154 | 0 | 0.45/0.39/0.21 | 2.84 (1.51) | 27.88 (1.06) |
| control-t1 | 221.21 | 50.9% | 200.82 | 275.95 | 1.026 | 11445463 | 10027304 | 0.10/0.11/0.10 | 1.66 (0.88) | 25.94 (0.99) |
| c4b-t1 | 193.82 | 57.0% | 188.81 | 298.65 | 1.110 | 2234224 | 0 | 0.19/0.21/0.16 | 2.68 (1.43) | 27.08 (1.03) |
| control-t2 | 442.63 | 1.7% | 389.94 | 273.37 | 1.016 | 18209876 | 17961081 | 0.13/0.12/0.11 | 1.95 (1.04) | 26.43 (1.01) |
| c4b-t2 | 310.71 | 31.0% | 298.71 | 294.50 | 1.095 | 2663722 | 0 | 0.22/0.26/0.18 | 2.90 (1.54) | 27.35 (1.04) |
| control-t1t2 | 215.62 | 52.1% | 192.62 | 285.31 | 1.060 | 10664815 | 10128495 | 0.09/0.11/0.09 | 1.65 (0.88) | 25.31 (0.96) |
| c4b-t1t2 | 189.94 | 57.8% | 182.97 | 305.00 | 1.134 | 1747767 | 0 | 0.16/0.19/0.14 | 2.69 (1.43) | 26.43 (1.01) |

| arm | mid warm median / p90 ms (steady) | mid median (full) |
| --- | --- | --- |
| control | 1.88 / 5.49 | 1.71 |
| c4b | 2.84 / 15.20 | 2.65 |
| control-t1 | 1.66 / 5.57 | 1.58 |
| c4b-t1 | 2.68 / 14.26 | 2.53 |
| control-t2 | 1.95 / 5.54 | 1.73 |
| c4b-t2 | 2.90 / 14.32 | 2.64 |
| control-t1t2 | 1.65 / 5.47 | 1.52 |
| c4b-t1t2 | 2.69 / 14.07 | 2.54 |

Policy changes against the control (steady; first and full identical). Every dropped row was checked: the document holds the term under plain tokenization and not under the policy. No arm returned an extra row.

| policy | df1 | df2-100 | df1-natural | df2-100-natural | absent | mid | common |
| --- | --- | --- | --- | --- | --- | --- | --- |
| control-t1 | 42 q / 43 rows | 39 q / 212 rows | 20 q / 20 rows | 21 q / 161 rows | 0 q / 0 rows | 24 q / 11227 rows | 42 q / 23417 rows |
| control-t2 | 18 q / 18 rows | 3 q / 6 rows | 3 q / 3 rows | 0 q / 0 rows | 0 q / 0 rows | 9 q / 2008 rows | 1 q / 2894 rows |
| control-t1t2 | 60 q / 61 rows | 42 q / 218 rows | 23 q / 23 rows | 21 q / 161 rows | 0 q / 0 rows | 24 q / 13424 rows | 42 q / 26393 rows |
| c4b-t1 | 42 q / 43 rows | 39 q / 212 rows | 20 q / 20 rows | 21 q / 161 rows | 0 q / 0 rows | 24 q / 11227 rows | 42 q / 23417 rows |
| c4b-t2 | 18 q / 18 rows | 3 q / 6 rows | 3 q / 3 rows | 0 q / 0 rows | 0 q / 0 rows | 9 q / 2008 rows | 1 q / 2894 rows |
| c4b-t1t2 | 60 q / 61 rows | 42 q / 218 rows | 23 q / 23 rows | 21 q / 161 rows | 0 q / 0 rows | 24 q / 13424 rows | 42 q / 26393 rows |

S2 churn, C4b under t1t2: every size and count line identical to C4b without a policy (merge CPU 43.67 s vs 43.75 s). The trace's synthetic terms hold no base64 runs or timestamps.
