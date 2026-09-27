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
    assert!(matches!(error("re:("), ParseError::Regex(arg, _) if arg == "re:("));
    assert!(matches!(
        Query::from_args([b"re:\xff".as_slice()], now()),
        Err(ParseError::NotUtf8(_))
    ));
}

#[test]
fn an_unknown_prefix_is_just_a_word() {
    // Names may contain `:`; only the documented prefixes are atoms.
    assert_eq!(plan("foo:bar").1, "heap scan for \"foo:bar\" (folded)");
}
