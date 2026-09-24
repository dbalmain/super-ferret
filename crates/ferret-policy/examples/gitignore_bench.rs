//! Machine-specific D16 benchmark. Run one process at a time with:
//! `cargo run -p ferret-policy --release --example gitignore_bench`.

// The example compiles the private matcher as a sibling so production does not
// expose a benchmark-only API. Re-inclusion-only methods are unused here.
#[allow(dead_code)]
#[path = "../src/gitignore/mod.rs"]
mod gitignore;

use std::fmt::Debug;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use ferret_policy::{Config, Decision, DirRules, Entry, IgnoreFiles};
use gitignore::{Gitignore, Match};

const ROOT: &str = "/home/dave/w";
const TYPICAL: &str = "/home/dave/w/aic-edit/.gitignore";
const LARGEST: &str = "/home/dave/w/intpack-bench/data/corpus-src/cpython/.gitignore";
const GLOBAL: &str = "*.tmp\n*.bak\n.cache/\n";
const MIN_TIMED_PATHS: usize = 500_000;
const DEFAULTS: &str = "\
.git/
node_modules/
target/
.venv/
__pycache__/
.cache/
dist/
build/
.next/
vendor/
.direnv/
result
";

fn main() {
    let inputs = [
        RuleSet::new("built-in defaults", DEFAULTS, Path::new(ROOT), 500_000),
        RuleSet::from_file("typical: aic-edit", Path::new(TYPICAL), 500_000),
        RuleSet::from_file("largest: CPython", Path::new(LARGEST), 500_000),
    ];

    for input in &inputs {
        let (ours, our_errors) = Gitignore::compile(&input.patterns);
        let (theirs, ignore_errors) = compile_ignore(&input.patterns);
        assert_matchers_agree(input, &ours, &theirs);
        let repetitions = MIN_TIMED_PATHS.div_ceil(input.paths.len());
        let visits = repetitions * input.paths.len();
        let our_time = best_of_three(|| time_ours(&ours, &input.paths, repetitions));
        let ignore_time = best_of_three(|| time_ignore(&theirs, &input.paths, repetitions));
        println!(
            "match\t{}\tpaths={}\tlines={}\terrors={}/{}\tours={:.2} ns/path\tignore={:.2} ns/path",
            input.label,
            input.paths.len(),
            input.patterns.lines().count(),
            our_errors.len(),
            ignore_errors,
            nanos_per_path(our_time, visits),
            nanos_per_path(ignore_time, visits)
        );
    }

    let largest = &inputs[2].patterns;
    let builds = 200;
    let start = Instant::now();
    for _ in 0..builds {
        std::hint::black_box(Gitignore::compile(largest));
    }
    let our_build = start.elapsed().as_nanos() as f64 / f64::from(builds);
    let start = Instant::now();
    for _ in 0..builds {
        std::hint::black_box(compile_ignore(largest));
    }
    let ignore_build = start.elapsed().as_nanos() as f64 / f64::from(builds);
    println!(
        "build\tlargest: CPython\tours={our_build:.0} ns/build\tignore={ignore_build:.0} ns/build"
    );

    let typical = &inputs[1];
    let (chain, errors) = DirRules::root(
        &typical.base,
        Some(GLOBAL),
        IgnoreFiles {
            git_root: true,
            gitignore: Some(&typical.patterns),
            ..IgnoreFiles::default()
        },
        Config::default(),
    );
    assert!(errors.is_empty(), "policy chain errors: {errors:?}");
    let repetitions = MIN_TIMED_PATHS.div_ceil(typical.paths.len());
    let chain_time = best_of_three(|| time_policy(&chain, &typical.paths, repetitions));
    println!(
        "policy\tbuilt-ins + global + aic-edit\tpaths={}\tours={:.2} ns/path",
        typical.paths.len(),
        nanos_per_path(chain_time, repetitions * typical.paths.len())
    );
}

struct RuleSet {
    label: &'static str,
    patterns: String,
    base: PathBuf,
    paths: Vec<(PathBuf, bool)>,
}

