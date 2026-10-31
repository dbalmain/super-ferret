//! JSON values, as far as the request schema needs them: a bounded
//! recursive-descent parser, numbers kept as text, and base64 bytes.

use super::*;

/// A parsed JSON value. Strings always hold a valid Rust `String`: the
/// parser rejects a lone surrogate rather than ever substituting U+FFFD, so
/// every string that parses is already valid Unicode text. (Byte payloads
/// instead use the `{"base64":"..."}` convention at the schema level, not a
/// different `Value` shape.)
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Value {
    Null,
    Bool(bool),
    Num(Num),
    Str(String),
    Arr(Vec<Value>),
    Obj(Vec<(String, Value)>),
}

/// A JSON number, kept as its literal text; callers convert to the type
/// their field needs (here, only `as_u64`), and a copied value is written
/// back exactly as it was read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Num {
    literal: String,
}

impl Num {
    /// The value, when the literal is a non-negative integer without a
    /// fraction or exponent that fits.
    pub(crate) fn as_u64(&self) -> Option<u64> {
        if !self.literal.bytes().all(|byte| byte.is_ascii_digit()) {
            return None;
        }
        self.literal.parse().ok()
    }
    /// The number as it appeared in the input; always valid JSON.
    pub(crate) fn literal(&self) -> &str {
        &self.literal
    }
}

pub(super) struct Parser<'a> {
    pub(super) input: &'a [u8],
    pub(super) pos: usize,
}

impl<'a> Parser<'a> {
    pub(super) fn peek(&self) -> Option<u8> {
        self.input.get(self.pos).copied()
    }

    pub(super) fn skip_ws(&mut self) {
        while matches!(self.peek(), Some(b' ' | b'\t' | b'\n' | b'\r')) {
            self.pos += 1;
        }
    }

    pub(super) fn parse_value(&mut self, depth: u32) -> Result<Value, RequestErrorKind> {
        self.skip_ws();
        match self.peek() {
            Some(b'{') => {
                if depth > MAX_NESTING_DEPTH {
                    return Err(RequestErrorKind::NestingTooDeep);
                }
                self.parse_object(depth)
            }
            Some(b'[') => {
                if depth > MAX_NESTING_DEPTH {
                    return Err(RequestErrorKind::NestingTooDeep);
                }
                self.parse_array(depth)
            }
            Some(b'"') => self.parse_string().map(Value::Str),
            Some(b't') => self.expect_literal("true").map(|()| Value::Bool(true)),
            Some(b'f') => self.expect_literal("false").map(|()| Value::Bool(false)),
            Some(b'n') => self.expect_literal("null").map(|()| Value::Null),
            Some(b'-' | b'0'..=b'9') => self.parse_number().map(Value::Num),
            _ => Err(RequestErrorKind::InvalidJson),
        }
    }

    fn expect_literal(&mut self, lit: &str) -> Result<(), RequestErrorKind> {
        let bytes = lit.as_bytes();
        if self.input[self.pos..].starts_with(bytes) {
            self.pos += bytes.len();
            Ok(())
        } else {
            Err(RequestErrorKind::InvalidJson)
        }
    }

    fn parse_object(&mut self, depth: u32) -> Result<Value, RequestErrorKind> {
        self.pos += 1; // '{'
        let mut fields: Vec<(String, Value)> = Vec::new();
        self.skip_ws();
        if self.peek() == Some(b'}') {
            self.pos += 1;
            return Ok(Value::Obj(fields));
        }
        loop {
            self.skip_ws();
            if self.peek() != Some(b'"') {
                return Err(RequestErrorKind::InvalidJson);
            }
            let key = self.parse_string()?;
            if fields.iter().any(|(k, _)| *k == key) {
                return Err(RequestErrorKind::DuplicateKey);
            }
            self.skip_ws();
            if self.peek() != Some(b':') {
                return Err(RequestErrorKind::InvalidJson);
            }
            self.pos += 1;
            let value = self.parse_value(depth + 1)?;
            fields.push((key, value));
            self.skip_ws();
            match self.peek() {
                Some(b',') => {
                    self.pos += 1;
                }
                Some(b'}') => {
                    self.pos += 1;
                    break;
                }
                _ => return Err(RequestErrorKind::InvalidJson),
            }
        }
        Ok(Value::Obj(fields))
    }

    fn parse_array(&mut self, depth: u32) -> Result<Value, RequestErrorKind> {
        self.pos += 1; // '['
        let mut items = Vec::new();
        self.skip_ws();
        if self.peek() == Some(b']') {
            self.pos += 1;
            return Ok(Value::Arr(items));
        }
        loop {
            let value = self.parse_value(depth + 1)?;
            items.push(value);
            self.skip_ws();
            match self.peek() {
                Some(b',') => {
                    self.pos += 1;
                }
                Some(b']') => {
                    self.pos += 1;
                    break;
                }
                _ => return Err(RequestErrorKind::InvalidJson),
            }
        }
        Ok(Value::Arr(items))
    }

