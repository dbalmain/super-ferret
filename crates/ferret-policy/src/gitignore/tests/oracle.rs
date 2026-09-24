//! Git 2.54 as a black-box oracle for every table row.
//!
//! All cases share one repository: case `i` lives in directory `cNNNN/` with
//! its own `.gitignore`, so patterns are relative to their ignore file exactly
//! as in a single-file test, and one `check-ignore` call answers everything.
//!
//! Git reports an ignored ancestor's rule for every path beneath it, which says
//! nothing about the path itself, so a case whose ancestor git ignores fails
//! here: every row must be evidence about its own path.

use std::ffi::OsStr;
use std::fs;
use std::io::Write;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use super::{Dir, Ignore, Match, Unmatched, Whitelist, all_cases};

#[test]
fn every_table_row_agrees_with_git() {
    let cases = all_cases();
    let repository = Scratch::new("oracle");
    let mut queries = Vec::new();
    let mut spans = Vec::new();

    for (index, case) in cases.iter().enumerate() {
        let root = format!("c{index:04}");
        let root_path = repository.path.join(&root);
        must(fs::create_dir(&root_path));
        must(fs::write(root_path.join(".gitignore"), &case.patterns));

        let ancestors: Vec<&[u8]> = case
            .path
            .iter()
            .enumerate()
            .filter(|(_, byte)| **byte == b'/')
            .map(|(slash, _)| &case.path[..slash])
            .collect();
        let target = root_path.join(OsStr::from_bytes(&case.path));
        if case.kind == Dir {
            must(fs::create_dir_all(&target));
        } else {
            if let Some(parent) = target.parent() {
                must(fs::create_dir_all(parent));
            }
            must(fs::write(&target, []));
        }

        let start = queries.len();
        for relative in ancestors.iter().copied().chain([&case.path[..]]) {
            let mut query = root.clone().into_bytes();
            query.push(b'/');
            query.extend_from_slice(relative);
            queries.push(query);
        }
        spans.push(start..queries.len());
    }

    let answers = check_ignore(&repository.path, &queries);
    let mut counts = [0; 3];
    let failures: Vec<String> = cases
        .iter()
        .zip(spans)
        .filter_map(|(case, span)| {
            let (git, ancestors) = answers[span].split_last()?;
            counts[match git {
                Unmatched => 0,
                Ignore => 1,
                Whitelist => 2,
            }] += 1;
            if ancestors.contains(&Ignore) {
                return Some(format!(
                    "{}\n    an ancestor is ignored, so git's {git:?} is not about this path",
                    case.describe()
                ));
            }
            (*git != case.want).then(|| {
                format!(
                    "{}\n    table says {:?}, git says {git:?}",
                    case.describe(),
                    case.want
                )
            })
        })
        .collect();
    assert!(
        failures.is_empty(),
        "{} of {} rows disagree with git:\n{}",
        failures.len(),
        cases.len(),
        failures.join("\n")
    );
    // Each answer must be well represented, or the tables have drifted into
    // testing only one outcome.
    let [unmatched, ignored, whitelisted] = counts;
    eprintln!(
        "git oracle: {} rows, None={unmatched}, Ignore={ignored}, Whitelist={whitelisted}",
        cases.len()
    );
    assert!(
        unmatched >= 100 && ignored >= 100 && whitelisted >= 50,
        "outcome coverage: None={unmatched}, Ignore={ignored}, Whitelist={whitelisted}"
    );
}

/// Asks git about each relative path in `repository`, in order.
pub(super) fn check_ignore(repository: &Path, paths: &[Vec<u8>]) -> Vec<Match> {
    let mut child = must(
        Command::new("git")
            .args([
                "-c",
                "core.excludesFile=/dev/null",
                "check-ignore",
                "--no-index",
                "--verbose",
                "--non-matching",
                "-z",
                "--stdin",
            ])
            .current_dir(repository)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn(),
    );
    {
        let mut stdin = child
            .stdin
            .take()
            .unwrap_or_else(|| panic!("piped git stdin was unavailable"));
        for path in paths {
            must(stdin.write_all(path));
            must(stdin.write_all(&[0]));
        }
    }
    let output = must(child.wait_with_output());
    assert!(
        output.status.success() || output.status.code() == Some(1),
        "git check-ignore failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let mut fields: Vec<&[u8]> = output.stdout.split(|byte| *byte == 0).collect();
    if fields.last() == Some(&&[][..]) {
        fields.pop();
    }
    assert_eq!(fields.len(), paths.len() * 4, "unexpected git output shape");
    fields
        .chunks_exact(4)
        .map(|record| {
            if record[0].is_empty() {
                Unmatched
            } else if record[2].starts_with(b"!") {
                Whitelist
            } else {
                Ignore
            }
        })
        .collect()
}

/// A fresh git repository under the system temp directory, removed on drop.
pub(super) struct Scratch {
    pub(super) path: PathBuf,
}

impl Scratch {
    pub(super) fn new(name: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "ferret-policy-gitignore-{name}-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&path);
        must(fs::create_dir(&path));
        let init = must(
            Command::new("git")
                .arg("init")
                .arg("-q")
                .arg(&path)
                .output(),
        );
        assert!(
            init.status.success(),
            "git init failed: {}",
            String::from_utf8_lossy(&init.stderr)
        );
        Self { path }
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

pub(super) fn must<T, E: std::fmt::Debug>(result: Result<T, E>) -> T {
    result.unwrap_or_else(|error| panic!("test setup failed: {error:?}"))
}
