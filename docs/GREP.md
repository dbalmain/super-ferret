# `ferret grep`

Contract for `ferret grep [OPTIONS] PATTERN [PATH...]`, following ripgrep
15.2.0, also the grep-compat oracle pin. Dave reproduced key cases on 15.1.0;
that corroborates those cases, not a second oracle pin. **Contract updated 2026-10-16 against answered D72–D75 and the merged scan
work (2026-10-13–15).** D62
requires this CLI; [FIND.md](FIND.md), D47 and [S3.md](S3.md) set its indexed
model. Anything not promised here is not promised. D72–D75 are answered in
[DECISIONS.md](DECISIONS.md).

## Modes and scope

| Input | Source |
| --- | --- |
| Recursive directory, default `.` | Pinned catalog and S3 trigram postings; current bytes for candidates |
| Explicit file, file symlink, or `-` | Direct current-byte search, without opening an index |
| `--no-ignore`, `-u`/`-uu` | Unrestricted live walk, without opening an index |
| `--live` (D73 A+D, M6) | Current tree with ferret policy; catalog supplies root anchors, not names/postings |

Resolve indexes as find does: `ferret --index DIR`, `FERRET_INDEX`, then XDG
data. Missing/incompatible indexes or unindexed directory starts exit 2 with
index/live advice. Mixed operands use their respective sources. Help/version
skip index/config; no query log or find config. Diagnose nonempty
`RIPGREP_CONFIG_PATH`: pass flags explicitly. Refuse global `ferret --json`
framing; `ferret grep --json` means rg output.

For content search, implicit stdin is selected only with **no path operands**
and readable piped stdin; otherwise no operands means `.`. **M3 clarification
(2026-10-17):** `--files` defaults to `.` even with piped stdin. Its positionals
are all paths, and explicit `-` lists `<stdin>` without reading it. Every explicit directory/file is searched
regardless of the pipe. Explicit `-` adds stdin alongside other operands; it
does not replace them. Resolve inputs before index opening. Streams/explicit
files scan directly; postings add cost and miss unindexed input. Never delegate
to rg. If `-f -` consumes stdin as patterns, it is unavailable as search input.

## Surface and tiers

Rank by distinct rg repositories, drops included; mixed-tool combinations do
not set the order. T1 is the useful minimum, T2 output/matching, T3 names, T4
completion. Accept rg long aliases, attached values, clusters, repeated flags,
`--` and documented override order. Validate before output; unbuilt tiers exit 2.

| rg rank | Flag | Repos | Tier / milestone |
| ---: | --- | ---: | --- |
| 1 | `-n` | 1,102 | T1 / M1 |
| 2 | `-g` | 1,031 | T1 / M1 |
| 3 | `-q` | 826 | T1 / M1 |
| 4 | `-i` | 543 | T2 / M2 |
| 5 | `-o` | 516 | T2 / M2 |
| 6 | `-l` | 496 | T2 / M2 |
| 7 | `-v` | 433 | T2 / M2 |
| 8 | `-F` | 423 | T2 / M2 |
| 9 | `--files` | 399 | T3 / M3; names only |
| 10 | `--hidden` | 362 | T3 / M3 |
| 11 | `-P` | 337 | Deferred: refuse, use rg for PCRE2 |
| 12 | `-e` | 310 | T2 / M2; OR repeated patterns |
| 13 | `-c` | 276 | T2 / M2 |
| 14 | `-U` | 249 | T4 / M7; multiline |
| 15 | `-r` | 201 | T4 / M8; replacement output, never writes files |

Remaining ranked flags, in ranking order within each milestone:

