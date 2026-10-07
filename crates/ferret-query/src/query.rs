//! [`Query`]: parsed atoms, compiled matchers, and the plan chosen for them.
//! The grammar is in the crate doc.

use std::fmt;
use std::time::{SystemTime, UNIX_EPOCH};

use ferret_catalog::{Kind, Section};
use ferret_verify::{Finder, Regex, RegexError, Text};

use crate::expr::{self, Node, Test, Token};
use crate::pattern::{glob_literal, glob_regex, regex_literal};

/// A parsed and compiled query. Build one with [`Query::parse`] or
/// [`Query::from_args`]; run it with [`Query::run`](crate::Query::run).
#[derive(Debug)]
pub struct Query {
    /// Tests on a name's bytes; every one must pass.
    pub(crate) names: Vec<NameTest>,
    /// Tests on a result's whole path.
    pub(crate) paths: Vec<PathTest>,
    /// Tests on the inode row.
    pub(crate) meta: Vec<MetaTest>,
    /// The literal the heap scan looks for, when a name test has one.
    pub(crate) driver: Option<Driver>,
    /// Seconds since the epoch that `mtime:` ages count back from.
    pub(crate) now: i64,
    /// Top-level conjuncts that are not plain name or metadata atoms: a
    /// content atom, `OR`, `NOT` or a group. Each must hold; they are
    /// evaluated per row, after the tests above. Empty for an S1 query.
    pub(crate) residual: Vec<Node<Test>>,
    /// The content atoms `Test::Content` indexes.
    pub(crate) texts: Vec<Text>,
}

/// How [`Query::run`](crate::Query::run) finds its candidates.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Strategy {
    /// Scan the name heap for the driving literal; test only the names it
    /// hits (D14). Reads the name sections, and no inode sections for plain
    /// output.
    HeapScan,
    /// Test every inode row against the metadata atoms, then walk the name
    /// rows for the inodes that pass. For a query with metadata atoms and no
    /// literal.
    InodeScan,
    /// Test every name: a query with neither a literal nor metadata (a regex
    /// with no required literal, a bare `*`, or nothing at all).
    AllNames,
}

#[derive(Debug)]
pub(crate) struct Driver {
    pub(crate) finder: Finder,
    /// The name test the literal came from. A plain substring is proved by
    /// the scan hit itself, so it is not re-tested.
    pub(crate) from: usize,
}

#[derive(Debug)]
pub(crate) enum NameTest {
    Substring(Finder),
    Term(Vec<u8>),
    /// A name ending in `.ext`, ASCII-folded when the flag is set.
    Ext(Vec<u8>, bool),
    Glob(Regex, Vec<u8>),
    Regex(Regex, String),
}

