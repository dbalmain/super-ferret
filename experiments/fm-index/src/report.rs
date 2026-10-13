//! Aggregates the per-query rows into the tables FMINDEX.md cites. Rows are
//! summed over segments per query first (a query visits every segment in
//! turn), then medians and p90s are taken over each class's queries.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::Path;

use crate::pop::CLASSES;
use crate::util::median_p90;

/// Per query, summed over segments: lookup columns 2..=15, and how many
/// segments contributed.
type Sums = BTreeMap<usize, (String, [f64; 14], usize)>;

fn lookups(dir: &Path, set: &str, class_suffix: &str) -> Sums {
    let mut out = Sums::new();
    for k in 0..4 {
        let path = dir
            .join("rows")
            .join(format!("lookup-{set}-k{k}{class_suffix}.tsv"));
        let Ok(text) = fs::read_to_string(&path) else {
            continue;
        };
        for line in text.lines() {
            let f: Vec<&str> = line.split('\t').collect();
            let q: usize = f[0].parse().expect("q");
            let e = out
                .entry(q)
                .or_insert_with(|| (f[1].to_string(), [0.0; 14], 0));
            for (i, v) in f[2..16].iter().enumerate() {
                e.1[i] += v.parse::<f64>().expect("num");
            }
            e.2 += 1;
        }
    }
    out
}

fn fmt((m, p): (f64, f64), scale: f64) -> String {
    if m.is_nan() {
        return "-".into();
    }
    let d = |v: f64| {
        let v = v / scale;
        if v >= 100.0 {
            format!("{v:.0}")
        } else if v >= 10.0 {
            format!("{v:.1}")
        } else if v >= 1.0 {
            format!("{v:.2}")
        } else {
            format!("{v:.3}")
        }
    };
    format!("{}/{}", d(m), d(p))
}

fn lookup_table(dir: &Path, set: &str, class_suffix: &str) {
    let sums = lookups(dir, set, class_suffix);
    if sums.is_empty() {
        return;
    }
    println!(
        "\n## lookup {set}{class_suffix} (ms median/p90 summed over segments; segments per query: {:?})",
        sums.values().map(|v| v.2).collect::<BTreeSet<_>>()
    );
    println!(
        "| class | queries | occurrences | located | LF steps | count cold | locate cold | total cold | count warm | locate warm | combine warm | total warm | candidate docs |"
    );
    for class in CLASSES {
        let rows: Vec<&[f64; 14]> = sums
            .values()
            .filter(|v| v.0 == class)
            .map(|v| &v.1)
            .collect();
        if rows.is_empty() {
            continue;
        }
        let col = |i: usize| median_p90(rows.iter().map(|r| r[i]).collect());
        println!(
            "| {class} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {} |",
            rows.len(),
            fmt(col(8), 1.0),
            fmt(col(9), 1.0),
            fmt(col(10), 1.0),
            fmt(col(0), 1e6),
            fmt(col(1), 1e6),
            fmt(col(3), 1e6),
            fmt(col(4), 1e6),
            fmt(col(5), 1e6),
            fmt(col(6), 1e6),
            fmt(col(7), 1e6),
            fmt(col(12), 1.0),
        );
        let located: f64 = rows.iter().map(|r| r[9]).sum();
        let locate_ns: f64 = rows.iter().map(|r| r[5]).sum();
        let steps: f64 = rows.iter().map(|r| r[10]).sum();
        if located > 0.0 {
            println!(
                "|  ↳ {class} | sums: located {located:.0}, warm locate {:.1} ns per occurrence, {:.1} LF steps per occurrence; locate is {:.0}% of warm lookup |",
                locate_ns / located,
                steps / located,
                100.0 * locate_ns / rows.iter().map(|r| r[7]).sum::<f64>()
            );
        }
    }
}