| Slice | Surface |
| --- | --- |
| M4 | `-m`, `-x`, `-w`, `-S`, `--count-matches`, `-f`, `-s`, `--files-without-match`, `--include-zero` |
| M5 | `--no-heading` (rank 16's label is `--heading-off`), `--no-filename`/`-I`, `-N`, `-A`, `-H`, `-C`, `--pretty`, `-0`, `-B`, `--heading` |
| M6 | `--no-ignore`, `-a`, `-u`, `--max-depth` |
| M7 | `--multiline-dotall` |
| M9 | `--json` (rank 25, postponed for coherent output work) |

M1 adds `--color=never`, help/version and `--scan`/`--scan-uncovered`; M5 adds
colour auto/always/ansi. Support rg aliases (`-.`, `-p`) and negating switches.
Only `-f` contents split into newline-separated alternatives (`-f -` reads
stdin); empty files match nothing, empty lines supply empty patterns.
**M4 probes (2026-10-17):** strip CR only as part of CRLF in pattern files;
a lone final CR and a leading UTF-8 BOM are literal pattern bytes. Invalid
UTF-8 is a pattern-file error. Resolved `--files` ignores `-f` without opening
it, including missing files and stdin. When `-f -` supplies search patterns,
no paths defaults to `.`; explicit search `-` is an error. Pattern-file paths
are byte paths, including attached and long equals spellings. Positional
and each `-e` argument remain intact, including actual LF bytes. Resolve line/
multiline mode, newline validation, `-F` escaping and qualifiers **before**
shared compilation/derivation. Without `-U`, literal LF patterns error; with
`-U` they match across lines, including under `-F`. Repeated `-e` patterns OR.

All other flags **refuse**, never pass through or silently disappear:
`-t`, `-T`, `--type-add/list/clear`; `-L`; `-z` (compressed search);
`--no-ignore-vcs/dot/files/global/parent/exclude/messages`; `--encoding`;
`--sort`, `--sortr`; `--passthru`; `--vimgrep`; `--path-separator`;
`--trim`; `--iglob`; `--max-columns`, `--max-columns-preview`; `--column`;
`--engine`, `--auto-hybrid-regex`; `--field-match-separator`,
`--field-context-separator`; `--max-filesize`; `--null-data`;
`--one-file-system`; `--context-separator`; `--binary`; `--hyperlink-format`;
`--no-unicode`; `-b`. Unranked flags, including preprocessor, custom colour,
thread/mmap/buffering/stats/debug/config controls, also refuse. Message:
`grep: FLAG is not supported; use rg for this command`. `--heading-off` is
not an rg spelling and refuses. BRE/ERE switches `-G`/`-E` are not grep
compatibility aliases (`-E` in rg selects encoding).

## Planning and scan policy

Reuse `GrepFlags::compile`: compile, then derive necessary `Trigrams`. Required
literal runs of at least three bytes can narrow after the index's ASCII byte
folding; derivation represents Unicode case alternatives, not Unicode-folded
index bytes. Concatenation ANDs, alternatives OR, required repetitions keep keys. Optional/empty branches can be All;
anchors/classes alone give no keys. Trimming only weakens filtering. Reuse
`GrepAtom`, candidate `Source`/cursors, coverage and name narrowing; S2 terms
cannot prove an arbitrary substring/regex match.

**D72 A (confirmed 2026-10-16):** grep automatically permits scans. `--scan` and its alias
are accepted but unnecessary; `search` retains M > live/4 and uncovered >
10,000 refusal. Short/All/dense patterns and incomplete coverage scan the
selected searchable files.
Estimates choose postings or enumeration, not refusal. No new index structures
or positions; density remains the priority. Trigram indexes are optional.

**Scan update (2026-10-16):** D76 C chose parallel verification before new
screens; the shared scan now has worker-owned candidate work, bracketed
streaming reads and required-run verification. SCAN.md reports roughly
139–154 ms warm and 3.4–4.2 s cold against rg’s 154–178 ms and 5.1–6.1 s on
its corpus after review fixes. These are search/name-output measurements, not
a grep line-output benchmark or a general guarantee. The earlier warning that
no-trigram grep should not be expected to beat rg is superseded by those
measurements. Broad scans are a viable plan, not inherently a fallback failure.
For parallel verification, reuse the D77 C index setting `search_threads`, capped by available parallelism,
with `FERRET_SEARCH_THREADS` override and `min(16, available_parallelism)`
fallback. D78 A keeps resident names built on Engine open for content search;
do not add lazy Engine residency as a grep prerequisite. **M3 clarification
(2026-10-17):** names-only uses the existing lazy Catalog namespace route
instead. Measure grep’s output and first-result costs separately.

After output-mode overrides, the plan explicitly declares its **required
output universe**, independently of candidate matches:

| Output requirement | Name universe / content work |
| --- | --- |
| Ordinary positive matches, `-l`, counts without zero | Positive candidates plus uncovered; emit only matching names |
| `-v` | Every selected searchable name; read lines even for posting-negative files |
| `--files-without-match` | Every selected searchable name; posting absence may establish no match |
| `-c` or `--count-matches` with `--include-zero` | Every selected searchable name; emit a zero record for posting absence |
| `--files` | Selected names, including non-content entries; no content reads |

Enumerate names for zero/negative records; postings cannot supply that universe.
D73 permits snapshot posting absence to establish zero/no-match (including
stale names); explicit/live input must read current bytes. Covered positive
candidates and uncovered names still verify; faults never establish zero. Zero records alone do not make exit 0.
`-v` selects nonmatching **lines**, never `NOT grep:` documents: even a positive
document can contain nonmatches. Under inversion, zero-count shortcuts cannot
use positive posting absence; read the selected lines. Quiet/name modes may
stop early; counts/context finish required work. **M4 implementation
(2026-10-17):** non-inverted zero/no-match modes enumerate all searchable names
while retaining the candidate set. The shared producer turns snapshot posting
absence into a no-lines job; explicit inputs and opaque fallback always read
current bytes. Inversion does not prepare or consult positive postings. Quiet
positive counts need no zero records and retain candidate enumeration.
Keep reads/output cancellable
and queues bounded.

## Freshness and operands

**D73 A (confirmed 2026-10-16): indexed mode is not exact over the current tree.** Names,
visibility and posting absence come from the pinned catalog/follow generations.
New files and new matches in old posting-negative files can be missed. A daemon
freshness target of about one second is not a deadline or a query barrier.
`--scan` overrides cost refusal; it does not discover new names or refresh.

Read candidates through S2's bracketed reader. Changed bytes yield path-local
answers; unchanged copies share content facts after independent full-key stats.
Drop vanished candidates even under inversion. Other read failures report an
error, never a negative match. Retry instability once, then continue with exit 2.
No atomic tree snapshot is promised; default `--files` keeps stale listings.
**D73 A+D (confirmed 2026-10-16):** `--live` (M6) enumerates current names and reads current
bytes with D13 policy now, independently of `--no-ignore`. Reconstruct
`DirRules::root`, `enter` and `traverse` per D13 from covering catalog roots;
outside-root directories still need indexing or unrestricted mode. Do not read
ignore files or probe worktrees in `Traverse` directories: they were traversed
for reinclusion, not entered for policy. Reset policy at nested configured
roots; D34 gives ownership to the innermost root. Naively loading every ancestor
rule can reverse decisions in both directions, as the crawl test at
`crates/ferret-crawl/src/tests/mod.rs:291` demonstrates. Never use stale names
or posting absence here. Host policy adapters prune the existing live walk;
they retain ignore/size/binary selection and apply grep hidden/glob filters.
Explicit starts keep their exceptions. `--live --no-ignore` chooses unrestricted
walking. **M6 implementation (2026-10-17):** both hosts use find's physical live
source, with current D13 `root` / `enter` / `traverse` state in the host adapter.
Only entered directories load ignore files or probe worktrees. Gitdir,
commondir, info/exclude, symlinked ignore inputs and lossy rule text follow the
crawl policy. Policy-input faults report an error; they do not establish zero
or no match. No names or posting absence are consulted for live exclusion.

**Review correction (2026-10-17):** live scoped loading calls crawl's loader,
including its 1 MiB stat check and bounded read (also for Git pointer files),
non-regular and missing-input rules, symlink handling and diagnostic events.
Malformed patterns warn and are skipped; valid rules in the same file remain.
The remaining host differences are deliberate: live probes the three policy
names directly rather than selecting them from crawl's prior directory listing
(so a newly created policy name can be seen); live rejects a scope on a policy
I/O fault rather than continuing with that input absent and recording coverage;
and live checks cancellation around each bounded load. It does not record
crawl's watch dependencies or worktree identities. Global configuration remains
a separate host input: no scoped size limit; missing configuration quietly uses
defaults without index's setup warnings or file creation; dangling links and
unreadable or non-regular files are errors. Its nonblocking open also rejects
FIFOs rather than blocking on them. All rule text uses crawl's lossy decoding.

The validated `Catalog::Roots` API also loads its format dependencies
`DirNames`, `Names`, `NameHeap` and `Strings`; "root anchors only" does not
mean those dependency bytes can be avoided with the current format. It does
not enumerate the stored names. Live mode leaves `Links`, `Entries` and all
content sections unloaded, proved through the production lazy section cells.
It resolves the existing thread setting without opening Engine.

Nested configured roots reset policy when reached or explicitly selected;
they do not revive an ancestor already pruned by policy or grep filters. This
matches the indexed source on an unchanged tree. Explicit ignored starts retain
the unrestricted opaque-fallback exception without loading nested ignore files,
even below a configured nested root. Select that root explicitly to apply its
own policy. This is the existing indexed fallback behavior, rather than a
freshness-dependent visibility change.

**Read-transaction clarification (2026-10-16, review R1):** regular explicit
files and opaque/ignored-directory fallback files use the same open-descriptor
version bracket and one changed-read retry as indexed candidates. A successful
attempt adds one closing fstat and publishes only that attempt’s buffered output;
it does not reread an unchanged file. This is per-file consistency, not an atomic
tree snapshot. Stdin and other non-regular streams cannot be version-checked:
their output describes the bytes received. Explicit special-file operands still
refuse under the operand rule below.

**Read-extent clarification (2026-10-17, review R3; corrects review R2):** a
regular file reporting size zero may still yield content (for example
`/proc/version`). Explicit operands and opaque/ignored-directory fallback read
such files to actual EOF, retaining the closing version bracket and one
changed-read retry. Read extent and validation are independent: zero size does
not disable change detection. Stable procfs metadata passes the bracket; an
ordinary file filled or mutated during the read is retried, with continued
instability reported as an error. Nonzero sizes retain the initial-size bound,
so appends cannot extend an attempt forever.
An overstated nonzero size (for example a sysfs attribute reporting 4096 bytes)
needs no separate path: EOF before that bound already ends the read. A short
read alone does not disable the bracket, since an ordinary file may have shrunk.

Explicit files bypass posting absence and ignore/glob filters, including new,
ignored and outside-root files. Follow file symlinks; dangling links error.
Ignored directory starts and indexed opaque directories reuse find's live
fallback without nested ignores. Other outside-root/new directory starts fail
with index/live advice. Content searches of special-file operands exit 2.
**M3 clarification (2026-10-17):** `--files` lists an explicit special-file
operand without opening it; walked specials and symlinks are skipped. Explicit
directory symlinks follow their start only, using find's trailing-slash source
resolution. Skip recursive symlinks;
`-L` is deferred.

Indexed recursive content excludes policy's non-content files (binary-sniffed
and over the default 8 MiB cap); name listing includes them. Changed candidates
that exceed the cap report an error, rather than silently disappear. `-a`
scans catalog-visible binary files but retains the recursive cap. Explicit
files/stdin and unrestricted live mode stream without that indexing cap;
multiline buffers whole input with fallible allocation and cancellation.
Oversize/allocation failures are errors, never truncation or a no-match answer.