#[derive(Debug)]
pub(crate) enum PathTest {
    Substring(Finder),
    Glob(Regex, Vec<u8>),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Cmp {
    Less,
    Greater,
    Equal,
}

#[derive(Clone, Copy, Debug)]
pub(crate) enum MetaTest {
    Size(Cmp, u64),
    /// Compared against `now - mtime`, in seconds.
    Age(Cmp, i64),
    Type(Kind),
}

/// Why an argument is not a valid atom. Each variant carries the argument.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ParseError {
    /// `size:` needs `N`, `<N` or `>N`, with an optional `k`, `M`, `G` or
    /// `T` (powers of 1024).
    Size(String),
    Term(String),
    /// `mtime:` needs `<N` or `>N` with a unit `s`, `m`, `h`, `d`, `w` or
    /// `y`.
    Age(String),
    /// `type:` needs `f`, `d` or `l` (or `file`, `dir`, `link`).
    Type(String),
    /// An empty argument, or a prefix with nothing after it, such as
    /// `ext:` or `case:`. An empty word would match at every name boundary.
    Empty(String),
    /// A `re:` pattern that does not compile.
    Regex(String, RegexError),
    /// An argument that is not UTF-8 where a pattern must be (`re:`).
    NotUtf8(Vec<u8>),
    /// An argument containing a NUL byte. No name contains one (it ends a
    /// name in the heap), so the atom could only match name boundaries.
    Nul(Vec<u8>),
    /// `text:` whose argument holds no token: nothing to search for.
    Text(String),
    /// `OR` or `NOT` (the operator) without a query on a side it needs.
    Dangling(&'static str),
    /// A `(` that no `)` closes.
    Unclosed,
    /// A `)` that no `(` opened.
    Unopened,
    /// `( )`, a group with nothing in it.
    EmptyGroup,
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Term(a) => write!(
                f,
                "`{a}`: name-term needs one alphanumeric/underscore token"
            ),
            Self::Size(a) => write!(f, "`{a}`: size is N, <N or >N, with k, M, G or T"),
            Self::Age(a) => write!(f, "`{a}`: mtime is <N or >N with s, m, h, d, w or y"),
            Self::Type(a) => write!(f, "`{a}`: type is f, d or l"),
            Self::Empty(a) if a.is_empty() => write!(f, "an empty argument is not a query"),
            Self::Empty(a) => write!(f, "`{a}` needs a value"),
            Self::Regex(a, e) => write!(f, "`{a}`: {e}"),
            Self::NotUtf8(a) => write!(f, "`{}`: a regex must be UTF-8", a.escape_ascii()),
            Self::Nul(a) => write!(f, "`{}`: a query cannot contain NUL", a.escape_ascii()),
            Self::Text(a) => write!(f, "`{a}`: text: needs a letter or digit to search for"),
            Self::Dangling("NOT") => write!(f, "`NOT` needs a query after it"),
            Self::Dangling(op) => write!(f, "`{op}` needs a query on each side"),
            Self::Unclosed => write!(f, "a `(` is not closed by a `)`"),
            Self::Unopened => write!(f, "a `)` has no `(` before it"),
            Self::EmptyGroup => write!(f, "`( )` holds no query"),
        }
    }
}

impl std::error::Error for ParseError {}

impl Query {
    /// Parses whitespace-separated atoms: a convenience for tests and
    /// benchmarks, where no atom contains a space.
    pub fn parse(text: &str, now: SystemTime) -> Result<Query, ParseError> {
        Self::from_args(text.split_whitespace().map(str::as_bytes), now)
    }

    /// Parses one atom per argument, as the shell split them: an argument
    /// with a space in it is one atom. `now` is what `mtime:` ages count
    /// back from.
    pub fn from_args<I, A>(args: I, now: SystemTime) -> Result<Query, ParseError>
    where
        I: IntoIterator<Item = A>,
        A: AsRef<[u8]>,
    {
        let now = match now.duration_since(UNIX_EPOCH) {
            Ok(d) => d.as_secs() as i64,
            Err(e) => -(e.duration().as_secs() as i64),
        };
        let mut tokens = Vec::new();
        for arg in args {
            let arg = arg.as_ref();
            if arg.contains(&0) {
                return Err(ParseError::Nul(arg.to_vec()));
            }
            tokens.push(match arg {
                b"OR" => Token::Or,
                b"NOT" => Token::Not,
                b"(" => Token::Open,
                b")" => Token::Close,
                _ => {
                    let (fold, atom) = match arg.strip_prefix(b"case:") {
                        Some(rest) => (false, rest),
                        None => (true, arg),
                    };
                    Token::Atom((fold, parse_atom(atom, fold, arg)?))
                }
            });
        }
        let mut query = Query {
            names: Vec::new(),
            paths: Vec::new(),
            meta: Vec::new(),
            driver: None,
            now,
            residual: Vec::new(),
            texts: Vec::new(),
        };
        // Candidate drivers: (literal, fold, name test index).
        let mut literals: Vec<(Vec<u8>, bool, usize)> = Vec::new();
        for conjunct in expr::parse(tokens)?.into_conjuncts() {
            let (fold, atom) = match conjunct {
                Node::Leaf((fold, atom)) if !matches!(atom, Atom::Text(_)) => (fold, atom),
                tree => {
                    let texts = &mut query.texts;
                    query.residual.push(tree.map(&mut |(_, atom)| atom.test(texts)));
                    continue;
                }
            };
            let name_index = query.names.len();
            match atom {
                Atom::Name(test, literal) => {
                    if let Some(literal) = literal {
                        literals.push((literal, fold, name_index));
                    }
                    query.names.push(test);
                }
                Atom::PathGlob(regex, glob) => {
                    // The last component matches the name, so its literal
                    // can still drive the scan; the path test does the rest.
                    if let Some(literal) = glob_literal(&glob) {
                        literals.push((literal, fold, usize::MAX));
                    }
                    query.paths.push(PathTest::Glob(regex, glob));
                }
                Atom::Path(test) => query.paths.push(test),
                Atom::Meta(test) => query.meta.push(test),
                Atom::Text(_) => unreachable!("content atoms are residual"),
            }
        }
        // The longest literal drives; a longer needle has fewer candidates
        // and a better pair to filter on.
        if let Some((literal, fold, from)) = literals
            .into_iter()
            .max_by_key(|(literal, _, index)| (literal.len(), std::cmp::Reverse(*index)))
        {
            query.driver = Some(Driver {
                finder: Finder::new(&literal, fold),
                from,
            });
        }
        Ok(query)
    }

