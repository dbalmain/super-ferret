//! Fixed timestamp fixtures exercise the actual directive compiler and live
//! evaluator; fractions and width rules differ from plausible Rust defaults.

use std::fs::{File, FileTimes};
use std::time::{Duration, UNIX_EPOCH};

use super::*;
use crate::find::action::tests::Tree;

#[test]
fn nanoseconds_have_ten_digits_and_time_fields_use_c_locale_utc() {
    let tree = Tree::new();
    let path = tree.0.join("a");
    let time = UNIX_EPOCH + Duration::new(946684800, 123456789);
    File::open(&path)
        .unwrap()
        .set_times(FileTimes::new().set_modified(time).set_accessed(time))
        .unwrap();
    let args = [
        "-I".into(),
        path.into_os_string(),
        "-printf".into(),
        "%T@|%TS|%T+|%Tc|%t|%AY|%Am|%Tz\n".into(),
    ];
    let (outcome, output) = crate::find::action::tests::run(&args);
    assert_eq!(outcome.errors, 0);
    assert_eq!(output.bytes,b"946684800.1234567890|00.1234567890|2000-01-01+00:00:00.1234567890|Sat Jan  1 00:00:00 2000|Sat Jan  1 00:00:00.1234567890 2000|2000|01|+0000\n");
}

#[test]
fn escapes_unknown_directives_width_and_precision_follow_gnu() {
    let tree = Tree::new();
    let (_, output) = tree.run(&[
        "-name",
        "a",
        "-printf",
        r"%010s|%-5s|%#m|%06m|%.2p|%L|\q|\012|\0|\cdiscard",
    ]);
    let prefix = tree.0.as_os_str().as_bytes()[..2].to_vec();
    let mut expected = b"         3|3    |0644|000644|".to_vec();
    expected.extend_from_slice(&prefix);
    expected.extend_from_slice(b"|%L|\\q|\n|\0|");
    assert_eq!(output.bytes, expected);
    assert_eq!(output.diagnostics.len(), 2);
    assert!(Format::compile(b"trailing%", &mut Vec::new()).is_err());
    let (_, output) = tree.run(&["-maxdepth", "0", "-printf", "x\\"]);
    assert_eq!(output.bytes, b"x\\");
    assert_eq!(output.diagnostics.len(), 1);
}

#[test]
fn list_escapes_spaces_and_uses_old_year_column() {
    let tree = Tree::new();
    let path = tree.0.join("space name");
    File::open(&path)
        .unwrap()
        .set_times(FileTimes::new().set_modified(UNIX_EPOCH + Duration::from_secs(946684800)))
        .unwrap();
    let (_, output) = tree.run(&["-name", "space name", "-ls"]);
    assert!(
        output
            .bytes
            .windows(13)
            .any(|bytes| bytes == b"Jan  1  2000 ")
    );
    assert!(output.bytes.ends_with(b"/space\\ name\n"));
}

#[test]
fn numeric_flags_and_incomplete_time_directives_follow_gnu() {
    let tree = Tree::new();
    let (_, output) = tree.run(&[
        "-maxdepth",
        "0",
        "-printf",
        "%#S|%.3S|%+d|% d|%010d|%10.3d|%#6.4m|%T%|%-10%|%T",
    ]);
    assert_eq!(
        output.bytes,
        b"1.00000|1|+0| 0|0000000000|       000|  0755|%|%-10|%T"
    );
    assert_eq!(output.diagnostics.len(), 1);
}

#[test]
fn iso_week_year_uses_the_adjacent_year_at_new_year() {
    let tree = Tree::new();
    let path = tree.0.join("a");
    let time = UNIX_EPOCH + Duration::from_secs(946684800);
    File::open(&path)
        .unwrap()
        .set_times(FileTimes::new().set_modified(time))
        .unwrap();
    let args = [
        "-I".into(),
        path.into_os_string(),
        "-printf".into(),
        "%TV|%TG|%Tg\n".into(),
    ];
    let (_, output) = crate::find::action::tests::run(&args);
    assert_eq!(output.bytes, b"52|1999|99\n");
}
