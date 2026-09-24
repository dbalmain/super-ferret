//! Last-match-wins and directory-only rules across every storage bucket.
//!
//! `Gitignore::push` files each pattern in one of several buckets and
//! `matched` combines them by line number. One representative per bucket, all
//! matching `src/foo.rs`, is set against every other in both orders; a wrong
//! combination in any pair of buckets changes a row's answer.

use super::{Case, Dir, File, Ignore, Kind, Match, Unmatched, Whitelist, assert_cases};

const PATH: &str = "src/foo.rs";
const UNRELATED: &str = "lib/bar.c";

/// `(bucket, pattern)`: every pattern matches `PATH` and not `UNRELATED`.
const REPRESENTATIVES: &[(&str, &str)] = &[
    ("literal basename", "foo.rs"),
    ("globstar literal basename", "**/foo.rs"),
    ("literal path", "src/foo.rs"),
    ("rooted literal path", "/src/foo.rs"),
    ("extension", "*.rs"),
    ("globstar extension", "**/*.rs"),
    ("prefix", "foo*"),
    ("suffix", "*o.rs"),
    ("contains", "*oo*"),
    ("fixed-width suffix", "*.r?"),
    ("general basename", "f*.rs"),
    ("anchored, two-byte literal start", "src/*.rs"),
    ("anchored, one-byte literal start", "s*/foo.rs"),
    ("anchored, wildcard start", "*/foo.rs"),
    ("anchored, globstar start", "**/src/f*"),
];

pub(super) fn cases() -> Vec<Case> {
    let case = |label: String, patterns: String, path: &str, kind: Kind, want: Match| Case {
        label,
        patterns,
        path: path.as_bytes().to_vec(),
        kind,
        want,
    };
    let mut cases = Vec::new();
    for &(bucket, pattern) in REPRESENTATIVES {
        cases.push(case(
            format!("{bucket} alone"),
            format!("{pattern}\n"),
            PATH,
            File,
            Ignore,
        ));
        cases.push(case(
            format!("{bucket} alone, unrelated path"),
            format!("{pattern}\n"),
            UNRELATED,
            File,
            Unmatched,
        ));
        cases.push(case(
            format!("{bucket}, directory-only, directory"),
            format!("{pattern}/\n"),
            PATH,
            Dir,
            Ignore,
        ));
        cases.push(case(
            format!("{bucket}, directory-only, file"),
            format!("{pattern}/\n"),
            PATH,
            File,
            Unmatched,
        ));
        cases.push(case(
            format!("{bucket}, later directory-only negation, file"),
            format!("{pattern}\n!{pattern}/\n"),
            PATH,
            File,
            Ignore,
        ));
        cases.push(case(
            format!("{bucket}, later directory-only negation, directory"),
            format!("{pattern}\n!{pattern}/\n"),
            PATH,
            Dir,
            Whitelist,
        ));
    }
    for &(earlier_bucket, earlier) in REPRESENTATIVES {
        for &(later_bucket, later) in REPRESENTATIVES {
            if earlier == later {
                continue;
            }
            cases.push(case(
                format!("{earlier_bucket} then negated {later_bucket}"),
                format!("{earlier}\n!{later}\n"),
                PATH,
                File,
                Whitelist,
            ));
            cases.push(case(
                format!("negated {earlier_bucket} then {later_bucket}"),
                format!("!{earlier}\n{later}\n"),
                PATH,
                File,
                Ignore,
            ));
        }
    }
    cases
}

#[test]
fn last_match_wins_across_every_pair_of_buckets() {
    assert_cases(cases());
}
