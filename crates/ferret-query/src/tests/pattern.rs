//! Glob lowering and literal extraction. Each case names the input where a
//! plausible wrong extractor gives a different answer.

use ferret_verify::{Matcher, Regex};

use crate::pattern::{glob_literal, glob_regex, regex_literal};

fn literal(pattern: &str, fold: bool) -> Option<String> {
    regex_literal(pattern, fold).map(|l| String::from_utf8(l).unwrap())
}

#[test]
fn a_regex_literal_is_only_what_every_match_contains() {
    let cases: &[(&str, bool, Option<&str>)] = &[
        (r"^main\.rs$", false, Some("main.rs")),
        // Optional characters are not required: `abc` would miss "bc".
        ("a?bc", false, Some("bc")),
        ("abc*d", false, Some("ab")),
        // `+` keeps its character but ends the run: "abbc" lacks "abc".
        ("ab+c", false, Some("ab")),
        // A counted repetition's body is not text: not "2".
        ("a{2}", false, None),
        ("xy{2,3}zzz", false, Some("zzz")),
        // An alternation guarantees nothing, however long its arms.
        ("foobar|x", false, None),
        // Flags can change what a literal means (`(?x)` drops spaces).
        ("(?x)a b c", false, None),
        // Scanning stops at a group or class; nothing inside one counts.
        ("ab(cdef)ghijk", false, Some("ab")),
        ("[]a]xyz", false, None),
        // `\x41` is "A", not "41"; scanning stops there.
        (r"ab\x41cdef", false, Some("ab")),
        (r"\d+\.rs", false, Some(".rs")),
        (r"\<word", false, Some("word")),
        // Folded, `k` and `s` also match non-ASCII letters (KELVIN SIGN,
        // LONG S), which the ASCII-folding scanner cannot find.
        ("kbar", true, Some("bar")),
        ("tests", true, Some("te")),
        ("tests", false, Some("tests")),
        ("caf\u{e9}s", true, Some("caf")),
        (".*", false, None),
    ];
    for &(pattern, fold, expect) in cases {
        assert_eq!(
            literal(pattern, fold).as_deref(),
            expect,
            "{pattern:?} fold={fold}"
        );
    }
}

#[test]
fn a_glob_literal_comes_from_its_last_component() {
    let cases: &[(&[u8], Option<&[u8]>)] = &[
        (b"*.rs", Some(b".rs")),
        (b"src/**/*_test.go", Some(b"_test.go")),
        (b"**", None),
        (b"src/**", None),
        (b"a[bc]def", Some(b"def")),
        (b"x?yy", Some(b"yy")),
        // `longname/` is a directory; the name is `*.c`.
        (b"longname/*.c", Some(b".c")),
    ];
    for &(glob, expect) in cases {
        assert_eq!(
            glob_literal(glob).as_deref(),
            expect,
            "{}",
            glob.escape_ascii()
        );
    }
}

#[test]
fn globs_lower_to_regexes_with_path_rules() {
    let matches = |glob: &[u8], subject: &[u8]| {
        Regex::new(&glob_regex(glob), false)
            .unwrap()
            .is_match(subject)
    };
    let cases: &[(&[u8], &[u8], bool)] = &[
        (b"*.rs", b"main.rs", true),
        (b"*.rs", b"main.rsx", false),
        (b"*.rs", b"main.RS", false),
        (b"?.rs", b"a.rs", true),
        (b"?.rs", b"ab.rs", false),
        (b"[ab].c", b"b.c", true),
        (b"[!ab].c", b"b.c", false),
        (b"[!ab].c", b"z.c", true),
        (b"[a-c]x", b"bx", true),
        (b"a.c", b"abc", false),
        // Regex syntax in a glob is literal.
        (b"a+(b)", b"a+(b)", true),
        (b"a+(b)", b"aab", false),
        // A path glob matches trailing components, at a boundary.
        (b"src/**/*.rs", b"/w/src/a/b.rs", true),
        (b"src/**/*.rs", b"/w/src/b.rs", true),
        (b"src/**/*.rs", b"/w/mysrc/b.rs", false),
        (b"src/*.rs", b"/w/src/a/b.rs", false),
        (b"/w/*.rs", b"/w/a.rs", true),
        (b"/w/*.rs", b"/x/w/a.rs", false),
        (b"src/**", b"/w/src/a/b", true),
        // Bytes that are not UTF-8 are matched as bytes.
        (b"\xff*", b"\xffx", true),
    ];
    for &(glob, subject, expect) in cases {
        assert_eq!(
            matches(glob, subject),
            expect,
            "{} on {}",
            glob.escape_ascii(),
            subject.escape_ascii()
        );
    }
}
