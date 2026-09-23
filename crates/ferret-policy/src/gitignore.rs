//! Gitignore pattern parsing and matching.
//!
//! The conformance suite is derived from `gitignore(5)`. Git 2.54's
//! `check-ignore` is used only as a black-box oracle where the manual is
//! ambiguous; neither Git nor third-party matcher source is used.

#[cfg(test)]
mod tests {
    use std::path::Path;

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
                // git check-ignore 2.54 reports abc/** for `abc/`: `**` can
                // match the empty suffix after the directory separator.
                rule: "trailing /** matches an empty suffix (Git 2.54 oracle)",
                patterns: "abc/**\n",
                path: "abc",
                is_dir: true,
                want: Match::Ignore,
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
}
