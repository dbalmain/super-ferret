//! `tokenize` against the implementation it replaced, which is kept here
//! verbatim as the oracle. Token-sequence identity with it is what keeps
//! `TOKENIZER_VERSION` at 1, so every test below drives the real `tokenize`
//! and compares, rather than restating the rule.

use std::ops::Range;

use crate::{Kind, MAX_TOKEN_BYTES, Scratch, Token, cap, has_token, tokenize};

// ── The oracle: `tokens()` as it was at TOKENIZER_VERSION 1, unchanged ──

pub fn tokens(bytes: &[u8], mut emit: impl FnMut(&[u8])) {
    let text = String::from_utf8_lossy(bytes);
    for run in text
        .split(|c: char| !c.is_alphanumeric() && c != '_')
        .filter(|s| !s.is_empty())
    {
        let whole = run.to_lowercase();
        emit(whole.as_bytes());
        let chars: Vec<_> = run.char_indices().collect();
        let mut start = 0;
        for i in 0..chars.len() {
            let (at, current) = chars[i];
            let previous = i.checked_sub(1).map(|j| chars[j].1);
            let next = chars.get(i + 1).map(|&(_, c)| c);
            let boundary = current == '_'
                || previous == Some('_')
                || previous.is_some_and(|p| {
                    p.is_numeric() != current.is_numeric()
                        || p.is_lowercase() && current.is_uppercase()
                        || p.is_uppercase()
                            && current.is_uppercase()
                            && next.is_some_and(char::is_lowercase)
                });
            if boundary {
                part(&run[start..at], &whole, &mut emit);
                start = at;
            }
            if current == '_' {
                start = at + current.len_utf8();
            }
        }
        part(&run[start..], &whole, &mut emit);
    }
}

fn part(text: &str, whole: &str, emit: &mut impl FnMut(&[u8])) {
    if !text.is_empty() {
        let part = text.to_lowercase();
        if part != whole {
            emit(part.as_bytes());
        }
    }
}

// ── Checking one input ──

#[derive(Debug, PartialEq, Eq)]
struct Owned {
    bytes: Vec<u8>,
    run: Range<usize>,
    kind: Kind,
}

fn oracle(input: &[u8]) -> Vec<Vec<u8>> {
    let mut out = Vec::new();
    tokens(input, |token| out.push(token.to_vec()));
    out
}

fn run_tokenize(input: &[u8], scratch: &mut Scratch) -> Vec<Owned> {
    let mut out = Vec::new();
    tokenize(input, scratch, |Token { bytes, run, kind }| {
        out.push(Owned {
            bytes: bytes.to_vec(),
            run,
            kind,
        });
    });
    out
}