    /// Whether the query has a content atom, and so needs the content index
    /// and a reader to run ([`Query::run_content`](crate::Query::run_content)).
    pub fn has_content(&self) -> bool {
        !self.texts.is_empty()
    }

    /// How [`Query::run`](crate::Query::run) will find candidates.
    pub fn strategy(&self) -> Strategy {
        if self.driver.is_some() {
            Strategy::HeapScan
        } else if !self.meta.is_empty() {
            Strategy::InodeScan
        } else {
            Strategy::AllNames
        }
    }

    /// The plan in one line, for `--explain` and the query log: the
    /// strategy, the driving literal, then every test in the order it runs.
    pub fn explain(&self) -> String {
        let mut out = match (&self.driver, self.strategy()) {
            (Some(d), _) => format!(
                "heap scan for \"{}\"{}",
                d.finder.needle().escape_ascii(),
                if d.finder.folds() { " (folded)" } else { "" }
            ),
            (None, Strategy::InodeScan) => format!(
                "inode scan for {}",
                self.meta
                    .iter()
                    .map(MetaTest::describe)
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            (None, _) => "all names".to_string(),
        };
        let mut tests = Vec::new();
        for (i, test) in self.names.iter().enumerate() {
            let driven = matches!(test, NameTest::Substring(_))
                && self.driver.as_ref().is_some_and(|d| d.from == i);
            if !driven {
                tests.push(test.describe());
            }
        }
        if self.strategy() != Strategy::InodeScan {
            tests.extend(self.meta.iter().map(MetaTest::describe));
        }
        tests.extend(self.paths.iter().map(PathTest::describe));
        tests.extend(self.residual.iter().map(|tree| tree.describe(&|t| self.describe_test(t))));
        if !tests.is_empty() {
            out.push_str("; then ");
            out.push_str(&tests.join(", "));
        }
        out
    }

    fn describe_test(&self, test: &Test) -> String {
        match test {
            Test::Name(t) => t.describe(),
            Test::Path(t) => t.describe(),
            Test::Meta(t) => t.describe(),
            Test::Content(i) => {
                let text = &self.texts[*i];
                let units: Vec<String> = text
                    .units()
                    .map(|u| u.escape_ascii().to_string())
                    .collect();
                format!(
                    "text \"{}\"{}",
                    units.join(" "),
                    if text.is_case_sensitive() { "" } else { " (folded)" }
                )
            }
        }
    }
}

enum Atom {
    /// A name test, and the literal every matching name contains.
    Name(NameTest, Option<Vec<u8>>),
    /// A glob with a `/`: its regex, and the glob for its last component's
    /// literal.
    PathGlob(Regex, Vec<u8>),
    Path(PathTest),
    Meta(MetaTest),
    /// `text:ARG`, a content atom.
    Text(Text),
}

impl Atom {
    /// The per-row test for this atom; a content atom is added to `texts`.
    fn test(self, texts: &mut Vec<Text>) -> Test {
        match self {
            Atom::Name(test, _) => Test::Name(test),
            Atom::PathGlob(regex, glob) => Test::Path(PathTest::Glob(regex, glob)),
            Atom::Path(test) => Test::Path(test),
            Atom::Meta(test) => Test::Meta(test),
            Atom::Text(text) => {
                texts.push(text);
                Test::Content(texts.len() - 1)
            }
        }
    }
}

fn parse_atom(atom: &[u8], fold: bool, arg: &[u8]) -> Result<Atom, ParseError> {
    let text = || String::from_utf8_lossy(arg).into_owned();
    if atom.is_empty() {
        return Err(ParseError::Empty(text()));
    }
    let value = |prefix: &[u8]| -> Result<Option<&[u8]>, ParseError> {
        match atom.strip_prefix(prefix) {
            Some([]) => Err(ParseError::Empty(text())),
            other => Ok(other),
        }
    };
    if let Some(v) = value(b"name-term:")? {
        let token = ferret_text::normalize_token(v).ok_or_else(|| ParseError::Term(text()))?;
        return Ok(Atom::Name(NameTest::Term(token), None));
    }
    if let Some(v) = value(b"re:")? {
        let pattern = std::str::from_utf8(v).map_err(|_| ParseError::NotUtf8(arg.to_vec()))?;
        let regex = Regex::new(pattern, fold).map_err(|e| ParseError::Regex(text(), e))?;
        let literal = regex_literal(pattern, fold);
        return Ok(Atom::Name(
            NameTest::Regex(regex, pattern.to_string()),
            literal,
        ));
    }
    if let Some(v) = value(b"ext:")? {
        let mut suffix = b".".to_vec();
        suffix.extend(v);
        return Ok(Atom::Name(
            NameTest::Ext(suffix.clone(), fold),
            Some(suffix),
        ));
    }
    if let Some(v) = value(b"path:")? {
        return Ok(Atom::Path(PathTest::Substring(Finder::new(v, fold))));
    }
    if let Some(v) = value(b"size:")? {
        return parse_size(v)
            .map(Atom::Meta)
            .ok_or_else(|| ParseError::Size(text()));
    }
    if let Some(v) = value(b"mtime:")? {
        return parse_age(v)
            .map(Atom::Meta)
            .ok_or_else(|| ParseError::Age(text()));
    }
    if let Some(v) = value(b"type:")? {
        let kind = match v {
            b"f" | b"file" => Kind::File,
            b"d" | b"dir" => Kind::Dir,
            b"l" | b"link" => Kind::Symlink,
            _ => return Err(ParseError::Type(text())),
        };
        return Ok(Atom::Meta(MetaTest::Type(kind)));
    }
    if let Some(v) = value(b"text:")? {
        return Text::new(v, !fold)
            .map(Atom::Text)
            .ok_or_else(|| ParseError::Text(text()));
    }
    if let Some(v) = value(b"name:")? {
        return word(v, fold, arg);
    }
    word(atom, fold, arg)
}

/// A word or glob: the meaning of an argument with no prefix, and of
/// `name:`'s value, which is never an operator or a prefix (`name:OR`,
/// `name:text:x`).
fn word(atom: &[u8], fold: bool, arg: &[u8]) -> Result<Atom, ParseError> {
    let text = || String::from_utf8_lossy(arg).into_owned();
    let is_glob = atom.iter().any(|b| matches!(b, b'*' | b'?' | b'['));
    let has_slash = atom.contains(&b'/');
    if is_glob {
        let regex =
            Regex::new(&glob_regex(atom), fold).map_err(|e| ParseError::Regex(text(), e))?;
        return Ok(if has_slash {
            Atom::PathGlob(regex, atom.to_vec())
        } else {
            Atom::Name(NameTest::Glob(regex, atom.to_vec()), glob_literal(atom))
        });
    }
    if has_slash {
        return Ok(Atom::Path(PathTest::Substring(Finder::new(atom, fold))));
    }
    Ok(Atom::Name(
        NameTest::Substring(Finder::new(atom, fold)),
        Some(atom.to_vec()),
    ))
}

fn comparison(v: &[u8]) -> (Cmp, &[u8]) {
    match v.split_first() {
        Some((b'<', rest)) => (Cmp::Less, rest),
        Some((b'>', rest)) => (Cmp::Greater, rest),
        _ => (Cmp::Equal, v),
    }
}

/// Splits `12k` into 12 and `k`.
fn number(v: &[u8]) -> Option<(u64, &[u8])> {
    let digits = v.iter().take_while(|b| b.is_ascii_digit()).count();
    let n = std::str::from_utf8(&v[..digits]).ok()?.parse().ok()?;
    Some((n, &v[digits..]))
}

fn parse_size(v: &[u8]) -> Option<MetaTest> {
    let (cmp, v) = comparison(v);
    let (n, unit) = number(v)?;
    let shift = match unit.to_ascii_lowercase().as_slice() {
        b"" | b"b" => 0,
        b"k" => 10,
        b"m" => 20,
        b"g" => 30,
        b"t" => 40,
        _ => return None,
    };
    // `checked_shl` only rejects a shift of 64 or more, not lost bits.
    (n <= u64::MAX >> shift).then_some(MetaTest::Size(cmp, n << shift))
}

fn parse_age(v: &[u8]) -> Option<MetaTest> {
    let (cmp, v) = comparison(v);
    if cmp == Cmp::Equal {
        return None;
    }
    let (n, unit) = number(v)?;
    let seconds: i64 = match unit {
        b"s" => 1,
        b"m" => 60,
        b"h" => 3600,
        b"d" => 86_400,
        b"w" => 7 * 86_400,
        b"y" => 365 * 86_400,
        _ => return None,
    };
    Some(MetaTest::Age(
        cmp,
        i64::try_from(n).ok()?.checked_mul(seconds)?,
    ))
}

// ── evaluation and description ──

impl Cmp {
    pub(crate) fn holds<T: Ord>(self, value: T, bound: T) -> bool {
        match self {
            Cmp::Less => value < bound,
            Cmp::Greater => value > bound,
            Cmp::Equal => value == bound,
        }
    }

