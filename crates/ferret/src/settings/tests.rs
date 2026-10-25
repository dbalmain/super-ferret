#![allow(clippy::unwrap_used)]

use std::path::PathBuf;

use super::*;

/// A fresh directory for one test, removed first if a past run left it.
fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("ferret-settings-{}-{name}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
}

/// A fixture `/sys/devices/system/cpu`: one `cpuN` per entry, holding its
/// `core_cpus_list` and, when given, its `online` flag.
fn cpus(name: &str, cpus: &[(usize, &str, Option<&str>)]) -> PathBuf {
    let dir = scratch(name);
    for &(n, list, online) in cpus {
        let cpu = dir.join(format!("cpu{n}"));
        fs::create_dir_all(cpu.join("topology")).unwrap();
        fs::write(cpu.join("topology/core_cpus_list"), format!("{list}\n")).unwrap();
        if let Some(online) = online {
            fs::write(cpu.join("online"), format!("{online}\n")).unwrap();
        }
    }
    dir
}

/// Eight logical CPUs as four cores of two hardware threads (`0,4`, …),
/// laid out as the kernel does on a 16-core, 32-thread part, count four.
/// `cpu0` has no `online` file, as the boot CPU doesn't. The non-CPU
/// entries beside them are skipped.
#[test]
fn hardware_thread_siblings_count_as_one_core() {
    let siblings: Vec<_> = (0..8)
        .map(|n| {
            (
                n,
                ["0,4", "1,5", "2,6", "3,7"][n % 4],
                (n > 0).then_some("1"),
            )
        })
        .collect();
    let dir = cpus("siblings", &siblings);
    fs::create_dir_all(dir.join("cpufreq")).unwrap();
    fs::create_dir_all(dir.join("cpuidle")).unwrap();
    fs::write(dir.join("online"), "0-7\n").unwrap();
    assert_eq!(physical_cores(&dir), Some(4));
}

/// An offline CPU is not counted, even with a core list of its own.
#[test]
fn offline_cpus_are_excluded() {
    let dir = cpus(
        "offline",
        &[
            (0, "0", None),
            (1, "1", Some("1")),
            (2, "2", Some("0")),
            (3, "3", Some("0")),
        ],
    );
    assert_eq!(physical_cores(&dir), Some(2));
}

/// No sysfs, no CPUs, or an online CPU without a topology: no count, not a
/// partial one.
#[test]
fn unreadable_topology_counts_nothing() {
    assert_eq!(physical_cores(&scratch("absent").join("cpu")), None);
    assert_eq!(physical_cores(&scratch("empty")), None);
    let dir = cpus("partial", &[(0, "0", None), (1, "1", Some("1"))]);
    fs::remove_file(dir.join("cpu1/topology/core_cpus_list")).unwrap();
    assert_eq!(physical_cores(&dir), None);
}

#[test]
fn rendered_settings_parse_back() {
    for settings in [
        Settings {
            search_threads: Some(24),
        },
        Settings::default(),
    ] {
        assert_eq!(Settings::parse(&settings.render()).unwrap(), settings);
    }
}

/// A hand edit with spacing and comments is read as written.
#[test]
fn hand_edited_settings_parse() {
    let text = "  # tuned by hand\n\nversion=1\n   search_threads   =   12  \n";
    assert_eq!(Settings::parse(text).unwrap().search_threads, Some(12));
}

#[test]
fn invalid_settings_are_rejected() {
    for (text, reason) in [
        ("", "no `version` line"),
        ("search_threads = 8\n", "no `version` line"),
        ("version = 2\n", "line 1: unsupported version `2`"),
        (
            "version = 1\nsearch_threads = 0\n",
            "line 2: search_threads must be",
        ),
        (
            "version = 1\nsearch_threads = many\n",
            "line 2: search_threads must be",
        ),
        (
            "version = 1\nserch_threads = 8\n",
            "line 2: unknown setting `serch_threads`",
        ),
        (
            "version = 1\nversion = 1\n",
            "line 2: `version` is set twice",
        ),
        (
            "version = 1\n\u{0}garbage\n",
            "line 2: expected `key = value`",
        ),
    ] {
        let error = Settings::parse(text).unwrap_err().to_string();
        assert!(error.starts_with(reason), "{text:?}: {error}");
    }
}

#[test]
fn reading_a_missing_file_is_none_and_bytes_that_are_not_utf8_fail() {
    let index = scratch("read");
    assert!(Settings::read(&index).unwrap().is_none());
    fs::write(index.join(FILE), b"version = 1\n\xff\n").unwrap();
    assert!(matches!(
        Settings::read(&index),
        Err(ReadError::Parse { line: 0, .. })
    ));
}

/// A FIFO named `settings`, which once blocked a search opening it, is an
/// unusable file, read on a thread with a deadline so a regression fails.
#[test]
fn a_fifo_settings_file_is_an_error_not_a_hang() {
    let index = scratch("fifo");
    let made = std::process::Command::new("mkfifo")
        .arg(index.join(FILE))
        .status()
        .unwrap();
    assert!(made.success());
    let (done, finished) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = done.send(Settings::read(&index).map_err(|e| e.to_string()));
    });
    let read = finished
        .recv_timeout(std::time::Duration::from_secs(10))
        .expect("reading a FIFO blocked");
    assert_eq!(read, Err("not a regular file".to_owned()));
}

/// The first write creates the file and leaves no temporary behind; a
/// second keeps whatever is there, such as a hand edit.
#[test]
fn write_new_creates_once_and_keeps_an_existing_file() {
    let index = scratch("write");
    let detected = Settings {
        search_threads: Some(16),
    };
    assert!(detected.write_new(&index).unwrap());
    assert_eq!(Settings::read(&index).unwrap(), Some(detected));

    fs::write(index.join(FILE), "version = 1\nsearch_threads = 6\n").unwrap();
    assert!(!Settings::default().write_new(&index).unwrap());
    assert_eq!(
        Settings::read(&index).unwrap().unwrap().search_threads,
        Some(6)
    );
    let names: Vec<_> = fs::read_dir(&index)
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect();
    assert_eq!(names, [FILE]);
}