impl RuleSet {
    fn new(label: &'static str, patterns: &str, base: &Path, limit: usize) -> Self {
        Self {
            label,
            patterns: patterns.to_owned(),
            base: base.to_owned(),
            paths: collect_paths(base, limit),
        }
    }

    fn from_file(label: &'static str, file: &Path, limit: usize) -> Self {
        let base = file
            .parent()
            .unwrap_or_else(|| panic!("ignore file has no parent: {file:?}"));
        Self::new(label, &read_lossy(file), base, limit)
    }
}

fn compile_ignore(text: &str) -> (ignore::gitignore::Gitignore, usize) {
    let mut builder = ignore::gitignore::GitignoreBuilder::new(".");
    let mut errors = 0;
    for line in text.lines() {
        if builder.add_line(None, line).is_err() {
            errors += 1;
        }
    }
    (must(builder.build()), errors)
}

fn assert_matchers_agree(input: &RuleSet, ours: &Gitignore, theirs: &ignore::gitignore::Gitignore) {
    for (path, is_dir) in &input.paths {
        let our_match = ours.matched(path, *is_dir);
        let ignore_match = match theirs.matched(path, *is_dir) {
            ignore::Match::None => Match::None,
            ignore::Match::Ignore(_) => Match::Ignore,
            ignore::Match::Whitelist(_) => Match::Whitelist,
        };
        assert_eq!(
            our_match, ignore_match,
            "{} mismatch for {path:?}, is_dir={is_dir}",
            input.label
        );
    }
}

fn time_ours(matcher: &Gitignore, paths: &[(PathBuf, bool)], repetitions: usize) -> Duration {
    let start = Instant::now();
    let mut matches = 0;
    for _ in 0..repetitions {
        for (path, is_dir) in paths {
            matches += usize::from(matcher.matched(path, *is_dir) != Match::None);
        }
    }
    std::hint::black_box(matches);
    start.elapsed()
}

fn time_ignore(
    matcher: &ignore::gitignore::Gitignore,
    paths: &[(PathBuf, bool)],
    repetitions: usize,
) -> Duration {
    let start = Instant::now();
    let mut matches = 0;
    for _ in 0..repetitions {
        for (path, is_dir) in paths {
            matches += usize::from(!matcher.matched(path, *is_dir).is_none());
        }
    }
    std::hint::black_box(matches);
    start.elapsed()
}

fn time_policy(rules: &DirRules, paths: &[(PathBuf, bool)], repetitions: usize) -> Duration {
    let start = Instant::now();
    let mut skipped = 0;
    for _ in 0..repetitions {
        for (path, is_dir) in paths {
            let entry = if *is_dir {
                Entry::Dir
            } else {
                Entry::File { size: 0 }
            };
            skipped += usize::from(rules.decide(path.as_os_str(), entry) == Decision::Skip);
        }
    }
    std::hint::black_box(skipped);
    start.elapsed()
}

fn best_of_three(mut run: impl FnMut() -> Duration) -> Duration {
    (0..3).map(|_| run()).min().unwrap_or(Duration::MAX)
}

fn nanos_per_path(duration: Duration, paths: usize) -> f64 {
    duration.as_nanos() as f64 / paths as f64
}

fn collect_paths(root: &Path, limit: usize) -> Vec<(PathBuf, bool)> {
    let mut paths = Vec::with_capacity(limit);
    let mut pending = vec![root.to_owned()];
    while let Some(directory) = pending.pop() {
        let entries = match fs::read_dir(&directory) {
            Ok(entries) => entries,
            Err(_) => continue,
        };
        for entry in entries.flatten() {
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            let path = entry.path();
            let Ok(relative) = path.strip_prefix(root) else {
                continue;
            };
            paths.push((relative.to_owned(), file_type.is_dir()));
            if paths.len() == limit {
                return paths;
            }
            if file_type.is_dir() {
                pending.push(path);
            }
        }
    }
    paths
}

fn read_lossy(path: &Path) -> String {
    String::from_utf8_lossy(&must(fs::read(path))).into_owned()
}

fn must<T, E: Debug>(result: Result<T, E>) -> T {
    result.unwrap_or_else(|error| panic!("benchmark setup failed: {error:?}"))
}
