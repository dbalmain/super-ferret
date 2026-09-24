//! Ignore-file bytes before pattern syntax: BOM, line endings, NUL, tabs.

use super::{File, Ignore, Row, Unmatched, assert_rows};

pub(super) const ROWS: &[Row] = &[
    (
        "UTF-8 BOM is stripped",
        "\u{feff}alpha\n",
        "alpha",
        File,
        Ignore,
    ),
    (
        "BOM is stripped only once",
        "\u{feff}\u{feff}alpha\n",
        "\u{feff}alpha",
        File,
        Ignore,
    ),
    (
        "BOM on a later line is data",
        "x\n\u{feff}beta\n",
        "\u{feff}beta",
        File,
        Ignore,
    ),
    (
        "BOM on a later line is not stripped",
        "x\n\u{feff}beta\n",
        "beta",
        File,
        Unmatched,
    ),
    (
        "final CR without LF is stripped",
        "omega\r",
        "omega",
        File,
        Ignore,
    ),
    ("CRLF drops the CR", "f\r\n", "f", File, Ignore),
    (
        "one of two CR bytes before LF is line ending",
        "foo\r\r\n",
        "foo\r",
        File,
        Ignore,
    ),
    (
        "CR immediately before NUL remains pattern data",
        "foo\r\0bar\n",
        "foo\r",
        File,
        Ignore,
    ),
    ("CRLF on every line", "one\r\ntwo\r\n", "two", File, Ignore),
    ("inner CR is data", "a\rb\n", "a\rb", File, Ignore),
    (
        "last line needs no newline",
        "first\nlast",
        "last",
        File,
        Ignore,
    ),
    (
        "NUL ends the line",
        "prefix\0suffix\n",
        "prefix",
        File,
        Ignore,
    ),
    (
        "NUL drops the rest of the line",
        "prefix\0suffix\n",
        "prefixsuffix",
        File,
        Unmatched,
    ),
    (
        "line after a NUL applies",
        "a\0b\nnext\n",
        "next",
        File,
        Ignore,
    ),
    ("trailing tab is data", "tab\t\n", "tab\t", File, Ignore),
    (
        "trailing tab is required",
        "tab\t\n",
        "tab",
        File,
        Unmatched,
    ),
];

#[test]
fn file_normalisation() {
    assert_rows(ROWS);
}
