//! Git 2.54 oracle facts found during the round-one review.

use std::path::Path;

use super::{Gitignore, Match};

struct Case {
    label: &'static str,
    patterns: &'static str,
    path: &'static str,
    want: Match,
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
