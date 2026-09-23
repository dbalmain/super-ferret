//! Gitignore pattern parsing and matching.
//!
//! The conformance suite is derived from `gitignore(5)`. Git 2.54's
//! `check-ignore` is used only as a black-box oracle where the manual is
//! ambiguous; neither Git nor third-party matcher source is used.

use std::collections::HashMap;
use std::path::Path;

/// The result of matching one path against one ignore file.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Match {
    None,
    Ignore,
    Whitelist,
}

/// One invalid line, omitted from the compiled matcher.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct LineError {
    pub(crate) line: usize,
    pub(crate) pattern: String,
    pub(crate) detail: String,
}

/// A compiled ignore file. Pattern positions are retained so the fast-path
/// buckets and general patterns can jointly implement last-match-wins.
#[derive(Clone, Debug, Default)]
pub(crate) struct Gitignore {
    literals: HashMap<Vec<u8>, Vec<FastMatch>>,
    extensions: HashMap<Vec<u8>, Vec<FastMatch>>,
    general: Vec<Pattern>,
}

#[derive(Clone, Copy, Debug)]
struct FastMatch {
    index: usize,
    result: Match,
    directory_only: bool,
}

#[derive(Clone, Debug)]
struct Pattern {
    index: usize,
    result: Match,
    directory_only: bool,
    basename_only: bool,
    tokens: Vec<Token>,
}

#[derive(Clone, Debug)]
enum Token {
    Literal(u8),
    Any,
    Star,
    /// A leading `**/`: zero or more complete path components.
    LeadingDirectories,
    /// A middle `/**/`: a slash and zero or more complete components.
    MiddleDirectories,
    /// A trailing `/**`: descendants, or the directory before the suffix.
    TrailingEverything,
    Class {
        negated: bool,
        ranges: Vec<(u8, u8)>,
    },
}

impl Gitignore {
    /// Compiles all valid lines and returns invalid ones separately. A bad line
    /// never prevents another line in the same file from applying.
    pub(crate) fn compile(text: &str) -> (Self, Vec<LineError>) {
        let mut matcher = Self::default();
        let mut errors = Vec::new();
        for (index, original) in text.lines().enumerate() {
            match Pattern::compile(index, original) {
                Ok(Some(pattern)) => matcher.push(pattern),
                Ok(None) => {}
                Err(detail) => errors.push(LineError {
                    line: index + 1,
                    pattern: original.to_owned(),
                    detail,
                }),
            }
        }
        (matcher, errors)
    }

    /// Returns the last matching line's action for `path` itself. Exclusion by
    /// a matching parent directory is deliberately the walker's responsibility.
    pub(crate) fn matched(&self, path: &Path, is_dir: bool) -> Match {
        let bytes = path.as_os_str().as_encoded_bytes();
        let basename = bytes
            .iter()
            .rposition(|byte| *byte == b'/')
            .map_or(bytes, |slash| &bytes[slash + 1..]);

        let mut best = self
            .literals
            .get(basename)
            .and_then(|entries| latest_fast(entries, is_dir));
        if let Some(dot) = basename.iter().rposition(|byte| *byte == b'.')
            && let Some(candidate) = self
                .extensions
                .get(&basename[dot..])
                .and_then(|entries| latest_fast(entries, is_dir))
            && best.is_none_or(|current| candidate.index > current.index)
        {
            best = Some(candidate);
        }

        for pattern in self.general.iter().rev() {
            if best.is_some_and(|candidate| pattern.index < candidate.index) {
                break;
            }
            if pattern.matches(bytes, basename, is_dir) {
                return pattern.result;
            }
        }
        best.map_or(Match::None, |candidate| candidate.result)
    }

    fn push(&mut self, pattern: Pattern) {
        let fast = FastMatch {
            index: pattern.index,
            result: pattern.result,
            directory_only: pattern.directory_only,
        };
        if pattern.basename_only
            && let Some(literal) = literal_bytes(&pattern.tokens)
        {
            self.literals.entry(literal).or_default().push(fast);
            return;
        }
        if pattern.basename_only
            && let Some(extension) = simple_extension(&pattern.tokens)
        {
            self.extensions.entry(extension).or_default().push(fast);
            return;
        }
        self.general.push(pattern);
    }
}

