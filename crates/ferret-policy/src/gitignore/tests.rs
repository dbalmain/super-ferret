//! Git 2.54 oracle facts found during the round-one review.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Debug;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use super::{Gitignore, Match, Pattern};

struct Case {
    label: &'static str,
    patterns: &'static str,
    path: &'static str,
    want: Match,
}

#[test]
fn pattern_format_from_gitignore_manual() {
    let cases = [
        ("blank", "\n", "anything", false, Match::None),
        (
            "comment",
            "# generated\n",
            "# generated",
            false,
            Match::None,
        ),
        ("escaped hash", "\\#notes\n", "#notes", false, Match::Ignore),
        ("escaped bang", "\\!keep\n", "!keep", false, Match::Ignore),
        (
            "trailing spaces",
            "plain   \n",
            "plain",
            false,
            Match::Ignore,
        ),
        (
            "quoted space",
            "quoted\\ \n",
            "quoted ",
            false,
            Match::Ignore,
        ),
        (
            "quoted space differs",
            "quoted\\ \n",
            "quoted",
            false,
            Match::None,
        ),
        (
            "negation",
            "*.log\n!important.log\n",
            "important.log",
            false,
            Match::Whitelist,
        ),
        (
            "last match",
            "!important.log\n*.log\n",
            "important.log",
            false,
            Match::Ignore,
        ),
        (
            "leading slash",
            "/doc/frotz\n",
            "doc/frotz",
            false,
            Match::Ignore,
        ),
        (
            "leading slash anchors",
            "/doc/frotz\n",
            "a/doc/frotz",
            false,
            Match::None,
        ),
        (
            "middle slash",
            "doc/frotz\n",
            "doc/frotz",
            false,
            Match::Ignore,
        ),
        (
            "middle slash anchors",
            "doc/frotz\n",
            "a/doc/frotz",
            false,
            Match::None,
        ),
        (
            "basename any depth",
            "frotz\n",
            "a/doc/frotz",
            false,
            Match::Ignore,
        ),
        (
            "directory suffix",
            "build/\n",
            "a/build",
            true,
            Match::Ignore,
        ),
        (
            "directory suffix rejects file",
            "build/\n",
            "a/build",
            false,
            Match::None,
        ),
        ("star", "foo/*\n", "foo/test.json", false, Match::Ignore),
        (
            "star stops at slash",
            "foo/*\n",
            "foo/bar/hello.c",
            false,
            Match::None,
        ),
        ("question", "file?.txt\n", "file1.txt", false, Match::Ignore),
        (
            "question is one byte",
            "file?.txt\n",
            "file12.txt",
            false,
            Match::None,
        ),
        ("range", "[a-c].txt\n", "b.txt", false, Match::Ignore),
        (
            "negated class !",
            "[!a].txt\n",
            "b.txt",
            false,
            Match::Ignore,
        ),
        (
            "negated class ^",
            "[^b].log\n",
            "a.log",
            false,
            Match::Ignore,
        ),
        (
            "] first in class",
            "[]a].dat\n",
            "].dat",
            false,
            Match::Ignore,
        ),
        (
            "leading globstar zero",
            "**/foo\n",
            "foo",
            false,
            Match::Ignore,
        ),
        (
            "leading globstar many",
            "**/foo/bar\n",
            "a/foo/bar",
            false,
            Match::Ignore,
        ),
        (
            "globstar suffix fixed",
            "**/foo/bar\n",
            "a/foo/x/bar",
            false,
            Match::None,
        ),
        (
            "trailing globstar",
            "abc/**\n",
            "abc/x/y",
            false,
            Match::Ignore,
        ),
        (
            "trailing globstar is inside",
            "abc/**\n",
            "abc",
            true,
            Match::None,
        ),
        (
            "middle globstar zero",
            "a/**/b\n",
            "a/b",
            false,
            Match::Ignore,
        ),
        (
            "middle globstar many",
            "a/**/b\n",
            "a/x/y/b",
            false,
            Match::Ignore,
        ),
        (
            "ordinary star run",
            "a***b\n",
            "axxxb",
            false,
            Match::Ignore,
        ),
        (
            "star matches hidden",
            "*\n",
            ".hidden",
            false,
            Match::Ignore,
        ),
        ("escaped star", "\\*.txt\n", "*.txt", false, Match::Ignore),
    ];

    for (rule, patterns, path, is_dir, want) in cases {
        let (matcher, errors) = Gitignore::compile(patterns);
        assert!(errors.is_empty(), "{rule}: {errors:?}");
        assert_eq!(matcher.matched(Path::new(path), is_dir), want, "{rule}");
    }
}

