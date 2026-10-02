//! Byte-oriented fnmatch with flags zero (and ASCII case folding for -i*).
//! Stars span slashes and leading dots. C-locale character classes are
//! explicit; no regex dialect or policy glob semantics leak into find
//! expressions.

#[derive(Debug)]
enum Token {
    Literal(u8),
    Any,
    Star,
    Class(Box<[bool; 256]>),
    Never,
}

pub(super) fn matches(pattern: &[u8], text: &[u8], fold: bool) -> bool {
    let tokens = compile(pattern, fold);
    let mut row = vec![false; text.len() + 1];
    row[0] = true;
    for token in tokens {
        let mut next = vec![false; row.len()];
        if matches!(token, Token::Star) {
            next[0] = row[0];
            for i in 1..next.len() {
                next[i] = row[i] || next[i - 1];
            }
        } else {
            for (i, &byte) in text.iter().enumerate() {
                next[i + 1] = row[i]
                    && match &token {
                        Token::Literal(want) => {
                            if fold {
                                want.eq_ignore_ascii_case(&byte)
                            } else {
                                *want == byte
                            }
                        }
                        Token::Any => true,
                        Token::Class(set) => set[usize::from(byte)],
                        Token::Never | Token::Star => false,
                    };
            }
        }
        row = next;
    }
    row[text.len()]
}

fn compile(pattern: &[u8], fold: bool) -> Vec<Token> {
    let mut tokens = Vec::new();
    let mut at = 0;
    while at < pattern.len() {
        let byte = pattern[at];
        at += 1;
        tokens.push(match byte {
            b'*' => Token::Star,
            b'?' => Token::Any,
            b'\\' => match pattern.get(at) {
                Some(&literal) => {
                    at += 1;
                    Token::Literal(literal)
                }
                None => Token::Never,
            },
            b'[' => match bracket(&pattern[at..], fold) {
                Some((set, length)) => {
                    at += length;
                    Token::Class(Box::new(set))
                }
                None => Token::Literal(b'['),
            },
            literal => Token::Literal(literal),
        });
    }
    tokens
}

fn bracket(bytes: &[u8], fold: bool) -> Option<([bool; 256], usize)> {
    let mut at = 0;
    let negate = bytes.first().is_some_and(|b| b"!^".contains(b));
    if negate {
        at += 1;
    }
    let start = at;
    let mut set = [false; 256];
    while let Some(&byte) = bytes.get(at) {
        if byte == b']' && at > start {
            if fold {
                for value in b'A'..=b'Z' {
                    let lower = usize::from(value.to_ascii_lowercase());
                    let upper = usize::from(value);
                    let member = set[lower] || set[upper];
                    set[lower] = member;
                    set[upper] = member;
                }
            }
            if negate {
                for member in &mut set {
                    *member = !*member;
                }
            }
            return Some((set, at + 1));
        }
        if byte == b'[' && bytes.get(at + 1) == Some(&b':') {
            let end = bytes[at + 2..].windows(2).position(|pair| pair == b":]")? + at + 2;
            let name = &bytes[at + 2..end];
            for value in 0..=u8::MAX {
                set[usize::from(value)] |= class(name, value)?;
            }
            at = end + 2;
            continue;
        }
        let first = bracket_byte(bytes, &mut at)?;
        if bytes.get(at) == Some(&b'-') && bytes.get(at + 1).is_some_and(|&b| b != b']') {
            at += 1;
            let last = bracket_byte(bytes, &mut at)?;
            for value in first..=last {
                set[usize::from(value)] = true;
            }
        } else {
            set[usize::from(first)] = true;
        }
    }
    None
}

fn bracket_byte(bytes: &[u8], at: &mut usize) -> Option<u8> {
    let mut byte = *bytes.get(*at)?;
    *at += 1;
    if byte == b'\\' {
        byte = *bytes.get(*at)?;
        *at += 1;
    }
    Some(byte)
}

fn class(name: &[u8], byte: u8) -> Option<bool> {
    Some(match name {
        b"alnum" => byte.is_ascii_alphanumeric(),
        b"alpha" => byte.is_ascii_alphabetic(),
        b"blank" => byte == b' ' || byte == b'\t',
        b"cntrl" => byte.is_ascii_control(),
        b"digit" => byte.is_ascii_digit(),
        b"graph" => byte.is_ascii_graphic(),
        b"lower" => byte.is_ascii_lowercase(),
        b"print" => byte.is_ascii_graphic() || byte == b' ',
        b"punct" => byte.is_ascii_punctuation(),
        b"space" => matches!(byte, b' ' | b'\t' | b'\n' | b'\r' | 11 | 12),
        b"upper" => byte.is_ascii_uppercase(),
        b"xdigit" => byte.is_ascii_hexdigit(),
        _ => return None,
    })
}