impl Pattern {
    fn compile(index: usize, original: &str) -> Result<Option<Self>, String> {
        let line = trim_trailing_spaces(original);
        if line.is_empty() || line.starts_with('#') {
            return Ok(None);
        }

        let (result, mut body) = match line.strip_prefix('!') {
            Some(rest) => (Match::Whitelist, rest),
            None => (Match::Ignore, line),
        };
        if body.is_empty() {
            return Ok(None);
        }

        let leading_slash = body.starts_with('/');
        if leading_slash {
            body = &body[1..];
        }
        let directory_only = body.ends_with('/');
        if directory_only {
            body = &body[..body.len() - 1];
        }
        if body.is_empty() {
            return Ok(None);
        }

        let basename_only = !leading_slash && !body.as_bytes().contains(&b'/');
        let tokens = compile_tokens(body.as_bytes())?;
        Ok(Some(Self {
            index,
            result,
            directory_only,
            basename_only,
            tokens,
        }))
    }

    fn matches(&self, path: &[u8], basename: &[u8], is_dir: bool) -> bool {
        if self.directory_only && !is_dir {
            return false;
        }
        let candidate = if self.basename_only { basename } else { path };
        matches_tokens(&self.tokens, candidate)
    }
}

fn latest_fast(entries: &[FastMatch], is_dir: bool) -> Option<FastMatch> {
    entries
        .iter()
        .rev()
        .copied()
        .find(|entry| !entry.directory_only || is_dir)
}

fn literal_bytes(tokens: &[Token]) -> Option<Vec<u8>> {
    tokens
        .iter()
        .map(|token| match token {
            Token::Literal(byte) => Some(*byte),
            _ => None,
        })
        .collect()
}

fn simple_extension(tokens: &[Token]) -> Option<Vec<u8>> {
    let [Token::Star, Token::Literal(b'.'), rest @ ..] = tokens else {
        return None;
    };
    if rest.is_empty()
        || rest
            .iter()
            .any(|token| !matches!(token, Token::Literal(byte) if *byte != b'.'))
    {
        return None;
    }
    let mut extension = Vec::with_capacity(rest.len() + 1);
    extension.push(b'.');
    extension.extend(rest.iter().filter_map(|token| match token {
        Token::Literal(byte) => Some(*byte),
        _ => None,
    }));
    Some(extension)
}

fn trim_trailing_spaces(mut line: &str) -> &str {
    while line.ends_with(' ') {
        let before = &line.as_bytes()[..line.len() - 1];
        let backslashes = before
            .iter()
            .rev()
            .take_while(|byte| **byte == b'\\')
            .count();
        if backslashes % 2 == 1 {
            break;
        }
        line = &line[..line.len() - 1];
    }
    line
}

fn compile_tokens(pattern: &[u8]) -> Result<Vec<Token>, String> {
    let mut tokens = Vec::with_capacity(pattern.len());
    let mut at = 0;
    while at < pattern.len() {
        match pattern[at] {
            b'\\' => {
                let Some(&escaped) = pattern.get(at + 1) else {
                    return Err("trailing backslash".to_owned());
                };
                tokens.push(Token::Literal(escaped));
                at += 2;
            }
            b'?' => {
                tokens.push(Token::Any);
                at += 1;
            }
            b'[' => {
                if let Some((class, end)) = compile_class(pattern, at) {
                    tokens.push(class);
                    at = end;
                } else {
                    tokens.push(Token::Literal(b'['));
                    at += 1;
                }
            }
            b'*' => {
                let end = pattern[at..]
                    .iter()
                    .position(|byte| *byte != b'*')
                    .map_or(pattern.len(), |offset| at + offset);
                let count = end - at;
                if count == 2 && at == 0 && pattern.get(end) == Some(&b'/') {
                    tokens.push(Token::LeadingDirectories);
                    at = end + 1;
                } else {
                    tokens.push(Token::Star);
                    at = end;
                }
            }
            b'/' if pattern[at..].starts_with(b"/**/") => {
                tokens.push(Token::MiddleDirectories);
                at += 4;
            }
            b'/' if pattern[at..] == *b"/**" => {
                tokens.push(Token::TrailingEverything);
                at = pattern.len();
            }
            byte => {
                tokens.push(Token::Literal(byte));
                at += 1;
            }
        }
    }
    Ok(tokens)
}

