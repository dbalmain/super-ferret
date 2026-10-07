//! The phrase matcher on named cases, then against a reference over random
//! documents. [`reference`] is an oracle in the sense of `tests.rs`: it
//! builds each document's unit list whole and slides a window over it,
//! which is obviously right and allocates freely, where the matcher streams.

use ferret_text::{Kind, MAX_TOKEN_BYTES, Scratch, tokenize};

use super::*;

fn holds(arg: &str, doc: &str) -> bool {
    let text = Text::new(arg.as_bytes(), false).unwrap();
    TextMatcher::new().is_match(&text, doc.as_bytes())
}

fn holds_case(arg: &str, doc: &str) -> bool {
    let text = Text::new(arg.as_bytes(), true).unwrap();
    TextMatcher::new().is_match(&text, doc.as_bytes())
}

#[test]
fn a_phrase_matches_however_the_identifier_is_spelled() {
    for doc in [
        "fn requestHandler() {}",
        "request_handler",
        "RequestHandler",
        "the request handler",
        "request,\n\thandler",
        "REQUEST-HANDLER",
        "makeRequestHandlerFor(x)",
    ] {
        assert!(holds("request handler", doc), "{doc}");
        assert!(holds("requestHandler", doc), "{doc}");
    }
}

#[test]
fn a_phrase_needs_its_units_adjacent_and_in_order() {
    for doc in [
        "request x handler",
        "requestXHandler",
        "handler request",
        "request",
        "requesthandler2 handler", // `requesthandler2`'s parts are `requesthandler`, `2`
        "requests handler",
    ] {
        assert!(!holds("request handler", doc), "{doc}");
    }
}

#[test]
fn one_run_matches_its_whole_token_anywhere_the_tokenizer_emits_it() {
    // The whole token as a part of a longer identifier: `my_httprequest`
    // emits `httprequest` as a part.
    assert!(holds("HttpRequest", "my_httprequest"));
    assert!(holds("HttpRequest", "httprequest"));
    // Or the parts in sequence.
    assert!(holds("HttpRequest", "http request"));
    assert!(holds("HttpRequest", "parseHTTPRequest2"));
    // A single run without parts: any token equal to it.
    assert!(holds("requesthandler", "requestHandler"), "the whole run");
    assert!(holds("request", "requestHandler"), "a part");
    assert!(!holds("requesthandler", "request handler"));
}

#[test]
fn units_repeat_and_overlap() {
    assert!(
        holds("a a b", "a a a b"),
        "KMP falls back to a shorter prefix"
    );
    assert!(holds("a b a b c", "a b a b a b c"));
    assert!(!holds("a b a b c", "a b a b a c"));
    assert!(holds("x_x_x", "x x x"));
}

#[test]
fn a_phrase_inside_a_run_longer_than_the_cap() {
    // The run is far past the 64-byte cap, so its whole token is stored
    // capped; its parts are short and stored whole.
    let long = format!("{}RequestHandler", "a".repeat(MAX_TOKEN_BYTES));
    assert!(long.len() > MAX_TOKEN_BYTES);
    assert!(holds("request handler", &long));
    // The whole long run matches itself, and not another run sharing its
    // first 64 bytes, which would share its capped term.
    let other = format!("{}RequestHandles", "a".repeat(MAX_TOKEN_BYTES));
    assert!(holds(&long, &long));
    assert!(!holds(&long, &other));
    assert_eq!(
        ferret_text::cap(long.to_lowercase().as_bytes()),
        ferret_text::cap(other.to_lowercase().as_bytes())
    );
    // Units straddling byte 64 of the run: `cap` cuts inside `request`.
    let pad = "b".repeat(60);
    let straddle = format!("{pad}RequestHandler");
    assert!(holds("request handler", &straddle));
    assert!(holds(&format!("{pad} request"), &straddle));
    assert!(!holds("b request", &straddle));
}

#[test]
fn non_ascii_parts() {
    assert!(holds("größe änderung", "größeÄnderung"));
    assert!(holds("Größe Änderung", "GRÖSSE größe_änderung"));
    assert!(!holds("größe änderung", "größe x änderung"));
    // Final sigma: the part `ΑΣ` lowercases to `ας`, alone or in a run.
    assert!(holds("ας βc", "ΑΣΒc"));
    assert!(holds("ΑΣΒc", "ασβc"));
}

#[test]
fn case_text_compares_the_original_bytes() {
    assert!(holds_case("Foo", "Foo"));
    assert!(holds_case("Foo", "FooBar"), "a part's own span");
    assert!(!holds_case("Foo", "foo"));
    assert!(!holds_case("Foo", "FOO"));
    assert!(!holds_case("Foo", "fooBar"));
    assert!(holds_case("Request handler", "Request_handler"));
    assert!(!holds_case("Request handler", "request_handler"));
    // The uncased forms all match without `case:`.
    assert!(holds("Foo", "FOO") && holds("Foo", "foo"));
}

