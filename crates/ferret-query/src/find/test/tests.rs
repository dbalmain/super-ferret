use super::*;
use std::fs::FileTimes;
use std::os::unix::fs::{PermissionsExt, symlink};

struct Fixture(PathBuf);
impl Fixture {
    fn new() -> Self {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "ferret-find-test-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).unwrap();
        Self(path)
    }
    fn file(&self, name: &str, size: usize, age: u64) -> Entry {
        let path = self.0.join(name);
        fs::write(&path, vec![0u8; size]).unwrap();
        let file = fs::File::options().write(true).open(&path).unwrap();
        file.set_times(
            FileTimes::new().set_modified(UNIX_EPOCH + Duration::from_secs(2_000_000_000 - age)),
        )
        .unwrap();
        Entry::new(path, 0, FileKind::File)
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[test]
fn shared_numeric_parser_accepts_comparators_and_fractional_times_and_matches_gnu_overflow() {
    for value in [b"1".as_slice(), b"+1", b"-1", b"1.5"] {
        assert!(parse_number(value, true).is_some());
    }
    for value in [b"".as_slice(), b"++1", b"1  ", b"1.2.3"] {
        assert!(parse_number(value, false).is_none(), "{value:?}");
    }
    assert!(parse_number(b"18446744073709551616", false).is_some());
}

#[test]
fn fstype_types_a_symlink_where_it_lives_not_where_it_points() {
    let fixture = Fixture::new();
    let link = fixture.0.join("into-proc");
    symlink("/proc/self", &link).unwrap();
    let proc = Test::FsType("proc".to_owned());
    assert!(
        proc.evaluate(&Entry::new(
            PathBuf::from("/proc/self"),
            0,
            FileKind::Symlink
        ))
        .unwrap()
    );
    assert!(
        !proc
            .evaluate(&Entry::new(link, 0, FileKind::Symlink))
            .unwrap()
    ); // Resolving the path would say proc.
}

#[test]
fn size_rounds_each_entry_up_before_comparison() {
    let fixture = Fixture::new();
    let empty = fixture.file("empty", 0, 0);
    let one = fixture.file("one", 1, 0);
    let fifteen_hundred = fixture.file("fifteen-hundred", 1500, 0);
    let less_one_k = Test::size(b"-1k").unwrap();
    assert!(less_one_k.evaluate(&empty).unwrap());
    assert!(!less_one_k.evaluate(&one).unwrap()); // Discriminates byte comparison from GNU's rounded unit count.
    assert!(
        Test::size(b"2k")
            .unwrap()
            .evaluate(&fifteen_hundred)
            .unwrap()
    ); // 1500 bytes rounds up to 2 KiB.
    assert!(Test::size(b"0").unwrap().evaluate(&empty).unwrap());
    assert!(!Test::size(b"-1").unwrap().evaluate(&one).unwrap());
}

#[test]
fn minutes_round_up_and_days_round_down_on_the_same_fixed_clock() {
    let fixture = Fixture::new();
    let now = UNIX_EPOCH + Duration::from_secs(2_000_000_000);
    let thirty = fixture.file("thirty", 0, 30);
    let ninety = fixture.file("ninety", 0, 90);
    let hundred_fifty = fixture.file("one-fifty", 0, 150);
    assert!(
        Test::time(b"-mmin", b"1", now, false)
            .unwrap()
            .evaluate(&thirty)
            .unwrap()
    );
    assert!(
        Test::time(b"-mmin", b"2", now, false)
            .unwrap()
            .evaluate(&ninety)
            .unwrap()
    );
    assert!(
        Test::time(b"-mmin", b"+1", now, false)
            .unwrap()
            .evaluate(&ninety)
            .unwrap()
    );
    assert!(
        Test::time(b"-mmin", b"+1", now, false)
            .unwrap()
            .evaluate(&hundred_fifty)
            .unwrap()
    );
    assert!(
        !Test::time(b"-mmin", b"1", now, false)
            .unwrap()
            .evaluate(&ninety)
            .unwrap()
    );
    let twenty = fixture.file("twenty", 0, 20);
    let forty = fixture.file("forty", 0, 40);
    assert!(
        Test::time(b"-mmin", b"0.5", now, false)
            .unwrap()
            .evaluate(&twenty)
            .unwrap()
    );
    assert!(
        !Test::time(b"-mmin", b"0.5", now, false)
            .unwrap()
            .evaluate(&forty)
            .unwrap()
    );
    let day_and_three_quarters = fixture.file("day-and-three-quarters", 0, 86_400 + 64_800);
    let day_and_quarter = fixture.file("day-and-quarter", 0, 86_400 + 21_600);
    assert!(
        Test::time(b"-mtime", b"1.5", now, false)
            .unwrap()
            .evaluate(&day_and_three_quarters)
            .unwrap()
    );
    assert!(
        !Test::time(b"-mtime", b"1.5", now, false)
            .unwrap()
            .evaluate(&day_and_quarter)
            .unwrap()
    );
    assert!(
        Test::time(b"-mtime", b"0", now, false)
            .unwrap()
            .evaluate(&thirty)
            .unwrap()
    );
    let day_old = fixture.file("day-old", 0, 86_400);
    assert!(
        !Test::time(b"-mtime", b"0", now, false)
            .unwrap()
            .evaluate(&day_old)
            .unwrap()
    );
    assert!(
        !Test::time(b"-mtime", b"-1", now, false)
            .unwrap()
            .evaluate(&day_old)
            .unwrap()
    );
}

#[test]
fn daystart_measures_from_the_end_of_today_and_future_timestamps_remain_negative_ages() {
    let fixture = Fixture::new();
    let now = UNIX_EPOCH + Duration::from_secs(2_000_000_000);
    let midnight = 2_000_000_000u64 / 86_400 * 86_400;
    let path = fixture.0.join("last-night");
    fs::write(&path, b"").unwrap();
    fs::File::options()
        .write(true)
        .open(&path)
        .unwrap()
        .set_times(FileTimes::new().set_modified(UNIX_EPOCH + Duration::from_secs(midnight - 3600)))
        .unwrap();
    let entry = Entry::new(path, 0, FileKind::File);
    // 4.5 hours old by the clock, 25 hours old from the end of today.
    let matches = |age: &[u8], daystart| {
        Test::time(b"-mtime", age, now, daystart)
            .unwrap()
            .evaluate(&entry)
            .unwrap()
    };
    assert!(matches(b"0", false));
    assert!(!matches(b"0", true));
    assert!(matches(b"1", true));
    let future_path = fixture.0.join("future");
    fs::write(&future_path, b"").unwrap();
    fs::File::options()
        .write(true)
        .open(&future_path)
        .unwrap()
        .set_times(FileTimes::new().set_modified(now + Duration::from_secs(3600)))
        .unwrap();
    let future = Entry::new(future_path, 0, FileKind::File);
    assert!(
        Test::time(b"-mtime", b"-1", now, false)
            .unwrap()
            .evaluate(&future)
            .unwrap()
    );
    assert!(
        !Test::time(b"-mtime", b"0", now, false)
            .unwrap()
            .evaluate(&future)
            .unwrap()
    );
}

#[test]
fn permission_prefixes_and_zero_start_symbolic_modes_match_find_rules() {
    let fixture = Fixture::new();
    let path = fixture.0.join("mode");
    fs::write(&path, b"").unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o744)).unwrap();
    let entry = Entry::new(path, 0, FileKind::File);
    assert!(Test::perm(b"744").unwrap().evaluate(&entry).unwrap());
    assert!(Test::perm(b"-u+r").unwrap().evaluate(&entry).unwrap());
    assert!(Test::perm(b"/000").unwrap().evaluate(&entry).unwrap());
    assert!(Test::perm(b"-000").unwrap().evaluate(&entry).unwrap());
    assert!(Test::perm(b"-u+x").unwrap().evaluate(&entry).unwrap());
    assert!(!Test::perm(b"u+x").unwrap().evaluate(&entry).unwrap());
    let executable_dir = fixture.0.join("x-dir");
    fs::create_dir(&executable_dir).unwrap();
    fs::set_permissions(&executable_dir, fs::Permissions::from_mode(0o111)).unwrap();
    let entry = Entry::new(executable_dir, 0, FileKind::Directory);
    assert!(Test::perm(b"a+X").unwrap().evaluate(&entry).unwrap());
}