fn compile_class(pattern: &[u8], start: usize) -> Option<(Token, usize)> {
    let mut at = start + 1;
    let negated = matches!(pattern.get(at), Some(b'!') | Some(b'^'));
    if negated {
        at += 1;
    }

    let mut members = Vec::new();
    if pattern.get(at) == Some(&b']') {
        members.push(b']');
        at += 1;
    }
    while at < pattern.len() && pattern[at] != b']' {
        if pattern[at] == b'\\' {
            at += 1;
            let &escaped = pattern.get(at)?;
            members.push(escaped);
            at += 1;
        } else {
            members.push(pattern[at]);
            at += 1;
        }
    }
    if pattern.get(at) != Some(&b']') || members.is_empty() {
        return None;
    }

    let mut ranges = Vec::new();
    let mut member = 0;
    while member < members.len() {
        if member + 2 < members.len() && members[member + 1] == b'-' {
            ranges.push((members[member], members[member + 2]));
            member += 3;
        } else {
            ranges.push((members[member], members[member]));
            member += 1;
        }
    }
    Some((Token::Class { negated, ranges }, at + 1))
}

fn matches_tokens(tokens: &[Token], path: &[u8]) -> bool {
    match_tokens_from(tokens, 0, path, 0)
}

fn match_tokens_from(tokens: &[Token], token_at: usize, path: &[u8], path_at: usize) -> bool {
    let Some(token) = tokens.get(token_at) else {
        return path_at == path.len();
    };
    match token {
        Token::Literal(want) => {
            path.get(path_at) == Some(want)
                && match_tokens_from(tokens, token_at + 1, path, path_at + 1)
        }
        Token::Any => {
            path.get(path_at).is_some_and(|byte| *byte != b'/')
                && match_tokens_from(tokens, token_at + 1, path, path_at + 1)
        }
        Token::Class { negated, ranges } => path.get(path_at).is_some_and(|byte| {
            *byte != b'/'
                && (ranges
                    .iter()
                    .any(|(first, last)| first <= byte && byte <= last)
                    != *negated)
                && match_tokens_from(tokens, token_at + 1, path, path_at + 1)
        }),
        Token::Star => {
            let end = path[path_at..]
                .iter()
                .position(|byte| *byte == b'/')
                .map_or(path.len(), |offset| path_at + offset);
            (path_at..=end)
                .rev()
                .any(|next| match_tokens_from(tokens, token_at + 1, path, next))
        }
        Token::LeadingDirectories => {
            match_tokens_from(tokens, token_at + 1, path, path_at)
                || path[path_at..]
                    .iter()
                    .enumerate()
                    .filter(|(_, byte)| **byte == b'/')
                    .any(|(offset, _)| {
                        match_tokens_from(tokens, token_at + 1, path, path_at + offset + 1)
                    })
        }
        Token::MiddleDirectories => {
            path.get(path_at) == Some(&b'/')
                && (match_tokens_from(tokens, token_at + 1, path, path_at + 1)
                    || path[path_at + 1..]
                        .iter()
                        .enumerate()
                        .filter(|(_, byte)| **byte == b'/')
                        .any(|(offset, _)| {
                            match_tokens_from(tokens, token_at + 1, path, path_at + offset + 2)
                        }))
        }
        Token::TrailingEverything => path.get(path_at) == Some(&b'/'),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, BTreeSet};
    use std::fmt::Debug;
    use std::fs;
    use std::io::Write;
    use std::path::{Path, PathBuf};
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};

    use super::{Gitignore, Match};

    struct Case {
        /// The sentence or example in gitignore(5) exercised by this case.
        rule: &'static str,
        patterns: &'static str,
        path: &'static str,
        is_dir: bool,
        want: Match,
    }

    #[test]
    fn pattern_format_from_gitignore_manual() {
        let cases = [
            Case {
                rule: "blank line matches no files",
                patterns: "\n",
                path: "anything",
                is_dir: false,
                want: Match::None,
            },
            Case {
                rule: "line starting with # is a comment",
                patterns: "# generated\n",
                path: "# generated",
                is_dir: false,
                want: Match::None,
            },
            Case {
                rule: r#"backslash before the first hash makes it literal"#,
                patterns: "\\#notes\n",
                path: "#notes",
                is_dir: false,
                want: Match::Ignore,
            },
            Case {
                rule: r#"backslash before the first ! makes it literal"#,
                patterns: "\\!important!.txt\n",
                path: "!important!.txt",
                is_dir: false,
                want: Match::Ignore,
            },
            Case {
                rule: "trailing spaces are ignored",
                patterns: "plain   \n",
                path: "plain",
                is_dir: false,
                want: Match::Ignore,
            },
            Case {
                rule: "a trailing space quoted with backslash is significant",
                patterns: "quoted\\ \n",
                path: "quoted ",
                is_dir: false,
                want: Match::Ignore,
            },
            Case {
                rule: "a trailing space quoted with backslash is significant",
                patterns: "quoted\\ \n",
                path: "quoted",
                is_dir: false,
                want: Match::None,
            },
            Case {
                rule: "optional ! prefix negates the pattern",
                patterns: "*.log\n!important.log\n",
                path: "important.log",
                is_dir: false,
                want: Match::Whitelist,
            },
            Case {
                rule: "within one precedence level the last matching pattern decides",
                patterns: "!important.log\n*.log\n",
                path: "important.log",
                is_dir: false,
                want: Match::Ignore,
            },
            Case {
                rule: "a leading slash anchors at the ignore file's directory",
                patterns: "/doc/frotz\n",
                path: "doc/frotz",
                is_dir: false,
                want: Match::Ignore,
            },
            Case {
                rule: "a leading slash anchors at the ignore file's directory",
                patterns: "/doc/frotz\n",
                path: "a/doc/frotz",
                is_dir: false,
                want: Match::None,
            },
            Case {
                rule: "a middle slash makes the pattern relative to the ignore file",
                patterns: "doc/frotz\n",
                path: "doc/frotz",
                is_dir: false,
                want: Match::Ignore,
            },
            Case {
                rule: "a middle slash makes the pattern relative to the ignore file",
                patterns: "doc/frotz\n",
                path: "a/doc/frotz",
                is_dir: false,
                want: Match::None,
            },
            Case {
                rule: "without a slash a pattern may match at any level below",
                patterns: "frotz\n",
                path: "a/doc/frotz",
                is_dir: false,
                want: Match::Ignore,
            },
            Case {
                rule: "a trailing slash restricts a pattern to directories",
                patterns: "build/\n",
                path: "a/build",
                is_dir: true,
                want: Match::Ignore,
            },
            Case {
                rule: "a trailing slash restricts a pattern to directories",
                patterns: "build/\n",
                path: "a/build",
                is_dir: false,
                want: Match::None,
            },
            Case {
                rule: "* matches anything except slash",
                patterns: "foo/*\n",
                path: "foo/test.json",
                is_dir: false,
                want: Match::Ignore,
            },
            Case {
                rule: "* does not match slash",
                patterns: "foo/*\n",
                path: "foo/bar/hello.c",
                is_dir: false,
                want: Match::None,
            },
            Case {
                rule: "? matches one character except slash",
                patterns: "file?.txt\n",
                path: "file1.txt",
                is_dir: false,
                want: Match::Ignore,
            },
            Case {
                rule: "? matches exactly one character",
                patterns: "file?.txt\n",
                path: "file12.txt",
                is_dir: false,
                want: Match::None,
            },
            Case {
                rule: "range notation matches one character in the range",
                patterns: "[a-c].txt\n",
                path: "b.txt",
                is_dir: false,
                want: Match::Ignore,
            },
            Case {
                rule: "fnmatch class negation accepts !",
                patterns: "[!a].txt\n",
                path: "b.txt",
                is_dir: false,
                want: Match::Ignore,
            },
            Case {
                rule: "fnmatch class negation accepts ^",
                patterns: "[^b].log\n",
                path: "a.log",
                is_dir: false,
                want: Match::Ignore,
            },
            Case {
                rule: "fnmatch permits ] first in a bracket expression",
                patterns: "[]a].dat\n",
                path: "].dat",
                is_dir: false,
                want: Match::Ignore,
            },
            Case {
                rule: "leading **/ matches in all directories, including zero",
                patterns: "**/foo\n",
                path: "foo",
                is_dir: false,
                want: Match::Ignore,
            },
            Case {
                rule: "leading **/ matches in all directories",
                patterns: "**/foo/bar\n",
                path: "a/foo/bar",
                is_dir: false,
                want: Match::Ignore,
            },
            Case {
                rule: "leading **/ does not absorb components after its suffix",
                patterns: "**/foo/bar\n",
                path: "a/foo/x/bar",
                is_dir: false,
                want: Match::None,
            },
            Case {
                rule: "trailing /** matches everything inside with infinite depth",
                patterns: "abc/**\n",
                path: "abc/x/y",
                is_dir: false,
                want: Match::Ignore,
            },
            Case {
                // check-ignore reports abc/** for an `abc/` command-line
                // query because it retains the trailing separator. Our API
                // receives `abc` plus is_dir and asks about that directory
                // itself; the manual says /** matches everything inside.
                rule: "trailing /** matches inside, not the directory itself",
                patterns: "abc/**\n",
                path: "abc",
                is_dir: true,
                want: Match::None,
            },
            Case {
                rule: "/**/ matches zero or more directories",
                patterns: "a/**/b\n",
                path: "a/b",
                is_dir: false,
                want: Match::Ignore,
            },
            Case {
                rule: "/**/ matches zero or more directories",
                patterns: "a/**/b\n",
                path: "a/x/y/b",
                is_dir: false,
                want: Match::Ignore,
            },
            Case {
                rule: "other consecutive asterisks act as ordinary asterisks",
                patterns: "a***b\n",
                path: "axxxb",
                is_dir: false,
                want: Match::Ignore,
            },
            Case {
                // Unlike shell glob expansion, gitignore uses fnmatch without
                // FNM_PERIOD. Confirmed with Git 2.54 check-ignore.
                rule: "* also matches a leading dot (Git 2.54 oracle)",
                patterns: "*\n",
                path: ".hidden",
                is_dir: false,
                want: Match::Ignore,
            },
            Case {
                rule: "backslash can escape any character",
                patterns: "\\*.txt\n",
                path: "*.txt",
                is_dir: false,
                want: Match::Ignore,
            },
        ];

        for case in cases {
            let (matcher, errors) = Gitignore::compile(case.patterns);
            assert!(errors.is_empty(), "{}: {errors:?}", case.rule);
            assert_eq!(
                matcher.matched(Path::new(case.path), case.is_dir),
                case.want,
                "{}: patterns {:?}, path {:?}, is_dir {}",
                case.rule,
                case.patterns,
                case.path,
                case.is_dir
            );
        }
    }

    #[test]
    fn invalid_trailing_backslash_is_reported_and_dropped() {
        let (matcher, errors) = Gitignore::compile("bad\\\n*.ok\n");
        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0].line, 1);
        assert_eq!(errors[0].pattern, "bad\\");
        assert_eq!(matcher.matched(Path::new("bad"), false), Match::None);
        assert_eq!(matcher.matched(Path::new("still.ok"), false), Match::Ignore);
    }

    #[test]
    fn agrees_with_git_check_ignore_on_seeded_patterns_and_paths() {
        let mut random = XorShift::new(0xd16_f3e2_7a91_4c05);
        let patterns = random_patterns(&mut random, 240);
        let paths = random_paths(&mut random, 3_000);
        let repository = TempRepository::new(&patterns, &paths);
        let queries: Vec<&str> = paths.keys().map(String::as_str).collect();
        let oracle = repository.check_ignore(&queries);
        let (matcher, _) = Gitignore::compile(&patterns);

        let by_path: BTreeMap<&str, Match> = queries.iter().copied().zip(oracle).collect();
        let mut compared = 0;
        for (path, is_dir) in &paths {
            // check-ignore folds an ignored parent into its answer. `matched`
            // is intentionally direct because the walker already queried each
            // parent, so only compare paths with no ignored parent.
            if parents(path).any(|parent| by_path.get(parent) == Some(&Match::Ignore)) {
                continue;
            }
            let want = by_path[path.as_str()];
            let got = matcher.matched(Path::new(path), *is_dir);
            assert_eq!(
                got, want,
                "Git differential mismatch for path {path:?}, is_dir={is_dir}\npatterns:\n{patterns}"
            );
            compared += 1;
        }
        assert!(compared >= 1_000, "only {compared} direct paths compared");
    }

    #[test]
    #[ignore = "manual release-mode benchmark over /home/dave/w"]
    fn benchmark_against_ignore_crate() {
        const ROOT: &str = "/home/dave/w";
        const TYPICAL: &str = "/home/dave/w/aic-edit/.gitignore";
        const LARGEST: &str = "/home/dave/w/intpack-bench/data/corpus-src/cpython/.gitignore";

        let paths = collect_real_paths(Path::new(ROOT), 500_000);
        assert!(!paths.is_empty(), "benchmark root {ROOT} has no paths");
        let rule_sets = [
            ("built-in defaults", crate::rules::DEFAULTS.to_owned()),
            ("typical: aic-edit", read_lossy(Path::new(TYPICAL))),
            ("largest: CPython", read_lossy(Path::new(LARGEST))),
        ];
        eprintln!("benchmark paths: {} under {ROOT}", paths.len());

        for (label, text) in &rule_sets {
            let (ours, our_errors) = Gitignore::compile(text);
            let (theirs, ignore_errors) = compile_ignore(text);
            eprintln!(
                "rules: {label}; bytes={}; lines={}; errors ours/ignore={}/{}",
                text.len(),
                text.lines().count(),
                our_errors.len(),
                ignore_errors
            );
            assert_matchers_agree(label, &ours, &theirs, &paths);

            let our_time = best_of_three(|| time_ours(&ours, &paths));
            let ignore_time = best_of_three(|| time_ignore(&theirs, &paths));
            eprintln!(
                "match: {label}; ours={:.2} ns/path; ignore={:.2} ns/path",
                nanos_per_path(our_time, paths.len()),
                nanos_per_path(ignore_time, paths.len())
            );
        }

        let largest = &rule_sets[2].1;
        let builds = 200;
        let start = Instant::now();
        for _ in 0..builds {
            std::hint::black_box(Gitignore::compile(largest));
        }
        let our_build = start.elapsed().as_nanos() as f64 / f64::from(builds);
        let start = Instant::now();
        for _ in 0..builds {
            std::hint::black_box(compile_ignore(largest));
        }
        let ignore_build = start.elapsed().as_nanos() as f64 / f64::from(builds);
        eprintln!(
            "build: largest: CPython; ours={our_build:.0} ns/build; ignore={ignore_build:.0} ns/build"
        );
    }

    fn compile_ignore(text: &str) -> (ignore::gitignore::Gitignore, usize) {
        let mut builder = ignore::gitignore::GitignoreBuilder::new(".");
        let mut errors = 0;
        for line in text.lines() {
            if builder.add_line(None, line).is_err() {
                errors += 1;
            }
        }
        (must(builder.build()), errors)
    }

    fn assert_matchers_agree(
        label: &str,
        ours: &Gitignore,
        theirs: &ignore::gitignore::Gitignore,
        paths: &[(PathBuf, bool)],
    ) {
        for (path, is_dir) in paths {
            let our_match = ours.matched(path, *is_dir);
            let ignore_match = match theirs.matched(path, *is_dir) {
                ignore::Match::None => Match::None,
                ignore::Match::Ignore(_) => Match::Ignore,
                ignore::Match::Whitelist(_) => Match::Whitelist,
            };
            assert_eq!(
                our_match, ignore_match,
                "benchmark differential mismatch in {label} for {path:?}, is_dir={is_dir}"
            );
        }
    }

    fn time_ours(matcher: &Gitignore, paths: &[(PathBuf, bool)]) -> Duration {
        let start = Instant::now();
        let mut matches = 0;
        for (path, is_dir) in paths {
            matches += usize::from(matcher.matched(path, *is_dir) != Match::None);
        }
        std::hint::black_box(matches);
        start.elapsed()
    }

    fn time_ignore(matcher: &ignore::gitignore::Gitignore, paths: &[(PathBuf, bool)]) -> Duration {
        let start = Instant::now();
        let mut matches = 0;
        for (path, is_dir) in paths {
            matches += usize::from(!matcher.matched(path, *is_dir).is_none());
        }
        std::hint::black_box(matches);
        start.elapsed()
    }

    fn best_of_three(mut run: impl FnMut() -> Duration) -> Duration {
        (0..3).map(|_| run()).min().unwrap_or(Duration::MAX)
    }

    fn nanos_per_path(duration: Duration, paths: usize) -> f64 {
        duration.as_nanos() as f64 / paths as f64
    }

    fn collect_real_paths(root: &Path, limit: usize) -> Vec<(PathBuf, bool)> {
        let mut paths = Vec::with_capacity(limit);
        let mut pending = vec![root.to_owned()];
        while let Some(directory) = pending.pop() {
            let entries = match fs::read_dir(&directory) {
                Ok(entries) => entries,
                Err(_) => continue,
            };
            for entry in entries.flatten() {
                let Ok(file_type) = entry.file_type() else {
                    continue;
                };
                let path = entry.path();
                let Ok(relative) = path.strip_prefix(root) else {
                    continue;
                };
                paths.push((relative.to_owned(), file_type.is_dir()));
                if paths.len() == limit {
                    return paths;
                }
                if file_type.is_dir() {
                    pending.push(path);
                }
            }
        }
        paths
    }

    fn read_lossy(path: &Path) -> String {
        String::from_utf8_lossy(&must(fs::read(path))).into_owned()
    }

    struct TempRepository {
        path: std::path::PathBuf,
    }

    impl TempRepository {
        fn new(patterns: &str, paths: &BTreeMap<String, bool>) -> Self {
            let path = std::env::temp_dir().join(format!(
                "ferret-policy-gitignore-{}-d16",
                std::process::id()
            ));
            must(fs::create_dir(&path));
            let init = must(
                Command::new("git")
                    .arg("init")
                    .arg("-q")
                    .arg(&path)
                    .output(),
            );
            assert!(
                init.status.success(),
                "git init failed: {}",
                String::from_utf8_lossy(&init.stderr)
            );
            must(fs::write(path.join(".gitignore"), patterns));

            for (relative, is_dir) in paths {
                if *is_dir {
                    must(fs::create_dir_all(path.join(relative)));
                }
            }
            for (relative, is_dir) in paths {
                if !is_dir {
                    must(fs::write(path.join(relative), []));
                }
            }
            Self { path }
        }

        fn check_ignore(&self, paths: &[&str]) -> Vec<Match> {
            let mut child = must(
                Command::new("git")
                    .args([
                        "-c",
                        "core.excludesFile=/dev/null",
                        "check-ignore",
                        "--no-index",
                        "--verbose",
                        "--non-matching",
                        "-z",
                        "--stdin",
                    ])
                    .current_dir(&self.path)
                    .stdin(Stdio::piped())
                    .stdout(Stdio::piped())
                    .stderr(Stdio::piped())
                    .spawn(),
            );
            {
                let mut stdin = child
                    .stdin
                    .take()
                    .unwrap_or_else(|| panic!("piped git stdin was unavailable"));
                for path in paths {
                    must(stdin.write_all(path.as_bytes()));
                    must(stdin.write_all(&[0]));
                }
            }
            let output = must(child.wait_with_output());
            assert!(
                output.status.success() || output.status.code() == Some(1),
                "git check-ignore failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );

            let mut fields: Vec<&[u8]> = output.stdout.split(|byte| *byte == 0).collect();
            if fields.last() == Some(&&[][..]) {
                fields.pop();
            }
            assert_eq!(
                fields.len(),
                paths.len() * 4,
                "unexpected check-ignore output: {:?}",
                String::from_utf8_lossy(&output.stdout)
            );
            fields
                .chunks_exact(4)
                .map(|record| {
                    if record[0].is_empty() {
                        Match::None
                    } else if record[2].starts_with(b"!") {
                        Match::Whitelist
                    } else {
                        Match::Ignore
                    }
                })
                .collect()
        }
    }

    impl Drop for TempRepository {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.path);
        }
    }

    fn must<T, E: Debug>(result: Result<T, E>) -> T {
        result.unwrap_or_else(|error| panic!("test setup failed: {error:?}"))
    }

    fn parents(path: &str) -> impl Iterator<Item = &str> {
        path.match_indices('/').map(|(slash, _)| &path[..slash])
    }

    fn random_patterns(random: &mut XorShift, count: usize) -> String {
        const UNITS: &[&[u8]] = &[
            b"a", b"b", b"/", b"*", b"**", b"?", b"[", b"]", b"!", b".", b"\\",
        ];
        let mut patterns = String::new();
        for _ in 0..count {
            for _ in 0..random.range(1, 13) {
                let unit = UNITS[random.range(0, UNITS.len())];
                patterns.push_str(str_from_ascii(unit));
            }
            patterns.push('\n');
        }
        patterns
    }

    fn random_paths(random: &mut XorShift, count: usize) -> BTreeMap<String, bool> {
        const NAME_BYTES: &[u8] = b"ab*?[]!.\\";
        let mut generated = BTreeSet::new();
        while generated.len() < count {
            let mut path = String::new();
            for component in 0..random.range(1, 5) {
                if component != 0 {
                    path.push('/');
                }
                let start = path.len();
                for _ in 0..random.range(1, 7) {
                    path.push(char::from(NAME_BYTES[random.range(0, NAME_BYTES.len())]));
                }
                if matches!(&path[start..], "." | "..") {
                    path.insert(start, 'a');
                }
            }
            generated.insert(path);
        }

        let mut paths = BTreeMap::new();
        for path in generated {
            for parent in parents(&path) {
                paths.insert(parent.to_owned(), true);
            }
            paths.entry(path).or_insert_with(|| random.range(0, 4) == 0);
        }
        paths
    }

    fn str_from_ascii(bytes: &[u8]) -> &str {
        std::str::from_utf8(bytes).unwrap_or_else(|_| panic!("generator constants must be ASCII"))
    }

    struct XorShift(u64);

    impl XorShift {
        fn new(seed: u64) -> Self {
            Self(seed)
        }

        fn next(&mut self) -> u64 {
            let mut value = self.0;
            value ^= value << 13;
            value ^= value >> 7;
            value ^= value << 17;
            self.0 = value;
            value
        }

        fn range(&mut self, start: usize, end: usize) -> usize {
            start + self.next() as usize % (end - start)
        }
    }
}