#[test]
fn reviewer_oracle_corpus() {
    let cases = [
        Case {
            label: "adjacent globstars",
            patterns: "a/**/**/b\n",
            path: "a/b",
            want: Match::Ignore,
        },
        Case {
            label: "leading triple star is a globstar component",
            patterns: "***/b\n",
            path: "b",
            want: Match::Ignore,
        },
        Case {
            label: "middle triple star is a globstar component",
            patterns: "a/***/b\n",
            path: "a/b",
            want: Match::Ignore,
        },
        Case {
            label: "trailing triple star is a globstar component",
            patterns: "!a/***\n",
            path: "a/x/y",
            want: Match::Whitelist,
        },
        Case {
            label: "escaped separator still separates components",
            patterns: "a\\/**/b\n",
            path: "a/b",
            want: Match::Ignore,
        },
        Case {
            label: "escaped hyphen is not a range",
            patterns: "[a\\-c]\n",
            path: "b",
            want: Match::None,
        },
        Case {
            label: "escaped hyphen is a class member",
            patterns: "[a\\-c]\n",
            path: "-",
            want: Match::Ignore,
        },
        Case {
            label: "reversed range retains its first endpoint",
            patterns: "[z-a]\n",
            path: "z",
            want: Match::Ignore,
        },
        Case {
            label: "unclosed class never matches",
            patterns: "[abc\n",
            path: "[abc",
            want: Match::None,
        },
        Case {
            label: "empty class never matches",
            patterns: "[]\n",
            path: "[]",
            want: Match::None,
        },
        Case {
            label: "ordinary CRLF drops the CR",
            patterns: "f\r\n",
            path: "f",
            want: Match::Ignore,
        },
        Case {
            label: "literal tab is significant",
            patterns: "f\t\n",
            path: "f\t",
            want: Match::Ignore,
        },
        Case {
            label: "bang alone matches nothing",
            patterns: "!\n",
            path: "!",
            want: Match::None,
        },
        Case {
            label: "slash alone matches nothing",
            patterns: "/\n",
            path: "x",
            want: Match::None,
        },
        Case {
            label: "slash inside a class is still a separator",
            patterns: "a[/]b\n",
            path: "a/b",
            want: Match::None,
        },
        Case {
            label: "question mark consumes one byte, not one Unicode scalar",
            patterns: "f?\n",
            path: "fé",
            want: Match::None,
        },
        Case {
            label: "double star beside a literal is an ordinary star",
            patterns: "a**/b\n",
            path: "ax/y/b",
            want: Match::None,
        },
        Case {
            label: "double star beside a suffix is an ordinary star",
            patterns: "a/**b\n",
            path: "a/x/yb",
            want: Match::None,
        },
        Case {
            label: "POSIX digit class",
            patterns: "[[:digit:]]\n",
            path: "5",
            want: Match::Ignore,
        },
        Case {
            label: "unknown POSIX class never matches",
            patterns: "[[:bogus:]]\n",
            path: "5",
            want: Match::None,
        },
    ];

    for case in cases {
        let (matcher, _) = Gitignore::compile(case.patterns);
        assert_eq!(
            matcher.matched(Path::new(case.path), false),
            case.want,
            "{}: {:?} on {:?}",
            case.label,
            case.patterns,
            case.path
        );
    }
}

#[test]
fn trailing_escape_is_reported_and_never_matches() {
    let (matcher, errors) = Gitignore::compile("bad\\\n");
    assert_eq!(errors.len(), 1);
    assert_eq!(matcher.matched(Path::new("bad\\"), false), Match::None);
}

#[test]
fn posix_class_matrix() {
    let cases = [
        ("alnum", "A"),
        ("alpha", "z"),
        ("blank", "\t"),
        ("cntrl", "\u{7f}"),
        ("digit", "5"),
        ("graph", "!"),
        ("lower", "a"),
        ("print", " "),
        ("punct", "!"),
        ("space", "\t"),
        ("upper", "A"),
        ("xdigit", "F"),
    ];
    for (name, path) in cases {
        let (matcher, errors) = Gitignore::compile(&format!("[[:{name}:]]\n"));
        assert!(errors.is_empty(), "{name}: {errors:?}");
        assert_eq!(
            matcher.matched(Path::new(path), false),
            Match::Ignore,
            "{name}"
        );
    }
}