#[test]
fn newer_comparison_is_strict_and_preserves_nanoseconds() {
    let fixture = Fixture::new();
    let reference_path = fixture.0.join("reference");
    let equal_path = fixture.0.join("equal");
    let newer_path = fixture.0.join("newer");
    for path in [&reference_path, &equal_path, &newer_path] {
        fs::write(path, b"").unwrap();
    }
    let stamp = UNIX_EPOCH + Duration::from_secs(1_700_000_000) + Duration::from_nanos(123_456_789);
    for path in [&reference_path, &equal_path] {
        fs::File::options()
            .write(true)
            .open(path)
            .unwrap()
            .set_times(FileTimes::new().set_modified(stamp))
            .unwrap();
    }
    fs::File::options()
        .write(true)
        .open(&newer_path)
        .unwrap()
        .set_times(FileTimes::new().set_modified(stamp + Duration::from_nanos(1)))
        .unwrap();
    let reference = Test::reference(b"-newer", reference_path.clone(), false).unwrap();
    assert!(
        !reference
            .evaluate(&Entry::new(equal_path, 0, FileKind::File))
            .unwrap()
    );
    assert!(
        reference
            .evaluate(&Entry::new(newer_path.clone(), 0, FileKind::File))
            .unwrap()
    );
    let same = Test::reference(b"-samefile", newer_path.clone(), false).unwrap();
    assert!(
        same.evaluate(&Entry::new(newer_path, 0, FileKind::File))
            .unwrap()
    );
}