## Ignore rules and visibility

**D74 A (confirmed 2026-10-16):** indexed visibility is D47/D13, with rg hidden/glob filters
on top. It uses ferret global rules, `.gitignore`/`info/exclude` inside worktrees
at/below indexed roots, and higher-priority `.ferretignore`. Ignore-file changes
require refresh; a query does not read a second policy hierarchy.

Cases where rg can search what indexed grep cannot: ferret-only global or
`.ferretignore` exclusions; positive `-g` that overrides an index exclusion;
`--hidden` when the hidden tree was ferret-excluded (notably `.git`);
rg reinclusion that ferret's traversal did not retain (including different
negation precedence); new names; and content omitted by the size/binary
policy. Explicit starts/live mode are the exceptions above. `--hidden` alone
does not undo ferret ignore rules. **M3 clarification (2026-10-17):**
`-.` aliases `--hidden`; `--no-hidden` reverses it in argv order. The hidden
switch applies equally to content and name modes, without weakening negative
globs or ferret's excluded universe. `-g` last-match wins, supports slash, `**`,
classes and brace alternatives; positive globs can expose held hidden names,
but cannot resurrect excluded names. D13 reinclusion can retain ancestor directories. The rg glob layer still
prunes rejected ancestors: a file-only positive cannot revive a pruned or
held-hidden parent. Globs are relative to the query cwd; explicit directory
starts bypass their own filters (clarified by M1 probes, 2026-10-16). Apply rg
glob semantics, not find's `-path` glob dialect.

