//! Pure translator regressions include the default dialect's easy-to-miss
//! unescaped ? and escaped alternation, and basic's literal +.

use super::*;

#[test]
fn emacs_optional_character_and_group_alternation_are_operators() {
    let regex = FindRegex::new(br".*\.pyc?", Dialect::Emacs, false).unwrap();
    assert_eq!(regex.try_is_match(b"./m.py"), Ok(true));
    assert_eq!(regex.try_is_match(b"./m.pyc"), Ok(true));
    assert_eq!(regex.try_is_match(b"./m.pycc"), Ok(false));
    assert_eq!(regex.try_is_match(b"./m.py/other"), Ok(false));
    let regex = FindRegex::new(br".*\.\(pyc\|rej\)", Dialect::Emacs, false).unwrap();
    assert_eq!(regex.try_is_match(b"./a.pyc"), Ok(true));
    assert_eq!(regex.try_is_match(b"./b.rej"), Ok(true));
    assert_eq!(regex.try_is_match(b"./a.py"), Ok(false));
}

#[test]
fn basic_plus_is_literal_and_intervals_are_escaped() {
    let regex = FindRegex::new(br"./a+", Dialect::Basic, false).unwrap();
    assert_eq!(regex.try_is_match(b"./a+"), Ok(true));
    assert_eq!(regex.try_is_match(b"./aa"), Ok(false));
    for dialect in [Dialect::Emacs, Dialect::Basic, Dialect::MinimalBasic] {
        let regex = FindRegex::new(br"./a\{2,3\}", dialect, false).unwrap();
        assert_eq!(regex.try_is_match(b"./aa"), Ok(true));
        assert_eq!(regex.try_is_match(b"./aaa"), Ok(true));
        assert_eq!(regex.try_is_match(b"./a"), Ok(false));
    }
    let regex = FindRegex::new(br"./a{2}", Dialect::Awk, false).unwrap();
    assert_eq!(regex.try_is_match(b"./a{2}"), Ok(true));
    assert_eq!(regex.try_is_match(b"./aa"), Ok(false));
}

#[test]
fn dots_match_slashes_newlines_and_nonutf8_bytes_in_c_locale() {
    let regex = FindRegex::new(b"...", Dialect::Emacs, false).unwrap();
    assert_eq!(regex.try_is_match(b"/\xffa"), Ok(true));
    assert_eq!(regex.try_is_match(b"/\xff\n"), Ok(false));
    let newline = FindRegex::new(b"...", Dialect::Extended, false).unwrap();
    assert_eq!(newline.try_is_match(b"/\xff\n"), Ok(true));
    assert_eq!(regex.try_is_match(b"abcd"), Ok(false));
    let regex = FindRegex::new(br".*/[[:alpha:]]+", Dialect::Extended, true).unwrap();
    assert_eq!(regex.try_is_match(b"./Ab"), Ok(true));
    assert_eq!(regex.try_is_match(b"./\xff"), Ok(false));
}

#[test]
fn basic_midpattern_anchors_are_literals_but_extended_anchors_assert() {
    for dialect in [Dialect::Basic, Dialect::Emacs] {
        let regex = FindRegex::new(b"./a^b$c", dialect, false).unwrap();
        assert_eq!(regex.try_is_match(b"./a^b$c"), Ok(true));
    }
    let regex = FindRegex::new(b"./a^b$c", Dialect::Extended, false).unwrap();
    assert_eq!(regex.try_is_match(b"./a^b$c"), Ok(false));
}