#[test]
fn reference_symlinks_are_followed_only_when_requested() {
    let fixture = Fixture::new();
    let target = fixture.0.join("target");
    let link = fixture.0.join("link");
    fs::write(&target, b"").unwrap();
    symlink(&target, &link).unwrap();
    let physical = Test::reference(b"-samefile", link.clone(), false).unwrap();
    let followed = Test::reference(b"-samefile", link, true).unwrap();
    assert!(
        !physical
            .evaluate(&Entry::new(target.clone(), 0, FileKind::File))
            .unwrap()
    );
    assert!(
        followed
            .evaluate(&Entry::new(target, 0, FileKind::File))
            .unwrap()
    );
}

#[test]
fn empty_and_access_tests_use_live_directory_reads_and_access_checks() {
    let fixture = Fixture::new();
    let empty_file = fixture.file("empty", 0, 0);
    assert!(Test::Empty.evaluate(&empty_file).unwrap());
    let directory = fixture.0.join("empty-dir");
    fs::create_dir(&directory).unwrap();
    let empty_dir = Entry::new(directory.clone(), 0, FileKind::Directory);
    assert!(Test::Empty.evaluate(&empty_dir).unwrap());
    fs::write(directory.join("child"), b"x").unwrap();
    assert!(!Test::Empty.evaluate(&empty_dir).unwrap());

    let denied = fixture.0.join("denied");
    fs::create_dir(&denied).unwrap();
    fs::set_permissions(&denied, fs::Permissions::from_mode(0o0)).unwrap();
    let denied_entry = Entry::new(denied.clone(), 0, FileKind::Directory);
    assert!(Test::Empty.evaluate(&denied_entry).is_err());
    assert!(
        !Test::Access(rustix::fs::Access::READ_OK)
            .evaluate(&denied_entry)
            .unwrap()
    );
    fs::set_permissions(&denied, fs::Permissions::from_mode(0o700)).unwrap();
}

#[test]
fn minute_less_than_compares_the_unrounded_age() {
    let fixture = Fixture::new();
    let now = UNIX_EPOCH + Duration::from_secs(2_000_000_000);
    let entry = fixture.file("almost-six", 0, 359);
    let matches = |age: &[u8]| {
        Test::time(b"-mmin", age, now, false)
            .unwrap()
            .evaluate(&entry)
            .unwrap()
    };
    // Bucket 6 by rounding up, yet younger than six minutes.
    assert!(matches(b"6"));
    assert!(matches(b"-6"));
    assert!(!matches(b"-5"));
    assert!(matches(b"+5"));
}

#[test]
fn fractional_days_greater_than_means_a_whole_day_past_the_value() {
    let fixture = Fixture::new();
    let now = UNIX_EPOCH + Duration::from_secs(2_000_000_000);
    let matches = |age_secs, arg: &[u8]| {
        let entry = fixture.file(&format!("age{age_secs}"), 0, age_secs);
        Test::time(b"-mtime", arg, now, false)
            .unwrap()
            .evaluate(&entry)
            .unwrap()
    };
    // GNU: 1.0 and 1.4999 days are not `-mtime +0.5`; 1.5001 days is.
    assert!(!matches(86_400, b"+0.5"));
    assert!(!matches(129_590, b"+0.5"));
    assert!(matches(129_610, b"+0.5"));
    assert!(matches(43_190, b"-0.5"));
    assert!(!matches(43_210, b"-0.5"));
}

