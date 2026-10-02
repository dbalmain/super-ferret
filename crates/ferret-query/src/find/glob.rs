//! Byte-oriented fnmatch with flags zero (and ASCII case folding for -i*).
//! Stars span slashes and leading dots. C-locale character classes are
//! explicit; no regex dialect or policy glob semantics leak into find
//! expressions.

#[derive(Clone, Debug, PartialEq, Eq)]
enum Token {
    Literal(u8),
    Any,
    Star,
    Class(Box<[bool; 256]>),
    Never,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Pattern {
    tokens: Vec<Token>,
    fold: bool,
}

impl Pattern {
    pub fn new(pattern: &[u8], fold: bool) -> Self {
        Self {
            tokens: compile(pattern, fold),
            fold,
        }
    }

    pub fn matches(&self, text: &[u8]) -> bool {
        let mut row = vec![false; text.len() + 1];
        row[0] = true;
        for token in &self.tokens {
            let mut next = vec![false; row.len()];
            if matches!(token, Token::Star) {
                next[0] = row[0];
                for i in 1..next.len() {
                    next[i] = row[i] || next[i - 1];
                }
            } else {
                for (i, &byte) in text.iter().enumerate() {
                    next[i + 1] = row[i]
                        && match token {
                            Token::Literal(want) => {
                                if self.fold {
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
    let mut valid = true;
    while let Some(&byte) = bytes.get(at) {
        if byte == b']' && at > start {
            if !valid {
                set.fill(false);
            } else if negate {
                for member in &mut set {
                    *member = !*member;
                }
            }
            return Some((set, at + 1));
        }
        match bracket_atom(bytes, &mut at)? {
            BracketAtom::Single(first, case_sensitive) => {
                if bytes.get(at) == Some(&b'-') && bytes.get(at + 1).is_some_and(|&b| b != b']') {
                    at += 1;
                    if let BracketAtom::Single(last, _) = bracket_atom(bytes, &mut at)? {
                        let first = if fold {
                            first.to_ascii_lowercase()
                        } else {
                            first
                        };
                        let last = if fold {
                            last.to_ascii_lowercase()
                        } else {
                            last
                        };
                        for value in 0..=u8::MAX {
                            let candidate = if fold {
                                value.to_ascii_lowercase()
                            } else {
                                value
                            };
                            set[usize::from(value)] |= (first..=last).contains(&candidate);
                        }
                    } else {
                        valid = false;
                    }
                } else {
                    set[usize::from(first)] = true;
                    if fold && !case_sensitive {
                        set[usize::from(first.to_ascii_lowercase())] = true;
                        set[usize::from(first.to_ascii_uppercase())] = true;
                    }
                }
            }
            BracketAtom::Class(members) => {
                // GNU's C-locale classes ignore FNM_CASEFOLD: [:upper:]
                // still matches only upper case under -iname.
                for (member, included) in set.iter_mut().zip(*members) {
                    *member |= included;
                }
            }
            BracketAtom::Invalid => valid = false,
        }
    }
    None
}

enum BracketAtom {
    Single(u8, bool),
    Class(Box<[bool; 256]>),
    Invalid,
}

fn bracket_atom(bytes: &[u8], at: &mut usize) -> Option<BracketAtom> {
    let mut byte = *bytes.get(*at)?;
    if byte == b'[' && bytes.get(*at + 1).is_some_and(|b| b":=.".contains(b)) {
        let delimiter = bytes[*at + 1];
        let end = bytes[*at + 2..]
            .windows(2)
            .position(|pair| pair == [delimiter, b']'])?
            + *at
            + 2;
        let name = &bytes[*at + 2..end];
        *at = end + 2;
        return Some(if delimiter == b':' {
            let mut members = [false; 256];
            for value in 0..=u8::MAX {
                let Some(member) = class(name, value) else {
                    return Some(BracketAtom::Invalid);
                };
                members[usize::from(value)] = member;
            }
            BracketAtom::Class(Box::new(members))
        } else if let [literal] = name {
            // A single equivalence/collating symbol also stays case-sensitive.
            // When used as a range endpoint GNU folds it with the endpoint.
            BracketAtom::Single(*literal, true)
        } else {
            BracketAtom::Invalid
        });
    }
    *at += 1;
    if byte == b'\\' {
        byte = *bytes.get(*at)?;
        *at += 1;
    }
    Some(BracketAtom::Single(byte, false))
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
