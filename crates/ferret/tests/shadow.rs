//! The stand-in `rg` and its shadow comparison (D118, step 1), run as the
//! checked-in scripts against the pinned rg. Cases sit where a misread argv
//! would change the verdict or let the shadow write into the caller's search;
//! a fake ferret supplies the wrong answer the comparison must catch.
#![allow(clippy::unwrap_used)]

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

const FERRET: &str = env!("CARGO_BIN_EXE_ferret");

fn script(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../contrib/agents")
        .join(name)
}

fn rg() -> PathBuf {
    std::env::var_os("FERRET_RG")
        .unwrap_or_else(|| panic!("nix develop supplies the pinned FERRET_RG oracle"))
        .into()
}

/// A private home, a tree to search, and the shadow's state and scratch
/// directories, inside the tree when `inside`.
struct Bed {
    base: PathBuf,
    tree: PathBuf,
    state: PathBuf,
    runtime: PathBuf,
}

impl Bed {
    fn new(name: &str, inside: bool) -> Self {
        let base =
            std::env::temp_dir().join(format!("ferret-shadow-test-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        let tree = base.join("tree");
        let own = if inside { &tree } else { &base };
        let (state, runtime) = (own.join("shadow"), own.join("runtime"));
        for dir in [&tree, &base.join("home"), &runtime] {
            fs::create_dir_all(dir).unwrap();
        }
        fs::set_permissions(&runtime, fs::Permissions::from_mode(0o700)).unwrap();
        Self {
            base,
            tree,
            state,
            runtime,
        }
    }

    fn write(&self, path: &str, bytes: &[u8]) {
        let path = self.tree.join(path);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, bytes).unwrap();
    }

    /// A ferret that answers `grep ARGS` with rg's output through `filter`.
    fn fake(&self, name: &str, filter: &str) -> PathBuf {
        let path = self.base.join(name);
        let body = format!("#!/bin/sh\nshift\n{:?} \"$@\" | {filter}\n", rg());
        fs::write(&path, body).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    fn command(&self, program: &Path, ferret: &Path) -> Command {
        let home = self.base.join("home");
        let mut command = Command::new("timeout");
        command
            .arg("60")
            .arg(program)
            .current_dir(&self.tree)
            .env_remove("RIPGREP_CONFIG_PATH")
            .env_remove("FERRET_SHADOW_STDIN")
            .env("HOME", &home)
            .env("XDG_CONFIG_HOME", home.join("config"))
            .env("XDG_DATA_HOME", home.join("data"))
            .env("XDG_STATE_HOME", home.join("state"))
            .env("XDG_RUNTIME_DIR", &self.runtime)
            .env("FERRET_INDEX", self.base.join("index"))
            .env("FERRET_NO_DAEMON", "1")
            .env("FERRET_BIN", ferret)
            .env("FERRET_SHADOW_DIR", &self.state)
            .stdin(Stdio::null());
        command
    }

    /// Runs the shadow directly and returns the verdict it logged, if any.
    fn shadow(&self, ferret: &Path, args: &[&str], env: &[(&str, &str)]) -> Option<String> {
        let before = self.log().len();
        let output = self
            .command(&script("ferret-shadow"), ferret)
            .arg(rg())
            .args(args)
            .envs(env.iter().copied())
            .output()
            .unwrap();
        assert!(output.status.success(), "{args:?}: {output:?}");
        let log = self.log();
        assert!(log.len() <= before + 1, "{args:?}: {log:?}");
        log.get(before)
            .map(|line| line.split('\t').nth(1).unwrap().to_owned())
    }

    fn log(&self) -> Vec<String> {
        fs::read_to_string(self.state.join("log"))
            .map(|log| log.lines().map(str::to_owned).collect())
            .unwrap_or_default()
    }
}

impl Drop for Bed {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.base);
    }
}

fn verdict(bed: &Bed, ferret: &Path, args: &[&str]) -> String {
    bed.shadow(ferret, args, &[])
        .unwrap_or_else(|| panic!("{args:?}: nothing logged"))
}

