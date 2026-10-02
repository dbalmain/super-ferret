//! Pure translator regressions include the default dialect's easy-to-miss
//! unescaped ? and escaped alternation, and basic's literal +.
#![allow(clippy::unwrap_used)] // Failed regex compilation is a test failure.

use super::*;

#[test]
fn emacs_optional_character_and_group_alternation_are_operators() {
    let regex = FindRegex::new(br".*\.pyc?", Dialect::Emacs, false).unwrap();
    assert!(regex.is_match(b"./m.py"));
    assert!(regex.is_match(b"./m.pyc"));
    assert!(!regex.is_match(b"./m.pycc"));
    assert!(!regex.is_match(b"./m.py/other"));
    let regex = FindRegex::new(br".*\.\(pyc\|rej\)", Dialect::Emacs, false).unwrap();
    assert!(regex.is_match(b"./a.pyc"));
    assert!(regex.is_match(b"./b.rej"));
    assert!(!regex.is_match(b"./a.py"));
}

#[test]
fn basic_plus_is_literal_and_intervals_are_escaped() {
    let regex = FindRegex::new(br"./a+", Dialect::Basic, false).unwrap();
    assert!(regex.is_match(b"./a+"));
    assert!(!regex.is_match(b"./aa"));
    for dialect in [Dialect::Emacs, Dialect::Basic, Dialect::MinimalBasic] {
        let regex = FindRegex::new(br"./a\{2,3\}", dialect, false).unwrap();
        assert!(regex.is_match(b"./aa"));
        assert!(regex.is_match(b"./aaa"));
        assert!(!regex.is_match(b"./a"));
    }
    let regex = FindRegex::new(br"./a{2}", Dialect::Awk, false).unwrap();
    assert!(regex.is_match(b"./a{2}"));
    assert!(!regex.is_match(b"./aa"));
}

#[test]
fn dots_match_slashes_newlines_and_nonutf8_bytes_in_c_locale() {
    let regex = FindRegex::new(b"...", Dialect::Emacs, false).unwrap();
    assert!(regex.is_match(b"/\xffa"));
    assert!(!regex.is_match(b"/\xff\n"));
    let newline = FindRegex::new(b"...", Dialect::Extended, false).unwrap();
    assert!(newline.is_match(b"/\xff\n"));
    assert!(!regex.is_match(b"abcd"));
    let regex = FindRegex::new(br".*/[[:alpha:]]+", Dialect::Extended, true).unwrap();
    assert!(regex.is_match(b"./Ab"));
    assert!(!regex.is_match(b"./\xff"));
}

#[test]
fn basic_midpattern_anchors_are_literals_but_extended_anchors_assert() {
    for dialect in [Dialect::Basic, Dialect::Emacs] {
        let regex = FindRegex::new(b"./a^b$c", dialect, false).unwrap();
        assert!(regex.is_match(b"./a^b$c"));
    }
    let regex = FindRegex::new(b"./a^b$c", Dialect::Extended, false).unwrap();
    assert!(!regex.is_match(b"./a^b$c"));
}

#[test]
fn backreferences_are_explicitly_refused() {
    let error = FindRegex::new(br".*\(a\)\1", Dialect::Emacs, false).unwrap_err();
    assert!(error.to_string().contains("backreferences"));
}

#[test]
fn bracket_backslashes_are_literal_in_emacs_and_basic() {
    let regex = FindRegex::new(br"[a\b]", Dialect::Emacs, false).unwrap();
    for byte in [b'a', b'\\', b'b'] {
        assert!(regex.is_match(&[byte]));
    }
    assert!(!regex.is_match(b"c"));
}

#[test]
fn c_locale_collating_and_equivalence_symbols_are_single_bytes() {
    for pattern in [br"[[.a.]]".as_slice(), br"[[=a=]]".as_slice()] {
        let regex = FindRegex::new(pattern, Dialect::Emacs, false).unwrap();
        assert!(regex.is_match(b"a"));
        assert!(!regex.is_match(b"b"));
    }
    let regex = FindRegex::new(br"[[.a.]-[.c.]]", Dialect::Extended, false).unwrap();
    assert!(regex.is_match(b"b"));
    assert!(!regex.is_match(b"d"));
}

#[test]
fn awk_escapes_letters_literally_and_grep_pattern_newline_is_alternation() {
    for dialect in [Dialect::Awk, Dialect::PosixAwk, Dialect::GnuAwk] {
        let regex = FindRegex::new(br"./a\n", dialect, false).unwrap();
        assert!(regex.is_match(b"./an"));
        assert!(!regex.is_match(b"./a\n"));
    }
    for dialect in [Dialect::Grep, Dialect::Egrep] {
        let regex = FindRegex::new(b".*a\n.*b", dialect, false).unwrap();
        assert!(regex.is_match(b"./a"));
        assert!(regex.is_match(b"./b"));
    }
}
