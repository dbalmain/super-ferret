# R9 — Is semantic search possible at 1M files / 10-200GB on a desktop?

**Framing note**: this project is scoped lexical-first (boolean/phrase/regex,
Ferret-style — Dave wrote the original Ferret, the Ruby port of Lucene, so this
is written at peer level on the IR side). Semantic search is not core scope; the
question is curiosity-driven feasibility: _"I'd like to know if semantic search
is even possible at the scale we're talking."_ The deliverable here is therefore
a **verdict with arithmetic, plus a recommendation on whether it belongs as an
optional plugin over a chosen subdirectory rather than over the whole corpus** —
not an advocacy piece for building it.

Dave has also said he'd trade search speed for a denser index: _"I'd go for
slightly slower search performance for a more efficient denser index."_ That
preference is load-bearing throughout this report. Vector indexes are large
relative to a lexical index, so every comparison below leads with **GB of index
per GB of corpus** (a ratio against the lexical index), not an absolute GB
figure — "the vectors cost 6x what the entire inverted index costs" is the
sentence meant to land, not "the vectors are 176GB."

Scope: single Linux personal workstation, ~1M files (under ~5M), tens to a few
hundred GB. Content mix, in priority order: source code + text/config first,
then PDF/Office, then email, then media metadata. Two hardware scenarios
throughout: **CPU-only** (modern many-core desktop CPU) and **CPU + consumer
GPU** (assume something like an RTX 4070/4080-class card — 12-16GB VRAM — since
that's the realistic "decent GPU" on a workstation, not a datacenter A100).

Contrast target: Dave's other project (`~/w/lab`, soon `~/w/kb`) plans semantic
search over "thousands of notes." That's 3-4 orders of magnitude smaller than
this corpus. Where the two problems diverge is most of the point of this report.

**Licence note carried through this report**: every model or library named below
has its licence recorded at first mention. Permissive (MIT/Apache/BSD) is
preferred per the brief; several high-quality embedding models carry
non-commercial or bespoke research licences, which matters even for a personal
tool if Dave ever open-sources it or redistributes model weights.

---

## 1. Sizing the problem

### 1.1 Chunk count

Chunking convention for RAG/semantic search typically uses 256-512 token chunks
with 10-20% overlap for prose, and function/class-level chunks (highly variable
size) for code [estimated-arithmetic — this is standard practice, not a measured
figure; see e.g. LangChain/LlamaIndex docs for the convention, no single
canonical source].

Assume mixed corpus, average ~4 chars/token (English prose) to ~3.5 chars/token
for code-heavy text. At 200GB of _extracted text_ (not raw file bytes —
PDFs/Office files have large non-text overhead, so extracted text is
meaningfully smaller than the file size; assume 30-60% text yield after
stripping images/formatting/binary structure) [estimated-arithmetic]:

- 200GB raw files → ~80-120GB extracted text (assumption: 40-60% yield)
  [estimated-arithmetic]
- 100GB text ≈ 100 × 10⁹ bytes ÷ 4 bytes/token ≈ **25 billion tokens**
  [estimated-arithmetic]
- At 384 tokens/chunk with 15% overlap (~326 effective new tokens/chunk): 25B ÷
  326 ≈ **77 million chunks** [estimated-arithmetic]
- At 512 tokens/chunk with 15% overlap (~435 new tokens/chunk): 25B ÷ 435 ≈ **57
  million chunks** [estimated-arithmetic]
- At 1024 tokens/chunk: 25B ÷ 870 ≈ **29 million chunks** [estimated-arithmetic]

**Range to carry forward: 30-80 million chunks** for the 200GB/upper-bound case.
For the 10GB lower-bound case, divide by 20: **1.5-4 million chunks**. This is
the single most important number in this report and it swings 20x across the
stated size range — every downstream number should be read as "per this range,"
not as a point estimate.

Code needs separate treatment: source files chunk far more densely per byte
(short lines, little overlap value, often chunked by function/class boundary
rather than token count), and a code-heavy corpus will land toward the higher
end of the chunk-count range even at lower total byte counts. Email is
drastically smaller per-message than PDFs and often chunks 1:1 with the message.

### 1.2 Vector store cost — the table

Bytes per vector at each dimension/precision (dim × bytes/component, no index
overhead yet):

| Dim                         | fp32 (4B) | fp16 (2B) | int8 (1B) | binary (1 bit) |
| --------------------------- | --------- | --------- | --------- | -------------- |
| 384 (MiniLM, bge-small)     | 1536 B    | 768 B     | 384 B     | 48 B           |
| 768 (bge-base, nomic-embed) | 3072 B    | 1536 B    | 768 B     | 96 B           |
| 1024 (bge-large)            | 4096 B    | 2048 B    | 1024 B    | 128 B          |
| 1536 (OpenAI ada/small)     | 6144 B    | 3072 B    | 1536 B    | 192 B          |
| 3072 (OpenAI large)         | 12288 B   | 6144 B    | 3072 B    | 384 B          |

[estimated-arithmetic — pure dimension×byte-width multiplication, no per-vendor claim]

Now multiply by chunk count. Using the **57M-chunk midpoint** (512-token
chunking, 200GB case) and the **3M-chunk midpoint** for the 10GB case, raw
vector bytes only (before HNSW graph overhead, which adds 20-40% more — see §3):

| Dim / precision | 10GB corpus (~3M chunks) | 200GB corpus (~57M chunks) | GB vector-store per GB raw corpus |
| --------------- | ------------------------ | -------------------------- | --------------------------------- |
| 384-fp32        | 4.6 GB                   | 88 GB                      | ~0.44                             |
| 384-int8        | 1.2 GB                   | 22 GB                      | ~0.11                             |
| 384-binary      | 0.14 GB                  | 2.7 GB                     | ~0.014                            |
| 768-fp32        | 9.2 GB                   | 176 GB                     | ~0.88                             |
| 768-int8        | 2.3 GB                   | 44 GB                      | ~0.22                             |
| 768-binary      | 0.29 GB                  | 5.5 GB                     | ~0.027                            |
| 1024-fp32       | 12.3 GB                  | 234 GB                     | ~1.17                             |
| 1024-int8       | 3.1 GB                   | 58 GB                      | ~0.29                             |
| 1024-binary     | 0.38 GB                  | 7.3 GB                     | ~0.037                            |
| 1536-fp32       | 18.4 GB                  | 350 GB                     | ~1.75                             |

[estimated-arithmetic throughout — chunk-count × bytes-per-vector from the table
above; the last column is the same numbers expressed as a ratio to raw
corpus size, per Dave's stated preference for leading with density/GB-per-GB
rather than absolute size]

### 1.3 Compare to the lexical index — the headline ratio

A Tantivy/Lucene-style inverted index over the same corpus typically runs
**10-30% of the raw text size** including postings, positions, and term
dictionary [estimated-arithmetic — commonly cited range for BM25-style
indexes with positional postings, not a hard measurement here]. For 100GB of extracted
text that's roughly **10-30GB** for the lexical index — i.e. a lexical-index-to-corpus
ratio of **~0.1-0.3 GB/GB**, an order of magnitude denser than most of the vector
rows above.

**This is the number that should lead every comparison in this report: at
768-fp32, the vector store for the same corpus is 176GB against a 10-30GB
lexical index — the vectors cost roughly 6-18x what the entire inverted index
costs, for the same corpus.** Even at 384-dim int8 (a small, quantized model),
the vector store (22GB, ratio ~0.11 GB/GB) is comparable to or larger than the
lexical index ratio. Only **binary quantization at 384-dim** (2.7GB, ratio
~0.014 GB/GB) undercuts the lexical index outright — roughly an order of
magnitude _denser_ than the lexical index itself, which is the one configuration
in this table that actually satisfies "slightly slower search for a denser
index" rather than fighting it.

**Plainly stated: for anything above 768-fp32, the vector index is not an
addition to the search engine, it is the dominant storage cost of the whole
system, several times over.** This is the first hard number that should shape
the design: full-precision, high-dimensional embeddings across the whole corpus
are not a "nice to have" cost, they can double-or-more the total disk budget the
user signed up for with a "lexical index over 200GB" mental model.

---

## 2. Embedding throughput — the actual gate

### 2.1 Published throughput numbers

Hard, sourced numbers are surprisingly scarce for exactly this workload
(long-document chunk embedding, not short-query embedding), so treat the
following as directional:

- **bge-small-en-v1.5** (384-dim): on a mid-range consumer GPU (RTX 5060 Ti
  class) ~**255 chunks/sec**; on NVIDIA A100 (batch 32) ~**467 embeddings/sec**
  [third-party-benchmark, "The Best Local Embedding Model for RAG" benchmark
  post, https://adityarajsingh.com/best-local-embedding-model/]. No credible
  CPU-only chunks/sec figure for this exact model was found in this search pass
  — see Done-note.
- **bge-large / e5-large** (1024-dim): ~**30 chunks/sec** on the same RTX 5060
  Ti-class GPU [third-party-benchmark, same source as above] — roughly 8x slower
  than bge-small on the same hardware, consistent with the larger transformer
  body plus longer sequence handling, not just the wider output vector.
- Intel's own post on CPU-optimized embedding (Optimum Intel + fastRAG) claims
  meaningful CPU speedups over vanilla PyTorch/ONNX for BERT-class encoders but
  the post does not give an absolute chunks/sec figure comparable across
  hardware [vendor-claimed, https://huggingface.co/blog/intel-fast-embedding —
  direction only, not a number to build a budget on].
- No dedicated, current (2025-2026) chunks/sec numbers were found in this pass
  for nomic-embed-text, gte, jina-embeddings-v3, Qwen3-Embedding, or
  EmbeddingGemma under ONNX Runtime/llama.cpp/candle on CPU. This is a real gap
  — see Done-note — and the arithmetic below should be treated as order-of-
  magnitude, not a plan to schedule against.

**Licences for the models named above** (record and check before shipping
anything, especially if weights are ever redistributed):
**bge-small/base/large-en-v1.5** — MIT
[https://huggingface.co/BAAI/bge-small-en-v1.5], fully permissive, matches
Dave's stated preference. **nomic-embed-text** — Apache-2.0 [paper:
https://arxiv.org/pdf/2402.01613; weights and training code released
under Apache-2], also permissive. **all-MiniLM-L6-v2** — Apache-2.0 (sentence-
transformers/Hugging Face convention for this model family). **gte** (Alibaba) —
Apache-2.0 for most releases, check the specific checkpoint.
**jina-embeddings-v3** — CC-BY-NC-4.0 for the v3 weights on Hugging Face at time
of writing (Jina's commercial-use embeddings have historically required a
separate licence/API key) — **not permissive**, would need explicit checking
before any commercial or redistributable use, though fine for pure
personal/local use. **Qwen3-Embedding** — Apache-2.0 (consistent with Qwen's
general model-release licensing). **EmbeddingGemma** (Google) — Gemma's custom
licence, which is permissive for use but carries Google's specific Gemma terms
(usage restrictions, not a pure OSI licence) — read the Gemma terms before
assuming it behaves like Apache/MIT. None of these licence claims were
independently re-verified against the current model card in this pass beyond the
search snippets above — treat as a strong lead, confirm on the actual Hugging
Face model card before depending on it.

### 2.1a Code embeddings are a different problem from prose embeddings

This corpus's stated priority order is source code + text/config first, ahead of
PDF/Office and email — so whether general-purpose embedding models are any good
on code matters more here than in a typical RAG-over-documents writeup.

The honest answer is **mixed, and skews toward "surprisingly not bad for general
text embedders, but a wide gap remains to code-specialized or large proprietary
models."** A benchmark using CodeSearchNet-style natural-language-to-Java-code
retrieval found large proprietary/commercial embedding models performing very
well — **Voyage Code-3 at 97.3% MRR / 95% Recall@1**, **OpenAI
text-embedding-3-small at ~95% MRR**, **Cohere v3 at ~92.8% MRR** — while the
original **CodeBERT model scored only 11.7% MRR / 6.5% Recall@1** on the same
task [third-party-benchmark, summarized via
https://modal.com/blog/6-best-code-embedding-models-compared — the specific
benchmark methodology (Java code, natural-language queries) was not
independently re-verified in this pass]. Separately, other reporting notes that general-purpose
text embeddings "perform fairly well in code search especially in Python, even compared
to code-specific embedding models," attributing this partly to broad pretraining
corpora that already include large amounts of GitHub code [third-party-benchmark
summary,
same source]. GraphCodeBERT and newer contrastively- trained models (e.g.
CodeCSE, arXiv:2407.06360) improve meaningfully over CodeBERT specifically by
incorporating data-flow structure or contrastive objectives, but none of these
were shown beating the strongest general-purpose or code-specific commercial
models on the cited benchmark.

**What this means for the plugin recommendation below**: (a) the _old_
CodeBERT-style code-specialized models are not a safe default — they
underperformed badly on the cited benchmark; (b) modern general-purpose
embedders (bge/nomic/gte/Qwen3-Embedding class, all seen in the wild trained on
mixed web+code corpora) are a more defensible default than reaching for a
bespoke code model, consistent with the "text embeddings do fine on code"
finding; (c) semantic search over code is fundamentally a different retrieval
problem than semantic search over prose — code similarity is often
structural/API-shape similarity that a natural-language embedding space captures
only partially — so lexical/regex/symbol search (grep, ctags-style, AST-aware
search) should remain the **primary** tool for code in this project regardless
of what a semantic layer can do, and semantic code search should be scoped as an
experimental enhancement, not a code-search replacement. This reinforces
recommendation #2 in the Verdict below: prioritize semantic indexing for
prose-heavy content (PDF/Office/email) over code, both because that's where
semantic retrieval's paraphrase- tolerance actually earns its keep and because
the code-quality evidence here is thinner and more mixed than the prose case.

### 2.2 First full-embed wall-clock time — show the arithmetic

Using bge-small at ~250 chunks/sec (GPU, rounding the 255 figure above) and the
~57M-chunk 200GB case:

- 57,000,000 chunks ÷ 250 chunks/sec ≈ 228,000 sec ≈ **63 hours ≈ 2.6 days of
  continuous GPU time** [estimated-arithmetic, throughput figure
  is third-party-benchmark, chunk count is estimated-arithmetic]

For a larger model (bge-large/e5-large, ~30 chunks/sec GPU):

- 57,000,000 ÷ 30 ≈ 1,900,000 sec ≈ **528 hours ≈ 22 days of continuous GPU
  time** [estimated-arithmetic]

CPU-only, absent a solid measured chunks/sec figure, the honest thing to do is
bound it by a plausible slowdown factor rather than invent a number. Published
cross-hardware comparisons for BERT-class encoder inference commonly show
CPU-vs-consumer-GPU slowdowns in the **5-15x** range for batch inference of this
size class [estimated-arithmetic — a bracket, not a measurement; no single
apples-to-apples CPU vs GPU chunks/sec benchmark for these specific embedding
models was located]. Applying that bracket to the small-model GPU figure:

- CPU-only, bge-small: 63 hours × 5 to 63 hours × 15 ≈ **13-40 days of
  continuous CPU time** [estimated-arithmetic, low-confidence — built on
  an assumed slowdown factor, not a measured one]

Even at the low end of that bracket, CPU-only first-embed of a 200GB mixed
corpus is **multiple weeks of continuous compute** for anything past the
smallest embedding models. GPU brings it down to "a long weekend" for a small
model, or "the better part of a month" for a large one. **Neither number is
"runs overnight."** For the 10GB / ~3M-chunk case, divide everything above by
~19: bge- small on GPU becomes ~3.3 hours, CPU-only ~0.7-2 days — genuinely
tractable.

This is the load-bearing conclusion of this section: **the corpus-size boundary
between "runs overnight" and "runs for weeks" sits somewhere between 10GB and
200GB, likely in the 20-50GB band**, not at 200GB. Dave's `~/w/lab` (thousands
of notes, almost certainly well under 100MB of text) is so far below this
boundary that embedding cost is a non-issue there — full-corpus embedding
finishes in seconds to minutes regardless of model choice or hardware. That is
the entire shape of "where it breaks": the lab project never sees the wall this
report is about.

### 2.3 Incremental re-embedding

Desktop file corpora churn continuously (edits, new mail, new PDFs). Incremental
cost is proportional to _changed_ chunks, not corpus size, which is the saving
grace: a normal day's worth of file changes (tens to low-thousands of files)
re-embeds in seconds to low minutes even CPU-only, using the same per-chunk
throughput figures above. The problem is exclusively the **first full index**,
plus any wholesale re-embed forced by a model upgrade (which is effectively "run
the whole job again," see §2.2 — model upgrades on a 200GB corpus are not a
light decision).

### 2.4 Energy/thermal reality

No sourced power-draw numbers for this exact workload were found in this pass.
Order-of-magnitude reasoning only [estimated-arithmetic]: a consumer GPU under
sustained inference load draws roughly 150-300W; 63 hours at ~200W average ≈
12.6 kWh for the small-model 200GB embed — trivial as an electricity cost (well
under $2 at typical residential rates) but _not_ trivial as sustained thermal
load on a workstation, and outright impractical on a laptop, where 60+ hours of
sustained GPU load will thermal-throttle, drain battery faster than AC can
replenish under heavy combined CPU+GPU load on some laptop PSUs, and audibly run
the fans continuously for days. **This is a workstation task, not a laptop
task**, independent of whether the laptop technically has a GPU.

---

## 3. ANN index structures

| Structure                 | Mechanism                                                                                                                                 | Build time                                                                  | Query-time memory                                                                  | Disk footprint                                                 | Recall/latency                                                                                                                                                                                                                                                                                                        | Update/delete                                                                                                                                                                  | Rust availability                                                                                                                               |
| ------------------------- | ----------------------------------------------------------------------------------------------------------------------------------------- | --------------------------------------------------------------------------- | ---------------------------------------------------------------------------------- | -------------------------------------------------------------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ | ----------------------------------------------------------------------------------------------------------------------------------------------- |
| **HNSW**                  | Multi-layer proximity graph, greedy search from top layer down                                                                            | O(n log n), M/efConstruction-dependent; slow at 50M+                        | Fully RAM-resident in nearly all implementations — this is the defining constraint | ~vector bytes + graph edges                                    | Very good recall/latency at small-to-mid scale; degrades in build time and RAM at 100M+                                                                                                                                                                                                                               | Insert supported; delete is usually tombstone/mark-only, real compaction is rare and expensive                                                                                 | `hnsw_rs`, `instant-distance`, `usearch` (Rust bindings), `hnswlib` (C++, has bindings)                                                         |
| **IVF / IVF-PQ / IVF-SQ** | Coarse quantizer (k-means) partitions space into cells; search probes nearest cells; PQ/SQ compress residuals                             | Requires a training pass over sample data before building                   | Much lower than HNSW at same recall — codes are small                              | Small (PQ codes)                                               | Recall/latency worse than HNSW at same memory unless well-tuned (nprobe)                                                                                                                                                                                                                                              | Better delete support than HNSW (cell-based), still not free                                                                                                                   | Faiss itself is C++/Python; Rust bindings exist but are thinner than for HNSW-family crates                                                     |
| **DiskANN / Vamana**      | Single-layer graph built to be disk-friendly; frequently-visited vertices cached in RAM, rest read from SSD on demand                     | Build is CPU/RAM-heavy even though the _query-time_ index lives on disk     | Small RAM footprint by design — graph lives mostly on SSD                          | Full index on SSD, sized like the vector data plus graph edges | On billion-scale benchmarks, competitive recall at low query latency when tuned; but "high latency for queries due to limited DRAM and limited parallelism" is a documented weakness relative to SPANN [third-party-benchmark comparison, see SPANN paper below]                                                      | Original DiskANN was largely build-once; later work (SPFresh, arXiv:2410.14452) specifically targets incremental in-place update as an open problem DiskANN did not solve well | `diskann` crate exists but is far less mature/used than hnswlib-family crates; expect to build against the Microsoft C++ DiskANN or reimplement |
| **SPANN**                 | Inverted-index-style: only cluster **centroids** live in RAM; large posting lists live on disk, closest analog to a lexical postings list | Similar training cost to IVF (k-means-like clustering)                      | Very low — only centroids in RAM                                                   | Posting lists on disk, sized close to raw vector data          | "SPANN significantly outperforms DiskANN in both recall@1 and recall@10 especially in the low query latency budget (<4ms), and is more than two times faster than DiskANN to reach 95% recall" [paper-reported claim as summarized in later citing work; SPANN paper: https://arxiv.org/pdf/2111.08566, NeurIPS 2021] | SPFresh (arXiv:2410.14452) is explicitly the incremental-update follow-on to SPANN, implying vanilla SPANN's update story was originally weak too                              | No mainstream Rust crate found; Microsoft's reference implementation is C++                                                                     |
| **ScaNN**                 | Anisotropic vector quantization tuned to minimize error on the _actual_ inner-product objective, not generic reconstruction error         | Google-internal-grade tooling, less turnkey outside GCP/TF ecosystem        | Compact, quantization-based                                                        | Compact                                                        | Strong on ann-benchmarks.com/Glove-type benchmarks historically [ann-benchmarks]                                                                                                                                                                                                                                      | Largely a static/batch structure — not built for high delete/update rates                                                                                                      | No native Rust; Python/C++ library, would need FFI                                                                                              |
| **NGT** (Yahoo Japan)     | Graph-based (ANNG/ONNG), similar family to HNSW                                                                                           | Comparable to HNSW                                                          | RAM-resident                                                                       | Similar to HNSW                                                | Competitive on ann-benchmarks in some categories [ann-benchmarks]                                                                                                                                                                                                                                                     | Has delete support, historically stronger than raw HNSW here                                                                                                                   | No mainstream native Rust binding found                                                                                                         |
| **Annoy** (Spotify)       | Random-projection forest of trees                                                                                                         | Fast build, but static — designed to be built once and queried, not updated | RAM or mmap-able, read-only                                                        | Small                                                          | Adequate recall, generally out-competed by HNSW-family on modern benchmarks                                                                                                                                                                                                                                           | **No update/delete support at all** — the whole reason it is now considered legacy for a live index                                                                            | Not relevant to a live desktop index for exactly this reason                                                                                    |

**The scale-relevant conclusion for this report: HNSW-family structures are the
natural first choice because of Rust maturity and query latency, but they are
RAM-resident by construction.** At the upper end of this corpus (57M-88M+ chunks
depending on dimension/precision — see §1.2), a RAM-resident HNSW graph plus
vectors is asking for tens to a couple hundred GB of RAM depending on dimension
and quantization, which is a real but not absurd workstation spec (128-256GB RAM
workstations exist) — but it is **not** the same casual assumption as "the
lexical index lives on disk and gets mmap'd." DiskANN/SPANN exist precisely
because HNSW's RAM requirement stops being casual well before a billion vectors,
and this corpus's upper bound (tens of millions of chunks) is already in the
range where that tradeoff starts to bite, especially if the workstation is also
running everything else Dave uses it for.

### 3.1 Quantization — the two developments that change the answer

- **Binary quantization + Hamming rescoring**: pack each dimension to 1 bit,
  compare with Hamming distance (a handful of CPU instructions per comparison),
  then rescore the top candidates against full-precision vectors for final
  ranking. From the vector-store table in §1.2, binary quantization at 384-dim
  cuts the raw vector store from 88GB (fp32) to 2.7GB (binary) for the
  200GB-corpus case — a **32x** reduction, matching the fp32→binary byte-width
  ratio exactly (32:1) [estimated- arithmetic, direct consequence of
  the byte-width table].
- **RaBitQ** (SIGMOD 2024): a randomized quantization method using a
  Johnson-Lindenstrauss rotation that quantizes D-dimensional vectors to **D-bit
  strings with a theoretical error bound** rather than the heuristic error of
  naive binary quantization; the paper reports recall over 95% typically
  achievable with 100 or fewer reranking candidates on datasets over 1M vectors
  [paper-reported, RaBitQ, SIGMOD
  2024, https://dl.acm.org/doi/pdf/10.1145/3654970;
  code: https://github.com/VectorDB-NTU/RaBitQ-Library]. An extended version
  (SIGMOD 2025) improves further [paper-reported,
  https://github.com/VectorDB-NTU/Extended-RaBitQ]. This matters because it makes
  the extreme end of compression (binary-per-dimension) _usably accurate_ rather
  than a lossy hack that only works for easy queries.
- **Matryoshka embeddings** (truncatable dimensions, trained so that a prefix of
  the vector — e.g. the first 256 of 1024 dims — is itself a valid, usable,
  lower-quality embedding): lets a system store the full vector once and choose
  a smaller "view" of it for a coarse first pass, then use the fuller vector for
  reranking, without needing two separately-trained models or two separate
  stores. Combined with binary/RaBitQ-style quantization, this is the practical
  shape of a "cheap first pass, expensive rerank" pipeline within a _single_
  embedding artifact.

**Together, RaBitQ-style binary quantization plus a rerank-on-candidates step is
the single most important lever for making the storage table in §1.2
survivable**: instead of choosing between 88GB (fp32) and being stuck with it,
or accepting binary quantization's older reputation for poor recall, RaBitQ
specifically targets and closes that recall gap with a bound the older ad-hoc
binary methods didn't have.

### 3.2 Desktop-embeddable vector stores

| Store                        | Embeddable / single-binary?                                                                          | Rust?                                                                | Index type(s)                                                        | Demonstrated scale ceiling                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                          |
| ---------------------------- | ---------------------------------------------------------------------------------------------------- | -------------------------------------------------------------------- | -------------------------------------------------------------------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| **LanceDB / Lance format**   | Yes — designed as an embedded columnar format, similar spirit to Parquet/SQLite for vectors          | Core is Rust                                                         | IVF-PQ primarily                                                     | Marketed for large-scale multimodal datasets; no independent >100M-vector desktop benchmark found in this pass — treat scale claims as vendor-claimed until checked                                                                                                                                                                                                                                                                                                                                                                                                                 |
| **Qdrant**                   | Has a library/embedded mode as well as its normal server mode                                        | Yes, written in Rust                                                 | HNSW (with optional sparse-vector/BM42 support)                      | Widely benchmarked in server deployments; embedded-mode scale ceiling specifically for desktop use is not independently documented in this pass                                                                                                                                                                                                                                                                                                                                                                                                                                     |
| **Chroma**                   | Embedded-first design (SQLite-backed historically)                                                   | No (Python/Rust core is newer, ecosystem still mostly Python-facing) | HNSW                                                                 | Chroma has historically been positioned for small-to-mid scale (dev/prototype), not the 50M+ vector range                                                                                                                                                                                                                                                                                                                                                                                                                                                                           |
| **Milvus Lite**              | Yes, a lightweight embedded mode of Milvus exists specifically for this                              | Milvus core is Go/C++, not Rust                                      | IVF, HNSW depending on config                                        | Milvus proper is built for server-scale; Lite mode's independent scale ceiling wasn't found in this pass                                                                                                                                                                                                                                                                                                                                                                                                                                                                            |
| **usearch**                  | Yes — header-only/single-library, explicitly designed to be embedded                                 | Rust bindings exist                                                  | HNSW                                                                 | Used by several downstream projects at moderate scale; no independent billion-vector desktop measurement found here                                                                                                                                                                                                                                                                                                                                                                                                                                                                 |
| **sqlite-vec**               | Yes, a SQLite extension — about as embeddable as it gets                                             | C (loadable extension), usable from Rust via SQLite bindings         | **Brute-force** (no ANN index as of the v0.1.0 stable release)       | Explicitly documented as struggling above ~1M vectors for high-dimensional data because it's brute-force, not ANN [third-party-benchmark, https://alexgarcia.xyz/blog/2024/sqlite-vec-stable-release/index.html and node-vector-bench: 1M×128d build 3,957-4,589ms, query 33-35ms — usable, but this is brute-force scan cost, not sublinear ANN search, and will scale roughly linearly with corpus size]. **This directly caps sqlite-vec out of contention for the 30-80M-chunk upper end of this corpus** unless paired with a pre-filter that shrinks the candidate set first. |
| **DuckDB VSS**               | Yes, single-binary analytical engine                                                                 | DuckDB core is C++, not Rust                                         | HNSW extension                                                       | Reported to "nearly OOM a development machine" at 500K vectors × 512d [third-party-benchmark, https://media.patentllm.org/news/database/duckdb-lance-vector-search-sqlite-benchmarking-postgresql-va-20260705] — DuckDB is built to consume all available RAM for OLAP workloads, which is a bad fit for a background desktop-search daemon that must coexist with everything else running on the box                                                                                                                                                                               |
| **pgvector / pgvectorscale** | No — requires a running PostgreSQL server, not embeddable in the single-binary sense this tool wants | No                                                                   | IVF-Flat, HNSW; pgvectorscale adds DiskANN-inspired StreamingDiskANN | pgvectorscale specifically claims billion-scale-oriented improvements, but running a Postgres server is a deployment-model mismatch for a single-user desktop CLI/daemon tool, independent of its scale ceiling                                                                                                                                                                                                                                                                                                                                                                     |

**Bottom line for this table: none of the embeddable options has an
independently-verified, non-vendor-claimed demonstration of
tens-of-millions-of-vectors performance on commodity desktop hardware.**
sqlite-vec and DuckDB VSS are demonstrated to struggle well below this corpus's
chunk count. LanceDB and usearch are architecturally the most plausible
candidates (Rust-native or Rust-bindable, purpose-built for embedding,
IVF-PQ/HNSW respectively) but their scale claims at this range are vendor
assertions, not verified third-party results, in what this search pass found.

---

## 4. Hybrid retrieval

### 4.1 BM25 + dense fusion

**Reciprocal Rank Fusion (RRF)** — combine two independently-ranked lists
(lexical, dense) by scoring each document `1/(k + rank)` per list and summing —
is simple, needs no score normalization (which is otherwise a real headache:
BM25 scores and cosine-similarity scores live on incomparable scales), and is
the default approach in most production hybrid systems today. Learned fusion (a
small model trained to combine per-list features) can beat RRF but adds a
training/tuning burden that is hard to justify for a single-user desktop tool
with no query-log corpus to train on.

### 4.2 Learned sparse retrieval — SPLADE, uniCOIL, BM42

This is flagged by the brief as possibly the most useful finding here, and the
research bears that out, with an important caveat.

**The core claim is right in principle**: SPLADE-family models produce a sparse,
weighted bag-of-terms representation (each output dimension corresponds to a
vocabulary term, with a learned weight, rather than a dense semantic-space
coordinate). That representation is a **sparse vector**, and a sparse vector is
exactly what a normal inverted index already stores and searches efficiently —
it is architecturally a drop-in extension of the postings-list machinery a
lexical search engine already has, not a separate ANN subsystem. Qdrant runs a
public benchmark of SPLADE-encoded MSMARCO (8.8M passages) specifically as
sparse vectors [https://github.com/qdrant/sparse-vectors-benchmark], and SPLADE
is reported to outperform BM25 on most BEIR datasets
[paper/third-party-benchmark summary; verify against the original SPLADE
and BEIR papers before citing a specific number].

**The caveat, and it's a real one: BM42 — Qdrant's own attempt at a cheaper
attention-based sparse scheme, explicitly pitched as a lighter alternative to
SPLADE for exactly this "reuse the inverted index" idea — was shown by
independent scrutiny to be broken.** Jo Bergum (Vespa) pointed out that Qdrant's
chosen benchmark dataset (Quora, a duplicate-question dataset) was a poor fit
for evaluating retrieval quality, and that even on Qdrant's own published
numbers BM42 underperformed a plain Elasticsearch BM25 baseline (0.90 vs 0.85
recall@10, i.e. **worse**, not better) [third-party-benchmark
scrutiny, summarized
in https://buttondown.com/ainews/archive/ainews-qdrants-bm42/
and https://x.com/Nils_Reimers/status/1809334134088622217]. Qdrant's own
follow-up acknowledged the discrepancy and downgraded BM42 to "an experimental
approach, which requires further research and development before it can be used
in production" [vendor-claimed
retraction, https://github.com/qdrant/qdrant/issues/4628 discussion thread
and Qdrant's public correction].

**So: the architectural insight (sparse learned representations fit inside an
inverted index) is sound and SPLADE itself has real, less-contested evidence
behind it. But the specific implementation most directly marketed at Dave's
exact use case (small, cheap, drop-in sparse retrieval for hybrid search)
shipped with a benchmark that didn't hold up, and its own vendor walked it back
within months.** Do not adopt BM42 on the strength of its launch post. SPLADE
proper is worth prototyping — it fits the "we already have an inverted index"
architecture Dave is building toward — but budget time to independently verify
recall/precision on a held-out sample of his actual corpus before trusting it,
exactly per Qdrant's own advice after the BM42 episode ("please don't trust us,
always check performance on your own data").

SPLADE's practical cost for a desktop tool: it still requires running a
transformer forward pass per document at index time (same throughput constraints
as dense embedding, §2) and per query at search time (adds latency the plain
lexical path doesn't have) — it is not free, it just changes _where_ the
resulting representation is stored and searched.

**Given this is now the highest-value candidate for a Ferret-shaped,
from-scratch inverted index (per the coordinator's steer), quantify the index
bloat properly rather than waving at it:**

- A measured figure for SPLADE (splade-cocondenser-ensembledistil, MS MARCO):
  documents activate **~127 non-zero terms on average**, and a 100K-document
  sparse index built from that comes to **93 MB** [third-party-benchmark,
  via the NAVER/community discussion summarized at
  https://github.com/naver/splade/issues/71 and
  https://europe.naverlabs.com/blog/splade-a-sparse-bi-encoder-bert-based-model-achieves-effective-and-efficient-first-stage-ranking/].
  That's **~930 bytes/document** for the sparse posting entries — for comparison,
  a typical BM25 document in a Lucene/Tantivy-style index touches on the order of
  100-300 _actual_ distinct terms (natural English vocabulary, not model-expanded),
  so SPLADE's term-expansion roughly **doubles-to-quadruples the number of postings
  entries per document** relative to plain lexical indexing [estimated-arithmetic
  —
  the 100-300 baseline is a common rule-of-thumb for natural- language
  documents, not independently measured here; treat the multiplier
  as directional].
- The mechanism for the bloat is term expansion: SPLADE assigns weight to
  related vocabulary terms the document's actual text never contains (that's the
  whole point — it's what gives it paraphrase-tolerance a pure BM25 posting list
  doesn't have), and every one of those synthetic terms becomes a new
  postings-list entry. The SPLADE paper's own regularization (an L1-style FLOPS
  penalty during training) exists specifically to cap this expansion, because
  unconstrained sparsity blows the index up further and slows search by
  lengthening posting-list traversal [paper-reported mechanism, original
  SPLADE line of work, SIGIR 2021 and follow-ups; independent discussion of the
  posting-list-length/efficiency tradeoff summarized in the arXiv:2606.26441
  GPUSparse paper, which frames higher posting-list-length variance as
  improving effectiveness at the direct cost of retrieval efficiency].
- **Scaled to this corpus**: 930 bytes/doc-equivalent × ~57M chunks (200GB case,
  §1.1) ≈ **~53 GB** of additional sparse postings [estimated-arithmetic,
  linear scaling from the MS MARCO measurement — MS MARCO passages are
  short (~55-60 tokens average); this project's chunks are 2-10x longer
  (256-1024 tokens), so this 53GB figure is very likely an
  *undercount*, plausibly by the same 2-10x factor, i.e. real cost could land
  anywhere from ~50GB to ~500GB depending on chunk length and how aggressively
  FLOPS-regularization is tuned]. Set against the plain lexical index (~10-30GB for
  this corpus, §1.3), **a naively-tuned SPLADE layer could roughly double-to-quintuple
  the total index size** even though it lives inside the same postings-list data
  structure — the "reuses what you already have" framing is architecturally true
  but not size-free, and the honest range here is wide enough that it needs measuring
  on Dave's actual corpus, not assumed from this arithmetic.
- **Verdict on SPLADE specifically**: the architectural fit
  (posting-list-native, no separate ANN structure, no GB of dense float vectors)
  is real and is the most actionable idea in this whole report for a
  from-scratch Ferret-style index — but it is not "free density." A
  tightly-regularized SPLADE variant tuned for low average non-zero terms
  (closer to BM25's own term count than the 128-term MS MARCO default) is what
  would actually deliver on the "denser than dense vectors, barely bigger than
  lexical" promise; an off-the-shelf SPLADE checkpoint used without retuning
  that budget risks costing more index space than either binary-quantized dense
  vectors (§3.1) or the plain lexical index it's meant to complement.
  **Licence**: the original NAVER SPLADE checkpoints are released for research
  use — check the specific checkpoint's licence file before any redistribution
  (NAVER's release terms have historically been closer to
  research/non-commercial than a clean Apache/MIT grant; verify per-checkpoint
  rather than assuming).

### 4.3 Late interaction — ColBERT / ColBERTv2 / PLAID

ColBERT-family models keep a vector **per token**, not per document/chunk, and
compare query and document token vectors at search time (MaxSim). This gives
strong retrieval quality but the storage cost scales with **total token count in
the corpus**, not chunk or document count — a fundamentally worse scaling law
for this workload than single-vector-per-chunk embeddings.

Quantified: ColBERTv2's aggressive residual compression gets per-token-vector
storage down to 20-36 bytes (1-2 bit compression) from an uncompressed 256 bytes
per token vector, and the compressed MS MARCO index (8.8M passages) comes to
**16-25 GiB** versus **154 GiB** uncompressed [paper-reported, ColBERTv2 paper,
https://arxiv.org/pdf/2112.01488; PLAID engine paper,
https://arxiv.org/pdf/2205.09707]. Scale that ratio to this corpus's ~25 billion
tokens (§1.1) rather than MS MARCO's much smaller token count and the number becomes
enormous — MS MARCO's passages average roughly 55-60 tokens each across 8.8M passages
(≈500M tokens total), so this corpus at ~25B tokens is roughly **50x** MS MARCO's
token volume. Scaling the 16-25 GiB compressed figure linearly: **~800GB-1.25TB**
for a compressed ColBERT-style index over this whole corpus [estimated-arithmetic,
linear
extrapolation from the cited MS MARCO figures — token-count scaling
is architecturally linear per the ColBERT papers, but this specific
extrapolation is not itself a measured result]. **That rules out full-corpus ColBERT-style
indexing on a desktop outright** for this corpus size; it only makes sense as a reranking
step over a lexically- or dense-pre-filtered small candidate set (hundreds to low
thousands of chunks), not as the primary index.

### 4.4 Reranking as the cheaper alternative

Retrieve cheaply (lexical BM25, or a coarse/quantized dense pass), then rerank
the top-k (k in the tens to low hundreds) with a cross-encoder or small LLM.
This is the approach that best respects a desktop tool's actual latency budget:

- A cross-encoder forward pass over a (query, candidate) pair is roughly
  comparable in cost to a single embedding forward pass (§2's throughput
  figures) — call it low-single-digit milliseconds per pair on GPU, higher on
  CPU, though no exact cross-encoder-specific number was independently verified
  in this pass (see Done-note).
- Reranking 100 candidates at, generously, 5ms/pair on GPU ≈ 500ms; on CPU,
  using the 5-15x slowdown bracket from §2.2, ≈ 2.5-7.5 seconds
  [estimated-arithmetic, low confidence].
- **Against a ~200ms interactive budget, GPU reranking of a modest candidate set
  (tens, not hundreds) is plausible; CPU-only reranking of any meaningful
  candidate set is not**, unless the candidate count is kept very small (single
  digits to low tens) or the rerank is allowed to run asynchronously and update
  results after the fast lexical pass has already rendered something.

This is the shape most compatible with a lexical-first desktop tool: instant
lexical results, then a GPU-accelerated (or small-candidate-set CPU) rerank pass
that refines ranking within the interactive window, rather than trying to make
dense retrieval itself fast enough to be the first-paint path.

---

## Verdict

**The headline number: for a straightforward dense-vector setup (768-dim, fp32),
the vector store costs 6-18x what the entire lexical inverted index costs, for
the same corpus (§1.3).** That ratio, not any absolute GB figure, is the fact
that should drive the design decision, given the stated preference for a denser
index over a faster one.

**Is full-corpus semantic indexing of ~1M files / tens-to-a-few-hundred GB
possible on a desktop? Technically yes, with the right compromises. Is it what
you should build as a default, always-on part of this project? No.** The
better-argued answer, and the one this report recommends: **semantic search
belongs as an optional plugin over a chosen subdirectory or content-type filter,
not as a mode over the whole corpus** — a curated 1-10GB slice (a project's
docs, a PDF archive, an email folder) rather than the full 200GB tree. At that
scale the entire feasibility picture in this report inverts: first-embed drops
from days/weeks to minutes/hours (§2.2, scale linearly down), the vector store
drops to low-single-digit GB even at fp32 (§1.2), and HNSW's RAM-residency (§3)
stops being a consideration at all. **A well-argued "no, not over 200GB, but yes
over a curated 2GB subset, at this cost" is the answer this report lands on** —
not a recommendation to build full-corpus semantic indexing and hope the
compromises hold.

Specifically, for the full-corpus case (the one to avoid as a default):

- **Storage**: full-precision (fp32/fp16) dense vectors at 768+ dimensions
  across the whole corpus cost more disk than the lexical index itself, by a
  factor of 5-18x (§1.3). This alone should rule out "just embed everything at
  bge-base fp32" as a default.
- **Time**: first full embed of the corpus is measured in **days on GPU, weeks
  on CPU-only** for anything beyond the smallest embedding models (§2.2). This
  is not an "index it overnight" feature for the 200GB end of the range; it is a
  background job the user starts and waits on for a long time, or a job that
  only ever covers a subset.
- **Index structure**: HNSW (the mature, Rust-available choice) wants to live in
  RAM; at the upper end of this corpus's chunk count (tens of millions) that's a
  real, if not impossible, RAM commitment, and none of the disk-native
  alternatives (DiskANN, SPANN) has solid Rust tooling today (§3).
- **The desktop-embeddable vector stores that exist today** (sqlite-vec, DuckDB
  VSS) are independently documented to struggle well below this corpus's scale;
  the more promising ones (LanceDB, usearch) haven't been independently verified
  at this scale in what this pass found (§3.2).

**Compromises that make it work, most-recommended first:**

1. **Binary quantization (ideally RaBitQ-style) + rerank on full-precision
   candidates.** This is the single highest-leverage move: it cuts the raw
   vector store by ~32x (§3.1) and RaBitQ specifically closes the recall gap
   that made naive binary quantization unattractive before 2024. Combine with
   Matryoshka-style truncatable embeddings if the chosen model supports them, so
   the coarse pass and the rerank pass share one embedding artifact instead of
   two.
2. **Don't embed the whole corpus by default.** Prioritize by content type and
   use: documents/PDFs/ Office/email (where semantic search earns its keep —
   paraphrase-tolerant retrieval over prose) over source code (where
   lexical/structural search — grep, regex, symbol search — is usually both
   cheaper and more precise; semantic code search is a different, harder problem
   this report doesn't need to solve to hit the brief). This alone can cut the
   effective corpus by more than half for a mixed source-code-heavy tree.
3. **On-demand / lazily-expanding index rather than eagerly indexing everything
   upfront.** Index directories or file types as the user actually searches them
   semantically, rather than paying the multi-day upfront cost for content that
   may never be queried semantically at all.
4. **Prefer learned sparse retrieval (SPLADE, not BM42) as a cheaper middle
   ground** where it fits — it reuses the inverted index infrastructure the
   lexical engine already has instead of standing up a separate ANN subsystem,
   at the cost of a transformer pass per document (comparable to dense embedding
   cost) and independent verification against Dave's own corpus before trusting
   recall claims (§4.2).
5. **Treat ColBERT-style late interaction as a reranker over a small candidate
   set only, never as the primary index** — its storage cost scales with token
   count, and at this corpus's token volume a full late-interaction index would
   run into the terabyte range (§4.3).

**Plugin shape, concretely**: expose semantic indexing as a separate,
explicitly-opted-into subsystem scoped to a path
(`ferret semantic-index ~/docs/research/`) or a content-type filter
(PDF/Office/email only, never `.rs`/`.c`/`.py` by default), entirely separate
from the always-on lexical index over the full tree. This keeps the core lexical
engine's simplicity and disk budget untouched, makes the multi-day embed cost in
§2.2 something the user explicitly signs up for on a subset they've chosen, and
sidesteps the RAM-residency problem in §3 by keeping the vector index small
enough (low single-digit GB per §1.2, for anything up to a few GB of source
text) to be RAM-resident without effort. The lexical query path and the semantic
query path can be exposed through the same CLI/query surface with a flag or a
`~semantic` operator, without either one having to be architecturally aware of
the other beyond a shared result-merging step (§4.1's RRF).

**The tiered proposal:**

- **Tier 0 (default, cheap)**: lexical BM25/regex/boolean — the original scope.
  Instant, small, already solved.
- **Tier 1 (opt-in, targeted)**: dense semantic index limited to non-code
  documents (PDF/Office/ email), embedded at 384-dim (bge-small class) with
  binary quantization + fp32 rerank on the top candidates. Using the 200GB
  corpus's document-only subset (rough guess: half the corpus, so ~100GB raw /
  ~30M chunks at 512-token chunking), the binary-quantized vector store costs on
  the order of **1.5GB** (384-binary from §1.2's per-GB rate) and first-embed
  time on GPU at bge-small throughput is on the order of **33 hours** — a long
  background job, not an overnight one, but a one-time cost. CPU-only, budget
  **1-3 weeks**.
- **Tier 2 (advanced, GPU-gated)**: cross-encoder reranking of the top-k
  lexical+dense candidates for queries where the user opts into "best effort"
  mode, accepting sub-second-to-low-second latency.

**Where the break-even is**: somewhere in the **20-50GB** range of extracted
text is where "run the full embed overnight on a decent GPU" (tractable,
low-friction) turns into "run the full embed over multiple days to weeks" (a
background job the user has to plan around). Below that line, full-corpus
semantic indexing is close to a solved problem with off-the-shelf tools; above
it, the compromises in this verdict stop being optional. **This is exactly why
the contrast with `~/w/lab` holds**: thousands of notes is comfortably under a
few MB to tens of MB of text — three to four orders of magnitude below this
break-even point — so full-corpus semantic indexing there is not a hard
engineering problem at all, while for the 1M-file/200GB target it is the central
engineering problem of the feature.

**What changes this answer in 2 years**: (a) faster small embedding models — the
trend from MiniLM → bge-small → EmbeddingGemma-class models has been toward
better quality at the same or smaller size, which directly cuts §2's throughput
bottleneck; (b) wider availability of RaBitQ-class quantization in mainstream
Rust-embeddable vector stores would remove most of §1.3's storage objection; (c)
a mature, well-benchmarked Rust-native DiskANN/SPANN-family crate would remove
§3's RAM-residency objection, which is currently the biggest structural gap in
the Rust ecosystem for this specific problem; (d) if consumer GPUs commonly ship
with enough VRAM to hold the working set of embedding-model weights and a
meaningful KV/batch buffer simultaneously with everything else running on the
box, the "long weekend of GPU time" figure in §2.2 could plausibly halve. None
of these are close to certain on a 2-year horizon; treat the current verdict as
durable through at least 2027.

---

## Done-note

**What could not be verified in this pass:**

- No solid, current (2025-2026), apples-to-apples **CPU-only chunks/sec**
  benchmark was found for any of the embedding models named in the brief
  (all-MiniLM-L6-v2, bge-small/base, nomic-embed-text, gte, jina-embeddings-v3,
  Qwen3-Embedding, EmbeddingGemma) under ONNX Runtime, llama.cpp, or candle
  specifically. The CPU-time estimates in §2.2 rest on an assumed 5-15x
  GPU-vs-CPU slowdown bracket, not a measurement — this is the single number in
  this report I'd most want to replace with a real benchmark before using it to
  plan a project timeline. If Dave wants a firm number, running
  `sentence-transformers` with ONNX Runtime CPU provider on a representative
  sample chunk set, timed locally, would take under an hour and replace the
  weakest estimate in this report.
- Independent, non-vendor, at-scale (tens-of-millions-of-vectors) benchmarks for
  LanceDB and usearch specifically on commodity desktop hardware were not found.
  §3.2's assessment of them as "the most plausible candidates" is architectural
  reasoning (Rust-native, purpose-built for embedding), not a verified scale
  claim — treat it as a starting point for prototyping, not a decision already
  made.
- Cross-encoder-specific latency numbers (§4.4) were extrapolated from general
  embedding-model throughput rather than sourced from a cross-encoder benchmark
  directly; cross-encoders and bi-encoder embedding models have different
  architectures (joint attention over the pair vs. separate encode-then-compare)
  and the true number could differ meaningfully in either direction.
- The PDF/Office "text yield" fraction used in §1.1 (40-60% of raw bytes become
  extracted text) is a reasonable engineering assumption, not a measured figure
  for any specific corpus — Dave's actual mix of source code vs. PDF vs. email
  could shift the chunk-count range in §1.1 by 2x or more in either direction;
  nothing here should be read as more precise than "tens of millions of chunks,
  give or take a factor of 2."

**Contradictions encountered**: none rising to a real contradiction, but the
"vendor-claimed billions of vectors on a laptop" framing that motivated rule 4
in the brief is directly contradicted by the sqlite-vec (struggles above ~1M)
and DuckDB VSS (near-OOM at 500K) results in §3.2 — those are credible
independent reports of the _opposite_ of the marketing framing for the specific
embeddable, single-binary tools most relevant to this project, even though the
underlying algorithms (DiskANN, ScaNN, etc.) genuinely have been demonstrated at
billion-vector scale in server/cluster deployments with dedicated engineering
the desktop tools examined here don't have.

**The assumption most likely to be wrong in my own analysis**: the 5-15x CPU/GPU
slowdown bracket in §2.2, and by extension every CPU-only wall-clock estimate
downstream of it. It's a plausible order-of-magnitude bracket for BERT-class
transformer batch inference generally, but embedding models specifically (short
forward passes, often well-optimized ONNX INT8 paths on CPU) could easily sit
outside that bracket in either direction — a well-tuned quantized ONNX CPU path
could plausibly beat the low end of that bracket, which would make CPU-only
first-embed of even the 200GB corpus a "days, not weeks" proposition rather than
the "weeks" figure quoted above. This is the number I'd most want an actual
benchmark to correct before treating this report's Tier 1 timeline as a
commitment.