#[test]
fn agreement_needs_the_same_records() {
    let bed = Bed::new("agree", false);
    bed.write("a.txt", b"foo\n");
    bed.write("sub/b.txt", b"foo bar\n");
    let ferret = Path::new(FERRET);
    for args in [
        &["foo", "."][..],
        &["-l", "-0", "foo", "."],
        &["-c", "-l", "foo", "."],
        &["--files", "."],
    ] {
        assert_eq!(verdict(&bed, ferret, args), "agree", "{args:?}");
    }
    let wrong = bed.fake("wrong", "sed s/bar/baz/");
    assert_eq!(verdict(&bed, &wrong, &["foo", "."]), "differ");
    assert_eq!(fs::read_dir(bed.state.join("mismatch")).unwrap().count(), 1);
}

#[test]
fn json_normalising_follows_the_final_mode() {
    // Elapsed times are dropped only from real JSON. A plain line shaped like
    // rg's end message keeps them, so a changed value must still differ.
    let bed = Bed::new("json", false);
    bed.write(
        "input",
        b"{\"type\":\"end\",\"text\":\"--json\",\"elapsed\":{\"value\":1}}\n",
    );
    let wrong = bed.fake("wrong", "sed 's/\"value\":1/\"value\":2/'");
    for args in [
        &["--json", "--no-json", "type", "input"][..],
        &["-e", "--json", "input"],
        &["-e--json", "input"],
        &["--regexp", "--json", "input"],
    ] {
        assert_eq!(verdict(&bed, &wrong, args), "differ", "{args:?}");
    }
}

#[test]
fn nul_listings_compare_whole_records() {
    let bed = Bed::new("nul", false);
    bed.write("a.txt", b"foo\n");
    bed.write(".git/HEAD", b"foo\n");
    bed.write("x\n.git", b"foo\n");
    // Dropping only .git is the index's known difference; dropping a file
    // whose name merely contains a line ending in .git is not.
    let nogit = bed.fake("nogit", "grep -zv '^\\./\\.git/'");
    let greedy = bed.fake("greedy", "grep -zv 'git'");
    let args = ["--files", "-0", "--hidden", "."];
    assert_eq!(verdict(&bed, &nogit, &args), "known:git");
    assert_eq!(verdict(&bed, &greedy, &args), "differ");
}

#[test]
fn commands_reading_stdin_or_config_are_skipped() {
    let bed = Bed::new("skips", false);
    bed.write("a.txt", b"foo\n");
    let ferret = Path::new(FERRET);
    for args in [
        &["-nf-", "."][..],
        &["-f", "-", "."],
        &["--file=-", "."],
        &["-e", "foo", "--", "-"],
    ] {
        assert_eq!(verdict(&bed, ferret, args), "skipped:stdin", "{args:?}");
    }
    // `-` in the pattern's place is a pattern.
    assert_eq!(verdict(&bed, ferret, &["--", "-", "."]), "agree");
    // With no path rg reads a piped stdin, unless it only lists files.
    let piped = [("FERRET_SHADOW_STDIN", "1")];
    assert_eq!(
        bed.shadow(ferret, &["foo"], &piped).unwrap(),
        "skipped:stdin"
    );
    assert_eq!(bed.shadow(ferret, &["--files"], &piped).unwrap(), "agree");
    let config = [("RIPGREP_CONFIG_PATH", "/dev/null")];
    assert_eq!(
        bed.shadow(ferret, &["foo", "."], &config).unwrap(),
        "skipped:config"
    );
    assert_eq!(
        verdict(&bed, ferret, &["--pre", "cat", "foo", "."]),
        "skipped:effects"
    );
}

#[test]
fn searches_covering_the_shadow_write_nothing() {
    let bed = Bed::new("inside", true);
    bed.write("input", b"hit\n");
    bed.write("sub/b.txt", b"hit\n");
    let ferret = Path::new(FERRET);
    // `-c` overrides `--files`, so `hit` is the pattern and `.` the root.
    for args in [&["hit"][..], &["hit", "."], &["--files", "-c", "hit"]] {
        assert_eq!(bed.shadow(ferret, args, &[]), None, "{args:?}");
        assert!(!bed.state.exists(), "{args:?}");
        assert_eq!(fs::read_dir(&bed.runtime).unwrap().count(), 0, "{args:?}");
    }
    assert_eq!(verdict(&bed, ferret, &["hit", "sub"]), "agree");
}