    pub(super) fn parse_string(&mut self) -> Result<String, RequestErrorKind> {
        self.pos += 1; // opening '"'
        let mut out = String::new();
        loop {
            let b = self.peek().ok_or(RequestErrorKind::InvalidJson)?;
            match b {
                b'"' => {
                    self.pos += 1;
                    return Ok(out);
                }
                b'\\' => {
                    self.pos += 1;
                    let e = self.peek().ok_or(RequestErrorKind::InvalidJson)?;
                    match e {
                        b'"' => {
                            out.push('"');
                            self.pos += 1;
                        }
                        b'\\' => {
                            out.push('\\');
                            self.pos += 1;
                        }
                        b'/' => {
                            out.push('/');
                            self.pos += 1;
                        }
                        b'b' => {
                            out.push('\u{8}');
                            self.pos += 1;
                        }
                        b'f' => {
                            out.push('\u{c}');
                            self.pos += 1;
                        }
                        b'n' => {
                            out.push('\n');
                            self.pos += 1;
                        }
                        b'r' => {
                            out.push('\r');
                            self.pos += 1;
                        }
                        b't' => {
                            out.push('\t');
                            self.pos += 1;
                        }
                        b'u' => {
                            self.pos += 1;
                            let cp = self.parse_hex4()?;
                            let ch = self.resolve_escaped_codepoint(cp)?;
                            out.push(ch);
                        }
                        _ => return Err(RequestErrorKind::InvalidJson),
                    }
                }
                0x00..=0x1F => return Err(RequestErrorKind::InvalidJson),
                _ => {
                    let (ch, len) = decode_utf8_char(&self.input[self.pos..])
                        .ok_or(RequestErrorKind::InvalidJson)?;
                    out.push(ch);
                    self.pos += len;
                }
            }
        }
    }

    /// Resolves a `\uXXXX` code point already read as `cp`. A high surrogate
    /// must be followed immediately by a `\u` low surrogate, combined per
    /// RFC 8259 § 7; a low surrogate with no preceding high surrogate, or a
    /// high surrogate not followed by one, is rejected outright — never
    /// replaced with U+FFFD.
    fn resolve_escaped_codepoint(&mut self, cp: u16) -> Result<char, RequestErrorKind> {
        if (0xD800..=0xDBFF).contains(&cp) {
            if self.input[self.pos..].starts_with(b"\\u") {
                self.pos += 2;
                let low = self.parse_hex4()?;
                if !(0xDC00..=0xDFFF).contains(&low) {
                    return Err(RequestErrorKind::InvalidJson);
                }
                let combined =
                    0x10000u32 + (u32::from(cp - 0xD800) << 10) + u32::from(low - 0xDC00);
                char::from_u32(combined).ok_or(RequestErrorKind::InvalidJson)
            } else {
                Err(RequestErrorKind::InvalidJson)
            }
        } else if (0xDC00..=0xDFFF).contains(&cp) {
            Err(RequestErrorKind::InvalidJson)
        } else {
            char::from_u32(u32::from(cp)).ok_or(RequestErrorKind::InvalidJson)
        }
    }

    fn parse_hex4(&mut self) -> Result<u16, RequestErrorKind> {
        let bytes = self
            .input
            .get(self.pos..self.pos + 4)
            .ok_or(RequestErrorKind::InvalidJson)?;
        let text = std::str::from_utf8(bytes).map_err(|_| RequestErrorKind::InvalidJson)?;
        let value = u16::from_str_radix(text, 16).map_err(|_| RequestErrorKind::InvalidJson)?;
        self.pos += 4;
        Ok(value)
    }

    fn parse_number(&mut self) -> Result<Num, RequestErrorKind> {
        let start = self.pos;
        if self.peek() == Some(b'-') {
            self.pos += 1;
        }
        match self.peek() {
            Some(b'0') => {
                self.pos += 1;
            }
            Some(b'1'..=b'9') => {
                while matches!(self.peek(), Some(b'0'..=b'9')) {
                    self.pos += 1;
                }
            }
            _ => return Err(RequestErrorKind::InvalidJson),
        }
        if self.peek() == Some(b'.') {
            self.pos += 1;
            if !matches!(self.peek(), Some(b'0'..=b'9')) {
                return Err(RequestErrorKind::InvalidJson);
            }
            while matches!(self.peek(), Some(b'0'..=b'9')) {
                self.pos += 1;
            }
        }
        if matches!(self.peek(), Some(b'e' | b'E')) {
            self.pos += 1;
            if matches!(self.peek(), Some(b'+' | b'-')) {
                self.pos += 1;
            }
            if !matches!(self.peek(), Some(b'0'..=b'9')) {
                return Err(RequestErrorKind::InvalidJson);
            }
            while matches!(self.peek(), Some(b'0'..=b'9')) {
                self.pos += 1;
            }
        }
        // Every byte in this range is an ASCII digit, sign, point or
        // exponent mark by construction above, so a byte-by-byte cast to
        // `char` is exact and needs no fallible UTF-8 decode.
        let literal = self.input[start..self.pos]
            .iter()
            .map(|&b| b as char)
            .collect();
        Ok(Num { literal })
    }
}