Conversely ferret can search what default rg excludes: `.ignore`/`.rgignore`
are not ferret inputs; Git's user-global ignores differ from ferret's global
file; ignores above indexed roots and `.gitignore` outside a worktree are absent;
`.ferretignore` can override VCS exclusions and re-include below a pruned tree.
Ferret's symlinked-ignore/gitdir handling and lossy non-UTF-8 rule parsing can
also change visibility; pin those fixtures against rg. Suppress held hidden
files unless an explicit start, positive glob or `--hidden` permits them.
Tracked status is irrelevant; recursive symlinks stay skipped even if ferret's
content aliases would otherwise return them.

`--no-ignore`/`-u` uses find's unrestricted live walk, retaining the hidden
filter; `-uu` reveals hidden entries. `-uuu` refuses until `--binary` exists.
This bypasses ferret policy, not a fresh index query. **M6 probes
(2026-10-17):** each `-u` (or `--unrestricted`) applies its next step at that
argv position: first disable ignores, second reveal hidden paths, third refuse.
`--ignore` reverses the ignore bypass without resetting the `-u` count or the
hidden switch. `--no-text` reverses `-a` / `--text`. Unrestricted content
retains recursive binary sniffing unless text mode is requested, but has no
indexing size cap and needs no index. **M6 audit correction (2026-10-17):**
unrestricted recursion probes the initial 64 KiB scan window for NUL before
allowing early name/quiet matches, matching rg on a NUL beyond D13's 8 KiB
indexing probe. This is one buffered read, reused for scanning; explicit inputs
retain their existing streaming rule, and policy-controlled walks retain D13's
sniffer. Later unread bytes need not be checked after an early result.
`--files-without-match` over only a skipped recursive binary remains an
intentional binary-policy difference: ferret's absent input yields exit 1 and
no name; rg can yield exit 0 without a name. No selected name is invented.
`--max-depth` is relative to each explicit
start (depth zero); depth zero prunes directory contents but still searches
explicit file operands. Names-only applies depth and visibility without content
reads. Explicit directory symlinks follow the start only in every mode. `--files` reuses find's
catalog/live source and name output with grep filters; no content reads or
term/trigram sections. **M3 implementation (2026-10-17):** names-only opens a
lazy Catalog, loads find's shared minimal namespace section set, and keeps that
catalog alive through output. It does not open Engine (which loads resident
sections), a content store, document identities or candidate cursors. Base and
overlay section-state regressions check the actual lazy load cells after the
complete host operation. Binary, oversized and empty regular files are names.

`--files`, `-l` and `-c` resolve their output mode in argv order; `-q` remains
independent. Matching flags and supplied `-e` patterns are ignored by the resolved
names-only mode, including invalid regex syntax. Unsupported flags still refuse
before output. Names stream directly through find's bounded, cancellable sources;
there is no worker pool or content spill for this mode.

## Regex dialect

Keep `regex::bytes` behind `ferret-verify` (D17 A; D76 C retained the engine
and parallelised verification before new screens, confirmed 2026-10-16).
The locked regex 1.13.1 supports rg's regular syntax: quantifiers, alternation,
groups, class set operations, Unicode properties, inline flags and assertions.
Lookaround (106 rg repos) and backreferences (5) need PCRE2 in rg too.
Refuse every `-P`, even regular patterns; no own engine, PCRE dependency or
process fallback. Reconsider after screening/engine-share measurements.

