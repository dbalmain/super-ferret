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
    let text = std::str::from_utf8(text).unwrap();
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
            n if n > 0 && rng.one_in(6) => {
                fs::read_to_string(tree.root().join(&written[rng.below(n)])).unwrap()
            }
            _ if rng.one_in(10) => String::new(),
            _ => content(rng),
        };
        tree.write(&rel, text.as_bytes());
        written.push(rel);
    }
    written
}

/// A query tree, which serialises to arguments and evaluates on a path.
#[derive(Clone, Debug)]
enum Q {
    Text(String, bool),
    /// A bare word: a folded name substring.
    Word(String),
    /// `name:OR` (folded) or `case:OR` (exact).
    Prefixed(String, bool),
    And(Vec<Q>),
    Or(Vec<Q>),
    Not(Box<Q>),
}

fn atom(rng: &mut Rng) -> Q {
    match rng.below(6) {
        0 => Q::Word(rng.pick(NAMES).to_ascii_lowercase()),
        1 => Q::Prefixed("OR".to_owned(), rng.one_in(2)),
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
            Q::Text(arg, case) => {
                out.push(format!("{}text:{arg}", if *case { "case:" } else { "" }))
            }
            Q::Word(word) => out.push(word.clone()),
            Q::Prefixed(word, case) => {
                out.push(format!("{}{word}", if *case { "case:" } else { "name:" }))
            }
            Q::And(all) => {
                for q in all {
                    q.operand(rng, out, matches!(q, Q::Or(_)));
                }
            }
            Q::Or(any) => {
                for (i, q) in any.iter().enumerate() {
                    if i > 0 {
                        out.push("OR".to_owned());
                    }
                    q.operand(rng, out, matches!(q, Q::Or(_)));
                }
            }
            Q::Not(inner) => {
                out.push("NOT".to_owned());
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
        assert_eq!(search(&pin, &text, Some(3)).unwrap().0, paths(&[&a]));
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
        .search_content(&query(&["text:alpha beta"]), None, None, |row| {
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
