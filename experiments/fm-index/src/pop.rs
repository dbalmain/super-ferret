//! The screens population, read from the harness's scratch: the oracle cache
//! (one escaped pattern and its matching documents per query, in the
//! harness's order, 50 per class) and the frozen corpus directory.

use std::fs;
use std::path::{Path, PathBuf};

use ferret_verify::trigram::RequiredLiterals;
use ferret_verify::{Grep, GrepFlags};
use regex_syntax::hir::{Class, Hir, HirKind, Look};

pub const CLASSES: [&str; 5] = [
    "rare literal",
    "identifier pattern",
    "alternation",
    "broad",
    "code class",
];

pub struct Query {
    pub class: &'static str,
    pub grep: Grep,
    pub facts: RequiredLiterals,
    pub oracle: Vec<u32>,
    /// Whether verifying only the lines holding hits is sound: no match can
    /// contain a newline, and no anchor names the document's start or end.
    pub lines: Result<(), &'static str>,
}

pub struct Docs {
    pub dir: PathBuf,
    /// Readable documents, ascending.
    pub ids: Vec<u32>,
    pub sizes: Vec<u64>,
}

impl Docs {
    pub fn path(&self, doc: u32) -> PathBuf {
        self.dir.join(doc.to_string())
    }
}

/// Reverses `str::escape_default`.
fn unescape(s: &str) -> String {
    let mut out = String::new();
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('t') => out.push('\t'),
            Some('r') => out.push('\r'),
            Some('n') => out.push('\n'),
            Some('u') => {
                let hex: String = chars
                    .by_ref()
                    .skip(1)
                    .take_while(|&c| c != '}')
                    .collect();
                let code = u32::from_str_radix(&hex, 16).expect("escape");
                out.push(char::from_u32(code).expect("scalar"));
            }
            Some(other) => out.push(other),
            None => panic!("trailing backslash"),
        }
    }
    out
}

pub fn load(corpus: &Path, oracle: &Path) -> (Vec<Query>, Docs) {
    let text = fs::read_to_string(oracle).expect("oracle cache");
    let mut lines = text.lines();
    assert_eq!(lines.next(), Some("case_insensitive=true"));
    let flags = GrepFlags {
        case_insensitive: true,
    };
    let mut queries = Vec::new();
    for (q, line) in lines.enumerate() {
        let (pattern, docs) = line.rsplit_once('\t').expect("row");
        let pattern = unescape(pattern);
        assert_eq!(pattern.escape_default().to_string(), line.rsplit_once('\t').unwrap().0);
        let grep = flags.compile(&pattern).expect("compile");
        let facts = flags.required_literals(&pattern).expect("facts");
        let oracle = docs
            .split(',')
            .filter(|d| !d.is_empty())
            .map(|d| d.parse().expect("doc"))
            .collect();
        queries.push(Query {
            class: CLASSES[q / 50],
            lines: line_sound(&pattern),
            grep,
            facts,
            oracle,
        });
    }
    assert_eq!(queries.len(), 250);
    let mut ids: Vec<u32> = fs::read_dir(corpus)
        .expect("corpus")
        .filter_map(|e| e.ok()?.file_name().to_str()?.parse().ok())
        .collect();
    ids.sort_unstable();
    let sizes = ids
        .iter()
        .map(|d| fs::metadata(corpus.join(d.to_string())).expect("doc").len())
        .collect();
    (
        queries,
        Docs {
            dir: corpus.to_path_buf(),
            ids,
            sizes,
        },
    )
}

/// The verifier's parse (ferret-verify `GrepFlags::settings`, case-
/// insensitive), walked for anything that lets a match leave its line.
fn line_sound(pattern: &str) -> Result<(), &'static str> {
    let hir = regex_syntax::ParserBuilder::new()
        .utf8(false)
        .unicode(true)
        .case_insensitive(true)
        .multi_line(true)
        .dot_matches_new_line(false)
        .crlf(false)
        .line_terminator(b'\n')
        .swap_greed(false)
        .ignore_whitespace(false)
        .octal(false)
        .nest_limit(250)
        .build()
        .parse(pattern)
        .expect("parse");
    walk(&hir)
}

fn walk(hir: &Hir) -> Result<(), &'static str> {
    match hir.kind() {
        HirKind::Empty => Ok(()),
        HirKind::Literal(lit) => {
            if lit.0.contains(&b'\n') {
                Err("literal newline")
            } else {
                Ok(())
            }
        }
        HirKind::Class(Class::Unicode(set)) => {
            if set.ranges().iter().any(|r| r.start() <= '\n' && '\n' <= r.end()) {
                Err("class matches newline")
            } else {
                Ok(())
            }
        }
        HirKind::Class(Class::Bytes(set)) => {
            if set.ranges().iter().any(|r| r.start() <= b'\n' && b'\n' <= r.end()) {
                Err("class matches newline")
            } else {
                Ok(())
            }
        }
        HirKind::Look(look) => match look {
            Look::Start | Look::End => Err("text anchor"),
            _ => Ok(()),
        },
        HirKind::Repetition(rep) => walk(&rep.sub),
        HirKind::Capture(cap) => walk(&cap.sub),
        HirKind::Concat(parts) | HirKind::Alternation(parts) => parts.iter().try_for_each(walk),
    }
}

#[cfg(test)]
mod tests {
    use super::line_sound;

    #[test]
    fn newline_reaching_patterns_are_refused() {
        for (pattern, ok) in [
            ("foo", true),
            ("^foo$", true),
            ("foo.bar", true),
            (r"foo\sbar", false),
            ("foo[^x]bar", false),
            (r"\Afoo", false),
            (r"foo\z", false),
            (r"\bfoo\b", true),
            ("(?s)foo.bar", false),
            (r"foo\nbar", false),
            ("[a-z]+_[0-9]", true),
        ] {
            assert_eq!(line_sound(pattern).is_ok(), ok, "{pattern}");
        }
    }
}