#[test]
fn backreferences_backtrack_into_groups_and_fold_only_ascii() {
    for (dialect, pattern) in [
        (Dialect::Emacs, br"\(a*\)\1".as_slice()),
        (Dialect::Basic, br"\(a*\)\1"),
        (Dialect::Extended, br"(a*)\1"),
        (Dialect::PosixAwk, br"(a*)\1"),
        (Dialect::GnuAwk, br"(a*)\1"),
    ] {
        let regex = FindRegex::new(pattern, dialect, false).unwrap();
        assert_eq!(regex.try_is_match(b"aaaa"), Ok(true));
        assert_eq!(regex.try_is_match(b"aaa"), Ok(false));
        assert_eq!(regex.try_is_match(b""), Ok(true));
    }
    let regex = FindRegex::new(br"([^/]+)/\1", Dialect::Extended, true).unwrap();
    assert_eq!(regex.try_is_match(b"Ab/aB"), Ok(true));
    assert_eq!(regex.try_is_match(b"\xff/\xff"), Ok(true));
    assert_eq!(regex.try_is_match(b"\xc0/\xe0"), Ok(false));
    assert_eq!(regex.try_is_match(b"Ab/ac"), Ok(false));
    let regex = FindRegex::new(br"(a)\1", Dialect::Awk, false).unwrap();
    assert_eq!(regex.try_is_match(b"a1"), Ok(true));
    assert_eq!(regex.try_is_match(b"aa"), Ok(false));
}

#[test]
fn bracket_backslashes_are_literal_in_emacs_and_basic() {
    let regex = FindRegex::new(br"[a\b]", Dialect::Emacs, false).unwrap();
    for byte in [b'a', b'\\', b'b'] {
        assert_eq!(regex.try_is_match(&[byte]), Ok(true));
    }
    assert_eq!(regex.try_is_match(b"c"), Ok(false));
}

#[test]
fn c_locale_collating_and_equivalence_symbols_are_single_bytes() {
    for pattern in [br"[[.a.]]".as_slice(), br"[[=a=]]".as_slice()] {
        let regex = FindRegex::new(pattern, Dialect::Emacs, false).unwrap();
        assert_eq!(regex.try_is_match(b"a"), Ok(true));
        assert_eq!(regex.try_is_match(b"b"), Ok(false));
    }
    let regex = FindRegex::new(br"[[.a.]-[.c.]]", Dialect::Extended, false).unwrap();
    assert_eq!(regex.try_is_match(b"b"), Ok(true));
    assert_eq!(regex.try_is_match(b"d"), Ok(false));
}

#[test]
fn awk_escapes_letters_literally_and_grep_pattern_newline_is_alternation() {
    for dialect in [Dialect::Awk, Dialect::PosixAwk, Dialect::GnuAwk] {
        let regex = FindRegex::new(br"./a\n", dialect, false).unwrap();
        assert_eq!(regex.try_is_match(b"./an"), Ok(true));
        assert_eq!(regex.try_is_match(b"./a\n"), Ok(false));
    }
    for dialect in [Dialect::Grep, Dialect::Egrep] {
        let regex = FindRegex::new(b".*a\n.*b", dialect, false).unwrap();
        assert_eq!(regex.try_is_match(b"./a"), Ok(true));
        assert_eq!(regex.try_is_match(b"./b"), Ok(true));
    }
}

#[test]
fn missing_repetition_operand_is_literal_ignored_or_an_error() {
    for (dialect, pattern, matching) in [
        (Dialect::Emacs, br"*.py[co]".as_slice(), b"*.pyc".as_slice()),
        (Dialect::Emacs, br"\(*a\|b\)", b"*a"),
        (Dialect::Basic, br"^*a", b"*a"),
        (Dialect::GnuAwk, br"(*a)", b"*a"),
        (Dialect::Egrep, br"(*a)", b"a"),
        // GNU egrep ignores only the opening brace at branch start.
        (Dialect::Egrep, br"{2}a", b"2}a"),
    ] {
        let regex = FindRegex::new(pattern, dialect, false).unwrap();
        assert_eq!(
            regex.try_is_match(matching),
            Ok(true),
            "{dialect:?} {pattern:?}"
        );
        assert_eq!(regex.try_is_match(b"other"), Ok(false));
    }
    for dialect in [Dialect::Extended, Dialect::PosixAwk] {
        for pattern in [
            br"*a".as_slice(),
            br"(*a)",
            br"b|+a",
            br"^?a",
            br"a${regex}",
        ] {
            assert!(FindRegex::new(pattern, dialect, false).is_err());
        }
    }
    assert!(FindRegex::new(br"\{2\}a", Dialect::Basic, false).is_err());
}

