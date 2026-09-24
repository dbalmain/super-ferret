//! Seeded random and structured differential against git.
//!
//! Random patterns over a small alphabet of gitignore metacharacters, random
//! paths over a matching alphabet, and one structured batch; every answer the
//! matcher gives for a path with no ignored ancestor must equal git's.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::Path;

use super::oracle::{Scratch, check_ignore, must};
use super::{Gitignore, Match};

#[derive(Default)]
struct Coverage {
    none: usize,
    ignore: usize,
    whitelist: usize,
}

impl Coverage {
    fn record(&mut self, result: Match) {
        match result {
            Match::None => self.none += 1,
            Match::Ignore => self.ignore += 1,
            Match::Whitelist => self.whitelist += 1,
        }
    }
}

#[test]
fn agrees_with_git_on_seeded_random_and_structured_cases() {
    let mut random = XorShift::new(0x000d_1602_f3e2_7a91);
    let mut coverage = Coverage::default();
    let mut compared = 0;

    for batch in 0..6 {
        let patterns = random_patterns(&mut random, 40);
        let paths = random_paths(&mut random, 500);
        let (oracle, ignored_parents) = git_results(&patterns, &paths, batch);
        let (matcher, _) = Gitignore::compile(&patterns);

        for ((path, is_dir), want) in paths.iter().zip(oracle) {
            if parents(path).any(|parent| ignored_parents.contains(parent)) {
                continue;
            }
            let got = matcher.matched(Path::new(path), *is_dir);
            assert_eq!(
                got, want,
                "Git differential mismatch for {path:?}, is_dir={is_dir}\npatterns:\n{patterns}"
            );
            coverage.record(want);
            compared += 1;
        }
    }

    let (patterns, paths) = structured_cases(&mut random, 64);
    let (oracle, _) = git_results(&patterns, &paths, 99);
    let (matcher, errors) = Gitignore::compile(&patterns);
    assert!(errors.is_empty(), "structured generator errors: {errors:?}");
    for ((path, is_dir), want) in paths.iter().zip(oracle) {
        let got = matcher.matched(Path::new(path), *is_dir);
        assert_eq!(got, want, "structured mismatch for {path:?}\n{patterns}");
        coverage.record(want);
        compared += 1;
    }

    assert!(compared >= 2_000, "only {compared} cases compared");
    assert!(coverage.none >= 100, "None coverage: {}", coverage.none);
    assert!(
        coverage.ignore >= 50,
        "Ignore coverage: {}",
        coverage.ignore
    );
    assert!(
        coverage.whitelist >= 10,
        "Whitelist coverage: {}",
        coverage.whitelist
    );
    eprintln!(
        "Git differential coverage: None={}, Ignore={}, Whitelist={}",
        coverage.none, coverage.ignore, coverage.whitelist
    );
}

fn git_results(
    patterns: &str,
    paths: &BTreeMap<String, bool>,
    batch: usize,
) -> (Vec<Match>, BTreeSet<String>) {
    let repository = repository(patterns, paths, batch);
    let queries: Vec<Vec<u8>> = paths.keys().map(|path| path.as_bytes().to_vec()).collect();
    let results = check_ignore(&repository.path, &queries);
    let ignored = paths
        .keys()
        .zip(&results)
        .filter(|(_, result)| **result == Match::Ignore)
        .map(|(path, _)| path.clone())
        .collect();
    (results, ignored)
}