Grep's default is **case-sensitive**, as is `search grep:` (D67 A2;
corrected 2026-10-16). `-i` uses Unicode
simple folding; `-S` folds only when there is a literal and no uppercase literal
across patterns, not uppercase letters in escape spellings/property names.
**M4 probe (2026-10-17):** `-S -e red -e BLUE` matches only `red` and
`BLUE`, in either argument order: the case decision spans all arguments,
contrary to the M4 brief's per-pattern wording. Escaped literal values count
(`\x41` is uppercase); property and capture names and extended-mode comments
do not. Last case
switch wins; inline flags still scope normally. `-F` escapes literal patterns
before shared compilation/derivation, retaining Unicode `-i`. **2026-10-17 correction:**
compile a single argument unchanged; resolve each repeated `-e` argument's
syntax/inline flags before composing its alternatives, without reapplying
folding to that resolved syntax. Flags stay local (including `(?-i)` under
`-i`), and extended-mode trailing comments cannot consume inserted delimiters. The pinned
rg 15.2.0 rejects those trailing comments after wrapping them; ferret retains
M1's accepted syntax as an explicit compatibility exception. `-w` uses half
word boundaries, not ordinary `\b` on both ends; `-x` anchors whole lines.
Last `-w`/`-x` wins. These features are supported by the locked
[regex syntax](https://docs.rs/regex/1.13.1/regex/#syntax).

Line mode searches LF-delimited slices without terminators; `\A`/`\z` see each
line. Reject explicit newline patterns without `-U`; broad classes like `\s`
search within lines. S3 keeps whole-document scope. `-U` allows cross-line spans;
dotall (the flag or inline `(?s)`) makes dot match LF. Empty matches
advance as specified below. Compile/derive share flags.
Differences retained: whole-document S3 anchors versus grep line mode; resource
limits can reject patterns rg accepts. **D75 D (corrected 2026-10-16):** strip one initial UTF-8 BOM consistently
for indexed candidates, explicit files and stdin, before line matching and NUL
detection. Keep original indexed bytes for trigram filtering: removing a prefix
preserves every necessary substring key. UTF-16 decoding is deferred; its
misses are an approved encoding difference, including explicit input. Invalid
UTF-8 alone is text. M1 covers UTF-8 BOM positives and UTF-16 differences;
later offset-bearing output must specify its BOM offset convention.

## Output and exit status

Match plain output **byte for byte per file/group**, for stable text inputs and
supported flags: path spelling relative to operands, filename prefixes, 1-based
line numbers, LF termination (also for an unterminated last line), CR retained,
`-o` nonoverlapping spans **including empty spans**, and `-l` once per matching
path. Overlapping contexts merge; each line prints once; matches use `:`, context
uses `-`, groups use `--`. `-0` NUL-terminates names/prefixes, not contents. **M5 probes (2026-10-17):**
with heading output, NUL replaces the heading's LF; otherwise it replaces the
filename/content separator. Line numbers and count/content records still end
in LF. Filename, heading and line-number switches resolve in argv order;
headings alone do not force filenames on a single explicit file. Heading
file groups are separated by a blank line, including when `-I` suppresses
their path headings; count/name modes ignore headings.

**M5 context (2026-10-17):** `-A`/`-B` explicitly override the corresponding
half of `-C`, in either argv order. Within each option the last value wins.
Adjacent and overlapping windows merge; disjoint windows and non-heading
file groups use `--` followed by LF. After `-m`, finish only the requested
trailing window: matching lines there keep match formatting but neither
increase the count nor extend the window. With `-v -o`, selected lines remain
whole; regex-matching context lines emit their individual spans with context
separators. Counts and name/quiet modes ignore context.

Context is captured inside the same checked read as the matches. A retry
throws away both. Before-context uses a rolling, header-bearing spill spool,
with at most 64 KiB of captured bytes in memory; compaction bounds retained
spool bytes to twice the live window plus 64 KiB. Rendered groups retain the
existing 64 KiB spill threshold. Replaying a context line needs one line-sized
buffer, under the same unbounded-long-line exception as scanning; neither
context line count nor rendered output accumulates an unbounded memory list.

In line-oriented execution, empty `-o` spans emit their prefix and LF, not
nothing. Resume after a consuming match at its end; suppress an empty match
immediately at that previous match's end. After an empty match (emitted or
suppressed), advance one **byte** before searching again; emit an allowed
terminal empty once. Line terminators supply no extra searchable line. Thus
`abc\n` with line-oriented `-o ''` emits four LFs; `a*` emits `a` and empties at
offsets 2 and 3. On a six-byte UTF-8 line, `''` and `(?u:)` emit seven empties,
including inside codepoints; Unicode consuming patterns still match whole
scalars, byte-mode patterns may split them. Empty input has no lines (exit 1),
while an empty LF-terminated line supplies one empty (exit 0). Use the shared
byte matcher iterator; no Unicode-boundary postfilter. Multiline iteration,
counting and rendering are separate: on `abc\n`, `-U -o '(?s:.*?)'` prints
nothing and exits 0, while `-U -c '(?s:.*?)'` prints 4. Gate multiline rendering
separately in M7.

**M2 observation (2026-10-17, D79 open):** rg 15.2.0’s empty absolute-anchor
`-o`/`-co` output can carry state across lines: `-o '\A'` over three `red`
lines emits one empty record and two full lines. M2 provisionally retains the
per-line spans specified here; D79 asks whether to reproduce that artefact.
This is not yet an approved compatibility exception.

In line mode `-c` counts selected lines; `--count-matches` and `-c -o` count
iterator matches, including empties. With `-U`, shared compilation exposes a
resolved classification based on conservative LF exclusion, derived from the
same resolved HIR; `-c` uses match counts for the LF-including classification,
even if a particular match stays on one line, and selected-line counts
otherwise. This is not an exact proof that the pattern language can match LF:
on `foo foo\nfoo\n`, `foo` counts 2 but `foo|\A\z\n` counts 3 although that
alternative is impossible. `foo\nbar` over two lines counts 1. Under `-v`,
count selected nonmatching lines (also for
`--count-matches`); `-o -v` emits those lines. Suppress zero counts unless
`--include-zero`. **M4 probes (2026-10-17):** `--count-matches`, `-c`,
`-l`, `--files-without-match` and `--files` override output mode in argv order;
`-o` still modifies `-c`. `--no-include-zero` reverses zero emission. There are
no `--no-count-matches` or `--no-files-without-match` switches. A no-match name
(including an empty input) makes exit 0; zero counts alone do not. Input faults
and vanished candidates never yield a zero or no-match record. Snapshot
posting absence may still emit a stale name, as D73 permits.

In line mode `-m` limits selected lines, finishing every span on the last
selected line. `-m0` loads pattern files and validates globs, but skips regex
compilation and all search-input/index resolution; it prints nothing, including
under zero/no-match modes. Names-only ignores the limit. Separate `-m`'s search limit, line-boundary clipping after
that limit, and subsequent rendering/counting. Pin the clipping interaction:
`foo foo\nbar\nfoo\n`, `-U -m1 'foo(?:\nbar)?'` gives count 2 and `-o`
emits two three-byte `foo` records, not the later `bar` portion. `-m0` searches
nothing. On `foo foo\nfoo\n`, `-U -c 'foo(?:\nbar)?'` with `-m1`, `-m2`
and `-m3` gives 2/2/3. Trailing context may include more matches; M7 gates
these interactions, including LF classification for impossible alternatives,
classes, scoped dotall, fixed strings and repeated patterns; empty-span
rendering; and limit, clipping, counting and output behavior.

Pipe defaults: no colour, no heading, no line numbers, filename prefixes for
recursive/multiple operands; one explicit file/stdin omits its filename. Tty
defaults: colour, headings and line numbers. Overrides follow rg, including
single-file heading suppression unless filenames are forced. **M5 completion
(2026-10-17):** stdout's actual terminal state selects those defaults. `-p` /
`--pretty` sets heading, line numbers and always-colour in argv order; later
switches can override each. `--color=auto` emits colour only to a tty, unless
`NO_COLOR` is present (including empty) or `TERM=dumb`; `always` and `ansi`
override those environment settings. Pipe auto-colour remains off.