#[test]
fn interval_validation_uses_gnu_syntax_and_bound_limit() {
    for (dialect, patterns) in [
        (
            Dialect::Extended,
            [br"a{x".as_slice(), br"a{3,1}", br"a{99999}", br"a{}"],
        ),
        (
            Dialect::Basic,
            [br"a\{x", br"a\{3,1\}", br"a\{99999\}", br"a\{\}"],
        ),
    ] {
        for pattern in patterns {
            assert!(FindRegex::new(pattern, dialect, false).is_err());
        }
    }
    for dialect in [
        Dialect::Extended,
        Dialect::Egrep,
        Dialect::GnuAwk,
        Dialect::PosixAwk,
    ] {
        let regex = FindRegex::new(br"a{,3}", dialect, false).unwrap();
        assert_eq!(regex.try_is_match(b""), Ok(true));
        assert_eq!(regex.try_is_match(b"aaa"), Ok(true));
        assert_eq!(regex.try_is_match(b"aaaa"), Ok(false));
    }
    let regex = FindRegex::new(br"a{x", Dialect::Egrep, false).unwrap();
    assert_eq!(regex.try_is_match(b"a{x"), Ok(true));
}

#[test]
fn invalid_references_and_unmatched_captures_do_not_guess() {
    for pattern in [br"\1".as_slice(), br"(a\1)", br"(a)\2", br"\1(a)"] {
        assert!(FindRegex::new(pattern, Dialect::Extended, false).is_err());
    }
    let regex = FindRegex::new(br"(a)?\1", Dialect::Extended, false).unwrap();
    assert_eq!(regex.try_is_match(b"aa"), Ok(true));
    assert_eq!(regex.try_is_match(b""), Ok(false));
    let regex = FindRegex::new(br"(a|ab)\1", Dialect::Extended, false).unwrap();
    assert_eq!(regex.try_is_match(b"abab"), Ok(true));
    assert_eq!(regex.try_is_match(b"aba"), Ok(false));
}

#[test]
fn references_obey_branch_scope_and_capture_numbering() {
    for pattern in [br"(a)|\1".as_slice(), br"((a)|\2)", br"(a)|(b)\1"] {
        assert!(FindRegex::new(pattern, Dialect::Extended, false).is_err());
    }
    for (pattern, bytes) in [
        (br"((a)|b)\2".as_slice(), b"aa".as_slice()),
        (br"(a)(b|\1)", b"aa"),
        (br"(a)(b)(c)(d)(e)(f)(g)(h)(i)\9\1", b"abcdefghiia"),
        (br"(a)(b)(c)(d)(e)(f)(g)(h)(i)(j)\9", b"abcdefghiji"),
    ] {
        let regex = FindRegex::new(pattern, Dialect::Extended, false).unwrap();
        assert_eq!(regex.try_is_match(bytes), Ok(true));
    }
}

#[test]
fn backreference_dot_and_boundaries_share_the_linear_engines_byte_semantics() {
    for (dialect, pattern) in [
        (Dialect::Emacs, br"\(.\)\1".as_slice()),
        (Dialect::Extended, br"(.)\1"),
    ] {
        let regex = FindRegex::new(pattern, dialect, false).unwrap();
        assert_eq!(regex.try_is_match(b"\xff\xff"), Ok(true));
        assert_eq!(regex.try_is_match(b"\n\n"), Ok(dialect != Dialect::Emacs));
    }
    let regex = FindRegex::new(br"\b(a)\1\b", Dialect::Extended, false).unwrap();
    assert_eq!(regex.try_is_match(b"aa"), Ok(true));
    assert_eq!(regex.try_is_match(b"!aa"), Ok(false));
}

#[test]
fn an_exhausted_budget_is_an_error_not_a_non_match() {
    // The trailing a+ accepts every run of a, so the path is in the
    // language; the backreference branch exhausts the step budget first.
    let regex = FindRegex::new(br"(a|aa)*\1b|a+", Dialect::Extended, false).unwrap();
    assert_eq!(regex.try_is_match(&[b'a'; 32]), Err(MatchLimit::Steps));
}
