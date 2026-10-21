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
| Uncovered directory (D112 B) | Live rg visibility: git rules from the nearest enclosing `.git`, `.ignore`/`.rgignore` from every ancestor |
| Explicit file, file symlink, or `-` | Direct current-byte search, without opening an index |
| `--no-ignore`, `-u`/`-uu` | Unrestricted live walk, without opening an index |
| `--live` (D73 A+D, M6) | Current tree with ferret policy; catalog supplies root anchors, not names/postings |

Indexed directory content searches and `--files` use the resident daemon (D111 A), falling
back to the shared in-process host when it is unavailable or refuses the hello.
The client resolves presentation, terminal defaults, search threads, cwd and
pattern files. Only requests whose every operand is a covered directory route; any
explicit file, missing operand or `-` keeps the whole request local, because
ferretd would resolve its spelling again in its own process, where
`/proc/self/fd/N`, `/dev/fd/N` or `/proc/self/cwd` name its descriptors and
cwd, not the client's. Directory operands that resolve through such a procfs
link (`openat2` with `RESOLVE_NO_MAGICLINKS`) or onto procfs stay local too.
Explicit-file-only searches, `-u`/`-uu` and `--live` stay local; `--live`
loads only catalog roots, not the resident engine. Names-only directory
requests reuse the resident catalog; the fallback retains the existing
four-section lazy open. Debug `FERRET_TEST_*` injections also stay local.

When no daemon is running, the first indexed directory content or `--files`
call starts one and answers in-process immediately, as search does. Later calls
use the ready owner. Find and grep share endpoint ownership/mode, namespace /
credentials, index identity, protocol and incarnation checks, plus the existing
build-mismatch drain/replacement path. A loading, refused or pre-hello-closed
owner still causes query fallback; D110's behavior is unchanged.

Resolve indexes as find does: `ferret --index DIR`, `FERRET_INDEX`, then XDG
data. Incompatible indexes exit 2 with reindexing advice. An absent index or
uncovered directory selects an in-process live search, without opening the
content index or contacting the daemon. An operand is covered only when both
its canonical path and its spelling, with `.` and `..` folded, lie under a
configured root: the catalog resolves the spelling, so an alias into a root
(`alias -> root`) is searched live. Mixed covered/uncovered requests stay
local: each covered operand uses the snapshot, each uncovered operand walks
live, in operand order. Covered-only requests retain daemon routing.

The D112 fallback reads `.gitignore` from the work-tree top down,
`.git/info/exclude`, `$XDG_CONFIG_HOME/git/ignore` (or `~/.config/git/ignore`),
`.ignore`, and `.rgignore`, in increasing precedence. It applies no ferret
ignore file, `DEFAULT_IGNORE`, or content size cap. Ferret does not currently
read `core.excludesFile`. As in rg 15.2.0, the three git sources apply only
in directories inside a work tree: any ancestor holding a `.git` entry counts,
so a stray `.git` above a scratch directory makes it a work tree. Ancestor
`.gitignore` rules stop at a nested work tree's top; ancestor `.ignore` and
`.rgignore` rules, up to `/`, do not, so naming a subdirectory or a nested
repository keeps them. A `.git` file whose gitdir has no `commondir` (`git
init --separate-git-dir`) contributes no `info/exclude`; through a `commondir`
the common directory's applies. D13 indexing reads both. Slash-anchored global
patterns are relative to the command's cwd, including when an operand starts a
nested work tree. Hidden entries are skipped unless requested or whitelisted by ignore rules. Named
ignored operands are searched, while ancestor rules still apply inside them.
`-L`/`--follow` follows links in uncovered and unrestricted walks; indexed
`-L` remains unsupported (use `-u`). `--live` retains D13 and requires covering
catalog roots. Help/version
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

**Line scan optimization (2026-10-18):** non-inverted line mode without
context searches each remaining run before locating the selected line with
vectorized LF searches. Selected-line limits, counts, names, quiet, `-o`,
replacement and JSON retain their existing line-local processing. Eligibility
uses the verifier's resolved HIR proof: no consuming node admits LF, and
assertions must agree on a whole-line run and a detached line. Multi-line
`^`/`$` and word boundaries qualify; absolute anchors, disabled multi-line
anchors, CRLF assertions and LF-admitting classes/dotall retain per-line search.
`line_terminator` alone does not establish LF exclusion. Original line numbers,
source offsets, binary detection and searched-byte counts remain unchanged.
Multiline, inversion and context keep their existing scanners.

**Matcher/output update (2026-10-18):** grep borrows the compiled regex's cache
pool across inputs; content-search workers retain their owned search copies.
Line selection can narrow required-run searches to consuming-byte windows while
preserving the original left haystack and four bytes of right context for
LF/Unicode word assertions. An occurrence ending beyond the consuming window
cannot select it. Match spans for `-o`, `--count-matches`, highlighting and JSON
use the same windows (2026-10-20): each span is searched in the window around
the next required-run occurrence, not from the previous match to the line end,
which was quadratic on long lines and ran the engine's slow Unicode-`\b`
fallback over non-ASCII text. Captures and replacement still use the whole
source line. Uncoloured, unnumbered plain records combine path,
separator, data and LF through the capture's vectored writer. Grep shares one
capture allocation budget per request of (2T+1) × 4 MiB for configured concurrency T (D93,
D109 B), covering workers, queued groups and the publisher's current capture.
Above 4 MiB, a file takes the streaming slot when available, sends its captured
prefix, and streams the rest in 64 KiB chunks (D103 A). While another file
streams, it keeps capturing while the shared budget has room; one file may use
most of that budget. It waits only when the budget is full and takes a free
streaming slot on its next write, including when woken from a budget wait. A
file that finishes capturing first publishes as a complete checked group. The
stream owner keeps the publisher until its closing version check finishes;
groups never interleave. Active streams never wait for capture space. T=1
preserves walk order and streams on the calling thread. Stdin and cached-record
rendering use the same threshold. Counts, names and quiet modes create no large
capture. Multi-GiB output from a single file still streams serially.