#[test]
fn a_fifo_in_place_of_the_lock_is_refused() {
    let bed = Bed::new("fifo", false);
    bed.write("a.txt", b"foo\n");
    fs::create_dir_all(&bed.state).unwrap();
    let status = Command::new("mkfifo")
        .arg(bed.state.join("lock"))
        .status()
        .unwrap();
    assert!(status.success());
    assert_eq!(bed.shadow(Path::new(FERRET), &["foo", "."], &[]), None);
}

#[test]
fn the_stand_in_answers_as_rg_and_logs_in_the_background() {
    let bed = Bed::new("stand-in", false);
    bed.write("a.txt", b"foo\n");
    bed.write("b.txt", b"no\n");
    let direct = |program: &Path| -> Output {
        let mut command = bed.command(program, Path::new(FERRET));
        command
            .env("FERRET_REAL_RG", rg())
            .args(["-n", "foo", "."])
            .output()
            .unwrap()
    };
    let expected = direct(&rg());
    let actual = direct(&script("rg"));
    assert_eq!(actual.status.code(), expected.status.code());
    assert_eq!(actual.stdout, expected.stdout);
    assert_eq!(actual.stderr, expected.stderr);
    let deadline = Instant::now() + Duration::from_secs(30);
    while bed.log().is_empty() {
        assert!(Instant::now() < deadline, "the shadow logged nothing");
        std::thread::sleep(Duration::from_millis(50));
    }
    // The lock is released once the shadow exits; wait for that too, so the
    // tree is not removed under it.
    while bed.state.join("lock").exists()
        && Command::new("flock")
            .args(["-n"])
            .arg(bed.state.join("lock"))
            .arg("true")
            .status()
            .is_ok_and(|status| !status.success())
    {
        assert!(Instant::now() < deadline, "the shadow kept its lock");
        std::thread::sleep(Duration::from_millis(50));
    }
    assert_eq!(bed.log()[0].split('\t').nth(1), Some("agree"));
}

// Step 2: FERRET_ANSWER=ferret hands the caller to `ferret grep`, naming the
// real rg as its fallback. The fake ferret shows what it was given.
#[test]
fn answer_mode_hands_the_caller_to_ferret_with_rg_as_fallback() {
    let bed = Bed::new("answer", false);
    let fake = bed.base.join("echo-ferret");
    fs::write(
        &fake,
        b"#!/bin/sh\nprintf '%s|' \"$FERRET_GREP_FALLBACK\" \"$@\"; echo\n",
    )
    .unwrap();
    fs::set_permissions(&fake, fs::Permissions::from_mode(0o755)).unwrap();
    let call = |answer: &str| {
        bed.command(&script("rg"), &fake)
            .env("FERRET_REAL_RG", rg())
            .env("FERRET_ANSWER", answer)
            .env("FERRET_SHADOW", "0")
            .args(["-l", "foo", "."])
            .output()
            .unwrap()
    };
    let output = call("ferret");
    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        format!("{}|grep|-l|foo|.|\n", rg().display())
    );
    // Any other value keeps rg answering.
    bed.write("a.txt", b"foo\n");
    let output = call("rg");
    assert_eq!(output.stdout, b"./a.txt\n");
}

#[test]
fn heading_output_is_not_compared_line_by_line() {
    // Sorted lines lose which heading a line sat under, so bodies swapped
    // between files would read as agreement.
    let bed = Bed::new("heading", false);
    bed.write("a", b"red\n");
    bed.write("b", b"blue\n");
    let swapped = bed.fake("swapped", "sed 's/red/@/; s/blue/red/; s/@/blue/'");
    for args in [
        &["--heading", "red|blue", "a", "b"][..],
        &["-p", "red|blue", "a", "b"],
        &["--pretty", "--color=never", "red|blue", "a", "b"],
    ] {
        assert_eq!(verdict(&bed, &swapped, args), "skipped:heading", "{args:?}");
    }
    // A later --no-heading, or a mode without lines, is compared.
    for args in [
        &["--heading", "--no-heading", "red|blue", "a", "b"][..],
        &["--heading", "-c", "red|blue", "a", "b"],
    ] {
        assert_ne!(verdict(&bed, &swapped, args), "skipped:heading", "{args:?}");
    }
}