fn verify_table(dir: &Path, file: &str) {
    let Ok(text) = fs::read_to_string(dir.join("rows").join(file)) else {
        return;
    };
    println!("\n## {file} (MB = 10^6 bytes; ms median/p90)");
    println!(
        "| class | mode | queries | candidate docs | candidate MB | read MB | regex MB | device MB cold | cold ms | warm ms | missed oracle docs | matches == oracle | whole-doc / line verifications |"
    );
    let rows: Vec<Vec<String>> = text
        .lines()
        .map(|l| l.split('\t').map(String::from).collect())
        .collect();
    for class in CLASSES {
        for mode in ["line", "whole"] {
            let r: Vec<&Vec<String>> = rows
                .iter()
                .filter(|r| r[1] == class && r[2] == mode)
                .collect();
            if r.is_empty() {
                continue;
            }
            let col = |i: usize| {
                median_p90(r.iter().map(|x| x[i].parse().expect("num")).collect())
            };
            let sum = |i: usize| r.iter().map(|x| x[i].parse::<f64>().expect("num")).sum::<f64>();
            println!(
                "| {class} | {mode} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {}/{} | {}/{} |",
                r.len(),
                fmt(col(3), 1.0),
                fmt(col(4), 1e6),
                fmt(col(10), 1e6),
                fmt(col(11), 1e6),
                fmt(col(12), 1e6),
                fmt(col(7), 1e6),
                fmt(col(8), 1e6),
                sum(5),
                r.iter().filter(|x| x[6] == "true").count(),
                r.len(),
                sum(13),
                sum(14),
            );
        }
    }
}

/// Per-query total = lookup (summed over segments) + verification.
fn totals(dir: &Path, set: &str, class_suffix: &str, file: &str, mode: &str) {
    let sums = lookups(dir, set, class_suffix);
    let Ok(text) = fs::read_to_string(dir.join("rows").join(file)) else {
        return;
    };
    if sums.is_empty() {
        return;
    }
    let mut by_class: BTreeMap<&str, (Vec<f64>, Vec<f64>)> = BTreeMap::new();
    for line in text.lines() {
        let f: Vec<&str> = line.split('\t').collect();
        if f[2] != mode {
            continue;
        }
        let q: usize = f[0].parse().expect("q");
        let Some(l) = sums.get(&q) else { continue };
        let e = by_class.entry(CLASSES[q / 50]).or_default();
        e.0.push(l.1[3] + f[7].parse::<f64>().expect("cold"));
        e.1.push(l.1[7] + f[8].parse::<f64>().expect("warm"));
    }
    println!(
        "\n## total: lookup {set}{class_suffix} + {file} {mode} (ms median/p90)"
    );
    println!("| class | queries | total cold | total warm |");
    for class in CLASSES {
        if let Some((c, w)) = by_class.get(class) {
            println!(
                "| {class} | {} | {} | {} |",
                c.len(),
                fmt(median_p90(c.clone()), 1e6),
                fmt(median_p90(w.clone()), 1e6)
            );
        }
    }
}

pub fn run(dir: &Path) {
    let mut names: Vec<String> = fs::read_dir(dir.join("rows"))
        .expect("rows")
        .filter_map(|e| e.ok()?.file_name().into_string().ok())
        .collect();
    names.sort();
    let mut sets = BTreeSet::new();
    for n in &names {
        if let Some(rest) = n.strip_prefix("lookup-").and_then(|r| r.strip_suffix(".tsv")) {
            let (set, tail) = rest.rsplit_once("-k").expect("segment");
            let suffix = tail.trim_start_matches(|c: char| c.is_ascii_digit());
            sets.insert((set.to_string(), suffix.to_string()));
        }
    }
    for (set, suffix) in &sets {
        lookup_table(dir, set, suffix);
    }
    for n in &names {
        if n.starts_with("verify-") {
            verify_table(dir, n);
        }
    }
    for n in &names {
        let Some(rest) = n.strip_prefix("verify-") else {
            continue;
        };
        let vset = rest.rsplit_once("-t").expect("threads").0;
        for mode in ["line", "whole"] {
            totals(dir, vset, "", n, mode);
        }
        // FM runs whose candidates equal the verified set's (asserted at
        // lookup time) share its verification.
        for (s, suffix) in &sets {
            if s.starts_with("fm") && !s.contains("cap") {
                totals(dir, s, suffix, n, "line");
            }
        }
    }
}