#[test]
fn symbolic_perm_clauses_take_several_operators_and_a_leading_operator() {
    assert_eq!(parse_mode(b"+u+x"), Some((0o111, 0)));
    assert_eq!(parse_mode(b"u+x+w"), Some((0o300, 0)));
    assert_eq!(parse_mode(b"u+"), Some((0, 0)));
    assert_eq!(parse_mode(b"u=rwx,g=rx,o=x"), Some((0o751, 0)));
    assert_eq!(parse_mode(b"+066"), None);
    assert_eq!(parse_mode(b"z+x"), None);
}

#[test]
fn cnewer_compares_the_entry_ctime_with_the_reference_mtime() {
    let fixture = Fixture::new();
    // Setting an old mtime moves ctime to now, so the reference is
    // -cnewer than itself and not -newer than itself.
    let path = fixture.0.join("reference");
    fs::write(&path, b"").unwrap();
    fs::File::options()
        .write(true)
        .open(&path)
        .unwrap()
        .set_times(FileTimes::new().set_modified(UNIX_EPOCH + Duration::from_secs(1_000_000)))
        .unwrap();
    let reference = Entry::new(path, 0, FileKind::File);
    let cnewer = Test::reference(b"-cnewer", reference.path().to_owned(), false).unwrap();
    let newer = Test::reference(b"-newer", reference.path().to_owned(), false).unwrap();
    assert!(cnewer.evaluate(&reference).unwrap());
    assert!(!newer.evaluate(&reference).unwrap());
}

#[test]
fn numeric_identities_need_no_database_entry_but_names_do() {
    assert_eq!(identity(b"-user", OsStr::new("3999999")), Some(3_999_999));
    assert_eq!(identity(b"-group", OsStr::new(" 42")), Some(42));
    assert_eq!(identity(b"-user", OsStr::new("ferret-no-such-user")), None);
}

#[test]
fn all_distinct_corpus_newermt_values_parse() {
    let now = UNIX_EPOCH + Duration::from_secs(1_800_000_000);
    let values = [
        "-1 day",
        "-1 hour",
        "-10 min",
        "-10 minutes",
        "-100 seconds",
        "-12 hours",
        "-120 seconds",
        "-1200 seconds",
        "-14 hours",
        "-15 minutes",
        "-15 seconds",
        "-2 hours",
        "-2 minutes",
        "-20 minutes",
        "-20 seconds",
        "-3 hours",
        "-30 minutes",
        "-300 seconds",
        "-4 minutes",
        "-40 minutes",
        "-45 minutes",
        "-5 minutes",
        "-6 hours",
        "-6 seconds",
        "-60 seconds",
        "1 hour ago",
        "1 minute ago",
        "1 month ago",
        "1 second ago",
        "15 seconds ago",
        "1970-01-01",
        "1971-01-01",
        "2001-01-01",
        "2008-01-01",
        "2010-01-01",
        "2010-06-01",
        "2013-03-01",
        "2020-01-01",
        "2020-01-02",
        "2020-03-03",
        "2020-09-01",
        "2021-02-15 00:00:00",
        "2021-02-16 00:00:00",
        "2022-01-01 00:01",
        "2022-01-31 23:59",
        "2022-10-17T18:00-07:00",
        "2023-06-26 07:27:07",
        "2024-01-01",
        "2024-12-13",
        "2025-01-01",
        "2025-09-01",
        "2025-11-16",
        "2025-11-23",
        "2025-11-26",
        "2026-06-22",
        "2026-07-01",
        "2026-08-26",
        "24 hours ago",
        "3 day ago",
        "30 minutes ago",
        "5 minutes ago",
        "5 seconds ago",
        "6 minutes ago",
        "60 days ago",
        "7 day ago",
        "7 days ago",
        "7 hours ago",
        "@0",
        "@86400",
        "Oct 3 00:00",
        "Oct 4 00:00",
        "mar 03, 2010",
        "mar 03, 2010 09:00",
        "mar 11, 2010",
        "mar 12, 2021 18:50",
        "now",
        "now-1 days",
        "now-7 days",
        "today",
        "yesterday",
    ];
    for value in values {
        assert!(parse_date(OsStr::new(value), now).is_some(), "{value}");
    }
    assert_eq!(values.len(), 80);
    for value in ["nonsense", "2022-13-45"] {
        assert!(parse_date(OsStr::new(value), now).is_none(), "{value}");
    }
}
