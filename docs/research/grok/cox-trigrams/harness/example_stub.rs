//! CLI skeleton. Replace the function bodies. Speaks the course protocol so
//! the harness can run against you from the first compiling binary.
//!
//!   rustc -O example_stub.rs -o example_stub && ./example_stub match a a
//!
//! Stdlib only: JSON is written by hand below, no serde.

use std::process::ExitCode;

/// Return `Err(BadRegexp)` from any command to exit 2 (illegal pattern).
#[derive(Debug)]
struct BadRegexp(String);

type Groups = Vec<Option<(usize, usize)>>;

fn cmd_match(_pattern: &str, _text: &str) -> Result<bool, BadRegexp> {
    // Whole-string match.
    Ok(false)
}

fn cmd_search(_pattern: &str, _text: &str) -> Result<bool, BadRegexp> {
    // Substring match.
    Ok(false)
}

fn cmd_submatch(_pattern: &str, _text: &str) -> Result<Option<Groups>, BadRegexp> {
    // Some(groups) on match, None on non-match.
    // groups[0] is [start, end) of the whole match; then each '('.
    // Unmatched optional group: None.
    Ok(None)
}

fn cmd_index_search(_corpus_dir: &str, _pattern: &str) -> Result<(Vec<String>, Vec<String>), BadRegexp> {
    // Return (candidates, hits), each a list of basenames.
    Ok((Vec::new(), Vec::new()))
}

// ---- protocol plumbing; nothing below needs editing ----

fn json_str(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

fn json_str_array(items: &[String]) -> String {
    let parts: Vec<String> = items.iter().map(|s| json_str(s)).collect();
    format!("[{}]", parts.join(","))
}

fn json_groups(groups: &Groups) -> String {
    let parts: Vec<String> = groups
        .iter()
        .map(|g| match g {
            Some((s, e)) => format!("[{},{}]", s, e),
            None => "null".to_string(),
        })
        .collect();
    format!("[{}]", parts.join(","))
}

fn usage(msg: &str) -> ExitCode {
    eprintln!("{}", msg);
    ExitCode::from(1)
}

fn emit(result: Result<String, BadRegexp>) -> ExitCode {
    match result {
        Ok(json) => {
            println!("{}", json);
            ExitCode::SUCCESS
        }
        Err(BadRegexp(msg)) => {
            eprintln!("bad regexp: {}", msg);
            ExitCode::from(2)
        }
    }
}

fn main() -> ExitCode {
    let argv: Vec<String> = std::env::args().collect();
    let Some(cmd) = argv.get(1).map(String::as_str) else {
        return usage("usage: match|search|submatch|index-search ...");
    };
    match cmd {
        "match" | "search" | "submatch" => {
            if argv.len() != 4 {
                return usage(&format!("usage: {} PATTERN TEXT", cmd));
            }
            let (pattern, text) = (&argv[2], &argv[3]);
            emit(match cmd {
                "submatch" => cmd_submatch(pattern, text).map(|g| match g {
                    Some(groups) => format!("{{\"match\":true,\"groups\":{}}}", json_groups(&groups)),
                    None => "{\"match\":false,\"groups\":null}".to_string(),
                }),
                "search" => cmd_search(pattern, text).map(|m| format!("{{\"match\":{}}}", m)),
                _ => cmd_match(pattern, text).map(|m| format!("{{\"match\":{}}}", m)),
            })
        }
        "index-search" => {
            if argv.len() != 5 || argv[2] != "--corpus" {
                return usage("usage: index-search --corpus DIR PATTERN");
            }
            emit(cmd_index_search(&argv[3], &argv[4]).map(|(cands, hits)| {
                format!(
                    "{{\"candidates\":{},\"hits\":{}}}",
                    json_str_array(&cands),
                    json_str_array(&hits)
                )
            }))
        }
        _ => usage(&format!("unknown command {}", cmd)),
    }
}
