//! Each zone runs in a fresh process: the real find path snapshots TZ once,
//! without mutating the environment of concurrently running Rust tests.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use super::*;

pub(super) fn differential() {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    let year = super::super::zone::calendar::civil_date(now / 86400).0;
    let ordinal = now / 86400 - super::super::zone::calendar::days_from_civil(year, 1, 1);
    let transition_today = format!("AAA0BBB,{ordinal}/0,{}/0", (ordinal + 1) % 366);
    let bad_file = std::env::temp_dir().join(format!("ferret-bad-zone-{}", std::process::id()));
    fs::write(&bad_file, b"not TZif").unwrap();
    let bad_zone = format!(":{}", bad_file.display());
    for zone in [
        "UTC",
        "Australia/Sydney",
        "America/New_York",
        "Asia/Kolkata",
        "Australia/Adelaide",
        "EST5EDT,M3.2.0/2,M11.1.0/2",
        ":Australia/Sydney",
        ":/etc/localtime",
        "Missing/Zone",
        "!!!",
        "ABC",
        "",
        "XXX5YYY",
        "AAA0BBB,0/0,J365/25",
        "AAA0BBB,J60/1,J300/2",
        "AAA0BBB,M3.5.0/-2,M10.5.0/26",
        &transition_today,
        &bad_zone,
        "unset",
        "missing-directory",
        "discovery",
    ] {
        let mut command = Command::new(std::env::current_exe().unwrap());
        command
            .args([
                "find::tests::timezones::zone_process",
                "--exact",
                "--nocapture",
            ])
            .env("FERRET_TEST_FIND_ZONE", zone)
            .env("TZ", zone)
            .env("TZDIR", super::super::zone::directory());
        match zone {
            "unset" => {
                command.env_remove("TZ");
            }
            "discovery" => {
                command.env("TZ", "Australia/Sydney").env_remove("TZDIR");
            }
            "missing-directory" => {
                command
                    .env("TZ", "Australia/Sydney")
                    .env("TZDIR", bad_file.join("missing"));
            }
            _ => {}
        }
        let status = command.status().unwrap();
        assert!(status.success(), "zone {zone}");
    }
    fs::remove_file(bad_file).unwrap();
}