    fn symbol(self) -> &'static str {
        match self {
            Cmp::Less => "<",
            Cmp::Greater => ">",
            Cmp::Equal => "=",
        }
    }
}

impl NameTest {
    pub(crate) fn matches(&self, name: &[u8]) -> bool {
        match self {
            NameTest::Substring(finder) => finder.is_match(name),
            NameTest::Term(token) => ferret_text::has_token(name, token),
            NameTest::Ext(suffix, fold) => {
                let Some(end) = name.len().checked_sub(suffix.len()).filter(|&at| at > 0) else {
                    return false;
                };
                let end = &name[end..];
                if *fold {
                    end.eq_ignore_ascii_case(suffix)
                } else {
                    end == suffix.as_slice()
                }
            }
            NameTest::Glob(regex, _) | NameTest::Regex(regex, _) => {
                ferret_verify::Matcher::is_match(regex, name)
            }
        }
    }

    fn describe(&self) -> String {
        match self {
            NameTest::Term(token) => format!("name term {}", token.escape_ascii()),
            NameTest::Substring(f) => format!(
                "name has \"{}\"{}",
                f.needle().escape_ascii(),
                if f.folds() { " (folded)" } else { "" }
            ),
            NameTest::Ext(suffix, fold) => format!(
                "name ends \"{}\"{}",
                suffix.escape_ascii(),
                if *fold { " (folded)" } else { "" }
            ),
            NameTest::Glob(_, glob) => format!("name glob {}", glob.escape_ascii()),
            NameTest::Regex(_, pattern) => format!("name regex {pattern}"),
        }
    }
}

