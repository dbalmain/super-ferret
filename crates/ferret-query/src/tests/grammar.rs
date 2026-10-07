//! The grammar: what each argument parses to, and the plan it gets.

use super::now;
use crate::{ParseError, Query, Strategy};

fn plan(text: &str) -> (Strategy, String) {
    let query = Query::parse(text, now()).unwrap();
    (query.strategy(), query.explain())
}

fn error(text: &str) -> ParseError {
    Query::parse(text, now()).unwrap_err()
}

#[test]
fn each_atom_parses_to_its_test_and_the_plan_says_so() {
    let cases = [
        (
            "main",
            Strategy::HeapScan,
            "heap scan for \"main\" (folded)",
        ),
        ("case:Main", Strategy::HeapScan, "heap scan for \"Main\""),
        (
            "ext:RS",
            Strategy::HeapScan,
            "heap scan for \".rs\" (folded); then name ends \".RS\" (folded)",
        ),
        (
            "case:ext:c",
            Strategy::HeapScan,
            "heap scan for \".c\"; then name ends \".c\"",
        ),
        (
            "*.rs",
            Strategy::HeapScan,
            "heap scan for \".rs\" (folded); then name glob *.rs",
        ),
        (
            "src/**/*.rs",
            Strategy::HeapScan,
            "heap scan for \".rs\" (folded); then path glob src/**/*.rs",
        ),
        // Folded, `s` may match LONG S, so the literal stops before it.
        (
            r"re:^main\.rs$",
            Strategy::HeapScan,
            r#"heap scan for "main.r" (folded); then name regex ^main\.rs$"#,
        ),
        (
            r"case:re:^main\.rs$",
            Strategy::HeapScan,
            r#"heap scan for "main.rs"; then name regex ^main\.rs$"#,
        ),
        (
            "re:^[a-z]+$",
            Strategy::AllNames,
            "all names; then name regex ^[a-z]+$",
        ),
        ("*", Strategy::AllNames, "all names; then name glob *"),
        ("", Strategy::AllNames, "all names"),
        (
            "path:src/",
            Strategy::AllNames,
            "all names; then path has \"src/\"",
        ),
        (
            "src/deep",
            Strategy::AllNames,
            "all names; then path has \"src/deep\"",
        ),
        ("size:>1k", Strategy::InodeScan, "inode scan for size >1024"),
        (
            "mtime:<1d type:f",
            Strategy::InodeScan,
            "inode scan for age <86400s, type File",
        ),
        (
            "size:>1M rs",
            Strategy::HeapScan,
            "heap scan for \"rs\" (folded); then size >1048576",
        ),
    ];
    for (text, strategy, explain) in cases {
        assert_eq!(plan(text), (strategy, explain.to_string()), "{text:?}");
    }
}

#[test]
fn the_longest_literal_drives_and_the_rest_filter() {
    // `parse_http` is longer than `rs`, so it drives; `rs` still filters.
    assert_eq!(
        plan("rs parse_HTTP").1,
        "heap scan for \"parse_http\" (folded); then name has \"rs\" (folded)"
    );
    // A regex's literal competes with a word's on length alone; ties go
    // to the earlier atom.
    assert_eq!(
        plan(r"case:re:^parse_\w+ rs").1,
        "heap scan for \"parse_\"; then name regex ^parse_\\w+, name has \"rs\" (folded)"
    );
    assert_eq!(
        plan("ab cd").1,
        "heap scan for \"ab\" (folded); then name has \"cd\" (folded)"
    );
    assert_eq!(
        plan("cd ab").1,
        "heap scan for \"cd\" (folded); then name has \"ab\" (folded)"
    );
}

#[test]
fn malformed_atoms_are_errors_that_name_the_argument() {
    assert_eq!(error("size:big"), ParseError::Size("size:big".into()));
    assert_eq!(error("size:>1q"), ParseError::Size("size:>1q".into()));
    // An age needs a direction: "modified exactly a day ago" is meaningless.
    assert_eq!(error("mtime:1d"), ParseError::Age("mtime:1d".into()));
    assert_eq!(error("mtime:<1x"), ParseError::Age("mtime:<1x".into()));
    assert_eq!(error("type:socket"), ParseError::Type("type:socket".into()));
    assert_eq!(error("ext:"), ParseError::Empty("ext:".into()));
    // One empty argument, as the shell passes `''`: `parse` would split it
    // to no arguments at all. An empty word matched past the last name.
    for arg in ["", "case:", "case:ext:", "case:path:"] {
        assert_eq!(
            Query::from_args([arg], now()).unwrap_err(),
            ParseError::Empty(arg.into()),
            "{arg:?}"
        );
    }
    assert!(matches!(error("re:("), ParseError::Regex(arg, _) if arg == "re:("));
    assert!(matches!(
        Query::from_args([b"re:\xff".as_slice()], now()),
        Err(ParseError::NotUtf8(_))
    ));
    // 2^54 KiB is 2^64 bytes: the shift loses the high bit, which must be
    // an error, not `size:>0`.
    assert_eq!(
        error("size:>18014398509481984k"),
        ParseError::Size("size:>18014398509481984k".into())
    );
    assert_eq!(
        plan("size:>18014398509481983k").1,
        "inode scan for size >18446744073709550592"
    );
}