/// Decodes one UTF-8 scalar value at the start of `bytes`. `None` for a
/// truncated or invalid sequence — the caller rejects rather than guesses.
fn decode_utf8_char(bytes: &[u8]) -> Option<(char, usize)> {
    let b0 = *bytes.first()?;
    let len = if b0 < 0x80 {
        1
    } else if b0 & 0xE0 == 0xC0 {
        2
    } else if b0 & 0xF0 == 0xE0 {
        3
    } else if b0 & 0xF8 == 0xF0 {
        4
    } else {
        return None;
    };
    let slice = bytes.get(..len)?;
    let s = std::str::from_utf8(slice).ok()?;
    let ch = s.chars().next()?;
    Some((ch, len))
}

/// Decodes standard, padded base64 (RFC 4648 § 4), matching what
/// [`json`](crate::json) encodes (its encoder is private to that module, so
/// this is written independently against the RFC, not derived from it).
/// Rejects bad padding and non-alphabet bytes rather than guessing.
pub(crate) fn base64_decode(s: &str) -> Option<Vec<u8>> {
    let bytes = s.as_bytes();
    if !bytes.len().is_multiple_of(4) {
        return None;
    }
    let n = bytes.len();
    let mut out = Vec::with_capacity(n / 4 * 3);
    for (chunk_index, chunk) in bytes.chunks(4).enumerate() {
        let is_last = (chunk_index + 1) * 4 == n;
        let mut vals = [0u8; 4];
        let mut pad_from = 4usize;
        for (i, &b) in chunk.iter().enumerate() {
            if b == b'=' {
                pad_from = pad_from.min(i);
            } else {
                if pad_from < 4 {
                    // a data character after padding started in this group
                    return None;
                }
                vals[i] = decode_base64_char(b)?;
            }
        }
        if pad_from < 4 && !is_last {
            return None;
        }
        match pad_from {
            4 => {
                out.push(vals[0] << 2 | vals[1] >> 4);
                out.push(vals[1] << 4 | vals[2] >> 2);
                out.push(vals[2] << 6 | vals[3]);
            }
            3 => {
                if vals[2] & 0x03 != 0 {
                    return None; // non-zero bits that padding should have zeroed
                }
                out.push(vals[0] << 2 | vals[1] >> 4);
                out.push(vals[1] << 4 | vals[2] >> 2);
            }
            2 => {
                if vals[1] & 0x0F != 0 {
                    return None;
                }
                out.push(vals[0] << 2 | vals[1] >> 4);
            }
            _ => return None, // a group with 0 or 1 data characters
        }
    }
    Some(out)
}

fn decode_base64_char(b: u8) -> Option<u8> {
    match b {
        b'A'..=b'Z' => Some(b - b'A'),
        b'a'..=b'z' => Some(b - b'a' + 26),
        b'0'..=b'9' => Some(b - b'0' + 52),
        b'+' => Some(62),
        b'/' => Some(63),
        _ => None,
    }
}

// Socket envelopes and client events use the same bounded JSON reader as
// requests.
pub(crate) fn parse_object(line: &[u8]) -> Option<Value> {
    let line = line.strip_suffix(b"\n").unwrap_or(line);
    let line = line.strip_suffix(b"\r").unwrap_or(line);
    if line.len() > MAX_LINE_BYTES {
        return None;
    }
    let mut parser = Parser {
        input: line,
        pos: 0,
    };
    parser.skip_ws();
    let value = parser.parse_value(1).ok()?;
    parser.skip_ws();
    (parser.pos == line.len() && matches!(value, Value::Obj(_))).then_some(value)
}
impl Value {
    pub(crate) fn field(&self, name: &str) -> Option<&Self> {
        match self {
            Self::Obj(fields) => find_field(fields, name),
            _ => None,
        }
    }
    pub(crate) fn text(&self) -> Option<&str> {
        match self {
            Self::Str(s) => Some(s),
            _ => None,
        }
    }
    pub(crate) fn number(&self) -> Option<u64> {
        match self {
            Self::Num(n) => n.as_u64(),
            _ => None,
        }
    }
}
