//! `text:` queries through the real pipeline (crawl, content index, planner,
//! the crawl's checked reader for verification, row emission) against a
//! brute-force oracle that reads every file and evaluates the expression per
//! path.
//!
//! The oracle's tokenizer is this file's own, written from docs/S2.md §
//! Tokenizer for the ASCII alphabet the corpus is generated in: `ferret`
//! has no `ferret-text` edge, and a second implementation keeps the oracle
//! independent of the code it checks. Its phrase matcher is written from §
//! What `text:ARG` means, without `TextMatcher`.
#![allow(clippy::unwrap_used)] // A fixture failure should stop the test.

use std::collections::BTreeSet;
use std::ffi::OsStr;
use std::fs;
use std::ops::ControlFlow;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::SystemTime;

use ferret::engine::{Engine, QuerySession};
use ferret_crawl::{IndexOptions, Refresh, index};
use ferret_index::Budget;
use ferret_query::{ContentReport, Query, RunError};
use ferret_verify::{GrepFlags, Matcher};

// ---------------------------------------------------------------- fixture

struct Tree(PathBuf);

impl Tree {
    fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let base = std::env::temp_dir().join(format!(
            "ferret-content-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = fs::remove_dir_all(&base);
        fs::create_dir_all(base.join("tree")).unwrap();
        Self(base)
    }
    fn root(&self) -> PathBuf {
        self.0.join("tree")
    }
    fn index(&self) -> PathBuf {
        self.0.join("index")
    }
    fn write(&self, rel: &str, bytes: &[u8]) -> PathBuf {
        let path = self.root().join(rel);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, bytes).unwrap();
        path
    }
    /// Crawls the tree and opens a writer engine on the catalog.
    fn engine(&self) -> Engine {
        index(&self.index(), &[self.root()], Refresh::All, &options()).unwrap();
        let mut writer = ferret_catalog::WriterSession::open(&self.index()).unwrap();
        writer.set_compaction_limits(ferret_catalog::CompactionLimits {
            log_bytes: u64::MAX,
            records: u64::MAX,
            dirty_percent: 100,
            dead_percent: 100,
        });
        Engine::from_writer(writer)
    }
    fn refresh(&self, engine: &Engine) {
        let request = ferret_crawl::RefreshRequest {
            expected_generation: engine.pin().generation(),
            scopes: vec![ferret_crawl::RefreshScope::Root(self.root())],
            rename_hints: Vec::new(),
            reason: ferret_crawl::RefreshReason::Burst,
        };
        engine.refresh(request, &options()).unwrap();
    }
}