#[cfg(unix)]
#[test]
fn question_mark_matches_one_non_utf8_byte() {
    use std::ffi::OsStr;
    use std::os::unix::ffi::OsStrExt;

    let (matcher, errors) = Gitignore::compile("f?\n");
    assert!(errors.is_empty());
    assert_eq!(
        matcher.matched(Path::new(OsStr::from_bytes(b"f\xff")), false),
        Match::Ignore
    );
}

#[test]
fn pathological_stars_have_bounded_work() {
    let (matcher, errors) = Gitignore::compile("*a*a*a*a*a*a*a*a*a*b\n");
    assert!(errors.is_empty());
    assert_eq!(
        matcher.matched(Path::new("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"), false),
        Match::None
    );
    let pattern = match Pattern::compile(0, "*a*a*a*a*a*a*a*a*a*b") {
        Ok(Some(pattern)) => pattern,
        other => panic!("pathological pattern did not compile: {other:?}"),
    };
    let (matched, steps) =
        pattern.basename_match_steps(b"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
    assert!(!matched);
    assert!(steps <= 1_000, "iterative matcher used {steps} steps");
}

#[test]
fn byte_normalization_oracle_cases_are_explicitly_deferred() {
    // Round 2 excludes ignore-file byte normalization. Keep the exact Git
    // facts visible so a later pass cannot accidentally omit them.
    let deferred = [
        (
            "UTF-8 BOM is stripped",
            "\u{feff}foo\n",
            "foo",
            Match::Ignore,
        ),
        ("final CR is stripped", "f\r", "f", Match::Ignore),
        ("NUL truncates a line", "foo\0bar\n", "foo", Match::Ignore),
    ];
    assert_eq!(deferred.len(), 3);
    assert!(
        deferred
            .iter()
            .all(|(_, _, _, want)| *want == Match::Ignore)
    );
}

#[derive(Default)]
struct Coverage {
    none: usize,
    ignore: usize,
    whitelist: usize,
}

impl Coverage {
    fn record(&mut self, result: Match) {
        match result {
            Match::None => self.none += 1,
            Match::Ignore => self.ignore += 1,
            Match::Whitelist => self.whitelist += 1,
        }
    }
}

#[test]
fn agrees_with_git_on_seeded_random_and_structured_cases() {
    let mut random = XorShift::new(0x000d_1602_f3e2_7a91);
    let mut coverage = Coverage::default();
    let mut compared = 0;

    for batch in 0..6 {
        let patterns = random_patterns(&mut random, 40);
        let paths = random_paths(&mut random, 500);
        let (oracle, ignored_parents) = git_results(&patterns, &paths, batch);
        let (matcher, _) = Gitignore::compile(&patterns);

        for ((path, is_dir), want) in paths.iter().zip(oracle) {
            if parents(path).any(|parent| ignored_parents.contains(parent)) {
                continue;
            }
            let got = matcher.matched(Path::new(path), *is_dir);
            assert_eq!(
                got, want,
                "Git differential mismatch for {path:?}, is_dir={is_dir}\npatterns:\n{patterns}"
            );
            coverage.record(want);
            compared += 1;
        }
    }

    let (patterns, paths) = structured_cases(&mut random, 64);
    let (oracle, _) = git_results(&patterns, &paths, 99);
    let (matcher, errors) = Gitignore::compile(&patterns);
    assert!(errors.is_empty(), "structured generator errors: {errors:?}");
    for ((path, is_dir), want) in paths.iter().zip(oracle) {
        let got = matcher.matched(Path::new(path), *is_dir);
        assert_eq!(got, want, "structured mismatch for {path:?}\n{patterns}");
        coverage.record(want);
        compared += 1;
    }

    assert!(compared >= 2_000, "only {compared} cases compared");
    assert!(coverage.none >= 100, "None coverage: {}", coverage.none);
    assert!(
        coverage.ignore >= 50,
        "Ignore coverage: {}",
        coverage.ignore
    );
    assert!(
        coverage.whitelist >= 10,
        "Whitelist coverage: {}",
        coverage.whitelist
    );
    eprintln!(
        "Git differential coverage: None={}, Ignore={}, Whitelist={}",
        coverage.none, coverage.ignore, coverage.whitelist
    );
}

fn git_results(
    patterns: &str,
    paths: &BTreeMap<String, bool>,
    batch: usize,
) -> (Vec<Match>, BTreeSet<String>) {
    let repository = TempRepository::new(patterns, paths, batch);
    let queries: Vec<&str> = paths.keys().map(String::as_str).collect();
    let results = repository.check_ignore(&queries);
    let ignored = queries
        .iter()
        .zip(&results)
        .filter(|(_, result)| **result == Match::Ignore)
        .map(|(path, _)| (*path).to_owned())
        .collect();
    (results, ignored)
}

fn structured_cases(random: &mut XorShift, count: usize) -> (String, BTreeMap<String, bool>) {
    let mut patterns = String::new();
    let mut paths = BTreeMap::new();
    for case in 0..count {
        let key = format!(
            "Case-{case}:{}",
            char::from(b'A' + random.range(0, 26) as u8)
        );
        let (pattern, witness, near) = match case % 5 {
            0 => (
                format!("/{key}/**/tail.A"),
                format!("{key}/x/y/tail.A"),
                format!("{key}/x/y/tail.B"),
            ),
            1 => (
                format!("/{key}/***/hash#A"),
                format!("{key}/hash#A"),
                format!("{key}/hash#B"),
            ),
            2 => (
                format!("/{key}/[a-c]-file"),
                format!("{key}/b-file"),
                format!("{key}/Z-file"),
            ),
            3 => (
                format!("/{key}/[[:digit:]] space"),
                format!("{key}/5 space"),
                format!("{key}/A space"),
            ),
            _ => (
                format!("!/{key}/**/keep:A"),
                format!("{key}/x/keep:A"),
                format!("{key}/x/drop:A"),
            ),
        };
        patterns.push_str(&pattern);
        patterns.push('\n');
        insert_path(&mut paths, witness, false);
        insert_path(&mut paths, near, false);
    }
    (patterns, paths)
}

fn random_patterns(random: &mut XorShift, count: usize) -> String {
    const UNITS: &[&[u8]] = &[
        b"a", b"b", b"A", b"Z", b"/", b"*", b"**", b"?", b"[", b"]", b"!", b".", b"\\", b"-", b":",
        b" ", b"#",
    ];
    let mut patterns = String::new();
    for _ in 0..count {
        for _ in 0..random.range(1, 13) {
            let unit = UNITS[random.range(0, UNITS.len())];
            patterns.push_str(ascii(unit));
        }
        patterns.push('\n');
    }
    patterns
}

fn random_paths(random: &mut XorShift, count: usize) -> BTreeMap<String, bool> {
    const NAME_BYTES: &[u8] = b"abAZ*?[]!.\\-:# ";
    let mut generated = BTreeSet::new();
    while generated.len() < count {
        let mut path = String::new();
        for component in 0..random.range(1, 5) {
            if component != 0 {
                path.push('/');
            }
            let start = path.len();
            // Avoid Git's `:(magic)` pathspec prefix while still exercising
            // every generated byte within components.
            path.push('a');
            for _ in 0..random.range(0, 6) {
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
        insert_path(&mut paths, path, random.range(0, 4) == 0);
    }
    paths
}

fn insert_path(paths: &mut BTreeMap<String, bool>, path: String, is_dir: bool) {
    for parent in parents(&path) {
        paths.insert(parent.to_owned(), true);
    }
    paths.entry(path).or_insert(is_dir);
}

fn parents(path: &str) -> impl Iterator<Item = &str> {
    path.match_indices('/').map(|(slash, _)| &path[..slash])
}

fn ascii(bytes: &[u8]) -> &str {
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

struct TempRepository {
    path: PathBuf,
}

impl TempRepository {
    fn new(patterns: &str, paths: &BTreeMap<String, bool>, batch: usize) -> Self {
        let path = std::env::temp_dir().join(format!(
            "ferret-policy-gitignore-{}-{batch}",
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
        assert_eq!(fields.len(), paths.len() * 4);
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