impl PathTest {
    pub(crate) fn matches(&self, path: &[u8]) -> bool {
        match self {
            PathTest::Substring(finder) => finder.is_match(path),
            PathTest::Glob(regex, _) => ferret_verify::Matcher::is_match(regex, path),
        }
    }

    fn describe(&self) -> String {
        match self {
            PathTest::Substring(f) => format!("path has \"{}\"", f.needle().escape_ascii()),
            PathTest::Glob(_, glob) => format!("path glob {}", glob.escape_ascii()),
        }
    }
}

impl MetaTest {
    /// The catalog sections evaluating this test reads: the one inode
    /// field it tests, or for a type the link rows, where
    /// [`Catalog::kind`](ferret_catalog::Catalog::kind) looks a
    /// non-directory up. A metadata-first scan loads the union of these
    /// before it tests a row, and nothing else.
    pub(crate) fn sections(&self) -> &'static [Section] {
        match self {
            MetaTest::Size(..) => &[Section::Size],
            MetaTest::Age(..) => &[Section::Mtime],
            MetaTest::Type(_) => &[Section::Links],
        }
    }

    fn describe(&self) -> String {
        match self {
            MetaTest::Size(cmp, n) => format!("size {}{n}", cmp.symbol()),
            MetaTest::Age(cmp, s) => format!("age {}{s}s", cmp.symbol()),
            MetaTest::Type(kind) => format!("type {kind:?}"),
        }
    }
}