impl Drop for Tree {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn options() -> IndexOptions {
    IndexOptions {
        workers: 2,
        ..IndexOptions::default()
    }
}

fn query(args: &[&str]) -> Query {
    Query::from_args(args.iter().map(|a| a.as_bytes()), SystemTime::now()).unwrap()
}

/// The rows' paths, sorted, and the run's content report.
fn search(
    pin: &QuerySession,
    query: &Query,
    bound: Option<u32>,
) -> Result<(BTreeSet<Vec<u8>>, Option<ContentReport>), RunError> {
    let mut rows = BTreeSet::new();
    let stats = pin.search_content(query, bound, None, |row| {
        assert!(rows.insert(row.path.to_vec()), "one row per path (D15)");
        ControlFlow::Continue(())
    })?;
    Ok((rows, stats.content))
}

fn paths(paths: &[&Path]) -> BTreeSet<Vec<u8>> {
    paths
        .iter()
        .map(|p| p.as_os_str().as_bytes().to_vec())
        .collect()
}

// ------------------------------------------------------- oracle tokenizer

/// One token: lowercased, and as spelled.
#[derive(Clone, Debug)]
struct Tok {
    lower: String,
    raw: String,
}

/// One run: its whole token, and its parts when they differ from it.
struct Run {
    whole: Tok,
    parts: Vec<Tok>,
}

fn tok(raw: &str) -> Tok {
    Tok {
        lower: raw.to_ascii_lowercase(),
        raw: raw.to_owned(),
    }
}

/// S2 § Tokenizer, ASCII only: runs of alphanumerics and `_`; parts split
/// at `_`, lower → upper, `UPPER` → `Upper`-lower and letter ↔ digit.
fn runs(text: &[u8]) -> Vec<Run> {
    let text = String::from_utf8_lossy(text);
    let mut out = Vec::new();
    for run in text
        .split(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
        .filter(|r| !r.is_empty())
    {
        let mut parts = Vec::new();
        for piece in run.split('_').filter(|p| !p.is_empty()) {
            let c: Vec<char> = piece.chars().collect();
            let mut start = 0;
            for i in 1..c.len() {
                let lower_upper = c[i - 1].is_ascii_lowercase() && c[i].is_ascii_uppercase();
                let acronym = c[i - 1].is_ascii_uppercase()
                    && c[i].is_ascii_uppercase()
                    && c.get(i + 1).is_some_and(char::is_ascii_lowercase);
                let digit = c[i - 1].is_ascii_digit() != c[i].is_ascii_digit();
                if lower_upper || acronym || digit {
                    parts.push(tok(&piece[start..i]));
                    start = i;
                }
            }
            parts.push(tok(&piece[start..]));
        }
        let whole = tok(run);
        if parts.len() == 1 && parts[0].lower == whole.lower {
            parts.clear();
        }
        out.push(Run { whole, parts });
    }
    out
}

/// A run's units: its parts, or the whole run when it has none.
fn units(runs: &[Run]) -> Vec<&Tok> {
    runs.iter()
        .flat_map(|r| {
            if r.parts.is_empty() {
                vec![&r.whole]
            } else {
                r.parts.iter().collect()
            }
        })
        .collect()
}

/// Whether `doc` holds `text:arg` (`case:text:` when `case`): a single
/// run's whole token emitted anywhere, or the argument's units contiguous
/// in the document's.
fn holds(arg: &str, case: bool, doc: &[u8]) -> bool {
    let eq = |a: &Tok, b: &Tok| {
        if case {
            a.raw == b.raw
        } else {
            a.lower == b.lower
        }
    };
    let want = runs(arg.as_bytes());
    let have = runs(doc);
    if let [one] = want.as_slice() {
        let emitted = have
            .iter()
            .flat_map(|r| std::iter::once(&r.whole).chain(&r.parts));
        if emitted.into_iter().any(|t| eq(t, &one.whole)) {
            return true;
        }
    }
    let (want, have) = (units(&want), units(&have));
    have.windows(want.len())
        .any(|window| window.iter().zip(&want).all(|(a, b)| eq(a, b)))
}

// ---------------------------------------------------------- the generator

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        // splitmix64
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
    fn pick<T: Copy>(&mut self, items: &[T]) -> T {
        items[self.below(items.len())]
    }
    fn one_in(&mut self, n: usize) -> bool {
        self.below(n) == 0
    }
}

/// Words chosen so parts, acronyms, digits and case overlap.
const WORDS: &[&str] = &[
    "alpha",
    "Beta",
    "beta",
    "gamma",
    "GAMMA",
    "Delta",
    "alphaBeta",
    "HTTPReq",
    "http",
    "req",
    "x2",
    "foo_bar",
    "foo",
    "bar",
];
const SEPARATORS: &[&str] = &[" ", "\n", ".", "-", ", ", "("];
const NAMES: &[&str] = &["notes", "main", "readme", "lib", "OR", "color", "todo"];
/// Directories are rows with no document, so `NOT text:` holds there: some
/// carry a name the name atoms match.
const DIRS: &[&str] = &["", "src/", "notes/", "src/main/", "lib/"];

fn content(rng: &mut Rng) -> String {
    let mut text = String::new();
    for i in 0..rng.below(10) {
        if i > 0 {
            text.push_str(rng.pick(SEPARATORS));
        }
        text.push_str(rng.pick(WORDS));
    }
    text.push('\n');
    text
}

/// A random corpus: some files share content (one document, several
/// names), and some are empty.
fn corpus(tree: &Tree, rng: &mut Rng, files: usize) -> Vec<String> {
    let mut written: Vec<String> = Vec::new();
    for i in 0..files {
        let rel = format!("{}{}{i}", rng.pick(DIRS), rng.pick(NAMES));
        let text = match written.len() {
            n if n > 0 && rng.one_in(6) => String::from_utf8_lossy(
                &fs::read(tree.root().join(&written[rng.below(n)])).unwrap(),
            )
            .into_owned(),
            _ if rng.one_in(10) => String::new(),
            _ => content(rng),
        };
        let mut bytes = text.into_bytes();
        if rng.one_in(3) {
            bytes.extend_from_slice(b" \xff \x80ab ");
        }
        if rng.one_in(3) {
            bytes.extend_from_slice(" Kelvin foo\n".as_bytes());
        }
        tree.write(&rel, &bytes);
        written.push(rel);
    }
    written
}

/// A query tree, which serialises to arguments and evaluates on a path.
#[derive(Clone, Debug)]
enum Q {
    Text(String, bool),
    Grep(String, bool),
    /// A bare word: a folded name substring.
    Word(String),
    /// A reserved operator word under `name:` (folded) or `case:` (exact).
    Prefixed(String, bool),
    And(Vec<Q>),
    Or(Vec<Q>),
    Not(Box<Q>),
}

fn atom(rng: &mut Rng) -> Q {
    match rng.below(9) {
        8 => Q::Text("onlyonce".to_owned(), rng.one_in(3)),
        6 | 7 => Q::Grep(
            rng.pick(&[
                "alpha",
                "Alpha",
                "(?i)Alpha",
                "a.*beta",
                "(?-u:\\xFF)",
                "^foo",
                "(?-m)^foo",
                "(?i)kelvin",
                "(?-u:[\\x80-\\x81])ab",
                "x*",
                "\\d+",
                "(alpha|req|foo)",
                "\\bbar\\b",
            ])
            .to_owned(),
            rng.one_in(3),
        ),
        0 => Q::Word(rng.pick(NAMES).to_ascii_lowercase()),
        1 => Q::Prefixed(rng.pick(&["OR", "and", "Not"]).to_owned(), rng.one_in(2)),
        2 => {
            let sep = rng.pick(&[" ", ".", "-", "_"]);
            Q::Text(
                format!("{}{sep}{}", rng.pick(WORDS), rng.pick(WORDS)),
                rng.one_in(5),
            )
        }
        _ => {
            let word = rng.pick(WORDS);
            let word = match rng.below(4) {
                0 => word.to_ascii_lowercase(),
                1 => word.to_ascii_uppercase(),
                _ => word.to_owned(),
            };
            Q::Text(word, rng.one_in(5))
        }
    }
}

fn tree_of(rng: &mut Rng, depth: usize) -> Q {
    if depth == 0 || rng.one_in(3) {
        return atom(rng);
    }
    match rng.below(3) {
        0 => Q::Not(Box::new(tree_of(rng, depth - 1))),
        1 => Q::And(
            (0..2 + rng.below(2))
                .map(|_| tree_of(rng, depth - 1))
                .collect(),
        ),
        _ => Q::Or(
            (0..2 + rng.below(2))
                .map(|_| tree_of(rng, depth - 1))
                .collect(),
        ),
    }
}

impl Q {
    /// The arguments, with parentheses where precedence needs them and,
    /// at random, where it does not.
    fn args(&self, rng: &mut Rng, out: &mut Vec<String>) {
        match self {
            Q::Grep(arg, case) => {
                out.push(format!("{}grep:{arg}", if *case { "case:" } else { "" }))
            }
            Q::Text(arg, case) => {
                let prefix = rng.pick(&["text:", "content:"]);
                out.push(format!("{}{prefix}{arg}", if *case { "case:" } else { "" }))
            }
            Q::Word(word) => {
                // D71 reserves lowercase operator words too; escape name atoms.
                if matches!(word.as_str(), "and" | "or" | "not") {
                    out.push(format!("name:{word}"));
                } else {
                    out.push(word.clone());
                }
            }
            Q::Prefixed(word, case) => {
                out.push(format!("{}{word}", if *case { "case:" } else { "name:" }))
            }
            Q::And(all) => {
                for (i, q) in all.iter().enumerate() {
                    if i > 0 && rng.one_in(2) {
                        out.push(rng.pick(&["AND", "and", "AnD"]).to_owned());
                    }
                    q.operand(rng, out, matches!(q, Q::Or(_)));
                }
            }
            Q::Or(any) => {
                for (i, q) in any.iter().enumerate() {
                    if i > 0 {
                        out.push(rng.pick(&["OR", "or", "Or", "oR"]).to_owned());
                    }
                    q.operand(rng, out, matches!(q, Q::Or(_)));
                }
            }
            Q::Not(inner) => {
                out.push(rng.pick(&["NOT", "not", "Not", "nOt"]).to_owned());
                let needs = matches!(**inner, Q::And(_) | Q::Or(_));
                inner.operand(rng, out, needs);
            }
        }
    }

    fn operand(&self, rng: &mut Rng, out: &mut Vec<String>, needs: bool) {
        if needs || rng.one_in(5) {
            out.push("(".to_owned());
            self.args(rng, out);
            out.push(")".to_owned());
        } else {
            self.args(rng, out);
        }
    }