#[test]
fn find_reports_the_span_of_the_match() {
    let text = Text::new(b"request handler", false).unwrap();
    let mut matcher = TextMatcher::new();
    let doc = b"let x = makeRequestHandler();";
    assert_eq!(matcher.find(&text, doc), Some(12..26));
    assert_eq!(&doc[12..26], b"RequestHandler");
    let doc = b"request\n  handler";
    assert_eq!(matcher.find(&text, doc), Some(0..doc.len()));
    let whole = Text::new(b"xyz", false).unwrap();
    assert_eq!(matcher.find(&whole, b"ab XYZ"), Some(3..6));
}

#[test]
fn an_argument_without_tokens_is_refused() {
    assert_eq!(Text::new(b"", false), None);
    assert_eq!(Text::new(b" -- ...", false), None);
}

#[test]
fn the_text_exposes_what_the_index_looks_up() {
    let text = Text::new(b"HttpRequest", false).unwrap();
    assert_eq!(text.whole(), Some(&b"httprequest"[..]));
    assert_eq!(text.units().collect::<Vec<_>>(), [&b"http"[..], b"request"]);
    let text = Text::new(b"foo.bar_baz", false).unwrap();
    assert_eq!(text.whole(), None);
    assert_eq!(
        text.units().collect::<Vec<_>>(),
        [&b"foo"[..], b"bar", b"baz"]
    );
}

// ── Against the reference ──

/// Every token of `doc` and its unit list, built whole.
fn reference(arg: &[u8], doc: &[u8]) -> bool {
    let runs = |bytes: &[u8]| {
        let mut runs: Vec<(Vec<u8>, Vec<Vec<u8>>)> = Vec::new();
        tokenize(bytes, &mut Scratch::default(), |t| match t.kind {
            Kind::Whole => runs.push((t.bytes.to_vec(), Vec::new())),
            Kind::Part => runs.last_mut().unwrap().1.push(t.bytes.to_vec()),
        });
        runs
    };
    let units = |runs: &[(Vec<u8>, Vec<Vec<u8>>)]| -> Vec<Vec<u8>> {
        runs.iter()
            .flat_map(|(whole, parts)| {
                if parts.is_empty() {
                    vec![whole.clone()]
                } else {
                    parts.clone()
                }
            })
            .collect()
    };
    let (query, document) = (runs(arg), runs(doc));
    if let [(whole, _)] = query.as_slice()
        && document
            .iter()
            .any(|(w, parts)| w == whole || parts.contains(whole))
    {
        return true;
    }
    let (want, have) = (units(&query), units(&document));
    have.windows(want.len()).any(|w| w == want.as_slice())
}

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }

    /// Words from a tiny vocabulary in random case styles and separators,
    /// so phrases recur and identifiers split.
    fn text(&mut self, words: usize) -> String {
        const WORDS: [&str; 5] = ["a", "ab", "b", "größe", "x1"];
        const JOINS: [&str; 6] = [" ", "_", "", "-", ".\n", "__"];
        let mut out = String::new();
        for i in 0..words {
            if i > 0 {
                out.push_str(JOINS[self.below(JOINS.len())]);
            }
            let word = WORDS[self.below(WORDS.len())];
            match self.below(3) {
                0 => out.push_str(word),
                1 => out.push_str(&word.to_uppercase()),
                _ => {
                    let mut chars = word.chars();
                    out.extend(chars.next().map(|c| c.to_ascii_uppercase()));
                    out.extend(chars);
                }
            }
        }
        out
    }
}

#[test]
fn the_matcher_agrees_with_the_reference() {
    let mut rng = Rng(0x5eed);
    let mut matcher = TextMatcher::new();
    let (mut hits, mut misses) = (0, 0);
    for _ in 0..20_000 {
        let (arg_words, doc_words) = (1 + rng.below(3), rng.below(12));
        let arg = rng.text(arg_words);
        let doc = rng.text(doc_words);
        let Some(text) = Text::new(arg.as_bytes(), false) else {
            continue;
        };
        let expected = reference(arg.as_bytes(), doc.as_bytes());
        assert_eq!(
            matcher.is_match(&text, doc.as_bytes()),
            expected,
            "{arg:?} in {doc:?}"
        );
        if expected { hits += 1 } else { misses += 1 }
    }
    assert!(hits > 1000 && misses > 1000, "{hits} hits, {misses} misses");
}
