//! Trailing `/`: directory-only rules, and the directory bit of the API.

use super::{Dir, File, Ignore, Row, Unmatched, assert_rows};

pub(super) const ROWS: &[Row] = &[
    (
        "directory-only takes a directory",
        "build/\n",
        "build",
        Dir,
        Ignore,
    ),
    (
        "directory-only spares a file",
        "build/\n",
        "build",
        File,
        Unmatched,
    ),
    (
        "directory-only at depth",
        "generated/\n",
        "old/generated",
        Dir,
        Ignore,
    ),
    (
        "directory-only at depth spares a file",
        "generated/\n",
        "old/generated",
        File,
        Unmatched,
    ),
    (
        "directory-only does not anchor",
        "build/\n",
        "a/build",
        Dir,
        Ignore,
    ),
    ("rooted directory-only", "/build/\n", "build", Dir, Ignore),
    (
        "rooted directory-only anchors",
        "/build/\n",
        "x/build",
        Dir,
        Unmatched,
    ),
    ("anchored directory-only", "a/b/\n", "a/b", Dir, Ignore),
    (
        "anchored directory-only spares a file",
        "a/b/\n",
        "a/b",
        File,
        Unmatched,
    ),
    ("wildcard directory-only", "*.d/\n", "x.d", Dir, Ignore),
    (
        "wildcard directory-only spares a file",
        "*.d/\n",
        "x.d",
        File,
        Unmatched,
    ),
    (
        "plain rule takes a directory",
        "build\n",
        "build",
        Dir,
        Ignore,
    ),
    ("plain rule takes a file", "build\n", "build", File, Ignore),
    (
        "trailing globstar is not its parent",
        "cache/**\n",
        "cache",
        Dir,
        Unmatched,
    ),
    (
        "doubled trailing slash",
        "build//\n",
        "build",
        Dir,
        Unmatched,
    ),
    ("doubled inner slash", "a//b\n", "a/b", File, Unmatched),
];

#[test]
fn directory_only() {
    assert_rows(ROWS);
}