fn is_word(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// Tokenizes `input` and checks everything the contract promises: the
/// oracle's sequence, run ranges, and the token invariants. Returns the
/// tokens for cases that also pin an exact answer.
fn check(input: &[u8], scratch: &mut Scratch) -> Vec<Owned> {
    let got = run_tokenize(input, scratch);
    let shown = String::from_utf8_lossy(input);
    let sequence: Vec<_> = got.iter().map(|t| t.bytes.clone()).collect();
    assert_eq!(
        sequence,
        oracle(input),
        "sequence differs from the oracle on {shown:?}"
    );

    let mut whole: Option<Range<usize>> = None;
    for token in &got {
        let text = std::str::from_utf8(&token.bytes)
            .unwrap_or_else(|_| panic!("token not UTF-8 on {shown:?}: {token:?}"));
        assert!(!text.is_empty(), "empty token on {shown:?}");
        assert_eq!(
            text.to_lowercase(),
            text,
            "token not lowercase on {shown:?}"
        );

        let source = std::str::from_utf8(&input[token.run.clone()])
            .unwrap_or_else(|_| panic!("run not UTF-8 on {shown:?}: {token:?}"));
        assert!(
            source.chars().all(is_word),
            "run holds a separator on {shown:?}: {token:?}"
        );
        assert_eq!(
            source.to_lowercase(),
            text,
            "run does not lowercase to token on {shown:?}"
        );
        match token.kind {
            Kind::Whole => {
                if let Some(previous) = &whole {
                    assert!(previous.end < token.run.start, "runs overlap on {shown:?}");
                }
                assert_maximal(input, &token.run);
                whole = Some(token.run.clone());
            }
            Kind::Part => {
                let outer = whole
                    .as_ref()
                    .unwrap_or_else(|| panic!("part first on {shown:?}"));
                assert!(
                    outer.start <= token.run.start && token.run.end <= outer.end,
                    "part outside its run on {shown:?}: {token:?}"
                );
            }
        }
    }
    got
}

/// The characters either side of a whole run, as std decodes them, are not
/// word characters: the run is the original run, not a piece of one.
fn assert_maximal(input: &[u8], run: &Range<usize>) {
    let after = input[run.end..]
        .utf8_chunks()
        .next()
        .and_then(|c| c.valid().chars().next());
    let before = input[..run.start]
        .utf8_chunks()
        .last()
        .filter(|c| c.invalid().is_empty())
        .and_then(|c| c.valid().chars().next_back());
    let shown = String::from_utf8_lossy(input);
    assert!(
        !after.is_some_and(is_word),
        "run {run:?} not maximal on {shown:?}"
    );
    assert!(
        !before.is_some_and(is_word),
        "run {run:?} not maximal on {shown:?}"
    );
}

fn expect(input: &[u8], tokens: &[(&str, Range<usize>, Kind)]) {
    let got = check(input, &mut Scratch::default());
    let want: Vec<_> = tokens
        .iter()
        .map(|(bytes, run, kind)| Owned {
            bytes: bytes.as_bytes().to_vec(),
            run: run.clone(),
            kind: *kind,
        })
        .collect();
    assert_eq!(got, want, "on {:?}", String::from_utf8_lossy(input));
}

// ── Discriminating cases ──

use Kind::{Part, Whole};

/// The ASCII path meets a non-ASCII letter mid-run and must redo the whole run
/// on the char path, keeping `foo` and `Bar` in the same run as `É`.
#[test]
fn a_non_ascii_letter_mid_run_moves_the_whole_run_to_the_char_path() {
    expect(
        "fooÉBar".as_bytes(),
        &[
            ("fooébar", 0..8, Whole),
            ("foo", 0..3, Part),
            ("é", 3..5, Part),
            ("bar", 5..8, Part),
        ],
    );
}

/// D9's example: the acronym rule needs one byte of lookahead.
#[test]
fn an_acronym_splits_before_its_last_capital() {
    expect(
        b"parseHTTPRequest2",
        &[
            ("parsehttprequest2", 0..17, Whole),
            ("parse", 0..5, Part),
            ("http", 5..9, Part),
            ("request", 9..16, Part),
            ("2", 16..17, Part),
        ],
    );
}

/// Underscores close parts but belong to none, and leading or doubled ones
/// leave no empty part behind.
#[test]
fn underscores_belong_to_the_whole_run_only() {
    expect(
        b"__a__b",
        &[
            ("__a__b", 0..6, Whole),
            ("a", 2..3, Part),
            ("b", 5..6, Part),
        ],
    );
}

/// The last boundary falls one character before the run's end.
#[test]
fn a_digit_to_letter_boundary_at_the_end_of_a_run() {
    expect(
        b"sha256d",
        &[
            ("sha256d", 0..7, Whole),
            ("sha", 0..3, Part),
            ("256", 3..6, Part),
            ("d", 6..7, Part),
        ],
    );
}

/// The run ranges count the invalid byte, though it yields no token.
#[test]
fn a_lone_invalid_byte_separates_runs() {
    expect(b"a\xffb", &[("a", 0..1, Whole), ("b", 2..3, Whole)]);
}

/// `str::to_lowercase` reads context for `Σ`, so a part is not a slice of the
/// lowercased whole: inside `ΑΣΒc` the sigma is medial, inside the part `ΑΣ`
/// it is final.
#[test]
fn a_part_ending_in_sigma_takes_the_final_form() {
    expect(
        "ΑΣΒc".as_bytes(),
        &[
            ("ασβc", 0..7, Whole),
            ("ας", 0..4, Part),
            ("βc", 4..7, Part),
        ],
    );
}

/// Final_Sigma skips case-ignorable characters, which can sit inside a run:
/// U+02B0 is a modifier letter and U+0345 an alphabetic combining mark.
/// Digits and `_` are neither cased nor ignorable, so they end the scan.
#[test]
fn final_sigma_skips_case_ignorable_letters() {
    for input in [
        "Aʰ\u{3a3}",
        "\u{3a3}ʰA",
        "ªΣ",
        "Σª",
        "1Σ",
        "Σ\u{345}Α",
        "_Σ_",
    ] {
        check(input.as_bytes(), &mut Scratch::default());
    }
}

/// Lowercasing changes the byte length: `İ` grows, the Kelvin sign shrinks to
/// ASCII `k`. Part ranges are in the input, so they must not drift.
#[test]
fn lowercase_that_changes_length_keeps_runs_in_input_bytes() {
    expect(
        "xİy\u{212a}Z".as_bytes(),
        &[
            ("xi\u{307}ykz", 0..8, Whole),
            ("x", 0..1, Part),
            ("i\u{307}y", 1..4, Part),
            ("kz", 4..8, Part),
        ],
    );
    expect(
        "fooİBar".as_bytes(),
        &[
            ("fooi\u{307}bar", 0..8, Whole),
            ("foo", 0..3, Part),
            ("i\u{307}", 3..5, Part),
            ("bar", 5..8, Part),
        ],
    );
}

#[test]
fn has_token_wraps_tokenize() {
    assert!(has_token(b"parseHTTPRequest2.rs", b"http"));
    assert!(has_token(b"parseHTTPRequest2.rs", b"rs"));
    assert!(!has_token(b"parseHTTPRequest2.rs", b"httprequest"));
}

// ── Property tests ──

/// xorshift64*: deterministic, no dependency.
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

    fn pick<'a>(&mut self, table: &[&'a str]) -> &'a str {
        table[self.below(table.len())]
    }
}