Rendered output never spills, including captures queued behind a streaming
file. The separate before-context spool still spills past 64 KiB. A spool
create/write/read-back fault before publication discards the file; after
publication it retains the prefix. Both cases name the input and temporary-file
fault, continue with other files, and exit 2 (D102 A). A streamed file is never
retried. JSON closes a begun group with only observed partial statistics on a
file error. A broken pipe on stdout still ends silently with exit 0 (D94);
other stdout faults stop the run with exit 2. Find's capture publication remains transactional.
Redirected grep stdout uses a 64 KiB block buffer
written to descriptor 1 itself, with no second descriptor (preserving shared
stdout/stderr offsets); terminal stdout retains line buffering. NUL-terminated
name modes and `--live`, whose policy warnings come from search workers, also
retain the original line buffer and its diagnostic ordering. Each other
published group is flushed before the next operand or diagnostic. Redirected
path-only output may batch records, but is flushed at each operand boundary
and before any diagnostic. Help/version and the final JSON summary also flush
explicitly. A publication fault also drains its previously accepted prefix
before reporting the error.
Failed destinations are discarded without an implicit drop retry. Checked
publication and the 64 KiB cancellation polling bound remain in force;
staging faults are file-local, while stdout faults stop the run.

## Freshness and operands

**D73 A (confirmed 2026-10-16): indexed mode is not exact over the current tree.** Names,
visibility and posting absence come from the pinned catalog/follow generations.
New files and new matches in old posting-negative files can be missed. A daemon
freshness target of about one second is not a deadline or a query barrier.
`--scan` overrides cost refusal; it does not discover new names or refresh.

Read candidates through S2's bracketed reader. Changed bytes yield path-local
answers; unchanged copies share content facts after independent full-key stats.
Drop vanished candidates even under inversion. Other read failures report an
error, never a negative match. Up to 4 MiB of rendered output is transactional:
retry instability once, then continue with exit 2. A larger capture waiting for
the streaming slot also remains transactional until publication starts. Once a
file starts streaming, the guarantee is **one version, or a diagnostic**: a changed closing version or a file error
keeps the already printed prefix, names the file, exits 2 and does not retry.
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
(so a newly created policy name can be seen); live reports the first policy
I/O fault with grep's diagnostic prefix and rejects the scope, rather than
continuing with that input absent and recording all coverage faults; and live
checks cancellation around each bounded load. Live opens canonical directory
paths with a no-follow final component, independently of find's earlier walk
observations; crawl follows the configured root operand and uses checked
parent-relative handles below it. Live does not record crawl's watch
dependencies or worktree identities. Global configuration remains
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

**Review correction (2026-10-17):** a failed configured-root resolution is
reported for a requested scope containing that root or contained by it. A
provably unrelated failed root does not abort a healthy-root query. Relevant
anchor failures prevent both line and names output for that scope.

