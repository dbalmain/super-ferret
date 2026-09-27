//! Globs lowered to regexes, and the literal each name pattern guarantees,
//! which is what lets a glob or regex drive the name-heap scan.
//!
//! Both are syntax-only and conservative: a literal returned here occurs in
//! every name the pattern matches, and `None` means "scan every name", never
//! a wrong answer.

/// A glob lowered to a regex over name bytes (or path bytes, if the glob has
/// a `/`), for [`ferret_verify::Regex`]. Matching is bytewise: `*` and `?`
/// match any bytes but `/`, `**` as a whole component matches any number of
/// components, `[...]` is a byte class (`[!...]` or `[^...]` negated) that
/// never matches `/`, and everything else is literal. Case folding, when asked
/// for, is ASCII only, like the scanner's.
///
/// A glob without `/` must match the whole name. A glob with one matches a
/// path's trailing components: `src/**/*.rs` matches `/w/x/src/a/b.rs`, and
/// a leading `/` anchors it at the root instead.
pub(crate) fn glob_regex(glob: &[u8]) -> String {
    let mut out = String::from("(?s-u)");
    let (prefix, rest) = match glob.split_first() {
        Some((b'/', rest)) => ("^/", rest),
        _ if glob.contains(&b'/') => ("(?:^|/)", glob),
        _ => ("^", glob),
    };
    out.push_str(prefix);
    let components: Vec<&[u8]> = rest.split(|&b| b == b'/').collect();
    let last = components.len() - 1;
    for (i, component) in components.iter().enumerate() {
        match (*component == b"**", i == last) {
            (true, true) => out.push_str(".*"),
            (true, false) => out.push_str("(?:.*/)?"),
            (false, is_last) => {
                component_regex(component, &mut out);
                if !is_last {
                    out.push('/');
                }
            }
        }
    }
    out.push('$');
    out
}

fn component_regex(component: &[u8], out: &mut String) {
    let mut i = 0;
    while i < component.len() {
        match component[i] {
            b'*' => {
                // `**` inside a component is two stars.
                out.push_str("[^/]*");
            }
            b'?' => out.push_str("[^/]"),
            b'[' => match class_end(component, i) {
                Some(end) => {
                    // Intersected with `[^/]`: a negated class, or a range
                    // such as `.-0`, would otherwise cross a component.
                    out.push_str("[[");
                    let mut body = &component[i + 1..end];
                    if let Some((b'!' | b'^', rest)) = body.split_first() {
                        out.push('^');
                        body = rest;
                    }
                    for (k, &b) in body.iter().enumerate() {
                        let range = b == b'-' && k > 0 && k + 1 < body.len();
                        if range {
                            out.push('-');
                        } else {
                            push_byte(b, out);
                        }
                    }
                    out.push_str("]&&[^/]]");
                    i = end;
                }
                None => push_byte(b'[', out),
            },
            b => push_byte(b, out),
        }
        i += 1;
    }
}

/// The `]` closing the class that opens at `start`, if any. A `]` first in
/// the class (after any negation) is a member, as in shell globs.
fn class_end(component: &[u8], start: usize) -> Option<usize> {
    let mut i = start + 1;
    if matches!(component.get(i), Some(b'!' | b'^')) {
        i += 1;
    }
    if component.get(i) == Some(&b']') {
        i += 1;
    }
    (i..component.len()).find(|&k| component[k] == b']')
}

/// One literal byte, as regex syntax. Everything but ASCII letters and
/// digits is written `\xHH`, which under `(?-u)` is exactly that byte.
fn push_byte(b: u8, out: &mut String) {
    if b.is_ascii_alphanumeric() {
        out.push(b as char);
    } else {
        out.push_str(&format!("\\x{b:02x}"));
    }
}

