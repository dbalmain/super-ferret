//! Behaviour-organised tests for the gitignore matcher.
//!
//! Each area module holds a table of rows: the ignore-file text, one path, its
//! kind, and git's answer. Every row is checked twice: its area test drives
//! `Gitignore::compile` + `matched`, and `oracle` asks `git check-ignore` the
//! same question, so no expected value rests on a reading of the manual alone.
//! `differential` keeps the seeded random comparison with git.
//!
//! Written blind (D16): from `gitignore(5)`, `.ai/matcher-behaviours.md` and
//! git as an oracle; neither `ignore`'s nor `globset`'s tests were read.
#![cfg(unix)]

mod anchoring;
mod buckets;
mod classes;
mod differential;
mod directory;
mod globstar;
mod normalisation;
mod oracle;
mod ordering;
mod syntax;

use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

use super::Gitignore;
use super::Match::{self, Ignore, None as Unmatched, Whitelist};

/// Whether the queried path is a file or a directory.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    File,
    Dir,
}

use Kind::{Dir, File};

/// `(label, ignore-file text, path, kind, git's answer)`.
type Row = (&'static str, &'static str, &'static str, Kind, Match);

/// One question for both the matcher and git. Paths are bytes so non-UTF-8
/// names can be asked too.
#[derive(Clone, Debug)]
struct Case {
    label: String,
    patterns: String,
    path: Vec<u8>,
    kind: Kind,
    want: Match,
}

impl From<&Row> for Case {
    fn from(&(label, patterns, path, kind, want): &Row) -> Self {
        Self {
            label: label.to_owned(),
            patterns: patterns.to_owned(),
            path: path.as_bytes().to_vec(),
            kind,
            want,
        }
    }
}

impl Case {
    fn matched(&self) -> (Match, usize) {
        let (matcher, errors) = Gitignore::compile(&self.patterns);
        let got = matcher.matched(Path::new(OsStr::from_bytes(&self.path)), self.kind == Dir);
        (got, errors.len())
    }

    fn describe(&self) -> String {
        format!(
            "{}: {:?} on {:?} ({:?})",
            self.label,
            self.patterns,
            String::from_utf8_lossy(&self.path),
            self.kind
        )
    }
}

/// Runs every case through the real matcher, expecting no line errors, and
/// reports all failures together rather than stopping at the first.
fn assert_cases(cases: impl IntoIterator<Item = Case>) {
    let mut total = 0;
    let failures: Vec<String> = cases
        .into_iter()
        .inspect(|_| total += 1)
        .filter_map(|case| {
            let (got, errors) = case.matched();
            (got != case.want || errors != 0).then(|| {
                format!(
                    "{}\n    want {:?}, got {got:?}, {errors} line error(s)",
                    case.describe(),
                    case.want
                )
            })
        })
        .collect();
    assert!(
        failures.is_empty(),
        "{} of {total} cases failed:\n{}",
        failures.len(),
        failures.join("\n")
    );
}

fn assert_rows(rows: &[Row]) {
    assert_cases(rows.iter().map(Case::from));
}

/// Every case any area test asserts, for the git oracle.
fn all_cases() -> Vec<Case> {
    [
        syntax::ROWS,
        syntax::INVALID_ROWS,
        classes::ROWS,
        anchoring::ROWS,
        globstar::ROWS,
        normalisation::ROWS,
        ordering::ROWS,
        directory::ROWS,
    ]
    .into_iter()
    .flatten()
    .map(Case::from)
    .chain(syntax::byte_cases())
    .chain(buckets::cases())
    .collect()
}