**Review round 3 correction (2026-10-17):** stored root paths can retain symlink
spellings. Relevance uses expanded paths for operands and anchors. Walking
spelled ancestors (round 2's approach) misses dangling symlink targets: failed
anchors instead expand links component by component with `read_link`, including
relative/absolute targets and parent components, with at most 40 expansions.
Missing components retain their destination spelling. If a component cannot be
inspected, or expansion exceeds the bound, the destination is unknown: treat
the failure as relevant, with empty stdout and exit 2. This deliberately fails
closed even for an otherwise healthy operand whose independence cannot be
established; it supersedes round 1's inaccessible-unrelated-root exception.

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
attempt adds one closing fstat; search also checks the name’s `(dev, ino)`
with one stat (a no-follow fstatat in the checked parent for indexed names).
A surviving hard link cannot hide replacement of the searched name. Strict
crawl reads already reject unlink/replacement through their ctime bracket.
The attempt publishes only that attempt’s buffered output;
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
A walked file is opened beneath the parent descriptor the walk listed it from,
without following a final symlink, and its closing name check uses the same
name, also no-follow; a name swapped for a symlink after listing reads as
changed (one retry, then exit 2). Content walks therefore hold one directory
descriptor per depth level per worker (review R2-1, 2026-10-20), charged to a
process-wide budget of a quarter of the open-file limit, at most 1,024, as the
crawl's held directories are (D21). Past it a listed directory is closed after
listing, keeping its `(dev, ino)`; its files reopen it by path and read as
changed unless it is still that directory. A 300-deep tree under `ulimit -n 64`
searches as without the limit.

Grep also admits transient descriptor use before each listing/policy step or
complete file read. A shared budget leaves room for both retained-directory
caches, sixteen host descriptors and six descriptors per active operation,
including reopened parents, input files and simultaneous context spools during
compaction. It always permits one operation; additional workers wait when the
limit is tight, without changing the thread count or warning. At limit 1,024
with sixteen workers, every worker and the producer fit and local admission
takes no mutex. Daemon queries reserve fixed shares of the process allowance;
query admission falls below four when fewer complete allowances exist. At
NOFILE=64 two admitted requests each retain one allowance, so a stopped output
consumer cannot starve the other. Client thread requests are capped at the
resident engine's search-verification grant before sizing pools; each request
keeps its own capture pool. Each read reserves its whole allowance through the closing check, so a
stream holding the publisher never needs a descriptor permit held by another
reader. Capture-byte waits preserve this order: descriptor admission
before reading or capturing, and no further admission until that read closes;
the active streamer must continue without waiting for capture bytes. The shared
capture budget grants streaming ownership before checking available space, so
even a full budget cannot prevent that owner from replaying its prefix and
finishing. Capture-space waiters poll cancellation at most every 20 ms, as
descriptor waiters do; dropping captures or finishing a stream also wakes them.

Indexed recursive content skips catalogued binary files unless `-a` is set.
**D113:** the default 8 MiB cap bounds content indexing only. Catalogued files
above it are read live beside posting candidates, using rg's initial
recursive binary probe and the existing first-NUL output policy below.
Candidates that grow past the cap are also searched. These reads retain
the checked catalog path and stat bracket and never enter the document cache
as an old version. `-a` also removes binary suppression for oversized files.
Explicit files/stdin, uncovered fallback and unrestricted walks have no size
cap. D13 `--live` retains its indexing policy cap. Multiline buffers whole
input with fallible allocation and cancellation; allocation failures are
errors, never truncation or a no-match answer.

## Ignore rules and visibility

**D74 A (confirmed 2026-10-16):** indexed visibility is D47/D13, with rg hidden/glob filters
on top. It uses ferret global rules, `.gitignore`/`info/exclude` inside worktrees
at/below indexed roots, and higher-priority `.ferretignore`. Ignore-file changes
require refresh; a query does not read a second policy hierarchy.

Cases where rg can search what indexed grep cannot: ferret-only global or
`.ferretignore` exclusions; positive `-g` that overrides an index exclusion;
`--hidden` when the hidden tree was ferret-excluded (notably `.git`);
rg reinclusion that ferret's traversal did not retain (including different
negation precedence); new names; and catalogued binary content without `-a`.
Explicit starts/live mode are the exceptions above. `--hidden` alone
does not undo ferret ignore rules. **M3 clarification (2026-10-17):**
`-.` aliases `--hidden`; `--no-hidden` reverses it in argv order. The hidden
switch applies equally to content and name modes, without weakening negative
globs or ferret's excluded universe. `-g` last-match wins, supports slash, `**`,
classes and brace alternatives; positive globs can expose held hidden names,
but cannot resurrect excluded names. D13 reinclusion can retain ancestor directories. The rg glob layer still
prunes rejected ancestors: a file-only positive cannot revive a pruned or
held-hidden parent. Globs are relative to the query cwd; explicit directory
starts bypass their own filters (clarified by M1 probes, 2026-10-16). Apply rg
glob semantics, not find's `-path` glob dialect. A walked cwd is matched as
the empty path, as in rg: `*`, `**`, `**/*` and `/*` whitelist a hidden cwd
below the operand, `?*` and `*/**` do not (review R2-2, 2026-10-20).

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
`-o` nonoverlapping non-empty spans (an empty span prints no record, D79 D),
and `-l` once per matching path. Overlapping contexts merge; each line prints
once; matches use `:`, context uses `-`, groups use `--`. `-0` NUL-terminates names/prefixes, not contents. **M5 probes (2026-10-17):**
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
trailing window: its lines are context (D98), never searched, so they are not
highlighted, replaced or split by `-o`, and they do not extend the window.
With `-v -o`, selected lines remain whole; regex-matching context lines before
the limit emit their individual spans with context separators. Counts and
name/quiet modes ignore context.

Context is captured inside the same checked read as the matches. A retry
throws away both. Without `-v`, a line-local pattern finds the next selected line
as the plain scan does; only trailing context and the last `-B` lines
before it are visited, so context costs O(N) lines per match. Before-context uses a rolling, header-bearing spill spool,
with at most 64 KiB of captured bytes in memory; compaction bounds retained
spool bytes to twice the live window plus 64 KiB. Rendered groups retain the
existing 64 KiB spill threshold. Replaying a context line needs one line-sized
buffer, under the same unbounded-long-line exception as scanning; neither
context line count nor rendered output accumulates an unbounded memory list.