#[test]
fn a_named_fifo_is_left_to_the_caller() {
    let bed = Bed::new("named-fifo", false);
    let fifo = bed.tree.join("fifo");
    mkfifo(&fifo);
    // Checked before anything opens it: a replay would block here, or with a
    // writer, take part of the caller's input.
    let ferret = Path::new(FERRET);
    for args in [
        &["hit", "fifo"][..],
        &["-f", "fifo", "."],
        &["--ignore-file=fifo", "x", "."],
    ] {
        assert_eq!(verdict(&bed, ferret, args), "skipped:special", "{args:?}");
    }
    // Through the stand-in, the caller reads the whole stream.
    let child = bed
        .command(&script("rg"), ferret)
        .env("FERRET_REAL_RG", rg())
        .arg("-c")
        .arg("hit")
        .arg(&fifo)
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut writer = fs::OpenOptions::new().write(true).open(&fifo).unwrap();
    for _ in 0..100 {
        std::io::Write::write_all(&mut writer, &b"hit\n".repeat(100)).unwrap();
        std::thread::sleep(Duration::from_millis(1));
    }
    drop(writer);
    let output = child.wait_with_output().unwrap();
    assert_eq!(output.stdout, b"10000\n", "{output:?}");
    let deadline = Instant::now() + Duration::from_secs(30);
    while bed.log().len() < 4 {
        assert!(Instant::now() < deadline, "the shadow logged nothing");
        std::thread::sleep(Duration::from_millis(50));
    }
    assert_eq!(bed.log()[3].split('\t').nth(1), Some("skipped:special"));
}

fn mkfifo(path: &Path) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    assert!(Command::new("mkfifo").arg(path).status().unwrap().success());
}

#[test]
fn any_reachable_special_file_skips_the_comparison() {
    // rg opens ignore files by name, in and above the roots and through
    // symlinks its walk would not follow, so the shadow refuses when the roots
    // reach anything that is not a file or a directory.
    use std::os::unix::fs::symlink;
    let ferret = Path::new(FERRET);
    let bed = |name: &str| {
        let bed = Bed::new(name, false);
        bed.write("sub/x", b"hit\n");
        bed
    };
    let skipped = |bed: &Bed, args: &[&str]| {
        assert_eq!(verdict(bed, ferret, args), "skipped:special", "{args:?}")
    };

    let plain = bed("reach-plain");
    mkfifo(&plain.tree.join("sub/.ignore"));
    skipped(&plain, &["hit", "."]);
    fs::remove_file(plain.tree.join("sub/.ignore")).unwrap();
    assert_eq!(verdict(&plain, ferret, &["hit", "."]), "agree");
    // A broken symlink opens nothing.
    symlink("missing", plain.tree.join("sub/broken")).unwrap();
    assert_eq!(verdict(&plain, ferret, &["hit", "."]), "agree");

    let above = bed("reach-above");
    mkfifo(&above.tree.join(".gitignore"));
    skipped(&above, &["hit", "sub"]);

    let linked = bed("reach-linked");
    let outside = linked.base.join("outside");
    mkfifo(&outside.join(".ignore"));
    symlink(&outside, linked.tree.join("sub/link")).unwrap();
    skipped(&linked, &["hit", "."]);
    symlink(&outside, linked.tree.join("root")).unwrap();
    skipped(&linked, &["hit", "root"]);

    let gitlink = bed("reach-gitlink");
    let gitdir = gitlink.base.join("gitdir");
    mkfifo(&gitdir.join("info/exclude"));
    symlink(&gitdir, gitlink.tree.join(".git")).unwrap();
    skipped(&gitlink, &["hit", "."]);

    let gitfile = bed("reach-gitfile");
    let gitdir = gitfile.base.join("worktree");
    fs::create_dir_all(&gitdir).unwrap();
    fs::write(gitdir.join("commondir"), "../common\n").unwrap();
    mkfifo(&gitfile.base.join("common/info/exclude"));
    let pointer = format!("gitdir: {}\n", gitdir.display());
    gitfile.write("sub/.git", pointer.as_bytes());
    skipped(&gitfile, &["hit", "."]);
    skipped(&gitfile, &["hit", "sub"]);

    let excludes = bed("reach-excludes");
    let file = excludes.base.join("excludes");
    mkfifo(&file);
    let config = format!("[core]\n\texcludesFile = {}\n", file.display());
    fs::write(excludes.base.join("home/.gitconfig"), config).unwrap();
    skipped(&excludes, &["hit", "."]);

    // The check itself reads no config that is not a regular file, and
    // follows includes.
    let config = bed("reach-config");
    let gitconfig = config.base.join("home/.gitconfig");
    mkfifo(&gitconfig);
    skipped(&config, &["hit", "."]);
    fs::remove_file(&gitconfig).unwrap();
    let included = config.base.join("included");
    fs::write(&gitconfig, "[include]\n\tpath = ../included\n").unwrap();
    mkfifo(&included);
    skipped(&config, &["hit", "."]);
    fs::remove_file(&included).unwrap();
    let file = config.base.join("excludes");
    mkfifo(&file);
    let excluding = format!("[core]\n\texcludesFile = \"{}\"\n", file.display());
    fs::write(&included, excluding).unwrap();
    skipped(&config, &["hit", "."]);
    fs::remove_file(&file).unwrap();
    // An ordinary repository, and config naming ordinary files, compare.
    fs::create_dir_all(config.tree.join(".git/info")).unwrap();
    fs::write(config.tree.join(".git/config"), "[core]\n\tbare = false\n").unwrap();
    fs::write(config.tree.join(".git/info/exclude"), "").unwrap();
    assert_eq!(verdict(&config, ferret, &["hit", "."]), "agree");
    assert_eq!(verdict(&config, ferret, &["hit", "sub"]), "agree");
    fs::remove_file(config.tree.join(".git/config")).unwrap();
    mkfifo(&config.tree.join(".git/config"));
    skipped(&config, &["hit", "sub"]);

    let looped = bed("reach-loop");
    symlink("..", looped.tree.join("sub/up")).unwrap();
    skipped(&looped, &["hit", "."]);
}