#[test]
fn zone_process() {
    let Ok(zone) = std::env::var("FERRET_TEST_FIND_ZONE") else {
        return;
    };
    if zone == "extreme" {
        let tree = Tree::new("extreme-date");
        let args = tree.args(&["-newermt", "292277026596-01-01"]);
        let (outcome, output) = run(&args);
        assert_eq!(outcome.errors, 0);
        assert!(output.bytes.is_empty());
        // This date's wall-clock seconds are already outside i64; reject it.
        assert!(Plan::parse(&tree.args(&["-newermt", "292277026597-01-01"])).is_err());
        return;
    }
    let tree = Tree::new("timezones");
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let stamps = [
        0,
        1704067200,
        1710052200,
        1710055800,
        1730611800,
        1730615400,
        1712417400,
        1712421000,
        1728142200,
        1728145800,
        4102444800,
        4118083200,
        now - 86400,
        now - 2 * 86400,
        now / 86400 * 86400 - 3601,
        now / 86400 * 86400 - 3599,
        now / 86400 * 86400 - 86400 - 3601,
        now / 86400 * 86400 - 86400 - 3599,
        now / 86400 * 86400 - 1,
        now / 86400 * 86400 + 1,
    ];
    for (index, stamp) in stamps.into_iter().enumerate() {
        let path = tree.0.join(format!("stamp-{index}"));
        fs::write(&path, b"").unwrap();
        fs::File::options()
            .write(true)
            .open(path)
            .unwrap()
            .set_times(fs::FileTimes::new().set_modified(UNIX_EPOCH + Duration::from_secs(stamp)))
            .unwrap();
    }
    if zone == "discovery" && !Path::new("/usr/share/zoneinfo/Australia/Sydney").exists() {
        // On NixOS the oracle's compiled directory is missing. Preserve that
        // observation separately from the local-zone comparisons below.
        let oracle = gnu::output(
            gnu::command()
                .env("TZ", "Australia/Sydney")
                .env_remove("TZDIR")
                .arg(tree.0.join("stamp-1"))
                .args(["-printf", "%TZ %Tz"]),
        );
        assert!(oracle.status.success());
        assert_eq!(oracle.stdout, b"Australia +0000");
    }
    // D139: neither an ordinary plan nor an epoch-only directive may open a
    // zone file. Explicit ISO offsets bypass local-zone selection too.
    assert!(!super::super::zone::loaded());
    for expression in [
        vec!["-name", "*.txt"],
        vec!["-mtime", "1"],
        vec!["-printf", "%T@ %Ts"],
        vec!["-newermt", "2024-01-01T10:00Z"],
        vec!["-newermt", "2024-01-01T10:00+05:30"],
    ] {
        let (outcome, _) = run(&tree.args(&expression));
        assert_eq!(outcome.errors, 0);
        assert!(!super::super::zone::loaded(), "{expression:?}");
    }
    for expression in [
        vec!["-printf", "%p %T@ %Tc %TZ %Tk %Tz %t %AT %AZ %CT %CZ\n"],
        vec!["-ls"],
        vec!["-daystart", "-mtime", "0"],
        vec!["-daystart", "-mtime", "1"],
        vec!["-daystart", "-mtime", "2"],
        vec!["-newermt", "2024-03-10 02:30"],
        vec!["-newermt", "2024-11-03 01:30"],
        vec![
            "-newermt",
            "2024-01-01",
            ",",
            "-newermt",
            "2024-11-03 01:30",
        ],
        vec![
            "-newermt",
            "2024-07-01",
            ",",
            "-newermt",
            "2024-11-03 01:30",
        ],
        vec![
            "-newermt",
            "2024-01-01",
            ",",
            "-newermt",
            "2024-04-07 02:30",
        ],
        vec![
            "-newermt",
            "2024-07-01",
            ",",
            "-newermt",
            "2024-04-07 02:30",
        ],
        vec!["-newermt", "2024-10-06 02:30"],
        vec!["-newermt", "2024-04-07 02:30"],
        vec!["-newermt", "2024-01-01 10:00"],
        vec!["-newermt", "yesterday"],
        vec!["-newermt", "1 day ago"],
        vec!["-newermt", "2 days ago"],
        vec!["-newermt", "2100-01-01 10:00"],
        vec!["-newermt", "2024-01-01T10:00Z"],
        vec!["-newermt", "2024-01-01T10:00+05:30"],
        vec!["-newermt", "2024-01-01T10:00"],
    ] {
        let expression: Vec<_> = ["-type", "f", "("]
            .into_iter()
            .chain(expression)
            .chain([")"])
            .collect();
        let mut oracle = gnu::command();
        if let Some(tz) = std::env::var_os("TZ") {
            oracle.env("TZ", tz);
        } else {
            oracle.env_remove("TZ");
        }
        if zone == "discovery" {
            // Compare the same database: ferret discovers /etc/zoneinfo;
            // GNU needs TZDIR explicitly on this NixOS host.
            oracle.env("TZDIR", super::super::zone::directory());
        }
        let gnu = gnu::output(oracle.arg(&tree.0).args(&expression));
        let args = tree.args(&expression);
        if !gnu.status.success() {
            assert!(
                Plan::parse(&args).is_err(),
                "gap accepted {zone} {expression:?}"
            );
            continue;
        }
        let (outcome, output) = run(&args);
        assert_eq!(outcome.errors, 0, "{zone} {expression:?}");
        assert_eq!(
            records(&output.bytes, b'\n')
                .iter()
                .map(|line| String::from_utf8_lossy(line).into_owned())
                .collect::<Vec<_>>(),
            records(&gnu.stdout, b'\n')
                .iter()
                .map(|line| String::from_utf8_lossy(line).into_owned())
                .collect::<Vec<_>>(),
            "{zone} {expression:?}"
        );
    }
}

#[test]
fn extreme_date_arguments_do_not_overflow() {
    let output = Command::new(std::env::current_exe().unwrap())
        .args([
            "find::tests::timezones::zone_process",
            "--exact",
            "--nocapture",
        ])
        .env("FERRET_TEST_FIND_ZONE", "extreme")
        .env("TZ", "EST5EDT,M3.2.0,M11.1.0")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}