In line-oriented execution, resume after a consuming match at its end;
suppress an empty match immediately at that previous match's end. After an
empty match (counted or suppressed), advance one **byte** before searching
again. Each line has a match position before every byte and one at its end,
before its LF. An unterminated last line has the same end position, so `abc` and
`abc\n` both hold four `''` matches; the position after a final LF is on no line
and never matches (D98). Empty spans select and count their line, but `-o` never
prints an empty record (D79 D, GNU's rule): on `abc\n`, `-o ''` prints nothing
and exits 0, `--count-matches ''` prints 4, and `-o 'a*'` prints `a`. On a
six-byte UTF-8 line, `''` and `(?u:)` count seven empties, including inside
codepoints; Unicode consuming patterns still match whole scalars, byte-mode
patterns may split them. Empty input has no lines (exit 1), while an empty
LF-terminated line supplies one empty (exit 0). Use the shared byte matcher
iterator; no Unicode-boundary postfilter. Multiline iteration applies the same
rules to the whole input: on `abc\n`, `-U -o '(?s:.*?)'` prints nothing and exits
0, while `-U -c '(?s:.*?)'` prints 4.

**D79 (answered in D98):** line mode searches each line on its own, so
`-o '\A'` over three `red` lines matches at every line start. Each span is
empty, so `-o` prints nothing. rg 15.2.0 carries empty absolute-anchor state
across lines; ferret does not reproduce that.

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
under zero/no-match modes. Names-only ignores the limit.

With `-U`, `-m N` limits **line groups** (D98, D81 C). A group is the lines a
match touches together with every later match that *starts* on them, each taken
whole, so the last group can extend downward. Nothing is clipped: counts,
rendering, `-o`, `-r` captures and JSON submatches use the same whole matches.
On `red red\nblue\n`, `-U -n -m1 'red(?:\nblue)?'` prints `1:red red` and
`2:blue`, and `-c` prints 2. On `foo foo\nfoo\n`, `-U -c 'foo(?:\nbar)?'` with
`-m1`, `-m2` and `-m3` gives 2/3/3; rg limits matches rather than groups and
gives 2/2/3.

**Lines after the limit are context** (D98, GNU's rule), in line and multiline
mode, plain and JSON. Requested after-context prints them as context lines; they
are never searched, also under `-v`, so never re-classified as matches,
highlighted, replaced or split by `-o`. On `x\nab\n`, `-v -o -n -m1 -A1 a`
prints `1:x` and `2-ab`, and `-U -v -m1 -A1 -r X '\Aa'` on `x\nay\n` prints
`x` and `ay`.
On unterminated `a\nbc`, `-n -m1 -A1 '\z'` prints `1:a` and `2-bc`, although
`\z` matches at the end of `bc` too.

**M7 probes (2026-10-17):** `--multiline`/`-U` and `--no-multiline`
resolve in argv order; `--multiline-dotall`/`--no-multiline-dotall` resolve
independently and affect dot only when multiline is enabled. Inline/scoped
flags still override the default. Conservative LF exclusion visits the resolved
HIR, including classes and impossible branches; line/absolute assertions give
no LF-exclusion proof, while word assertions do. Thus `foo|^a$` also counts
matches rather than lines. Zero repetitions already removed by HIR lowering
have no LF effect. Fixed strings and repeated patterns use that same profile.

Multiline search takes **one pass of whole matches over the original input**
(D98, D81 C). Assertions see the whole input, and selection, counts and
rendering all use those spans. On `abc\nabc\n`, `-U -n '\A'` prints line 1
only, `-U -c '\A'` prints 1, and `-U -o '\A'` prints nothing, because its one
span is empty. A match at the end of an unterminated last line selects that
line. After a final LF the end position is on no line and never matches, as
in line mode, so it draws no before-context either: on `a\nb\nc\n`,
`-U -n -B1 '\z'` prints nothing and exits 1 (rg prints `3-c`), and
`-U -n -m1 -B1 'a|\z'` prints `1:a`. Adjacent highlighted spans merge
within each multiline output line. Multiline inverted output and its context
remain whole lines under `-o`; before the `-m` limit, context highlighting
searches each complete context line, including its LF.

**Inversion (D98, D80 A):** LF-including `-U -v` selects the lines no match
intersects. A line's LF belongs to that line, so a match intersects the next
line only if it continues past the LF: on `a\nb\n`, `-U -v 'a\n'` selects `b`,
and `-U -v 'a\nb'` selects nothing. `-m` limits those selected lines. Counts are
selected lines, also for `--count-matches`. On `x\na\ny\na\nz\n`,
`-U -vn -m1 -A1 a` prints `1:x` and `2-a`. With `'a|\n'` every line holds an
LF match, so `-U -vc` selects nothing and exits 1. On `foo foo\nbar\nfoo\n`,
`-U -v 'foo(?:\nbar)?'` also selects nothing.

`-U` buffers the complete checked input attempt (or the first-NUL prefix)
before matching, including for quiet/name output. Reads and metadata/span
iteration poll cancellation. Input growth and variable-size line/span metadata
use fallible allocation; an unrepresentable extent or allocation failure is an
input error. Input reads finish before rendering, but the closing version check
still follows rendering. Up to 4 MiB, a failed attempt publishes no prefix or
zero record; larger rendered groups follow the same streaming and no-retry
contract as line mode (D103 A).
There is **no small peak-memory bound** for explicit files, stdin or unrestricted
multiline input. The whole input buffer and line/span metadata grow with the
input and match count; rendered-capture and host-cache budgets do not cap them.
Only D13 `--live` retains its policy size cap. Indexed and uncovered grep,
explicit files, stdin and unrestricted input have no content size cap.

**Separators and headings follow printed records** (D98, fuzz B4). A file's
heading, an inter-file separator and a `--` are each printed only before a
record that prints something. Context-only file groups print, so they take part
in inter-file separators even when search status is no-match. A selected line
whose `-o` output is empty prints nothing but stays in its context window, so
`--` marks only lines that no window covered: on `a\n\na\n`,
`-U -on -A1 'a|\n'` prints `1:a` and `3:a`. A file whose matches print nothing
gets no heading. A zero count is published only with `--include-zero`,
independently of search success.

Binary results discard every buffered plain group, including context-only and
decorated output; search success decides only whether a binary warning is
printed.

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
profiles. With multiple threads, finished checked groups enter a bounded
completion queue and are published as soon as the destination is available
(D92 B); their groups never interleave. While the host is busy, workers hold
groups under 64 KiB in batches of up to 64 KiB; the host takes every held
batch before it sleeps, so a held group never waits on its worker's next file.
With one thread, groups retain walk order. Cross-file diagnostics follow
completion order too, and cannot split another file’s group. Grep reuses find’s
bounded capture buffer and query’s completion scheduler for capture/stream
handoffs.

**M2 output overrides (confirmed 2026-10-17):** the last `-l`/`-c` selects
name or count output; `-q` independently overrides both, in either argv order.
`-o` remains a count modifier under `-c`; under `-v` it emits whole selected
nonmatching lines. `--no-fixed-strings` and `--no-invert-match` undo those
switches. Do not invent negations rg does not accept (`--no-quiet`,
`--no-only-matching`, `--no-count`, `--no-ignore-case`). Case-sensitive `-s` / `--case-sensitive` belongs to M4; line-number negation
was completed in M5. **M4 probe (2026-10-17):** rg has no `--no-word-regexp` or
`--no-line-regexp`; those spellings refuse. Whole-word and whole-line switches
override one another in argv order.

**M8 replacement probes (2026-10-17):** `-r` / `--replace` changes output
only; input files are never written. Attached, equals and clustered values
are accepted; the last replacement wins. Replacement arguments must be UTF-8;
captured bytes may contain invalid UTF-8 and are preserved unchanged. Counts,
name modes, quiet and names-only retain original search semantics.

D79's per-line absolute anchors apply to replacement too. On three `red` lines,
`-nr '<$0:$1>' '()\A'` inserts `<:>` on every line; pinned rg inserts it only on
the first and prints later lines unchanged.

`$0` is the complete match; `$1`, `${1}`, `$name` and `${name}` interpolate
captures. Unbraced references consume the longest ASCII alphanumeric/underscore
run: on `(?P<word>red)`, `$wordX` is empty while `${word}X` is `redX`;
`$1X` is empty while `${1}X` is `redX`. `$$` emits a literal dollar. Missing
and non-participating captures expand to empty; an empty participating group
also expands to empty. Invalid/unfinished braced references are literal:
`${word!}` stays `${word!}`. Backslashes have no replacement escape syntax.
On `red blue` with `(red)|(blue)`, `<$1:$2>` produces `<red:> <:blue>`.

Replacement precedes colour and `-o` projection, retaining original capture
bounds. `-o` emits each expansion, and colour highlights replacement bytes.
Line-mode context is also replaced; inversion still selects original lines.
With replacement, multiline inverted context also uses line-local captures;
`-o` emits its non-empty expansions, and a context line with no match stays
whole.
Multiline replacement expands selected source groups before projecting display
lines; context separators retain original source boundaries even when expansion
adds/removes LF. LF-excluding profiles retain line-oriented replacement output,
so an embedded replacement LF gets no additional prefix. `-o` never prints an
empty expansion (D79 D); without `-o` an empty line still prints its LF.
Expansion is cancellable and fallibly allocated. It keeps one displayed line
and its highlights; rendered records still use the host's bounded cache and capture/stream
path. A source or expanded line can be arbitrarily long, under the existing
line-sized memory exception. It does not accumulate an expanded multiline
group. These presentation choices are settled (D84 B, through D98).
On `abc`, with or without a final LF, `-or X ''` emits four X records, and
`-or X '\z'` on unterminated `abc` emits `X`. `-or '$wordX' '(?P<word>red)'`
prints nothing.
On `a\nb\nc\nd\n`, `-UnC1 -r X 'b\nc'` prints `1-a`, `2:X`, `4-d`,
without an inserted group separator. Replacing with `X`-LF-`Y`-LF-`Z` prints
`2:X`, `3:Y`, `4:Z`, followed by original context `4-d`.

Replacement uses the same whole matches (D98; D85 is moot). On
`red red\nblue\ngap\nred\nblue\n`, `-Unm1 -A2 -r '<$0>' 'red(?:\nblue)?'`
prints `1:<red> <red`, `2:blue>`, `3-gap` and `4-red`. Context lines after the
limit are never replaced.

Selected multiline inverted lines have no replacement captures: on
`a\nb\nc\n`, `-Uv -r X '\z'` prints the three unchanged lines. Inverted
context lines are searched on their own, including their LF, for captures.

`--json` emits rg begin/match/context/end/summary JSON Lines: byte spans,
absolute offsets, line numbers, text-or-base64 fields and semantic counts.
Match schemas/values except key order, timings and physical work counters
(index skipping changes those). JSON ignores `-o`, heading and replacement;
refuse it with names/count modes. Ferret envelopes are not rg JSON.

**M9 probes (2026-10-17):** `--json` / `--no-json` resolve in argv order.
JSON defaults line numbers on and respects explicit `-N`; paths are always
present. Every byte field independently uses `{text:...}` for valid UTF-8 or
`{bytes:...}` for standard padded base64. Source LF is retained, CR is literal,
and an unterminated last line gains no LF. Strings escape as rg does, byte for byte:
`\"`, `\\`, `\b`, `\f`, `\n`, `\r`, `\t`, other C0 controls as `\u00xx`. Absolute offsets and binary offsets
are measured **after stripping the initial UTF-8 BOM**; submatch start/end are
byte offsets relative to the event's source bytes, including empty spans.
LF-including multiline matches publish a complete contiguous selected source
group in one event, retaining separate, unmerged submatches.

For `red red\nblue\nred`, JSON emits two match events at offsets 0 and 13;
semantic stats are matched_lines 2 and matches 3. For `a\nb\nc\nd\n` with
`-U 'b\nc'`, one match event contains `b\nc\n`, starts at line 2 / offset 2,
and has submatch [0,3); stats are matched_lines 2 and matches 1.

Begin/end are published for actual file records, including context-only files
that exit 1. No-match files without context publish neither. An empty successful
span presentation still publishes its selected source in JSON, independently
of `-o`. All file events, binary state and stats belong to the successful checked
read attempt; retry, disappearance and faults discard them together. Summary
is emitted once even with no file events; `-m0` emits nothing. Quiet emits only
summary and finishes the matching file's semantic stats before stopping work.

`matched_lines` counts source lines in **match events**; lines after `-m` are
context events. `matches` counts match-event submatches. Context events can
carry submatches but contribute neither count. `searches_with_match` counts
successful selected files independently of span count or context publication.
Summary sums these semantic fields from successful files. Every match event
carries its submatches; an inverted selected line has none. Quiet reports the
stats its events would have had (D98, D91 A): `-q '\z'` on unterminated `abc`
reports matches 1, and ordinary JSON publishes that line with the empty
submatch [3,3). On `a\nb\n` with `-Um1 'a\n\b'`, the a-LF event has submatch
[0,2): matched_lines 1, matches 1, searches_with_match 1. Timings and physical
`searches`, `bytes_searched`, `bytes_printed` may differ; the JSON audit must
retain the three semantic fields above.

**Contract/oracle differences:** rg 15.2.0 lets names/count modes override JSON;
ferret deliberately refuses their combination as required above. The earlier
claim that rg JSON ignores replacement is wrong: rg retains source bytes but
adds a `replacement` byte field to each submatch. Ferret keeps the
explicit ignore rule (D88 A, through D98); rg's optional metadata would be a
separate feature. JSON also follows D79's per-line assertions: on `abc\nabc\n`
with `'\A'`, ferret reports an empty submatch on each line, where rg reports it
only on the first. With `-m1 -A1 '\z'` on `a\nbc`, `bc` is a context event.

Limited multiline events carry whole matches (D98). On `red red\nblue\n` with
`-Um1 'red(?:\nblue)?'`, one event holds both lines with submatches [0,3) and
[4,12); rg's stream reader panics there. On `x\na\ny\na\nz\n` with
`-Uvm1 -A1 a`, ferret emits match x and context a, as rg's file reader does.
With `-Um1 -A1 'a|\z'` it emits match a and context y.

JSON retains the first-NUL prefix policy below: prior events survive, and
`end.binary_offset` identifies the sentinel. For `red\0red\n`, the selected
prefix event contains exactly `red`, has one submatch, and binary_offset 3;
rg substitutes LF for NUL and can search its received suffix. Ferret neither
invents that LF nor searches beyond its first-NUL stop. `-a` disables detection
and publishes original NUL bytes as escaped text.

Lines after the limit are context events and are never searched (D98). On
`a\nb\na\nb\n` with `--json -Um1 -A2 'a\nb'` or `'a(?:\nb)?'`, JSON emits one
match event containing `a\nb\n` at line 1 / offset 0, then context `a\n` at
line 3 / offset 4 and `b\n` at line 4 / offset 6: matches 1 and matched_lines
2. Inverted context lines are searched on their own, including their LF, for
their submatches.

A NUL-only input under line-mode `--json -v red` still selects the pending
sentinel line: an empty source-prefix event at line 1 / offset 0, no submatches,
matched_lines 1, matches 0, binary_offset 0 and exit 0. An LF before NUL leaves
the NUL on another real source line; its empty prefix is not a phantom EOF
record. Under `--json ''` the empty prefix carries the empty submatch [0,0),
so matches is 1 (fuzz D2). Pinned rg 15.2.0 selects the same lines, but emits
literal NUL for regular
files and a substituted LF for stdin. Ferret's empty text follows the existing
first-NUL raw-prefix policy, without either sentinel representation; this is a
binary-policy compatibility difference, not D74 visibility. Multiline buffers
the first-NUL prefix: an empty prefix or a prefix ending at LF supplies no extra
line there. These existing mode-specific outcomes are unchanged and explicitly
tested. Text mode retains the NUL and agrees with rg.

Binary policy differs: recursive binary-sniffed files are absent. On searched
input, first NUL stops searching and discards buffered plain file output.
Detect NUL in each read chunk, without waiting for LF or EOF (review R1,
2026-10-16). A producer that holds its stream open after NUL does not hold the
query open. For non-binary input with no LF, line buffering grows with the line:
explicit files and stdin have no invented cap and use fallible allocation;
indexed and uncovered recursive grep also have no size cap (D112/D113).
D13 `--live` retains its policy cap. Capture buffering bounds rendered output
memory, not this input-line buffer.
If a selected line preceded it, plain line/span output prints
`PATH: binary file matches` on stdout instead (stdin label: `(standard input)`).
If the group has already begun streaming, its printed prefix remains before
that warning.
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
reported; a fatal pre-search error is still 2. Broken stdout stops work:
broken pipe exits 0 silently, other destination faults exit 2.
With no path operand (grep searches `.`), a walk whose depth, glob, hidden and
ignore filters select no file exits 2 with a "no files were searched"
diagnostic, after any walk error it reported (D99). A file selected but
skipped by the index, or failing to open, still counts as searched. An
explicit path (`.` included) or stdin keeps exit 1, silent. A context-spool
fault names the input and temporary file in `TMPDIR`, never stdout; it fails
only that file, preserving any prefix already streamed. Through the daemon the
spool goes to the client's `TMPDIR`, made absolute against the client's cwd,
never ferretd's.
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
Keep pins through output. Direct-only input stays local; indexed directory
queries use the shared host through the daemon (D111 A). `ferret-policy` owns globs/hidden/size/binary; adapters
pass selection to query. No new edge/format: query cannot depend on policy.

**M1 internal prerequisite:** factor find source constructors, then supply a
typed grep reader adapter. `engine.rs`'s current `.ok()?` and query's
`Option<ReadVersion>` erase faults and cannot meet grep's exits. The host calls
crawl `Documents::stat_name` / `read_current_name_until`; never copy its bracket.
Query's neutral read outcome retains Stable(Catalogued/Current), Vanished,
RetryChanged, Failed(operation/error, size/cap, allocation or instability),
and Cancelled. Map crawl's typed `ContentFault` at the host, without a
query→crawl edge. ENOENT is Vanished; Changed renews the reader observation and
retries once, then reports instability; other faults report errors. Search
reads accept growth when the opened inode is unchanged and its closing size is
at least the opening size: they read only the opening size's prefix and return
the closing stat, so that prefix is never cached as the catalogued version.
A copytruncate rotation that truncates and regrows past the opening size during
a read can mix old and new bytes. Preserve
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
capture buffer. The same typed host adapter supplies search’s legacy optional
read result. There is no query-to-crawl/policy edge or new executor.

M1 emitted file groups sequentially. M2 extends the same verification scheduler
to line groups, using D77's thread setting as described below. SCAN.md's parallel
search numbers are consequently not M1 grep timings. Resident content-search
names still build on Engine open (D78 A); M3 names-only uses the lazy Catalog
route described above.

### M2 seam as built (2026-10-17)

The same query verifier Pool, ring, byte reservation, spawn path and stop guards
carry either document rows or completed file groups. The host's streaming
CatalogSource producer selects candidates with grep policy; a worker claims
one file from it, reads/validates, matches and renders it. **P2 (2026-10-19):**
completion-order grep runs that producer on its own thread, filling a queue of
at most four paths per worker that idle workers sleep on, so workers no longer
convoy on a producer lock and none holds a path while the producer blocks.
**D106 B / D107 B (2026-10-20), indexed mode:** the queue holds at most
max(4T, 16) paths, waking a worker at each 16 queued tasks and everyone at the
producer's end. Workers take consecutive runs of at most 16 tasks or 64 KiB of
summed catalog sizes; a larger file travels alone. Idle workers steal the far
end of another run without waiting for its owner's read to finish. Runs hold
at most 15 pending paths per worker, with no input reservation or output capture.
Explicit-only feeds retain immediate wakes. Parallel `-uu` and `--live` walks
use D105's scheduler instead.
**P5 (2026-10-19):** workers hand completed groups to the host in batches of
at most 64 KiB while it is busy, and the host collects every held batch before
it sleeps. One-file tasks and bounded handoffs bound queued
groups; no array of every path or second concurrent pool is introduced. The
calling thread publishes whole groups. A worker past 4 MiB hands over an
active stream through the same queue when it acquires the streaming slot,
after flushing its held batch. Until then it captures within the pool-wide
(2T+1) × 4 MiB allocation budget (D109 B), shared with completed groups until
publication or discard. The publisher consumes the active stream's bounded
chunk channel through completion; its owner never waits for capture space.
Other handoffs remain bounded in the queue. T=1 and spawn fallback handoffs
publish synchronously without waiting for their own task to send more data.
Stdin is a direct operand barrier;
workers never wait for its producer. The D105 parallel live walk (`-uu`,
`--live`) has no producer, and orders operands with two more barriers: file
operands are searched in order by one worker, and each directory operand's walk
ends before the next operand starts (DESIGN.md, Grep M2). Explicit operands
resolve before searching, including failed-path diagnostics even when a quiet
match is found elsewhere.

**D108 B (2026-10-21):** a search thread the system refuses (`RLIMIT_NPROC`,
a thread limit under load) costs speed, not output. Feed workers search on the
workers that started, or on the calling thread alone if none did or the
producer was refused. Nested walk workers leave a walk to fewer. A refused
live walk thread leaves every operand to the single-thread path. One stderr
line per run, `ferret: grep: searching with N of M threads: <cause>`, names
the first shortfall, N counting the threads that search; the exit status
describes the search alone. Debug builds refuse on demand for the CLI tests:
`FERRET_TEST_REFUSE_THREADS=ferret-walk=1` lets the first thread of that name
start and refuses the rest.

D77's index setting and environment override select workers. Explicit-only
commands keep the no-index rule and use the override/default count, limited to
the number of file operands. Inversion declares Searchable as its name universe
and does not load positive postings; ordinary positive output, names and nonzero
counts declare Candidates. Positive posting absence never supplies a -v result.

The host keeps bounded, prefix-free records only for duplicated catalogued
content: at most 64 KiB of accounted records per entry and 16 MiB total before
cache eviction, plus bounded container overhead. Every alias first passes its
own full-key catalog-version stat. Current bytes remain path-local and never
enter this DocId cache. Large groups skip caching and stream at the threshold;
even a small cached record set can cross it when an alias's rendered prefix
expands. Concurrent cache misses may independently read the same document. No
cache lock is held during file I/O, matching or output.

A shared cancellation flag reaches planning, typed reads, retry acceptance,
line/span iteration, streaming writes and context-spool replay. A worker's stable quiet
hit stops work immediately, even while an earlier position is still reading;
stdout faults also stop workers; context-spool faults fail only their file. Interrupted writes are not used as
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
Queued groups and active stream requests are bounded by the completion queue
and workers. Only before-context retains spill handles: its rolling disk
bound remains twice the live context window plus 64 KiB, without truncation. The existing long-line input
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
| M7 | Multiline matching/count/limit/output semantics. `ferret`, `ferret-query`, `ferret-verify` | 2,660 / 2,656 | at least 210 cases; conservative LF classification (impossible alternatives, classes, scoped dotall, fixed strings, repeated patterns); limits/counting/rendering, inversion, spans, empty iteration, soundness/cancellation/allocation |
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

### D111 daemon measurements (2026-10-21)

Warm release binaries, 62 copied source files (1.09 MB), one rare-literal
match, identical search trees with 64,318 or 251,062 catalog names. Hyperfine
`-N`, ten warmups and 100 runs, one benchmark at a time; the daemon's initial
backstop completed and its writer was idle before measurement. Default index
setting: 16 search threads. Means in milliseconds:

| Catalog names | In-process grep | Daemon grep | rg 15.2.0 |
| ---: | ---: | ---: | ---: |
| 64,318 | 10.82 | 2.53 | 3.69 |
| 251,062 | 31.69 | 2.54 | same tree |

Dense inverted output (30,522 lines, 1,574,130 bytes), one search thread,
Hyperfine `--shell=bash`, ten warmups / 30 runs: local 14.77 ms versus daemon
23.98 ms at 64k names, **62% slower**. A small-index control is 7.36 versus
24.51 ms, exposing roughly 17.15 ms of additional transport/codec work for
1.57 MB. This exceeds the 10% framing budget. Proposed follow-up: negotiate
length-delimited raw stdout/stderr payloads, preserving stream order, explicit
flush boundaries, cancellation and native end status; JSON remains the control
codec. Round 1 retained find's base64 JSON frames; Round 2 implements the negotiated
raw payload codec described in S1B.md.

## Answered decisions (checked 2026-10-16)

D72 A: automatic grep scans; `search --scan` remains explicit. D73 A+D:
snapshot membership/absence by default plus opt-in policy-preserving `--live`.
D74 A: D47 visibility with explicit/unrestricted exceptions; `.ignore` and
`.rgignore` support is a possible later feature, not scheduled. D74 A agrees
with this document’s earlier D47 recommendation; its provisional status was
stale, not its visibility choice. D75 D replaces the earlier raw-input A:
strip initial UTF-8 BOMs everywhere; retain the approved UTF-16 difference.
No new unresolved product decision was needed for these corrections.

**Round 2 raw frames (2026-10-21):** negotiated `raw-output-v1` keeps JSON
headers/control and sends exact-length stdout/stderr bytes. Native find shares
the codec; JSON find keeps its public envelopes. The same dense profile on a
frozen 62-source-file snapshot (30,558 lines / 1,576,029 output bytes), warm
Hyperfine `-N`, ten warmups / 30 runs at one search thread: daemon **7.77 ms**
versus local **14.94 ms** at 64k names. Small-index control: **7.78 versus
7.41 ms**, only **4.9% overhead**. Startup at the default 16 threads, 100 runs:
64k **2.69 ms**, 251k **2.66 ms**, local 64k 10.86 ms, rg 3.52 ms. The initial
backstop completed and the writer was idle before measurement. This resolves
Round 1's dense-output framing limitation.

**Round 2 names-only routing:** `--files` on the same frozen tree, warm `-N`,
ten warmups / 100 runs: before routing **2.66 / 6.30 ms** at 64k / 251k names;
after routing **1.91 / 2.00 ms**. Local controls remain 2.74 / 6.22 ms. The
resident path lists from its pinned catalog; unavailable-daemon fallback still
uses the four-section lazy open, with identical names, ordering and exits.


### D112 uncovered routing measurements (2026-10-21)

Warm release binaries, an isolated 60-file / 26,409-byte tree, 16 configured
search threads. Hyperfine `-N`, ten warmups and 100 runs, one benchmark at a
time after checking `pgrep -x hyperfine`. Daemon runs require a bound socket
and a private 0700 runtime directory. Mean times in milliseconds:

| Call | Before D112 | After D112 | rg 15.2.0 |
| --- | ---: | ---: | ---: |
| Indexed `grep -q 'needle' DIR`, daemon | 1.874 | 1.926 | — |
| Uncovered `grep -l 'x' DIR` | — | 2.448 | 3.497 |

The daemon startup difference is 0.052 ms, within run variation (standard
deviations 0.114 / 0.174 ms). Coverage reads checked root paths and strings,
without loading the name tables or opening content. A regression test damages
an unrelated content manifest and still requires uncovered content and names
searches to succeed without contacting a daemon. Mixed requests stay local
with an operand barrier between catalog and live sources.


**D113 final startup check:** on the same frozen catalog and tree, fresh
isolated runtime directories and daemon PID verified from hello before and
after each run: original binary **1.880 ms**, final D112+D113 binary
**1.958 ms** (standard deviations 0.130 / 0.139 ms; ten warmups / 100 runs,
Hyperfine `-N`). The observed difference is **+0.078 ms**. Existing late-NUL
count/diagnostic policy remains as documented above; D113 shares that policy
rather than introducing a separate binary-output contract for large files.
