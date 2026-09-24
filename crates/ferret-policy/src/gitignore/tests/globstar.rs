//! `**` as a whole component: leading, middle, trailing, repeated, and the
//! stars beside literals that are not globstars.
//!
//! Where a globstar would also ignore an intermediate directory, the row
//! re-includes directories (`!…/`) so git answers about the path itself.

use super::{Dir, File, Ignore, Row, Unmatched, Whitelist, assert_rows};

pub(super) const ROWS: &[Row] = &[
    // Leading.
    (
        "leading globstar, zero directories",
        "**/foo\n",
        "foo",
        File,
        Ignore,
    ),
    (
        "leading globstar, one directory",
        "**/foo/bar\n",
        "a/foo/bar",
        File,
        Ignore,
    ),
    (
        "leading globstar, many directories",
        "**/foo/bar\n",
        "a/b/foo/bar",
        File,
        Ignore,
    ),
    (
        "leading globstar keeps the rest fixed",
        "**/foo/bar\n",
        "a/foo/x/bar",
        File,
        Unmatched,
    ),
    (
        "rooted globstar, zero directories",
        "/**/needle.bin\n",
        "needle.bin",
        File,
        Ignore,
    ),
    (
        "rooted globstar, many directories",
        "/**/needle.bin\n",
        "old/cache/needle.bin",
        File,
        Ignore,
    ),
    (
        "leading globstar before an extension",
        "**/*.rs\n",
        "a/b/c.rs",
        File,
        Ignore,
    ),
    (
        "leading globstar before an extension at the root",
        "**/*.rs\n",
        "c.rs",
        File,
        Ignore,
    ),
    (
        "leading triple star is a globstar",
        "***/b\n",
        "b",
        File,
        Ignore,
    ),
    (
        "leading triple star, many directories",
        "***/b\n",
        "x/y/b",
        File,
        Ignore,
    ),
    // Middle.
    (
        "middle globstar, zero directories",
        "src/**/manifest.yml\n",
        "src/manifest.yml",
        File,
        Ignore,
    ),
    (
        "middle globstar, many directories",
        "src/**/manifest.yml\n",
        "src/a/b/manifest.yml",
        File,
        Ignore,
    ),
    (
        "middle globstar keeps its start anchored",
        "src/**/manifest.yml\n",
        "other/src/manifest.yml",
        File,
        Unmatched,
    ),
    (
        "middle globstar keeps its end whole",
        "src/**/manifest.yml\n",
        "src/a/manifest.yml.bak",
        File,
        Unmatched,
    ),
    (
        "middle globstar retries after a false start",
        "a/**/b/c\n",
        "a/b/x/b/c",
        File,
        Ignore,
    ),
    (
        "two globstars, zero each",
        "a/**/b/**/c\n",
        "a/b/c",
        File,
        Ignore,
    ),
    (
        "two globstars, one each",
        "a/**/b/**/c\n",
        "a/x/b/y/c",
        File,
        Ignore,
    ),
    (
        "two globstars need the middle",
        "a/**/b/**/c\n",
        "a/x/y/c",
        File,
        Unmatched,
    ),
    (
        "adjacent globstars, zero",
        "one/**/**/end\n",
        "one/end",
        File,
        Ignore,
    ),
    (
        "adjacent globstars, one",
        "one/**/**/end\n",
        "one/x/end",
        File,
        Ignore,
    ),
    (
        "adjacent globstars, two",
        "one/**/**/end\n",
        "one/x/y/end",
        File,
        Ignore,
    ),
    (
        "middle triple star is a globstar",
        "a/***/b\n",
        "a/b",
        File,
        Ignore,
    ),
    // Trailing: everything strictly inside.
    (
        "trailing globstar",
        "vendor/**\n",
        "vendor/code.c",
        File,
        Ignore,
    ),
    (
        "trailing globstar is inside, directory",
        "vendor/**\n",
        "vendor",
        Dir,
        Unmatched,
    ),
    (
        "trailing globstar is inside, file",
        "vendor/**\n",
        "vendor",
        File,
        Unmatched,
    ),
    (
        "trailing globstar at depth",
        "vendor/**\n!vendor/*/\n",
        "vendor/lib/code.c",
        File,
        Ignore,
    ),
    (
        "trailing globstar at depth, directory",
        "vendor/**\n!vendor/*/\n",
        "vendor/lib",
        Dir,
        Whitelist,
    ),
    (
        "trailing globstar keeps its start whole",
        "abc/**\n",
        "abcd/x",
        File,
        Unmatched,
    ),
    (
        "trailing globstar, one level",
        "abc/**\n",
        "abc/x",
        File,
        Ignore,
    ),
    (
        "trailing triple star is a globstar",
        "!a/***\n",
        "a/x/y",
        File,
        Whitelist,
    ),
    (
        "trailing globstar, directory-only",
        "a/**/\n",
        "a/x",
        Dir,
        Ignore,
    ),
    (
        "trailing globstar, directory-only, file",
        "a/**/\n",
        "a/x",
        File,
        Unmatched,
    ),
    // A lone globstar has no slash, so it is an ordinary basename star.
    ("lone globstar", "**\n", ".private", File, Ignore),
    (
        "lone globstar at depth",
        "**\n!sub/\n",
        "sub/.env",
        File,
        Ignore,
    ),
    (
        "lone globstar, re-included directory",
        "**\n!sub/\n",
        "sub",
        Dir,
        Whitelist,
    ),
    ("lone directory globstar", "**/\n", "d", Dir, Ignore),
    (
        "lone directory globstar, file",
        "**/\n",
        "f",
        File,
        Unmatched,
    ),
    // Stars beside literals are ordinary stars.
    (
        "double star beside a literal",
        "a**/report\n",
        "ax/y/report",
        File,
        Unmatched,
    ),
    (
        "double star beside a literal, one component",
        "a**/report\n",
        "axx/report",
        File,
        Ignore,
    ),
    (
        "double star beside a suffix",
        "a/**b\n",
        "a/x/yb",
        File,
        Unmatched,
    ),
    (
        "double star beside a suffix, one component",
        "a/**b\n",
        "a/xb",
        File,
        Ignore,
    ),
    (
        "escaped globstar star is a literal",
        "a/\\**/b\n",
        "a/*x/b",
        File,
        Ignore,
    ),
    (
        "escaped globstar star is no globstar",
        "a/\\**/b\n",
        "a/x/b",
        File,
        Unmatched,
    ),
];

#[test]
fn globstar() {
    assert_rows(ROWS);
}