/// Runs ferret-shadow-report over the bed's log.
fn report(bed: &Bed) -> Output {
    bed.command(&script("ferret-shadow-report"), Path::new(FERRET))
        .output()
        .unwrap()
}

#[test]
fn the_report_lists_each_mismatch_shape_once_with_its_kept_run() {
    let bed = Bed::new("report", false);
    bed.write("a.txt", b"foo bar\nquux bar\n-foo bar\n-quux bar\n");
    let ferret = Path::new(FERRET);
    assert_eq!(report(&bed).status.code(), Some(2), "no log yet");

    assert_eq!(verdict(&bed, ferret, &["-l", "foo", "."]), "agree");
    let clean = report(&bed);
    assert_eq!(clean.status.code(), Some(0), "{clean:?}");
    assert!(String::from_utf8_lossy(&clean.stdout).contains("nothing needs a look"));

    let wrong = bed.fake("wrong", "sed s/bar/baz/");
    for pattern in ["foo", "quux"] {
        assert_eq!(
            verdict(&bed, &wrong, &["-g", "*.txt", pattern, "."]),
            "differ"
        );
    }
    let out = report(&bed);
    assert_eq!(out.status.code(), Some(1), "{out:?}");
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("2 x differ\n  shape:  -g X X X\n"), "{text}");
    assert!(
        text.contains("rg -g \\*.txt quux . "),
        "newest call replays: {text}"
    );
    assert_eq!(
        text.matches("/mismatch/").count(),
        2,
        "both kept runs: {text}"
    );

    // A repeated argv keeps every run it left; attached values and operands
    // after `--` that look like options are not part of the shape.
    assert_eq!(
        verdict(&bed, &wrong, &["-g", "*.txt", "foo", "."]),
        "differ"
    );
    for args in [
        ["-efoo", "."].as_slice(),
        &["-equux", "."],
        &["--", "-foo", "."],
        &["--", "-quux", "."],
    ] {
        assert_eq!(verdict(&bed, &wrong, args), "differ", "{args:?}");
    }
    let out = report(&bed);
    let text = String::from_utf8_lossy(&out.stdout);
    let group = |shape: &str| {
        let start = text
            .find(&format!("shape:  {shape}\n"))
            .unwrap_or_else(|| panic!("no {shape:?} group: {text}"));
        let rest = &text[start..];
        rest[..rest.find("\n\n").unwrap_or(rest.len())].to_owned()
    };
    assert!(text.contains("3 x differ\n  shape:  -g X X X\n"), "{text}");
    assert_eq!(group("-g X X X").matches("/mismatch/").count(), 3, "{text}");
    assert!(text.contains("2 x differ\n  shape:  -eX X\n"), "{text}");
    assert!(text.contains("2 x differ\n  shape:  -- X X\n"), "{text}");
}