#[test]
fn an_unknown_prefix_is_just_a_word() {
    // Names may contain `:`; only the documented prefixes are atoms.
    assert_eq!(plan("foo:bar").1, "heap scan for \"foo:bar\" (folded)");
}

#[test]
fn a_nul_in_any_atom_is_refused() {
    // A NUL is a name terminator in the heap: a word holding one would hit
    // name boundaries and report names that contain no NUL.
    for arg in [&b"\0"[..], b"a\0b", b"case:\0", b"path:a\0", b"*\0"] {
        assert_eq!(
            Query::from_args([arg], now()).unwrap_err(),
            ParseError::Nul(arg.to_vec()),
            "{}",
            arg.escape_ascii()
        );
    }
}

/// D62 A's operators. Adjacent atoms AND, which binds tighter than `OR`;
/// `NOT` binds tightest; a parenthesised group is one operand. The plain
/// name atoms of the top-level AND still plan as S1's, so the S1 driver
/// shows through; everything else is evaluated per row as a tree.
#[test]
fn operators_parse_with_and_tighter_than_or_and_not_tightest() {
    let a = r#"name has "a" (folded)"#;
    let b = r#"name has "b" (folded)"#;
    let c = r#"name has "c" (folded)"#;
    let d = r#"name has "d" (folded)"#;
    let cases = [
        (
            "a b OR c",
            format!("all names; then (({a} AND {b}) OR {c})"),
        ),
        (
            "a OR b c",
            format!("all names; then ({a} OR ({b} AND {c}))"),
        ),
        (
            "NOT a b",
            format!(r#"heap scan for "b" (folded); then NOT {a}"#),
        ),
        ("NOT NOT a", format!("all names; then NOT NOT {a}")),
        (
            "( a OR b ) c",
            format!(r#"heap scan for "c" (folded); then ({a} OR {b})"#),
        ),
        // A group in the top-level AND joins it: its conjuncts are the
        // query's.
        (
            "( ( a OR b ) ( c OR NOT d ) )",
            format!("all names; then ({a} OR {b}), ({c} OR NOT {d})"),
        ),
        (
            "a OR NOT ( b OR c )",
            format!("all names; then ({a} OR NOT ({b} OR {c}))"),
        ),
        ("( a )", r#"heap scan for "a" (folded)"#.to_string()),
        ("text:x", r#"all names; then text "x" (folded)"#.to_string()),
        (
            "NOT text:x",
            r#"all names; then NOT text "x" (folded)"#.to_string(),
        ),
        (
            "case:text:X a",
            r#"heap scan for "a" (folded); then text "x" (exact case, verified)"#.to_string(),
        ),
        (
            "text:x-y OR b",
            format!(r#"all names; then (text "x y" (folded) OR {b})"#),
        ),
    ];
    for (query, explain) in cases {
        assert_eq!(plan(query).1, explain, "{query}");
    }
}

/// A file named `OR`, `NOT`, `(` or `)` is reached through an atom prefix:
/// `name:` takes the rest as a bare word, `case:` as an exact one.
#[test]
fn an_operator_spelled_with_a_prefix_is_a_name_atom() {
    for (query, explain) in [
        ("name:OR", r#"heap scan for "or" (folded)"#),
        ("case:OR", r#"heap scan for "OR""#),
        ("name:NOT", r#"heap scan for "not" (folded)"#),
        ("name:(", r#"heap scan for "(" (folded)"#),
        ("case:)", r#"heap scan for ")""#),
        ("or", r#"heap scan for "or" (folded)"#),
    ] {
        assert_eq!(plan(query).1, explain, "{query}");
    }
}

#[test]
fn misplaced_operators_are_errors_that_say_what_is_missing() {
    let cases = [
        ("OR", ParseError::Dangling("OR")),
        ("OR a", ParseError::Dangling("OR")),
        ("a OR", ParseError::Dangling("OR")),
        ("a OR OR b", ParseError::Dangling("OR")),
        ("( a OR )", ParseError::Dangling("OR")),
        ("NOT", ParseError::Dangling("NOT")),
        ("a NOT", ParseError::Dangling("NOT")),
        ("NOT )", ParseError::Dangling("NOT")),
        ("( a", ParseError::Unclosed),
        ("( ( a ) b", ParseError::Unclosed),
        ("a )", ParseError::Unopened),
        (") a", ParseError::Unopened),
        ("( a ) )", ParseError::Unopened),
        ("( )", ParseError::EmptyGroup),
        ("text:-", ParseError::Text("text:-".into())),
    ];
    for (query, expected) in cases {
        assert_eq!(error(query), expected, "{query}");
    }
    let messages = [
        ("a OR", "`OR` needs a query on each side"),
        ("NOT", "`NOT` needs a query after it"),
        ("( a", "a `(` is not closed by a `)`"),
        ("a )", "a `)` has no `(` before it"),
        ("( )", "`( )` holds no query"),
    ];
    for (query, message) in messages {
        assert_eq!(error(query).to_string(), message, "{query}");
    }
}