M5 matches rg's default ANSI palette and resets: paths use
`ESC[0m ESC[35m`, numbers `ESC[0m ESC[32m`, and each nonempty match
`ESC[0m ESC[1m ESC[31m`, each closed by `ESC[0m` (spaces here separate
escapes, not output bytes). Separators and ordinary/context text stay plain;
regex matches within context are highlighted. Empty spans have no escapes.
Count numbers stay plain; count/name/files paths use the path palette.
Owned tests compare these bytes exactly through pipes and real ptys, including
NUL prefixes and heading/context groups. Other terminal capabilities, custom
palettes and hyperlinks are outside this tier; exact escape placement remains
an accepted-difference class, with no such difference found in M5's covered
profiles. Files finish in any order; their groups do not
interleave. Reuse find's bounded output/spill machinery where grouping needs it.

**M2 output overrides (confirmed 2026-10-17):** the last `-l`/`-c` selects
name or count output; `-q` independently overrides both, in either argv order.
`-o` remains a count modifier under `-c`; under `-v` it emits whole selected
nonmatching lines. `--no-fixed-strings` and `--no-invert-match` undo those
switches. Do not invent negations rg does not accept (`--no-quiet`,
`--no-only-matching`, `--no-count`, `--no-ignore-case`). Case-sensitive `-s` / `--case-sensitive` belongs to M4; line-number negation
was completed in M5. **M4 probe (2026-10-17):** rg has no `--no-word-regexp` or
`--no-line-regexp`; those spellings refuse. Whole-word and whole-line switches
override one another in argv order.

`--json` emits rg begin/match/context/end/summary JSON Lines: byte spans,
absolute offsets, line numbers, text-or-base64 fields and semantic counts.
Match schemas/values except key order, timings and physical work counters
(index skipping changes those). JSON ignores `-o`, heading and replacement;
refuse it with names/count modes. Ferret envelopes are not rg JSON.

Binary policy differs: recursive binary-sniffed files are absent. On searched
input, first NUL stops searching and discards buffered plain file output.
Detect NUL in each read chunk, without waiting for LF or EOF (review R1,
2026-10-16). A producer that holds its stream open after NUL does not hold the
query open. For non-binary input with no LF, line buffering grows with the line:
explicit files and stdin have no invented cap and use fallible allocation;
recursive input retains its existing file size cap. Output spill bounds output
memory, not this input-line buffer.
If a selected line preceded it, plain line/span output prints
`PATH: binary file matches` on stdout instead (stdin label: `(standard input)`).
`-l` prints the path and `-q` prints nothing. **M4 review R1 correction
(2026-10-17):** pinned rg keeps `-c`, `-co` and `--count-matches` output
numeric, with or without `-m`; the earlier blanket binary-message rule for
counts was wrong. Count the selected lines/spans before ferret's first-NUL
stop, retaining the documented binary stopping policy. Requested zero counts
can print zero after a no-match binary stop. `--files-without-match` prints a
name only when no selected line preceded the sentinel. Only selected lines or
names make exit 0; zero records alone do not.

An already-detected NUL remains binary even when `-m` stops at a complete
selected line before that byte in the received chunk. Pinned probes of
`-m1 red -` on `red\0x` and `red\nred\0x` both report binary, whereas
`-m1 -c` / `-m1 --count-matches` print `1` and unlimited counts print `1`
and `2` respectively. Early limits, `-q` and `-l` need not read ahead to find
an as-yet unread NUL. `-a` disables detection. Match rg's stdout channel, not
its buffer-dependent warning/offset
bytes. JSON keeps prior match events, sets `end.binary_offset`, and emits no
plain warning. An I/O error never becomes a binary skip.

Exit `0`: selected lines, matching names or listed files; `1`: none; `2`: parse,
unsupported feature, pattern/index/read/write error. Continue after recoverable
file errors. `-q` stops on a match and exits 0 even if a read error was already
reported; a fatal pre-search error is still 2. Broken output stops work, exit 2.
Error wording differs from rg; diagnostic presence and exit status are tested.

## Shared engine and crate seams

`ferret-verify` owns resolved match profiles, compile/derive, match iteration and
capture spans; extend `GrepFlags`, do not add a CLI-only regex executor.
`ferret-query` owns `grep` parsing, selection/planning and result records; reuse
`GrepAtom`, P/M estimates, candidate cursors and S2 coverage/read/cache rules.
`search grep:` keeps document truth, case-sensitive defaults and D67 refusal.
It shares the matcher/derivation/reader, not grep's line-output grammar or `-v` semantics.
D71 remains separate; grep arguments are patterns.

`ferret` owns CLI/output/XDG and policy adapters. Reuse `Engine::pin`/
`QuerySession` and find's `EntrySource`/`CatalogSource`/`LiveWalk`; factor
constructors preserving find behavior, never parse synthetic find commands.
Keep pins through output. Direct input stays local; resident queries later use
the existing host seam. `ferret-policy` owns globs/hidden/size/binary; adapters
pass selection to query. No new edge/format: query cannot depend on policy.

**M1 internal prerequisite:** factor find source constructors, then supply a
typed grep reader adapter. `engine.rs`'s current `.ok()?` and query's
`Option<ReadVersion>` erase faults and cannot meet grep's exits. The host calls
crawl `Documents::stat_name` / `read_current_name_until`; never copy its bracket.
Query's neutral read outcome retains Stable(Catalogued/Current), Vanished,
RetryChanged, Failed(operation/error, size/cap, allocation or instability),
and Cancelled. Map crawl's typed `ContentFault` at the host, without a
query→crawl edge. ENOENT is Vanished; Changed renews the reader observation and
retries once, then reports instability; other faults report errors. Preserve
cancellation and retry outcomes; failed bytes never enter caches. Constructor/
reader adaptation must pass fault/retry regressions before M1 CLI wiring;
direct streams use an input adapter with the same matcher and typed I/O errors.