fn structured_cases(random: &mut XorShift, count: usize) -> (String, BTreeMap<String, bool>) {
    let mut patterns = String::new();
    let mut paths = BTreeMap::new();
    for case in 0..count {
        let key = format!(
            "Case-{case}:{}",
            char::from(b'A' + random.range(0, 26) as u8)
        );
        let (pattern, witness, near) = match case % 5 {
            0 => (
                format!("/{key}/**/tail.A"),
                format!("{key}/x/y/tail.A"),
                format!("{key}/x/y/tail.B"),
            ),
            1 => (
                format!("/{key}/***/hash#A"),
                format!("{key}/hash#A"),
                format!("{key}/hash#B"),
            ),
            2 => (
                format!("/{key}/[a-c]-file"),
                format!("{key}/b-file"),
                format!("{key}/Z-file"),
            ),
            3 => (
                format!("/{key}/[[:digit:]] space"),
                format!("{key}/5 space"),
                format!("{key}/A space"),
            ),
            _ => (
                format!("!/{key}/**/keep:A"),
                format!("{key}/x/keep:A"),
                format!("{key}/x/drop:A"),
            ),
        };
        patterns.push_str(&pattern);
        patterns.push('\n');
        insert_path(&mut paths, witness, false);
        insert_path(&mut paths, near, false);
    }
    (patterns, paths)
}

fn random_patterns(random: &mut XorShift, count: usize) -> String {
    const UNITS: &[&[u8]] = &[
        b"a", b"b", b"A", b"Z", b"/", b"*", b"**", b"?", b"[", b"]", b"!", b".", b"\\", b"-", b":",
        b" ", b"#",
    ];
    let mut patterns = String::new();
    for _ in 0..count {
        for _ in 0..random.range(1, 13) {
            let unit = UNITS[random.range(0, UNITS.len())];
            patterns.push_str(ascii(unit));
        }
        patterns.push('\n');
    }
    patterns
}

fn random_paths(random: &mut XorShift, count: usize) -> BTreeMap<String, bool> {
    const NAME_BYTES: &[u8] = b"abAZ*?[]!.\\-:# ";
    let mut generated = BTreeSet::new();
    while generated.len() < count {
        let mut path = String::new();
        for component in 0..random.range(1, 5) {
            if component != 0 {
                path.push('/');
            }
            let start = path.len();
            // Avoid Git's `:(magic)` pathspec prefix while still exercising
            // every generated byte within components.
            path.push('a');
            for _ in 0..random.range(0, 6) {
                path.push(char::from(NAME_BYTES[random.range(0, NAME_BYTES.len())]));
            }
            if matches!(&path[start..], "." | "..") {
                path.insert(start, 'a');
            }
        }
        generated.insert(path);
    }

    let mut paths = BTreeMap::new();
    for path in generated {
        insert_path(&mut paths, path, random.range(0, 4) == 0);
    }
    paths
}

fn insert_path(paths: &mut BTreeMap<String, bool>, path: String, is_dir: bool) {
    for parent in parents(&path) {
        paths.insert(parent.to_owned(), true);
    }
    paths.entry(path).or_insert(is_dir);
}

fn parents(path: &str) -> impl Iterator<Item = &str> {
    path.match_indices('/').map(|(slash, _)| &path[..slash])
}

fn ascii(bytes: &[u8]) -> &str {
    std::str::from_utf8(bytes).unwrap_or_else(|_| panic!("generator constants must be ASCII"))
}

struct XorShift(u64);

impl XorShift {
    fn new(seed: u64) -> Self {
        Self(seed)
    }

    fn next(&mut self) -> u64 {
        let mut value = self.0;
        value ^= value << 13;
        value ^= value >> 7;
        value ^= value << 17;
        self.0 = value;
        value
    }

    fn range(&mut self, start: usize, end: usize) -> usize {
        start + self.next() as usize % (end - start)
    }
}

fn repository(patterns: &str, paths: &BTreeMap<String, bool>, batch: usize) -> Scratch {
    let repository = Scratch::new(&format!("differential-{batch}"));
    must(fs::write(repository.path.join(".gitignore"), patterns));
    for (relative, is_dir) in paths {
        if *is_dir {
            must(fs::create_dir_all(repository.path.join(relative)));
        }
    }
    for (relative, is_dir) in paths {
        if !is_dir {
            must(fs::write(repository.path.join(relative), []));
        }
    }
    repository
}