/// The longest run of literal bytes in a glob's last component, which every
/// matching name contains.
pub(crate) fn glob_literal(glob: &[u8]) -> Option<Vec<u8>> {
    let last = glob.rsplit(|&b| b == b'/').next().unwrap_or(glob);
    let mut best: &[u8] = &[];
    let (mut run_start, mut i) = (0, 0);
    while i <= last.len() {
        let wild_end = match last.get(i) {
            None => Some(i),
            Some(b'*' | b'?') => Some(i + 1),
            Some(b'[') => class_end(last, i).map(|end| end + 1),
            Some(_) => None,
        };
        match wild_end {
            Some(end) => {
                if i - run_start > best.len() {
                    best = &last[run_start..i];
                }
                run_start = end;
                i = end.max(i + 1);
            }
            None => i += 1,
        }
    }
    (!best.is_empty()).then(|| best.to_vec())
}

/// A literal every match of `pattern` (a `regex`-crate pattern) must
/// contain: the longest run of plain characters before the first group or
/// class, none of them made optional by a quantifier.
///
/// Deliberately simple, and `None` whenever that is not obviously sound: an
/// alternation anywhere, or any `(?` group (flags such as `x` or `i` change
/// what a literal means). Scanning stops at the first `(` or `[` and at a
/// `\x`, `\u`, `\U`, `\p` or `\P` escape. Escaped punctuation counts as
/// literal, except `\<` and `\>` (word boundaries); other escapes such as
/// `\d` or `\b` end a run. When `fold` is set, a run also ends at any
/// non-ASCII character and at `k` and `s`, which fold to non-ASCII
/// characters under Unicode case rules (KELVIN SIGN, LONG S) and so would
/// not be found by the ASCII-folding scanner.
pub(crate) fn regex_literal(pattern: &str, fold: bool) -> Option<Vec<u8>> {
    if pattern.contains("(?") || has_alternation(pattern) {
        return None;
    }
    let chars: Vec<char> = pattern.chars().collect();
    let mut best = String::new();
    let mut run = String::new();
    let end_run = |run: &mut String, best: &mut String| {
        if run.len() > best.len() {
            *best = std::mem::take(run);
        }
        run.clear();
    };
    let mut i = 0;
    while i < chars.len() {
        let (literal, next) = match chars[i] {
            '(' | '[' => break,
            '\\' => match chars.get(i + 1) {
                Some('x' | 'u' | 'U' | 'p' | 'P') | None => break,
                Some('<' | '>') => (None, i + 2),
                Some(&c) if c.is_ascii_punctuation() => (Some(c), i + 2),
                Some(_) => (None, i + 2),
            },
            // A counted repetition's body is not literal.
            '{' => (
                None,
                chars[i..]
                    .iter()
                    .position(|&c| c == '}')
                    .map_or(chars.len(), |at| i + at + 1),
            ),
            '.' | '^' | '$' | ')' | ']' | '?' | '*' | '+' | '}' => (None, i + 1),
            c => (Some(c), i + 1),
        };
        match literal {
            Some(c) => {
                let usable = !(fold && (!c.is_ascii() || matches!(c, 'k' | 'K' | 's' | 'S')));
                match chars.get(next) {
                    // Optional: this character is not required.
                    Some('?' | '*' | '{') => end_run(&mut run, &mut best),
                    // Required, but what follows it is not adjacent.
                    Some('+') => {
                        if usable {
                            run.push(c);
                        }
                        end_run(&mut run, &mut best);
                    }
                    _ if usable => run.push(c),
                    _ => end_run(&mut run, &mut best),
                }
            }
            None => end_run(&mut run, &mut best),
        }
        i = next;
    }
    end_run(&mut run, &mut best);
    (!best.is_empty()).then(|| best.into_bytes())
}

/// Whether `pattern` has a `|` outside a class and not escaped.
fn has_alternation(pattern: &str) -> bool {
    let mut escaped = false;
    let mut class = 0;
    for c in pattern.chars() {
        match (escaped, c) {
            (true, _) => escaped = false,
            (false, '\\') => escaped = true,
            (false, '[') => class += 1,
            (false, ']') if class > 0 => class -= 1,
            (false, '|') if class == 0 => return true,
            _ => {}
        }
    }
    false
}