### M1 seam as built (2026-10-16)

The document verifier’s worker ring carries document truth and one first-match
offset, rather than line groups. Feeding grep through its row callback would
require a second file read and would mishandle line anchors. M1 instead shares
`GrepFlags::compile_lines`, `Search`, `GrepAtom`, crawl’s bracketed reader and
the extracted streaming core (`ferret-query/src/scan.rs`). Query owns grep’s
parser, candidate selection and line records; the local host resolves operands,
adapts policy/read faults, and renders bounded per-file output using find’s
spill buffer. The same typed host adapter supplies search’s legacy optional
read result. There is no query-to-crawl/policy edge or new executor.

M1 emitted file groups sequentially. M2 extends the same verification scheduler
to line groups, using D77's thread setting as described below. SCAN.md's parallel
search numbers are consequently not M1 grep timings. Resident content-search
names still build on Engine open (D78 A); M3 names-only uses the lazy Catalog
route described above.

### M2 seam as built (2026-10-17)

The same query verifier Pool, ring, byte reservation, spawn path and stop guards
carry either document rows or completed file groups. A worker claims from the
host's streaming CatalogSource producer, selects candidates with grep policy,
reads/validates, matches and renders one file. One-file batches bound queued
groups; no array of every path or second concurrent pool is introduced. The
calling thread publishes whole groups. Stdin is a direct operand barrier;
workers never wait for its producer. Explicit operands resolve before searching,
including failed-path diagnostics even when a quiet match is found elsewhere.

D77's index setting and environment override select workers. Explicit-only
commands keep the no-index rule and use the override/default count, limited to
the number of file operands. Inversion declares Searchable as its name universe
and does not load positive postings; ordinary positive output, names and nonzero
counts declare Candidates. Positive posting absence never supplies a -v result.

The host keeps bounded, prefix-free records only for duplicated catalogued
content: at most 64 KiB of accounted records per entry and 16 MiB total before
cache eviction, plus bounded container overhead. Every alias first passes its
own full-key catalog-version stat. Current bytes remain path-local and never
enter this DocId cache. Large groups skip caching and use the ordinary spill
path; concurrent cache misses may independently read the same document. No
cache lock is held during file I/O, matching or output.

A shared cancellation flag reaches planning, typed reads, retry acceptance,
line/span iteration and chunked spill rendering/replay. A worker's stable quiet
hit stops work immediately, even while an earlier position is still reading;
fatal output/spill faults also stop workers. Interrupted writes are not used as
a cancellation signal because write_all retries them. **2026-10-17 review
correction:** the scheduler passes its combined pool/external stop predicate
into the streaming producer, which polls while filtering entries, switching
operands and before directory descent (including live fallback). A quiet hit or
closed stdout therefore also stops posting-negative traversal. **2026-10-17
round-2 correction:** listing cancellation uses find's shared next_with_cancel
mechanism. Catalog children (including ignored entries and nested roots) and
live fallback names poll every 256 listing iterations and at listing completion.
Partial listings are discarded on cancellation. The directory-batch callback
retains its position after names are collected; grep supplies a no-op there
because it has no directory-batch actions. Cancellation uses the separate
shared predicate, including in-progress listings.
Queued groups and spill
handles are bounded by the ring/workers; spill bytes follow the required output,
without silent truncation or an invented cap. The existing long-line input
buffer exception still applies. No dependency or index-format change.

## Differential acceptance

Oracle: grep-compat's pinned rg 15.2.0, empty rg config, controlled stdin/tty,
locale and colour. Harness remains read-only in this slice. Use a scratch adapter outside both repositories if the harness has no ferret
target; never modify the harness or copy its mined rows into owned tests
(updated 2026-10-16). Account for
all **3,332** rendered commands every milestone: exact agreement, supported
accepted difference, deferred refusal, or harness failure. Zero unexplained
supported differences and zero failures is the gate; refused rows are not agrees.

Keep independent denominators: E selects rows whose `-` feature labels belong
to the cumulative surface; S audits **all 3,332 actual argv** (aliases,
clusters, attached values, option operands and `--`). S is required support,
not a passing-test count. M1 has 993 supported within E, nine deferred actual
`-e` rows, plus 26 supported outside E whose `-e` labels are not actual options:
S = 1,002 − 9 + 26 = 1,019. M6–M9 defer four `-uuu`/binary rows within E;
M2–M5 have no within-E deferrals. These are the only permitted within-E
deferrals; all other deferrals must name the out-of-surface flag and its planned
slice or permanent refusal. `-N`/`--no-line-number` both start at M5; general
negating-switch support does not move them to M1.

Publish exact agrees + individually explained supported differences = S;
S + named deferred commands = 3,332. Also publish E's bridge to S. Syntax/
usage errors on supported switches stay in S, as do approved snapshot/policy
differences. No 'deferred combinations' bucket or shrinking denominators.

Write owned Rust fixtures from observations, never oracle source or copied
private corpus. Each flag needs positive/negative cases and interactions;
gate the stronger soundness invariant: **every match accepted by the resolved
execution profile satisfies the derived query over the original indexed bytes**.
Exercise inline/scoped flags, `-F`, invalid bytes, LF/CRLF and `-w`/`-x`, not
just matching builder settings. Any decoded-input profile must bypass raw
postings unless that invariant is proved. Cover stale/new/deleted/uncovered/
duplicate files, stdin, globs, UTF-8/BOM/NUL, Kelvin folding, class sets, empty
matches, anchors/context/captures and quiet/error precedence. The 2,534 exit-1 self-agrees
make positive fixtures essential. Compare whole file/context groups, not sorted
lines; JSON ignores only order/time/work. Check exit and diagnostic presence.

