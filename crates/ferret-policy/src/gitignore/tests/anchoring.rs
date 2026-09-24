//! Which part of the path a pattern is compared with: the basename at any
//! depth, or the whole path from the ignore file's directory.

use super::{File, Ignore, Row, Unmatched, assert_rows};

pub(super) const ROWS: &[Row] = &[
    // No slash: the basename, at any depth.
    (
        "basename at depth",
        "*.cache\n",
        "build/obj/cache.cache",
        File,
        Ignore,
    ),
    (
        "literal basename at depth",
        "frotz\n",
        "a/doc/frotz",
        File,
        Ignore,
    ),
    (
        "literal basename is whole",
        "frotz\n",
        "a/xfrotz",
        File,
        Unmatched,
    ),
    (
        "literal basename is not a directory name",
        "frotz\n",
        "frotz.d/x",
        File,
        Unmatched,
    ),
    // A slash anywhere but the end anchors to the ignore file's directory.
    ("middle slash", "doc/frotz\n", "doc/frotz", File, Ignore),
    (
        "middle slash anchors",
        "doc/frotz\n",
        "a/doc/frotz",
        File,
        Unmatched,
    ),
    (
        "middle slash wildcard",
        "logs/*.json\n",
        "logs/today.json",
        File,
        Ignore,
    ),
    (
        "middle slash wildcard anchors",
        "logs/*.json\n",
        "archive/logs/today.json",
        File,
        Unmatched,
    ),
    ("leading slash", "/doc/frotz\n", "doc/frotz", File, Ignore),
    (
        "leading slash anchors",
        "/doc/frotz\n",
        "a/doc/frotz",
        File,
        Unmatched,
    ),
    (
        "leading slash wildcard",
        "/settings/*.toml\n",
        "settings/local.toml",
        File,
        Ignore,
    ),
    (
        "leading slash wildcard anchors",
        "/settings/*.toml\n",
        "project/settings/local.toml",
        File,
        Unmatched,
    ),
    (
        "leading slash, one component",
        "/local.cfg\n",
        "local.cfg",
        File,
        Ignore,
    ),
    (
        "leading slash, one component, anchors",
        "/local.cfg\n",
        "sub/local.cfg",
        File,
        Unmatched,
    ),
    // Anchored patterns match the whole path, component by component.
    (
        "anchored literal is whole",
        "a/b\n",
        "a/bc",
        File,
        Unmatched,
    ),
    (
        "anchored literal is whole at the start",
        "a/b\n",
        "xa/b",
        File,
        Unmatched,
    ),
    (
        "first component is whole",
        "ab/*.c\n",
        "abc/x.c",
        File,
        Unmatched,
    ),
    (
        "first component, one byte, is whole",
        "a/*.c\n",
        "ab/x.c",
        File,
        Unmatched,
    ),
    ("wildcard first component", "*/x.c\n", "a/x.c", File, Ignore),
    (
        "wildcard first component is one component",
        "*/x.c\n",
        "a/b/x.c",
        File,
        Unmatched,
    ),
    (
        "wildcard first component is required",
        "*/x.c\n",
        "x.c",
        File,
        Unmatched,
    ),
    (
        "star cannot float over directories",
        "*prefix/path/item\n",
        "old/prefix/path/item",
        File,
        Unmatched,
    ),
    (
        "star prefix within a component",
        "*prefix/path/item\n",
        "oldprefix/path/item",
        File,
        Ignore,
    ),
    (
        "anchored pattern is not a suffix",
        "b/c\n",
        "a/b/c",
        File,
        Unmatched,
    ),
    (
        "anchored pattern is not a prefix",
        "a/b\n",
        "a/b.c",
        File,
        Unmatched,
    ),
    (
        "two-byte start, general pattern",
        "src/*.rs\n",
        "srcx/a.rs",
        File,
        Unmatched,
    ),
    (
        "question in an anchored component",
        "a/?/c\n",
        "a/b/c",
        File,
        Ignore,
    ),
    (
        "class in an anchored component",
        "a/[bc]/d\n",
        "a/c/d",
        File,
        Ignore,
    ),
];

#[test]
fn anchoring() {
    assert_rows(ROWS);
}