    /// The query's truth on one row: `name` is its last component, `doc`
    /// its bytes, `None` for a directory.
    fn eval(&self, name: &str, doc: Option<&[u8]>) -> bool {
        match self {
            Q::Grep(arg, _) => doc.is_some_and(|doc| {
                GrepFlags {
                    case_insensitive: false,
                }
                .compile(arg)
                .unwrap()
                .is_match(doc)
            }),
            Q::Text(arg, case) => doc.is_some_and(|doc| holds(arg, *case, doc)),
            Q::Word(word) => name.to_ascii_lowercase().contains(word.as_str()),
            Q::Prefixed(word, true) => name.contains(word.as_str()),
            Q::Prefixed(word, false) => name
                .to_ascii_lowercase()
                .contains(&word.to_ascii_lowercase()),
            Q::And(all) => all.iter().all(|q| q.eval(name, doc)),
            Q::Or(any) => any.iter().any(|q| q.eval(name, doc)),
            Q::Not(inner) => !inner.eval(name, doc),
        }
    }
}

/// The oracle over the pin's rows: every name the catalog lists, read
/// from disk now.
fn expected(pin: &QuerySession, q: &Q) -> BTreeSet<Vec<u8>> {
    let mut all = Vec::new();
    pin.search(&query(&[]), |row| {
        all.push(row.path.to_vec());
        ControlFlow::Continue(())
    })
    .unwrap();
    all.into_iter()
        .filter(|path| {
            let path = Path::new(OsStr::from_bytes(path));
            let name = path.file_name().unwrap().to_str().unwrap();
            let doc = (!path.is_dir()).then(|| fs::read(path).unwrap());
            q.eval(name, doc.as_deref())
        })
        .collect()
}

/// `queries` random queries on the pin, each against the oracle. Some must
/// find rows and some must verify, or the stage proved little.
fn check(pin: &QuerySession, rng: &mut Rng, queries: usize, stage: &str) {
    let (mut verifying, mut found) = (0, 0);
    for _ in 0..queries {
        let q = match rng.below(3) {
            0 => tree_of(rng, 3),
            1 => Q::Not(Box::new(atom(rng))),
            _ => Q::And(vec![tree_of(rng, 3), atom(rng)]),
        };
        let mut args = Vec::new();
        q.args(rng, &mut args);
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        let parsed = query(&args);
        let (got, report) = search(pin, &parsed, None).unwrap();
        assert_eq!(got, expected(pin, &q), "{stage}: {args:?}\n{report:?}");
        if let Some(report) = &report {
            assert!(
                report.bounds.maybe >= report.snapshot_maybe,
                "{stage}: M invariant {args:?} {report:?}"
            );
            assert!(
                report.bounds.possible >= report.snapshot_possible,
                "{stage}: P invariant {args:?} {report:?}"
            );
        }
        if report.is_some_and(|r| r.verified > 0) {
            verifying += 1;
        }
        found += usize::from(!got.is_empty());
    }
    assert!(
        verifying > 0 && found > 0,
        "{stage}: {verifying} verifying, {found} found"
    );
}

/// One corpus through every coverage state: no index, an empty one, a
/// partial follow, changes the index has not seen, and a full follow.
fn oracle(seed: u64, files: usize, queries: usize) {
    let mut rng = Rng(seed);
    let tree = Tree::new();
    let written = corpus(&tree, &mut rng, files);
    let engine = tree.engine();
    check(&engine.pin(), &mut rng, queries, "no index");

    engine.attach_content(&tree.index()).unwrap();
    check(&engine.pin(), &mut rng, queries, "empty index");

    let total: u64 = written
        .iter()
        .map(|rel| fs::metadata(tree.root().join(rel)).unwrap().len())
        .sum();
    let budget = Budget {
        bytes: total * (1 + rng.below(3) as u64) / 4,
        ..Budget::unbounded()
    };
    engine.follow_content(&budget, None).unwrap();
    check(&engine.pin(), &mut rng, queries, "partial");

    // Rewrite, add and delete files the index has seen; the catalog
    // catches up, the index does not.
    for _ in 0..1 + files / 4 {
        let rel = written[rng.below(written.len())].clone();
        let mut text = content(&mut rng);
        text.push_str("tail\n");
        tree.write(&rel, text.as_bytes());
    }
    tree.write("src/fresh_new", content(&mut rng).as_bytes());
    let _ = fs::remove_file(tree.root().join(&written[rng.below(written.len())]));
    tree.refresh(&engine);
    check(&engine.pin(), &mut rng, queries, "stale");

    engine.follow_content(&Budget::unbounded(), None).unwrap();
    let pin = engine.pin();
    let live = ferret::engine::live_documents(pin.catalog());
    assert!(pin.content().unwrap().uncovered(&live).is_empty());
    check(&pin, &mut rng, queries, "full");
}

#[test]
fn text_queries_equal_the_oracle_at_every_coverage() {
    for seed in 0..6 {
        oracle(seed, 16, 30);
    }
}

#[test]
#[ignore = "long: run with --ignored"]
fn text_queries_equal_the_oracle_at_every_coverage_long() {
    for seed in 100..160 {
        oracle(seed, 48, 120);
    }
}

#[test]
fn the_oracle_tokenizer_splits_as_the_contract_says() {
    let lower = |text: &str| -> Vec<Vec<String>> {
        runs(text.as_bytes())
            .iter()
            .map(|r| {
                std::iter::once(&r.whole)
                    .chain(&r.parts)
                    .map(|t| t.lower.clone())
                    .collect()
            })
            .collect()
    };
    assert_eq!(
        lower("parseHTTPRequest2 x.y"),
        [
            vec!["parsehttprequest2", "parse", "http", "request", "2"],
            vec!["x"],
            vec!["y"]
        ]
    );
    assert_eq!(
        lower("foo_bar GAMMA"),
        [vec!["foo_bar", "foo", "bar"], vec!["gamma"]]
    );
    assert!(holds("request handler", false, b"requestHandler"));
    assert!(holds("Foo", true, b"FooBar"));
    assert!(!holds("Foo", true, b"foo"));
    assert!(holds("HttpRequest", false, b"http_request"));
    assert!(!holds("alpha beta", false, b"beta alpha"));
}

// ------------------------------------------------------ deterministic cases

/// The discriminating case: `NOT text:x` where x's only document is
/// uncovered. The index cannot say No there; verification must, so the
/// file is excluded from the NOT and found by the positive query.
#[test]
fn not_text_on_an_uncovered_document_is_settled_by_verification() {
    let tree = Tree::new();
    let a = tree.write("a.txt", b"alpha\n");
    let b = tree.write("sub/b.txt", b"beta\n");
    let engine = tree.engine();
    engine.attach_content(&tree.index()).unwrap();
    engine.follow_content(&Budget::unbounded(), None).unwrap();
    let u = tree.write("u.txt", b"zeta\n");
    tree.refresh(&engine);
    let pin = engine.pin();

    // The directory has no document, so the NOT holds there too.
    let sub = tree.root().join("sub");
    let (rows, report) = search(&pin, &query(&["NOT", "text:zeta"]), None).unwrap();
    assert_eq!(rows, paths(&[&a, &b, &sub]));
    let report = report.unwrap();
    assert_eq!((report.uncovered, report.verified), (1, 1));

    let (rows, _) = search(&pin, &query(&["text:zeta"]), None).unwrap();
    assert_eq!(rows, paths(&[&u]));
    let (rows, _) = search(&pin, &query(&["NOT", "NOT", "text:zeta"]), None).unwrap();
    assert_eq!(rows, paths(&[&u]));
}

/// A file rewritten after indexing, before the catalog sees it. An exact
/// posting answers for the catalog's version, as name search does for a
/// stale catalog; a Maybe reads the current bytes even after a change.
/// A posting miss stays a snapshot miss. Once the catalog catches
/// up the new version is uncovered and verified like any other.
#[test]
fn changed_files_use_current_bytes_only_when_the_snapshot_yields_maybe() {
    let tree = Tree::new();
    let a = tree.write("a.txt", b"alpha beta\n");
    let b = tree.write("b.txt", b"gamma delta\n");
    let engine = tree.engine();
    engine.attach_content(&tree.index()).unwrap();
    engine.follow_content(&Budget::unbounded(), None).unwrap();
    tree.write("a.txt", b"zeta beta, longer now\n");
    let pin = engine.pin();

    let (rows, report) = search(&pin, &query(&["text:alpha"]), None).unwrap();
    assert_eq!(rows, paths(&[&a]), "the catalog's version");
    assert_eq!(report.unwrap().verified, 0);

    let (rows, report) = search(&pin, &query(&["text:alpha beta"]), None).unwrap();
    assert!(rows.is_empty());
    let report = report.unwrap();
    assert_eq!((report.verified, report.changed), (1, 1));

    let not = query(&["NOT", "text:alpha beta", "ext:txt"]);
    let (rows, report) = search(&pin, &not, None).unwrap();
    assert_eq!(
        rows,
        paths(&[&a, &b]),
        "current bytes settle the negative phrase"
    );
    assert_eq!(report.unwrap().changed, 1);

    let (rows, report) = search(&pin, &query(&["case:text:beta"]), None).unwrap();
    assert_eq!(
        rows,
        paths(&[&a]),
        "a changed candidate still matching current bytes"
    );
    let report = report.unwrap();
    assert_eq!((report.verified, report.changed), (1, 1));
    let (rows, report) = search(&pin, &query(&["text:zeta"]), None).unwrap();
    assert!(
        rows.is_empty(),
        "new term absent from the index is a snapshot miss"
    );
    assert_eq!(report.unwrap().verified, 0);
    let (rows, report) = search(&pin, &query(&["NOT", "text:zeta", "ext:txt"]), None).unwrap();
    assert_eq!(rows, paths(&[&a, &b]), "snapshot absence is exact too");
    assert_eq!(report.unwrap().verified, 0);

    tree.refresh(&engine);
    let pin = engine.pin();
    let (rows, report) = search(&pin, &query(&["text:zeta"]), None).unwrap();
    assert_eq!(rows, paths(&[&a]));
    assert_eq!(report.unwrap().uncovered, 1);
    let (rows, _) = search(&pin, &not, None).unwrap();
    assert_eq!(rows, paths(&[&a, &b]));
}

/// More uncovered documents than the bound is an error naming both
/// counts, with no index the same as with an empty one; no bound reads
/// them. A query with no `text:` atom never asks.
#[test]
fn the_uncovered_bound_refuses_and_none_lifts_it() {
    let tree = Tree::new();
    let a = tree.write("a.txt", b"alpha\n");
    tree.write("b.txt", b"beta\n");
    tree.write("c.txt", b"gamma\n");
    let engine = tree.engine();
    let text = query(&["text:alpha"]);
    for attached in [false, true] {
        if attached {
            engine.attach_content(&tree.index()).unwrap();
        }
        let pin = engine.pin();
        assert!(matches!(
            search(&pin, &text, Some(2)),
            Err(RunError::IndexIncomplete {
                uncovered: 3,
                live: 3
            })
        ));
        assert!(matches!(
            search(&pin, &text, Some(3)),
            Err(RunError::ScanRequired { maybe: 3, live: 3 })
        ));
        assert_eq!(search(&pin, &text, None).unwrap().0, paths(&[&a]));
        assert_eq!(
            search(&pin, &query(&["alpha"]), Some(0)).unwrap().0.len(),
            0
        );
    }
    tree.write("d.txt", b"delta\n");
    engine.follow_content(&Budget::unbounded(), None).unwrap();
    tree.refresh(&engine);
    let pin = engine.pin();
    assert!(matches!(
        search(&pin, &text, Some(0)),
        Err(RunError::IndexIncomplete {
            uncovered: 1,
            live: 4
        })
    ));
    assert_eq!(search(&pin, &text, Some(1)).unwrap().0, paths(&[&a]));
}

/// Two names of one document both get their rows, from either driver.
#[test]
fn every_name_of_a_matching_document_is_a_row() {
    let tree = Tree::new();
    let one = tree.write("one.txt", b"shared words\n");
    let two = tree.write("src/two.txt", b"shared words\n");
    tree.write("other.txt", b"elsewhere\n");
    let engine = tree.engine();
    engine.attach_content(&tree.index()).unwrap();
    engine.follow_content(&Budget::unbounded(), None).unwrap();
    let pin = engine.pin();
    for args in [
        &["text:shared"][..],
        &["text:shared", "txt"],
        &["text:shared words"],
        &["case:text:shared"],
    ] {
        let (rows, report) = search(&pin, &query(args), None).unwrap();
        assert_eq!(rows, paths(&[&one, &two]), "{args:?}");
        if args[0].starts_with("case:") || args[0].contains(' ') {
            assert_eq!(
                report.unwrap().verified,
                1,
                "one read, each copy has its own stat"
            );
        }
    }
    let (rows, _) = search(&pin, &query(&["text:shared", "two"]), None).unwrap();
    assert_eq!(rows, paths(&[&two]));
    let (rows, report) = search(&pin, &query(&["case:text:Shared"]), None).unwrap();
    assert!(rows.is_empty());
    assert_eq!(
        report.unwrap().verified,
        1,
        "a verified negative is a cacheable content fact too"
    );
}

/// Truncate a segment after its dictionaries are pinned: any postings read
/// now fails. An already-cancelled query must return without touching it.
#[test]
fn an_already_cancelled_content_query_reads_no_postings() {
    let tree = Tree::new();
    tree.write("a.txt", b"alpha beta");
    let engine = tree.engine();
    engine.attach_content(&tree.index()).unwrap();
    engine.follow_content(&Budget::unbounded(), None).unwrap();
    let pin = engine.pin();
    for segment in &pin.content().unwrap().manifest().segments {
        fs::OpenOptions::new()
            .write(true)
            .open(tree.index().join("index").join(segment.file_name()))
            .unwrap()
            .set_len(0)
            .unwrap();
    }
    let cancelled = std::sync::atomic::AtomicBool::new(true);
    let stats = pin
        .search_content(&query(&["text:alpha"]), None, Some(&cancelled), |_| {
            panic!("cancelled query emitted a row")
        })
        .unwrap();
    assert_eq!(stats.rows, 0);
    assert!(
        matches!(
            search(&pin, &query(&["text:alpha"]), None),
            Err(RunError::Index(_))
        ),
        "without cancellation it must attempt the broken postings"
    );
}

/// The first result callback can change the directory before the next read.
/// On one verification thread the next read follows the callback; with
/// more, it may already have been read ahead (D76 C), so this pins one.
#[test]
fn a_result_callback_cannot_redirect_later_directory_reads() {
    let tree = Tree::new();
    let a = tree.write("d/a.txt", b"alpha beta one");
    tree.write("d/b.txt", b"alpha beta two");
    let engine = tree.engine();
    engine.attach_content(&tree.index()).unwrap();
    engine.follow_content(&Budget::unbounded(), None).unwrap();
    let mut rows = Vec::new();
    engine
        .pin()
        .search_content_threads(&query(&["text:alpha beta"]), None, 1, None, |row| {
            rows.push(row.path.to_vec());
            if rows.len() == 1 {
                fs::rename(tree.root().join("d"), tree.0.join("moved")).unwrap();
                std::os::unix::fs::symlink(tree.0.join("moved"), tree.root().join("d")).unwrap();
            }
            ControlFlow::Continue(())
        })
        .unwrap();
    assert_eq!(rows, vec![a.as_os_str().as_bytes().to_vec()]);
}

/// Serial warm checked-read throughput on a real 100k-file synthetic tree.
#[test]
#[ignore = "S2 directory revalidation measurement; run release with --ignored --nocapture"]
fn measure_verification_on_a_hundred_thousand_files() {
    fn cpu() -> u64 {
        fs::read_to_string("/proc/thread-self/schedstat")
            .unwrap()
            .split_whitespace()
            .next()
            .unwrap()
            .parse()
            .unwrap()
    }
    let tree = Tree::new();
    for dir in 0..100 {
        for file in 0..1000 {
            tree.write(
                &format!("d{dir}/inner/f{file}.txt"),
                format!("alpha beta unique{dir}file{file}\n").as_bytes(),
            );
        }
    }
    let engine = tree.engine();
    let pin = engine.pin();
    let catalog = pin.catalog();
    let names: Vec<_> = catalog
        .name_reader()
        .runs_from(ferret_catalog::NameId(0))
        .filter_map(|(name, edge)| match edge.target() {
            ferret_catalog::Target::Inode(inode) if catalog.doc(inode).is_some() => Some(name),
            _ => None,
        })
        .collect();
    assert_eq!(names.len(), 100_000);
    let text = ferret_verify::Text::new(b"alpha beta", false).unwrap();
    for pass in 0..3 {
        let mut documents = ferret_crawl::Documents::by_name(catalog).unwrap();
        let mut matcher = ferret_verify::TextMatcher::new();
        let mut bytes = Vec::new();
        let cpu_before = cpu();
        let started = std::time::Instant::now();
        for &name in &names {
            documents.read_name(catalog, name, &mut bytes).unwrap();
            assert!(matcher.is_match(&text, &bytes));
        }
        let wall = started.elapsed();
        let cpu_ns = cpu() - cpu_before;
        println!(
            "VERIFY_100K pass={pass} files={} bytes={} cpu_ns={cpu_ns} wall_ns={} files_per_second={:.0}",
            documents.files_read,
            documents.bytes_read,
            wall.as_nanos(),
            names.len() as f64 / wall.as_secs_f64()
        );
    }

    engine.attach_content(&tree.index()).unwrap();
    while engine
        .follow_content(&Budget::unbounded(), None)
        .unwrap()
        .remaining
        > 0
    {}
    let (mut follow_cpu, mut follow_wall, mut changed) = (0u64, 0u128, 0u64);
    for round in 1..=40 {
        for file in 0..8 {
            let mut bytes = format!("document{file} revision{round} alpha beta ").into_bytes();
            bytes.extend_from_slice(&b"alpha beta requestHandler ".repeat(3000));
            bytes.truncate(64 << 10);
            tree.write(&format!("d0/inner/f{file}.txt"), &bytes);
        }
        tree.refresh(&engine);
        let before_cpu = cpu();
        let started = std::time::Instant::now();
        loop {
            let followed = engine.follow_content(&Budget::unbounded(), None).unwrap();
            changed += followed.bytes;
            if followed.remaining == 0 {
                break;
            }
        }
        follow_wall += started.elapsed().as_nanos();
        follow_cpu += cpu() - before_cpu;
    }
    assert_eq!(changed, 40 * 8 * (64 << 10));
    println!(
        "FOLLOW_100K refreshes=40 changed_bytes={changed} cpu_ns={follow_cpu} wall_ns={follow_wall}"
    );
}

/// An IO failure through one name is not a negative content fact for its
/// DocId. The independent unchanged copy must still be verified.
#[test]
fn a_failed_copy_does_not_suppress_an_unchanged_copy() {
    let tree = Tree::new();
    let a = tree.write("a.txt", b"alpha beta");
    let b = tree.write("b.txt", b"alpha beta");
    let engine = tree.engine();
    engine.attach_content(&tree.index()).unwrap();
    engine.follow_content(&Budget::unbounded(), None).unwrap();
    fs::remove_file(a).unwrap();
    let pin = engine.pin();
    assert_eq!(
        search(&pin, &query(&["case:text:alpha"]), None).unwrap().0,
        paths(&[&b])
    );
    assert_eq!(
        search(&pin, &query(&["case:text:alpha", "name:b.txt"]), None)
            .unwrap()
            .0,
        paths(&[&b])
    );
}

/// A verified match through one copy must not vouch for a changed sibling.
#[test]
fn a_matching_copy_does_not_vouch_for_a_changed_copy() {
    let tree = Tree::new();
    let a = tree.write("a.txt", b"alpha beta");
    tree.write("b.txt", b"alpha beta");
    let engine = tree.engine();
    engine.attach_content(&tree.index()).unwrap();
    engine.follow_content(&Budget::unbounded(), None).unwrap();
    tree.write("b.txt", b"zeta other longer");
    assert_eq!(
        search(&engine.pin(), &query(&["case:text:alpha"]), None)
            .unwrap()
            .0,
        paths(&[&a])
    );
}

/// Current-byte answers belong to the changed path, never its old DocId.
#[test]
fn current_copy_answers_do_not_enter_the_document_cache() {
    let tree = Tree::new();
    let a = tree.write("a.txt", b"alpha beta");
    let b = tree.write("b.txt", b"alpha beta");
    let engine = tree.engine();
    engine.attach_content(&tree.index()).unwrap();
    engine.follow_content(&Budget::unbounded(), None).unwrap();
    tree.write("a.txt", b"Alpha beta longer");
    let pin = engine.pin();
    assert_eq!(
        search(&pin, &query(&["case:text:Alpha"]), None).unwrap().0,
        paths(&[&a])
    );
    assert_eq!(
        search(&pin, &query(&["case:text:alpha"]), None).unwrap().0,
        paths(&[&b])
    );
    assert_eq!(
        search(&pin, &query(&["NOT", "case:text:alpha", "ext:txt"]), None)
            .unwrap()
            .0,
        paths(&[&a])
    );
}

#[test]
fn vanished_maybe_paths_are_dropped_in_both_polarities() {
    let tree = Tree::new();
    let a = tree.write("a.txt", b"alpha beta");
    let engine = tree.engine();
    engine.attach_content(&tree.index()).unwrap();
    engine.follow_content(&Budget::unbounded(), None).unwrap();
    fs::remove_file(a).unwrap();
    let pin = engine.pin();
    for args in [
        &["case:text:alpha"][..],
        &["NOT", "case:text:alpha", "ext:txt"],
    ] {
        assert!(search(&pin, &query(args), None).unwrap().0.is_empty());
    }
}

/// Use the kernel's thread-local rchar counter to catch actual postings I/O,
/// without replacing the production reader or relying on a corrupt dictionary.
#[test]
fn an_already_cancelled_query_performs_no_index_io() {
    fn rchar() -> (u64, u64) {
        let counters = fs::read_to_string("/proc/thread-self/io").unwrap();
        let value = counters
            .lines()
            .find_map(|line| {
                line.strip_prefix("rchar:")
                    .and_then(|s| s.trim().parse().ok())
            })
            .unwrap();
        (value, counters.len() as u64)
    }
    let tree = Tree::new();
    for i in 0..100 {
        tree.write(
            &format!("f{i}.txt"),
            format!("alpha beta unique{i}").as_bytes(),
        );
    }
    let engine = tree.engine();
    engine.attach_content(&tree.index()).unwrap();
    engine.follow_content(&Budget::unbounded(), None).unwrap();
    let pin = engine.pin();
    let terms = query(&["text:alpha"; 16]);
    assert_eq!(search(&pin, &terms, None).unwrap().0.len(), 100);
    let cancelled = std::sync::atomic::AtomicBool::new(true);
    let (before, counter_bytes) = rchar();
    assert_eq!(
        pin.search_content(&terms, None, Some(&cancelled), |_| panic!("cancelled row"))
            .unwrap()
            .rows,
        0
    );
    let (after, _) = rchar();
    assert_eq!(
        after - before,
        counter_bytes,
        "only reading the counter itself may add rchar"
    );
}

/// A sparse current replacement must be refused, never accepted as either a
/// positive match or a negative content fact. Both rows remain unverifiable.
#[test]
fn oversized_current_replacements_are_unverifiable_in_both_polarities() {
    use std::io::Write;
    let tree = Tree::new();
    let path = tree.write("a.txt", b"alpha");
    let engine = tree.engine();
    engine.attach_content(&tree.index()).unwrap();
    engine.follow_content(&Budget::unbounded(), None).unwrap();
    let mut replacement = fs::File::create(&path).unwrap();
    replacement.write_all(b"ALPHA").unwrap();
    // Large enough to exceed the shared 8 MiB policy cap, but safe even when
    // running the regression against the old unbounded reader.
    replacement.set_len(256 << 20).unwrap();
    let pin = engine.pin();
    let positive = search(&pin, &query(&["case:text:ALPHA"]), None).unwrap();
    let negative = search(&pin, &query(&["NOT", "case:text:alpha"]), None).unwrap();
    assert!(
        positive.0.is_empty(),
        "oversized positive: {:?}",
        positive.0
    );
    assert!(
        negative.0.is_empty(),
        "oversized negative: {:?}",
        negative.0
    );
    for (_, report) in [positive, negative] {
        let report = report.unwrap();
        assert_eq!(report.verified, 1);
        assert_eq!(
            report.changed, 1,
            "refusal must use the existing failure signal"
        );
        assert_eq!(
            report.matching.documents, 0,
            "refused bytes cannot be evaluated"
        );
    }
}

/// Every row of S3's refusal table, with bounds from the real dictionary
/// planner and actual certainty counts from the real cursors.
#[test]
fn grep_refusal_table_and_scan_override() {
    let tree = Tree::new();
    for i in 0..20 {
        tree.write(
            &format!("file{i}.{}", if i == 0 { "rs" } else { "txt" }),
            format!(
                "common the {} {} unique{i} 123\n",
                if i < 12 { "Foo" } else { "other" },
                if i == 0 { "serde rareliteral" } else { "" }
            )
            .as_bytes(),
        );
    }
    let engine = tree.engine();
    engine.attach_content(&tree.index()).unwrap();
    engine.follow_content(&Budget::unbounded(), None).unwrap();
    let pin = engine.pin();
    let cases: &[(&[&str], bool)] = &[
        (&[r"grep:\d+"], true),
        (&["NOT", r"grep:\d+"], true),
        (&[r"grep:\d+", "OR", "text:rare"], true),
        (&["NOT", "text:common", r"grep:\d+"], true),
        (&["NOT", "grep:rareliteral"], false),
        (&["ext:rs", r"grep:\d+"], false),
        (&["ext:txt", r"grep:\d+"], true),
        (&["text:serde", r"grep:v\d+"], false),
        (&["case:text:Foo"], true),
        (&["grep:the"], true),
    ];
    for &(args, refused) in cases {
        let q = query(args);
        let checked = search(&pin, &q, Some(10_000));
        assert_eq!(
            matches!(checked, Err(RunError::ScanRequired { .. })),
            refused,
            "{args:?}: {checked:?}"
        );
        let (_, report) = search(&pin, &q, None).unwrap();
        let report = report.unwrap();
        assert_eq!(report.bounds.maybe > u64::from(report.live) / 4, refused);
        assert!(report.bounds.maybe >= report.snapshot_maybe);
        assert!(report.bounds.possible >= report.snapshot_possible);
        assert!(report.plan.describe().contains("M="));
    }
}

#[test]
fn grep_case_defaults_verify_folded_candidates() {
    // D67 A2: folded trigram keys must not decide the regex's case semantics.
    let tree = Tree::new();
    let lower = tree.write("lower.txt", b"foo\n");
    let upper = tree.write("upper.txt", b"Foo\n");
    let kelvin = tree.write("kelvin.txt", "Kelvin\n".as_bytes());
    let engine = tree.engine();
    for covered in [false, true] {
        if covered {
            engine.attach_content(&tree.index()).unwrap();
            engine.follow_content(&Budget::unbounded(), None).unwrap();
        }
        for (arg, expected) in [
            ("grep:Foo", paths(&[&upper])),
            ("grep:(?i)Foo", paths(&[&lower, &upper])),
            ("case:grep:Foo", paths(&[&upper])),
            ("case:grep:(?i)Foo", paths(&[&lower, &upper])),
            ("grep:(?i)kelvin", paths(&[&kelvin])),
            ("grep:kelvin", paths(&[])),
            ("text:Foo", paths(&[&lower, &upper])),
            ("case:text:Foo", paths(&[&upper])),
        ] {
            assert_eq!(
                search(&engine.pin(), &query(&[arg]), None).unwrap().0,
                expected,
                "{covered} {arg}"
            );
        }
    }
}

#[test]
fn operator_cases_content_alias_and_phrases_reach_real_documents() {
    // D71: these spellings used to silently become name words or wrong phrases.
    let tree = Tree::new();
    let foo = tree.write("foo.txt", b"foo\n");
    let bar = tree.write("bar.txt", b"bar\n");
    let both = tree.write("foo-bar.txt", b"Foo bar\n");
    let and = tree.write("and", b"this or that\n");
    let or = tree.write("or", b"rock and roll\n");
    let not = tree.write("not", b"do not enter\n");
    let engine = tree.engine();
    for covered in [false, true] {
        if covered {
            engine.attach_content(&tree.index()).unwrap();
            engine.follow_content(&Budget::unbounded(), None).unwrap();
        }
        let pin = engine.pin();
        for op in ["or", "Or", "oR", "OR"] {
            assert_eq!(
                search(&pin, &query(&["text:foo", op, "text:bar"]), None)
                    .unwrap()
                    .0,
                paths(&[&foo, &bar, &both])
            );
        }
        for op in ["and", "And", "aNd", "AND"] {
            assert_eq!(
                search(&pin, &query(&["foo", op, "bar"]), None).unwrap().0,
                paths(&[&both])
            );
        }
        for op in ["not", "Not", "nOt", "NOT"] {
            assert_eq!(
                search(&pin, &query(&["ext:txt", op, "text:foo"]), None)
                    .unwrap()
                    .0,
                paths(&[&bar])
            );
        }
        for (args, expected) in [
            (vec!["content:foo"], paths(&[&foo, &both])),
            (vec!["case:content:Foo"], paths(&[&both])),
            (vec!["name:and"], paths(&[&and])),
            (vec!["name:or"], paths(&[&or])),
            (vec!["name:OR"], paths(&[&or])),
            (vec!["name:not"], paths(&[&not])),
            (vec!["text:this or that"], paths(&[&and])),
            (vec!["text:rock and roll"], paths(&[&or])),
            (vec!["text:do not enter"], paths(&[&not])),
        ] {
            assert_eq!(
                search(&pin, &query(&args), None).unwrap().0,
                expected,
                "{covered} {args:?}"
            );
        }
        for prefix in ["", "case:"] {
            let text = format!("{prefix}text:Foo");
            let content = format!("{prefix}content:Foo");
            assert_eq!(
                search(&pin, &query(&[&content]), None).unwrap().0,
                search(&pin, &query(&[&text]), None).unwrap().0
            );
        }
    }
    let err = Query::from_args(["text:foo OR text:bar"], SystemTime::now()).unwrap_err();
    assert!(
        err.to_string()
            .contains("pass the query as separate arguments")
    );
}

#[test]
fn grep_unicode_raw_bytes_anchors_and_uncovered_negation() {
    let tree = Tree::new();
    let kelvin = tree.write("kelvin.txt", "before\nKelvin\nafter\n".as_bytes());
    let raw = tree.write("raw.txt", b"\x80ab\n");
    let substring = tree.write("substring.txt", b"xHttpRequestx\n");
    let other = tree.write("other.txt", b"http_request\n");
    let engine = tree.engine();
    for covered in [false, true] {
        if covered {
            engine.attach_content(&tree.index()).unwrap();
            engine.follow_content(&Budget::unbounded(), None).unwrap();
        }
        let pin = engine.pin();
        for (args, expected) in [
            (vec!["grep:(?i)kelvin"], paths(&[&kelvin])),
            (vec![r"grep:(?-u:[\x80-\x81])ab"], paths(&[&raw])),
            (vec!["grep:^kelvin$"], paths(&[])),
            (vec!["grep:(?i)^kelvin$"], paths(&[&kelvin])),
            (vec!["grep:(?i)(?-m)^kelvin$"], paths(&[])),
            (vec!["case:grep:(?i)kelvin"], paths(&[&kelvin])),
            (vec!["case:grep:kelvin"], paths(&[])),
            (vec!["grep:HttpRequest"], paths(&[&substring])),
            (
                vec!["NOT", "grep:HttpRequest"],
                paths(&[&kelvin, &raw, &other]),
            ),
        ] {
            let (got, report) = search(&pin, &query(&args), None).unwrap();
            assert_eq!(got, expected, "{covered} {args:?}");
            let report = report.unwrap();
            assert!(report.bounds.maybe >= report.snapshot_maybe);
        }
    }
    for pattern in ["grep:(", "grep:a{100000000}"] {
        assert!(matches!(
            Query::parse(pattern, SystemTime::now()),
            Err(ferret_query::ParseError::Grep(
                _,
                ferret_verify::GrepError::Pattern(_)
            ))
        ));
    }
}

#[test]
fn conservative_bounds_cover_quarters_whole_tokens_caps_fingerprints_and_aliases() {
    let tree = Tree::new();
    let cap = "a".repeat(61);
    for i in 0..20 {
        let mut bytes = format!(
            "httprequest Foo {} singleton{i} {cap}𠀀tail 123 ",
            if i == 0 { "onlyonce" } else { "" }
        );
        for (quarter, term) in ["first", "second", "third", "fourth"].iter().enumerate() {
            if i / 5 != quarter {
                bytes.push_str(term);
                bytes.push(' ');
            }
        }
        tree.write(&format!("file{i}.txt"), bytes.as_bytes());
    }
    // One DocId has two differently named copies: NOT ext:rs may hold for
    // the txt copy, even though its rs copy matches the negated predicate.
    let bytes = fs::read(tree.root().join("file0.txt")).unwrap();
    tree.write("alias.rs", &bytes);
    let engine = tree.engine();
    let pin = engine.pin();
    let catalog = pin.catalog();
    let live = ferret::engine::live_documents(catalog);
    let catalog_view = ferret_index::CatalogView {
        incarnation: catalog.generation().incarnation,
        live: &live,
    };
    let mut writer = ferret_index::IndexWriter::open_with_codec(
        &tree.index().join("index"),
        &catalog_view,
        ferret_index::segment::FINGERPRINT_CODEC,
    )
    .unwrap();
    let mut reader = ferret_crawl::Documents::new(catalog).unwrap();
    writer
        .follow(&catalog_view, &Budget::unbounded(), &mut |doc, out| {
            reader
                .read(catalog, ferret_catalog::DocId(doc), out)
                .map_err(|_| ferret_index::Fault::Unreadable)
        })
        .unwrap();
    engine.open_content(&tree.index()).unwrap();
    let pin = engine.pin();
    for args in [
        vec![
            "NOT",
            "(",
            "text:first",
            "text:second",
            "text:third",
            "text:fourth",
            ")",
            r"grep:\d+",
        ],
        vec!["case:text:Foo"],
        vec!["text:HttpRequest"],
        vec!["text:onlyonce"],
        vec!["NOT", "ext:rs", r"grep:\d+"],
        vec![&format!("text:{cap}")],
    ] {
        let (_, report) = search(&pin, &query(&args), None).unwrap();
        let report = report.unwrap();
        assert!(
            report.bounds.maybe >= report.snapshot_maybe,
            "{args:?} {report:?}"
        );
        assert!(report.bounds.possible >= report.snapshot_possible);
        if args == ["case:text:Foo"] {
            assert_eq!(report.bounds.maybe, report.bounds.possible);
        }
        if args == ["text:onlyonce"] {
            // H planning counts boundary records conservatively without
            // reading the fingerprint block; the actual run is still one.
            assert!(report.bounds.maybe >= 1);
            assert_eq!(report.snapshot_maybe, 1);
            assert_eq!(report.verified, 1);
        }
        if args == [format!("text:{cap}").as_str()] {
            assert_eq!(report.bounds.maybe, 20);
            assert_eq!(report.snapshot_maybe, 20);
            assert_eq!(report.verified, 20);
        }
        if args == ["text:HttpRequest"] {
            assert_eq!(report.bounds.possible, 20);
            assert_eq!(report.snapshot_possible, 20);
        }
    }
}

#[test]
fn not_grep_verifies_the_only_match_past_the_frontier() {
    let tree = Tree::new();
    let old = tree.write("old.txt", b"unrelated content\n");
    let engine = tree.engine();
    engine.attach_content(&tree.index()).unwrap();
    engine.follow_content(&Budget::unbounded(), None).unwrap();
    tree.write("new.txt", b"rareliteral\n");
    tree.refresh(&engine);
    let (got, report) = search(&engine.pin(), &query(&["NOT", "grep:rareliteral"]), None).unwrap();
    assert_eq!(got, paths(&[&old]));
    let report = report.unwrap();
    assert_eq!(report.uncovered, 1);
    assert_eq!(report.snapshot_maybe, 1);
    assert_eq!(report.verified, 1);
}

/// H's uncertain singleton records must obey the same snapshot bounds under
/// partial coverage, unreadable entries and dictionary frequencies of dead
/// documents. The oracle remains brute-force verification of every path.
#[test]
fn mixed_queries_and_bounds_equal_the_oracle_with_h_records() {
    for seed in 10..13 {
        let mut rng = Rng(seed);
        let tree = Tree::new();
        let written = corpus(&tree, &mut rng, 20);
        tree.write("singleton-marker", b"onlyonce\n");
        let engine = tree.engine();
        let pin = engine.pin();
        let catalog = pin.catalog();
        let live = ferret::engine::live_documents(catalog);
        let catalog_view = ferret_index::CatalogView {
            incarnation: catalog.generation().incarnation,
            live: &live,
        };
        let mut writer = ferret_index::IndexWriter::open_with_codec(
            &tree.index().join("index"),
            &catalog_view,
            ferret_index::segment::FINGERPRINT_CODEC,
        )
        .unwrap();
        engine.open_content(&tree.index()).unwrap();
        check(&engine.pin(), &mut rng, 40, "H empty");
        let marker = catalog
            .names()
            .find_map(|(id, bytes)| {
                if bytes != b"singleton-marker" {
                    return None;
                }
                if let ferret_catalog::Target::Inode(inode) = catalog.name(id).target() {
                    catalog.doc(inode).map(|doc| doc.0)
                } else {
                    None
                }
            })
            .unwrap();
        let unreadable: Vec<_> = live
            .range(0, live.bound())
            .filter(|&doc| doc != marker && rng.one_in(5))
            .collect();
        let mut reader = ferret_crawl::Documents::new(catalog).unwrap();
        for partial in [true, false] {
            let budget = if partial {
                Budget {
                    bytes: 30 + rng.below(100) as u64,
                    ..Budget::unbounded()
                }
            } else {
                Budget::unbounded()
            };
            writer
                .follow(&catalog_view, &budget, &mut |doc, out| {
                    if unreadable.contains(&doc) {
                        return Err(ferret_index::Fault::Unreadable);
                    }
                    reader
                        .read(catalog, ferret_catalog::DocId(doc), out)
                        .map_err(|_| ferret_index::Fault::Unreadable)
                })
                .unwrap();
            engine.open_content(&tree.index()).unwrap();
            check(
                &engine.pin(),
                &mut rng,
                40,
                if partial {
                    "H partial"
                } else {
                    "H full with unreadable"
                },
            );
        }
        assert!(
            writer
                .view()
                .segments()
                .iter()
                .map(|segment| segment.fingerprint_records())
                .sum::<u64>()
                > 0
        );
        let retired = fs::read(tree.root().join(&written[0])).unwrap();
        for rel in &written {
            if fs::read(tree.root().join(rel)).unwrap() == retired {
                fs::remove_file(tree.root().join(rel)).unwrap();
            }
        }
        tree.refresh(&engine);
        engine.open_content(&tree.index()).unwrap();
        check(&engine.pin(), &mut rng, 40, "H dead postings");
    }
}

/// T1 (DECISIONS.md D65 update — token policy) through the real pipeline:
/// base64-like runs leave the term index, a blob `text:` argument is
/// answered through trigrams and verification, and a word occurring only
/// inside a blob is found by `grep:` but no longer by `text:`. The blobs are
/// synthetic: seeded random bytes, base64-encoded.
#[test]
fn t1_blobs_route_through_trigrams_and_words_inside_them_are_lost() {
    const BLOB: &str = "nkzzCi5csBBZh21Dh5Z7n8pP1UaPL+fox+KKcXB0xYeDUnGtBJe3j1LIyw==";
    const OTHER: &str = "1JOTcOPq4q3fcyTvh2TyF4M5UUU/RrO8u3Z0y+bzGU4Kq4CjigRY3CWFBF1S3Q==";
    let tree = Tree::new();
    let blob = tree.write("a.txt", format!("token = {BLOB}\n").as_bytes());
    let other = tree.write("b.txt", format!("token = {OTHER}\n").as_bytes());
    let prose = tree.write("c.txt", b"a quiet harbour at dawn\n");
    let engine = tree.engine();
    engine.attach_content(&tree.index()).unwrap();
    engine.follow_content(&Budget::unbounded(), None).unwrap();
    let blob_arg = format!("text:{BLOB}");
    let check = |engine: &Engine| {
        let pin = engine.pin();
        for (args, expected) in [
            (vec![blob_arg.as_str()], paths(&[&blob])),
            (vec!["text:harbour"], paths(&[&prose])),
            (vec!["text:token"], paths(&[&blob, &other])),
            (vec!["text:fox"], paths(&[])),
            (vec!["grep:fox"], paths(&[&blob])),
        ] {
            let (got, report) = search(&pin, &query(&args), None).unwrap();
            assert_eq!(got, expected, "{args:?}");
            if args == [blob_arg.as_str()] {
                // Trigrams screened out the other two; the matcher settled
                // the one left.
                let report = report.unwrap();
                assert_eq!(report.documents, 1, "{report:?}");
                assert_eq!(report.verified, 1, "{report:?}");
            }
        }
    };
    check(&engine);

    // An index recording the term version every index had before T1 is
    // discarded on open and rebuilt by the next follow, never read as if
    // its terms left blobs out.
    drop(engine);
    let dir = tree.index().join(ferret::engine::CONTENT_DIR);
    let mut manifest = ferret_index::Manifest::read(&dir).unwrap().unwrap();
    assert!(!manifest.segments.is_empty());
    manifest.tokenizer_version = 1;
    fs::write(dir.join(ferret_index::manifest::FILE), manifest.encode()).unwrap();
    let engine = tree.engine();
    engine.attach_content(&tree.index()).unwrap();
    let (_, report) = search(&engine.pin(), &query(&["text:harbour"]), None).unwrap();
    assert_eq!(report.unwrap().uncovered, 3, "the old index was discarded");
    engine.follow_content(&Budget::unbounded(), None).unwrap();
    check(&engine);
}

// ------------------------------------------------- parallel verification

/// One run's full output: each row's path, offset and kind in order, the
/// stats, and the content report with its times cleared (the one thing the
/// thread count may change; `verify.rs`).
type Output = (Vec<(Vec<u8>, Option<usize>, String)>, String);

fn output(
    pin: &QuerySession,
    query: &Query,
    bound: Option<u32>,
    threads: usize,
    limit: Option<usize>,
) -> Result<Output, String> {
    let mut rows = Vec::new();
    let stats = pin
        .search_content_threads(query, bound, threads, None, |row| {
            rows.push((row.path.to_vec(), row.offset, format!("{:?}", row.kind)));
            match limit {
                Some(limit) if rows.len() >= limit => ControlFlow::Break(()),
                _ => ControlFlow::Continue(()),
            }
        })
        .map_err(|e| e.to_string())?;
    let mut stats = stats;
    if let Some(report) = &mut stats.content {
        assert_eq!(report.threads, threads);
        report.verification_time = Default::default();
        report.regex_time = Default::default();
        report.threads = 0;
    }
    Ok((rows, format!("{stats:?}")))
}

/// The tree for the thread-count tests: documents of mixed sizes, so that
/// workers finish out of order; copies with equal bytes, so that one
/// document has several names; and, after indexing, changed, vanished and
/// oversized files, so that reads fail or see a current version.
fn mixed_tree(tree: &Tree) -> Engine {
    let path = |i: usize| format!("d{}/f{i:03}.txt", i % 7);
    // Every eleventh file shares its bytes with the file before.
    let copied = |i: usize| i % 11 == 10;
    for i in 0..240 {
        let n = if copied(i) { i - 1 } else { i };
        let pad = if n % 37 == 0 { 400_000 } else { n * 13 % 900 };
        let marker = match n % 5 {
            0 => "needle7 here",
            1 => "Needle42 there",
            2 => "need le",
            _ => "nothing",
        };
        tree.write(
            &path(i),
            format!("{} {marker} {n}\n", "x".repeat(pad)).as_bytes(),
        );
    }
    let engine = tree.engine();
    engine.attach_content(&tree.index()).unwrap();
    engine.follow_content(&Budget::unbounded(), None).unwrap();
    let pin = engine.pin();
    let catalog = pin.catalog();
    let doc = |i: usize| {
        let name = format!("f{i:03}.txt");
        catalog
            .names()
            .find_map(|(id, bytes)| match catalog.name(id).target() {
                ferret_catalog::Target::Inode(inode) if bytes == name.as_bytes() => {
                    catalog.doc(inode)
                }
                _ => None,
            })
            .unwrap_or_else(|| panic!("{name} has no document"))
    };
    for i in (0..240).filter(|&i| copied(i)) {
        assert_eq!(doc(i), doc(i - 1), "f{i:03} is a copy of the file before");
        assert_ne!(doc(i), doc(i + 1), "f{:03} is not a copy", i + 1);
    }
    drop(pin);
    for i in [3, 50, 51, 120] {
        fs::remove_file(tree.root().join(path(i))).unwrap();
    }
    for i in [5, 10, 21, 64] {
        tree.write(
            &path(i),
            format!("changed needle99 {i} and longer\n").as_bytes(),
        );
    }
    // Over the reader's size cap: unverifiable, so dropped.
    tree.write("d0/f070.txt", &vec![b'n'; 9 << 20]);
    engine
}

/// D76 C: over files that do not change during the query, output is
/// byte-identical at any verification thread count, including where a limit
/// stops it, and where reads fail. The scheduler picks the completion order
/// here; `ferret-query`'s `tests/verify.rs` forces the orders that matter.
#[test]
fn verification_output_is_the_same_at_every_thread_count() {
    let tree = Tree::new();
    let engine = mixed_tree(&tree);
    let pin = engine.pin();
    let queries: &[&[&str]] = &[
        &[r"grep:needle\d+"],
        &[r"grep:(?i)needle\d+"],
        &[r"grep:(?i)needle\d+", "OR", "text:nothing"],
        &["NOT", r"grep:needle\d+", "ext:txt"],
        &["text:needle7"],
        &["text:need", "name:f0"],
        &[r"grep:\d+"],
    ];
    for args in queries {
        let q = query(args);
        for bound in [None, Some(10_000)] {
            let one = output(&pin, &q, bound, 1, None);
            if let Ok((rows, _)) = &one {
                assert!(!rows.is_empty() || args[0] == "NOT", "{args:?}");
            }
            for threads in [2, 3, 8] {
                assert_eq!(
                    output(&pin, &q, bound, threads, None),
                    one,
                    "{args:?} {bound:?} at {threads} threads"
                );
            }
            let Ok((rows, _)) = one else {
                // The refusal: the same error at every count.
                continue;
            };
            for limit in 1..=rows.len().min(40) {
                let one = output(&pin, &q, bound, 1, Some(limit));
                for threads in [2, 8] {
                    assert_eq!(
                        output(&pin, &q, bound, threads, Some(limit)),
                        one,
                        "{args:?} {bound:?} limit {limit} at {threads} threads"
                    );
                }
            }
        }
    }
    // The error case is exercised: a refusal, and failed reads.
    assert!(output(&pin, &query(&[r"grep:\d+"]), Some(10_000), 4, None).is_err());
    let (_, stats) = output(&pin, &query(&[r"grep:(?i)needle\d+"]), None, 4, None).unwrap();
    assert!(!stats.contains("changed: 0"), "{stats}");
}