/// Bytes that are often UTF-8 lead or continuation bytes, among ASCII word
/// and separator bytes, so random strings hold valid characters as well as
/// broken ones.
fn random_bytes(rng: &mut Rng) -> Vec<u8> {
    const NEAR_TEXT: &[u8] = b"aZ_9 .\xc3\xa9\x89\xce\xa3\xe2\x84\xaa\xf0\x90\x80\xff\xc0";
    let len = rng.below(40);
    if rng.below(2) == 0 {
        (0..len).map(|_| rng.next() as u8).collect()
    } else {
        (0..len)
            .map(|_| NEAR_TEXT[rng.below(NEAR_TEXT.len())])
            .collect()
    }
}

fn random_identifier(rng: &mut Rng) -> Vec<u8> {
    const PIECES: &[&str] = &[
        "foo", "bar", "x", "HTTP", "ID", "A", "Request", "Parse", "2", "42", "007", "_", "__", " ",
        ".", "-", "/",
    ];
    (0..rng.below(8))
        .map(|_| rng.pick(PIECES))
        .collect::<String>()
        .into_bytes()
}

/// Letters, digits and case pairs whose lowercase changes length or depends
/// on context, plus ASCII so runs mix both paths.
fn random_unicode(rng: &mut Rng) -> Vec<u8> {
    const CHARS: &[&str] = &[
        "a", "Z", "q", "_", "7", " ", "-", "é", "É", "ß", "ẞ", "İ", "ı", "Σ", "σ", "ς", "Α", "β",
        "ǅ", "ʰ", "ª", "\u{345}", "\u{301}", "\u{212a}", "\u{2126}", "Ⅻ", "ⅰ", "٣", "²", "½", "中",
        "𐐀", "𐐨", "\u{fffd}",
    ];
    (0..rng.below(12))
        .map(|_| rng.pick(CHARS))
        .collect::<String>()
        .into_bytes()
}

/// Each input is checked whole and cut at a random byte, which splits UTF-8
/// mid-character; one scratch serves every case, as it would in an indexer.
#[test]
fn tokenize_matches_the_oracle_on_random_input() {
    let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
    let mut scratch = Scratch::default();
    for case in 0..6000 {
        let input = match case % 3 {
            0 => random_bytes(&mut rng),
            1 => random_identifier(&mut rng),
            _ => random_unicode(&mut rng),
        };
        check(&input, &mut scratch);
        let cut = rng.below(input.len() + 1);
        check(&input[..cut], &mut scratch);
    }
}

// ── cap ──

#[test]
fn cap_keeps_at_most_sixty_four_bytes() {
    let ascii = "a".repeat(200);
    for len in [0, 1, 63, 64, 65, 200] {
        let token = &ascii.as_bytes()[..len];
        assert_eq!(cap(token), &token[..len.min(MAX_TOKEN_BYTES)], "len {len}");
    }
}

/// A two-, three- or four-byte character straddling byte 64 is dropped whole;
/// one that ends at or starts on byte 64 is not.
#[test]
fn cap_never_splits_a_character() {
    for c in ["é", "中", "𐐀"] {
        for pad in 58..=64 {
            let token = format!("{}{c}{}", "a".repeat(pad), "z".repeat(10));
            let straddles = pad < MAX_TOKEN_BYTES && pad + c.len() > MAX_TOKEN_BYTES;
            let want = if straddles { pad } else { MAX_TOKEN_BYTES };
            let capped = cap(token.as_bytes());
            assert!(std::str::from_utf8(capped).is_ok(), "{c} after {pad}");
            assert_eq!(capped, &token.as_bytes()[..want], "{c} after {pad}");
        }
    }
}
