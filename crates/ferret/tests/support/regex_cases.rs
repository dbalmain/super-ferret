// Shared by the evaluator and CLI GNU oracles. Fixtures distinguish literal,
// ignored and operative repetition, malformed intervals and capture rollback.
pub const NAMES: &[&str] = &[
    "a", "aa", "aaa", "aaaa", "b", "ba", "*a", "+a", "?a", "{2}a", "2}a",
    "a{x", "a{,3}", "a{3,1}", "a{99999}", "${regex}", "a1", "11", "a}",
    "a{", "a{2", "a{2,x}", "a{2,}", "a{,}", "a{}", "abab", "ab", "aA",
    "*.pyc", "__pycache__", "m.pyc", "a\\b", "\n",
];

pub fn cases() -> Vec<(String, String, bool)> {
    let mut cases = Vec::new();
    for dialect in [
        "findutils-default", "awk", "ed", "egrep", "emacs", "gnu-awk", "grep",
        "posix-awk", "posix-basic", "posix-egrep", "posix-extended",
        "posix-minimal-basic", "sed",
    ] {
        let basic = matches!(dialect, "findutils-default" | "emacs" | "ed" | "grep" | "posix-basic" | "posix-minimal-basic" | "sed");
        let (open, close, alternate) = if basic { (r"\(", r"\)", r"\|") } else { ("(", ")", "|") };
        let mut add = |pattern: String, fold| cases.push((dialect.to_owned(), pattern, fold));
        for operator in ["*", "+", "?", "{2}", r"\+", r"\?", r"\{2\}"] {
            for pattern in [
                format!("{operator}.*/a"),
                format!(".*/{open}{operator}a{close}"),
                format!(".*/{open}b{alternate}{operator}a{close}"),
                format!("^{operator}.*/a"),
                format!(".*/a${operator}"),
            ] { add(pattern, false); }
        }
        for fragment in [
            "{2}", "{x", "${regex}", "{,3}", "{3,1}", "{99999}", "{", "{2",
            "{2,x}", "{2,}", "{,}", "{}", "}", "{0}", "{32767}", "{32768}",
        ] {
            add(format!(".*/a{fragment}"), false);
            add(format!(".*/a{}", fragment.replace('{', r"\{").replace('}', r"\}")), false);
        }
        for pattern in [
            format!(r".*/{open}a*{close}\1"),
            format!(r".*/{open}a{close}\1"),
            r".*/\1".into(),
            format!(r".*/{open}a\1{close}"),
            format!(r".*/{open}a{close}\2"),
            format!(r".*/{open}a{close}?\1"),
            format!(r".*/{open}a{alternate}ab{close}\1"),
            format!(r".*/{open}a*{close}*\1"),
            format!(r".*/{open}[[:alpha:]]{close}\1"),
            format!(r".*/{open}[^/]+{close}/\1$"),
            format!(r".*/{open}a{close}{open}b{close}\2\1"),
            format!(r".*/{open}a{close}{open}b{close}\1\2"),
        ] { add(pattern, false); }
        add(format!(r".*/{open}a{close}\1"), true);
        for number in 1..=9 {
            add(format!(r".*/{}\{number}", format!("{open}a{close}").repeat(number)), false);
        }
        for pattern in [r"\(*~\|.*__pycache__.*\|*.py[co]\)", r"\(.*__pycache__.*\|*.py[co]\)", r".*\(*.pyc\|__pycache__\).*", r"*.py[co]", r"./${regex}", r"./a{x"] {
            add(pattern.into(), false);
        }
    }
    cases
}