Accepted differences are only those specified above: snapshot freshness,
visibility/cap, binary messages/policy, deferred UTF-16 decoding under D75 D,
order, colour escapes, resource limits and JSON physical work/times. Record each
by command id, cause, channels and minimal reproducer; visibility takes precedence over accompanying
binary messages (709/571 visibility versus 33/4 binary-only GNU/git rows).
No blanket waiver for missing matches on an unchanged visible text file.

## Build slices M1…Mn

Each slice is one Sol-sized review, cumulative and green. D72–D75 are settled
(confirmed 2026-10-16); M1 includes D75 D UTF-8 BOM handling. All commits,
including the contract correction, run CLAUDE’s full gates per the M1 brief.

| Slice | Scope / crates touched | E / S | Acceptance gate |
| --- | --- | ---: | --- |
| M1 | Useful indexed plain regex, `-n -g -q`, explicit files/stdin, constructor/typed-reader prerequisite, then host wiring. `ferret`, `ferret-query`, `ferret-verify`, `ferret-policy` | 1,002 / 1,019 | at least 40 owned positive/negative differential cases; profile soundness, stdin operands, typed faults/retries and snapshot |
| M2 | `-i -o -l -v -F -e -c`, shared iteration and scan planning. `ferret`, `ferret-query`, `ferret-verify` | 2,138 / 2,138 | at least 80 cumulative owned cases; inversion, byte/Unicode empty-span advancement, duplicate/current-byte cache |
| M3 | `--files --hidden` over find's sources. `ferret`, `ferret-query`, `ferret-policy` | 2,275 / 2,275 | at least 100 cases; names-only never reads content sections; complete visibility matrix |
| M4 | Line qualifiers, smart case, pattern files and counts. `ferret`, `ferret-query`, `ferret-verify` | 2,437 / 2,437 | at least 130 cases; half boundaries, case/count/quiet; posting-positive/negative zero and no-match records |
| M5 | Context, heading/filename, NUL names and tty/colour. `ferret`, `ferret-query` | 2,565 / 2,565 | at least 160 cases; byte-exact context groups, tty matrix and bounded output |
| M6 | Policy-preserving `--live`, unrestricted mode, `-a`, depth. `ferret`, `ferret-query`, `ferret-policy` | 2,600 / 2,596 | at least 180 cases; freshness separate from visibility; unchanged tree and policy: indexed and `--live` agree; new-name/match, binary and operand matrix |
| M7 | Multiline matching/count/limit/output semantics. `ferret`, `ferret-query`, `ferret-verify` | 2,660 / 2,656 | at least 210 cases; conservative LF classification (impossible alternatives, classes, scoped dotall, fixed strings, repeated patterns); limits/clipping/counting/rendering, inversion, spans, empty iteration, soundness/cancellation/allocation |
| M8 | Replacement/capture rendering. `ferret`, `ferret-query`, `ferret-verify` | 2,792 / 2,788 | at least 230 cases; named/numbered/empty captures and unchanged input files |
| M9 | rg JSON events. `ferret`, `ferret-query` | 2,794 / 2,790 | at least 250 cases; invalid-byte fields, flag interactions and stable semantic counts |

All nine gates require zero unexplained supported differences and zero failures.
Account for the remaining rows of 3,332 as named refusals. Owned-case counts are
prospective minimums, not runs. Measure bottlenecks before performance work;
new screening structures/dependencies need separate review and any required
DESIGN.md change in the later implementation.

## Measurement and plan changes

Later, with no competing benchmark, compare rg on the same frozen `$HOME`
generation. Report counts only; keep paths/patterns/content private. Record
cold/warm median/p90, standalone/resident, phase times, candidates, verified
bytes, metadata calls, peak RSS and index B/B. Use equal searchable universes
or explain visibility first. Include process cost; separate index maintenance.

Measure each slice's new shapes: M1 rare/alternating/broad/short patterns with
`-n -g -q`; M2 inverted/fixed/case/count/span/names; M3 `--files -g`/hidden;
M4 qualifiers/pattern files; M5 context/tty; M6 live/binary/depth; M7 cross-line;
M8 replacement; M9 JSON. Include positive/zero-result queries and synthetic
10M-entry scaling, labelled separately. Fewer results are not a speed win.

Change the plan if selective warm p90 is slower than rg or reads over half the
selected bytes: measure stronger screening before engine work. The original
S3 measurement read ~1.38 GB for ~8 MB of rare-literal matches; four-key trimming
explained only 2.4%. D76 C and SCAN.md supersede that serial latency baseline
(update 2026-10-16), not the candidate-volume concern. If post-screening verification exceeds half of p90 and the
engine exceeds half of verification, revisit D17. If broad scans are over 2×
rg's comparable p90, fix enumeration/streaming or reopen D72; if new-name misses
harm agents, reopen D73. No global refresh, new positional index or PCRE engine
is justified merely by a compatibility rank.

## Answered decisions (checked 2026-10-16)

D72 A: automatic grep scans; `search --scan` remains explicit. D73 A+D:
snapshot membership/absence by default plus opt-in policy-preserving `--live`.
D74 A: D47 visibility with explicit/unrestricted exceptions; `.ignore` and
`.rgignore` support is a possible later feature, not scheduled. D74 A agrees
with this document’s earlier D47 recommendation; its provisional status was
stale, not its visibility choice. D75 D replaces the earlier raw-input A:
strip initial UTF-8 BOMs everywhere; retain the approved UTF-16 difference.
No new unresolved product decision was needed for these corrections.
