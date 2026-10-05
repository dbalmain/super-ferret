//! The batch request reader: parses one S1B batch-protocol request line
//! (`docs/S1B.md` § Batch protocol) into a [`Request`].
//!
//! Hand-written per D58 B: the wire format is JSON lines (D57 A), but the
//! `ferret` crate parses only its own small request schema, never a general
//! `serde_json::Value`. [`json`](crate::json) is the matching writer; a
//! byte-valued field here is the inverse of
//! [`json::Object::byte_strings`]'s per-item encoding: a UTF-8 string, or
//! `{"base64":"..."}` with the exact bytes.
//!
//! This reader takes untrusted input (other tools may eventually talk to the
//! daemon over this same codec), so [`parse_request`] never panics and never
//! allocates without a bound tied to the input it has already validated.
//!
//! The batch host calls this reader; the socket host will reuse the codec
//! unchanged.

/// Maximum length of one input line, in bytes, encoded form (S1B's batch
/// protocol limit).
pub const MAX_LINE_BYTES: usize = 1 << 20; // 1 MiB

/// Maximum number of elements in a request's `args` array (S1B's batch
/// protocol limit).
pub const MAX_ARGV_ELEMENTS: usize = 16_384;

/// Maximum JSON nesting depth (objects and arrays both count) accepted while
/// parsing a request. The request schema itself needs depth 3: the request
/// object, its `args` array, and a `{"base64":"..."}` wrapper for one
/// non-UTF-8 element. This bound allows a little more headroom — for an
/// optional nested field S1B adds later — without being large enough to let
/// adversarial input force deep recursion; depth is checked before each
/// descent, so the cost of rejecting over-deep input is O(depth), not O(input
/// size).
pub const MAX_NESTING_DEPTH: u32 = 8;

/// One parsed batch request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Request {
    pub id: String,
    pub op: Op,
    pub args: Vec<Vec<u8>>,
    pub cwd: Option<Vec<u8>>,
    pub limit: Option<u64>,
    pub capabilities: Vec<String>,
    pub child_stdin: Option<ChildStdin>,
    pub start_unix_ns: Option<u64>,
}

/// Child stdin policy, independent of the host's request transport.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ChildStdin {
    Null,
    Inherit,
}

/// The `op` field: which command this request runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Op {
    Search,
    Find,
    Status,
    Reload,
}

impl Op {
    fn from_name(name: &str) -> Option<Self> {
        match name {
            "search" => Some(Op::Search),
            "find" => Some(Op::Find),
            "status" => Some(Op::Status),
            "reload" => Some(Op::Reload),
            _ => None,
        }
    }
}

/// Why [`parse_request`] rejected a line. `id` is set whenever the `id`
/// field was readable before the error, so the host can tag its error
/// response; otherwise the S1B rule is a null id.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RequestError {
    pub id: Option<String>,
    pub kind: RequestErrorKind,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum RequestErrorKind {
    /// The line exceeded [`MAX_LINE_BYTES`].
    LineTooLong,
    /// The line is not one well-formed, non-empty JSON value, or is not a
    /// JSON object, or has trailing bytes after the object, or opens with a
    /// byte-order mark.
    InvalidJson,
    /// An object repeated a key.
    DuplicateKey,
    /// Nesting exceeded [`MAX_NESTING_DEPTH`].
    NestingTooDeep,
    /// A required field was absent.
    MissingField(&'static str),
    /// A field was present with the wrong shape (type, encoding, or, for
    /// `args`, over [`MAX_ARGV_ELEMENTS`] elements).
    InvalidField(&'static str),
    /// `op` was a string but not one of the known operations.
    UnknownOp,
}

impl std::fmt::Display for RequestErrorKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RequestErrorKind::LineTooLong => write!(f, "line exceeds {MAX_LINE_BYTES} bytes"),
            RequestErrorKind::InvalidJson => write!(f, "not a well-formed request object"),
            RequestErrorKind::DuplicateKey => write!(f, "duplicate object key"),
            RequestErrorKind::NestingTooDeep => {
                write!(f, "nesting exceeds depth {MAX_NESTING_DEPTH}")
            }
            RequestErrorKind::MissingField(name) => write!(f, "missing field {name:?}"),
            RequestErrorKind::InvalidField(name) => write!(f, "invalid field {name:?}"),
            RequestErrorKind::UnknownOp => write!(f, "unknown op"),
        }
    }
}

impl std::fmt::Display for RequestError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.id {
            Some(id) => write!(f, "request {id:?}: {}", self.kind),
            None => write!(f, "request (no id): {}", self.kind),
        }
    }
}

impl std::error::Error for RequestError {}

/// Parses one batch request line. Strict JSON: every escape, `\u` with
/// surrogate pairs (a lone surrogate is rejected, never substituted), and
/// numbers, which this schema uses only as unsigned integers (`limit`).
/// Unknown optional fields are ignored; unknown `op` values and missing
/// required fields are errors. Never panics on any input.
pub(crate) fn parse_request(line: &[u8]) -> Result<Request, RequestError> {
    if line.len() > MAX_LINE_BYTES {
        return Err(RequestError {
            id: None,
            kind: RequestErrorKind::LineTooLong,
        });
    }
    // Reject a leading UTF-8 byte-order mark rather than silently stripping
    // or choking on it later.
    if line.starts_with(&[0xEF, 0xBB, 0xBF]) {
        return Err(RequestError {
            id: None,
            kind: RequestErrorKind::InvalidJson,
        });
    }

    let mut parser = Parser {
        input: line,
        pos: 0,
    };
    parser.skip_ws();
    let value = match parser.parse_value(1) {
        Ok(value) => value,
        Err(kind) => {
            return Err(RequestError {
                id: recover_id(line),
                kind,
            });
        }
    };
    parser.skip_ws();
    if parser.pos != line.len() {
        return Err(RequestError {
            id: None,
            kind: RequestErrorKind::InvalidJson,
        });
    }
    let Value::Obj(fields) = value else {
        return Err(RequestError {
            id: None,
            kind: RequestErrorKind::InvalidJson,
        });
    };

    // Read `id` first so every later error can be tagged with it.
    let id = match find_field(&fields, "id") {
        Some(Value::Str(s)) if !s.is_empty() => s.clone(),
        Some(_) => {
            return Err(RequestError {
                id: None,
                kind: RequestErrorKind::InvalidField("id"),
            });
        }
        None => {
            return Err(RequestError {
                id: None,
                kind: RequestErrorKind::MissingField("id"),
            });
        }
    };

    build_request(&fields, id.clone()).map_err(|kind| RequestError { id: Some(id), kind })
}

/// Recovers a validated leading id even when a later part of the line is
/// malformed or exceeds the host's retained input bound.
pub(crate) fn recover_id(line: &[u8]) -> Option<String> {
    let mut parser = Parser {
        input: line,
        pos: 0,
    };
    parser.skip_ws();
    if parser.peek() != Some(b'{') {
        return None;
    }
    parser.pos += 1;
    parser.skip_ws();
    if parser.peek() != Some(b'"') || parser.parse_string().ok()? != "id" {
        return None;
    }
    parser.skip_ws();
    if parser.peek() != Some(b':') {
        return None;
    }
    parser.pos += 1;
    match parser.parse_value(2).ok()? {
        Value::Str(id) if !id.is_empty() => Some(id),
        _ => None,
    }
}

fn build_request(fields: &[(String, Value)], id: String) -> Result<Request, RequestErrorKind> {
    let op = match find_field(fields, "op") {
        Some(Value::Str(s)) => Op::from_name(s).ok_or(RequestErrorKind::UnknownOp)?,
        Some(_) => return Err(RequestErrorKind::InvalidField("op")),
        None => return Err(RequestErrorKind::MissingField("op")),
    };

    let args = match find_field(fields, "args") {
        Some(Value::Arr(items)) => {
            if items.len() > MAX_ARGV_ELEMENTS {
                return Err(RequestErrorKind::InvalidField("args"));
            }
            let mut out = Vec::with_capacity(items.len());
            for item in items {
                out.push(byte_value(item).ok_or(RequestErrorKind::InvalidField("args"))?);
            }
            out
        }
        Some(_) => return Err(RequestErrorKind::InvalidField("args")),
        // Control requests take no arguments; queries must say theirs.
        None if matches!(op, Op::Status | Op::Reload) => Vec::new(),
        None => return Err(RequestErrorKind::MissingField("args")),
    };

    // An explicit JSON `null` is the same as the field being absent — that
    // is what `json.rs`'s `opt_bytes`/`opt_int` write for `None`, and the
    // round-trip property relies on it.
    let cwd = match find_field(fields, "cwd") {
        Some(Value::Null) | None => None,
        Some(v) => Some(byte_value(v).ok_or(RequestErrorKind::InvalidField("cwd"))?),
    };

    let limit = match find_field(fields, "limit") {
        Some(Value::Null) | None => None,
        Some(Value::Num(n)) => Some(n.as_u64().ok_or(RequestErrorKind::InvalidField("limit"))?),
        Some(_) => return Err(RequestErrorKind::InvalidField("limit")),
    };

    let capabilities = match find_field(fields, "capabilities") {
        None | Some(Value::Null) => Vec::new(),
        Some(Value::Arr(values)) => values
            .iter()
            .map(|value| match value {
                Value::Str(name) => Ok(name.clone()),
                _ => Err(RequestErrorKind::InvalidField("capabilities")),
            })
            .collect::<Result<Vec<_>, _>>()?,
        Some(_) => return Err(RequestErrorKind::InvalidField("capabilities")),
    };
    let child_stdin = match find_field(fields, "child_stdin") {
        None | Some(Value::Null) => None,
        Some(Value::Str(name)) if name == "null" => Some(ChildStdin::Null),
        Some(Value::Str(name)) if name == "inherit" => Some(ChildStdin::Inherit),
        Some(_) => return Err(RequestErrorKind::InvalidField("child_stdin")),
    };

    Ok(Request {
        id,
        op,
        args,
        cwd,
        limit,
        capabilities,
        child_stdin,
        start_unix_ns: match find_field(fields, "start_unix_ns") {
            None | Some(Value::Null) => None,
            Some(Value::Num(n)) => Some(
                n.as_u64()
                    .ok_or(RequestErrorKind::InvalidField("start_unix_ns"))?,
            ),
            _ => return Err(RequestErrorKind::InvalidField("start_unix_ns")),
        },
    })
}

fn find_field<'a>(fields: &'a [(String, Value)], key: &str) -> Option<&'a Value> {
    fields.iter().find(|(k, _)| k == key).map(|(_, v)| v)
}

/// Decodes a byte-valued field: a UTF-8 string as its bytes, or
/// `{"base64":"..."}` as the decoded bytes. Anything else is `None`.
fn byte_value(value: &Value) -> Option<Vec<u8>> {
    match value {
        Value::Str(s) => Some(s.clone().into_bytes()),
        Value::Obj(fields) => {
            if fields.len() == 1 && fields[0].0 == "base64" {
                match &fields[0].1 {
                    Value::Str(s) => base64_decode(s),
                    _ => None,
                }
            } else {
                None
            }
        }
        _ => None,
    }
}

// ── JSON values ──

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

/// A JSON number, kept unevaluated beyond its grammar shape; callers convert
/// to the type their field needs (here, only `as_u64`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Num {
    negative: bool,
    integer: String,
    has_frac_or_exp: bool,
}

impl Num {
    pub(crate) fn as_u64(&self) -> Option<u64> {
        if self.negative || self.has_frac_or_exp {
            return None;
        }
        self.integer.parse().ok()
    }
}

struct Parser<'a> {
    input: &'a [u8],
    pos: usize,
}

impl<'a> Parser<'a> {
    fn peek(&self) -> Option<u8> {
        self.input.get(self.pos).copied()
    }

    fn skip_ws(&mut self) {
        while matches!(self.peek(), Some(b' ' | b'\t' | b'\n' | b'\r')) {
            self.pos += 1;
        }
    }

    fn parse_value(&mut self, depth: u32) -> Result<Value, RequestErrorKind> {
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

    fn parse_string(&mut self) -> Result<String, RequestErrorKind> {
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
        let negative = if self.peek() == Some(b'-') {
            self.pos += 1;
            true
        } else {
            false
        };
        let int_start = self.pos;
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
        // All bytes in this range are ASCII digits by construction above, so
        // a byte-by-byte cast to `char` is exact and needs no fallible
        // UTF-8 decode.
        let integer: String = self.input[int_start..self.pos]
            .iter()
            .map(|&b| b as char)
            .collect();

        let mut has_frac_or_exp = false;
        if self.peek() == Some(b'.') {
            has_frac_or_exp = true;
            self.pos += 1;
            if !matches!(self.peek(), Some(b'0'..=b'9')) {
                return Err(RequestErrorKind::InvalidJson);
            }
            while matches!(self.peek(), Some(b'0'..=b'9')) {
                self.pos += 1;
            }
        }
        if matches!(self.peek(), Some(b'e' | b'E')) {
            has_frac_or_exp = true;
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
        Ok(Num {
            negative,
            integer,
            has_frac_or_exp,
        })
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

#[cfg(test)]
mod tests {
    use super::*;

    // ── table tests ──

    fn ok(line: &[u8]) -> Request {
        parse_request(line).unwrap_or_else(|e| panic!("expected Ok, got {e:?} for {line:?}"))
    }

    fn err_kind(line: &[u8]) -> RequestErrorKind {
        parse_request(line).unwrap_err().kind
    }

    #[test]
    fn minimal_request_parses() {
        let r = ok(br#"{"id":"a","op":"search","args":[]}"#);
        assert_eq!(
            r,
            Request {
                id: "a".into(),
                op: Op::Search,
                args: vec![],
                cwd: None,
                limit: None,
                capabilities: Vec::new(),
                child_stdin: None,
                start_unix_ns: None,
            }
        );
    }

    #[test]
    fn capabilities_and_child_stdin_are_typed_and_unknown_options_remain_ignored() {
        let request = ok(br#"{"id":"a","op":"find","args":[],"capabilities":["local-effects","interactive","future"],"child_stdin":"inherit","future":{"nested":[true]}}"#);
        assert_eq!(
            request.capabilities,
            ["local-effects", "interactive", "future"]
        );
        assert_eq!(request.child_stdin, Some(ChildStdin::Inherit));
        assert_eq!(
            ok(br#"{"id":"a","op":"find","args":[],"child_stdin":"null"}"#).child_stdin,
            Some(ChildStdin::Null)
        );
        for (extra, field) in [
            (r#""capabilities":"local-effects""#, "capabilities"),
            (r#""capabilities":[true]"#, "capabilities"),
            (r#""child_stdin":"pipe""#, "child_stdin"),
            (r#""child_stdin":false"#, "child_stdin"),
        ] {
            let line = format!(r#"{{"id":"a","op":"find","args":[],{extra}}}"#);
            assert_eq!(
                err_kind(line.as_bytes()),
                RequestErrorKind::InvalidField(field)
            );
        }
    }

    #[test]
    fn every_escape_decodes() {
        let line = br#"{"id":"a","op":"search","args":["\"\\\/\b\f\n\r\t\u0041"]}"#;
        let r = ok(line);
        assert_eq!(r.args, vec![b"\"\\/\x08\x0c\n\r\tA".to_vec()]);
    }

    #[test]
    fn surrogate_pair_combines_to_one_codepoint() {
        // U+1F600 GRINNING FACE, as a UTF-16 surrogate pair.
        let line = br#"{"id":"a","op":"search","args":["\ud83d\ude00"]}"#;
        let r = ok(line);
        assert_eq!(r.args, vec!["\u{1F600}".as_bytes().to_vec()]);
    }

    #[test]
    fn lone_high_surrogate_is_rejected_not_replaced_with_u_fffd() {
        // The discriminating case: a plausible-but-wrong reader accepts this
        // and emits U+FFFD. Ours must reject it outright.
        let line = br#"{"id":"a","op":"search","args":["\ud800"]}"#;
        assert_eq!(err_kind(line), RequestErrorKind::InvalidJson);
    }

    #[test]
    fn lone_low_surrogate_is_rejected() {
        let line = br#"{"id":"a","op":"search","args":["\udc00"]}"#;
        assert_eq!(err_kind(line), RequestErrorKind::InvalidJson);
    }

    #[test]
    fn invalid_utf8_inside_a_string_is_rejected() {
        let mut line = br#"{"id":"a","op":"search","args":["#.to_vec();
        line.push(b'"');
        line.push(0xFF);
        line.extend_from_slice(br#""]}"#);
        assert_eq!(err_kind(&line), RequestErrorKind::InvalidJson);
    }

    #[test]
    fn base64_with_bad_padding_is_rejected() {
        let line = br#"{"id":"a","op":"search","args":[{"base64":"A==="}]}"#;
        assert_eq!(err_kind(line), RequestErrorKind::InvalidField("args"));
    }

    #[test]
    fn base64_decodes_exact_bytes() {
        let line = br#"{"id":"a","op":"search","args":[{"base64":"/w=="}]}"#;
        let r = ok(line);
        assert_eq!(r.args, vec![vec![0xffu8]]);
    }

    #[test]
    fn duplicate_key_is_rejected() {
        let line = br#"{"id":"a","id":"b","op":"search","args":[]}"#;
        assert_eq!(err_kind(line), RequestErrorKind::DuplicateKey);
    }

    #[test]
    fn unknown_op_is_rejected() {
        let line = br#"{"id":"a","op":"frobnicate","args":[]}"#;
        let e = parse_request(line).unwrap_err();
        assert_eq!(e.id, Some("a".into()));
        assert_eq!(e.kind, RequestErrorKind::UnknownOp);
    }

    #[test]
    fn control_requests_need_no_args_but_queries_do() {
        let status = parse_request(br#"{"id":"s","op":"status"}"#).unwrap();
        assert!(status.args.is_empty());
        let e = parse_request(br#"{"id":"q","op":"search"}"#).unwrap_err();
        assert_eq!(e.kind, RequestErrorKind::MissingField("args"));
    }

    #[test]
    fn missing_id_has_no_tagged_id() {
        let line = br#"{"op":"search","args":[]}"#;
        let e = parse_request(line).unwrap_err();
        assert_eq!(e.id, None);
        assert_eq!(e.kind, RequestErrorKind::MissingField("id"));
    }

    #[test]
    fn id_is_recovered_when_a_later_field_is_bad() {
        let line = br#"{"id":"keep-me","op":"search","args":"not-an-array"}"#;
        let e = parse_request(line).unwrap_err();
        assert_eq!(e.id, Some("keep-me".into()));
        assert_eq!(e.kind, RequestErrorKind::InvalidField("args"));
    }

    #[test]
    fn line_exactly_at_the_size_limit_parses() {
        // Pad with a long id so the whole line is exactly MAX_LINE_BYTES.
        let prefix = br#"{"id":""#;
        let suffix = br#"","op":"search","args":[]}"#;
        let pad_len = MAX_LINE_BYTES - prefix.len() - suffix.len();
        let mut line = prefix.to_vec();
        line.extend(std::iter::repeat_n(b'a', pad_len));
        line.extend_from_slice(suffix);
        assert_eq!(line.len(), MAX_LINE_BYTES);
        ok(&line);
    }

    #[test]
    fn line_one_byte_over_the_size_limit_is_rejected() {
        let prefix = br#"{"id":""#;
        let suffix = br#"","op":"search","args":[]}"#;
        let pad_len = MAX_LINE_BYTES - prefix.len() - suffix.len() + 1;
        let mut line = prefix.to_vec();
        line.extend(std::iter::repeat_n(b'a', pad_len));
        line.extend_from_slice(suffix);
        assert_eq!(line.len(), MAX_LINE_BYTES + 1);
        assert_eq!(err_kind(&line), RequestErrorKind::LineTooLong);
    }

    #[test]
    fn argv_at_the_bound_parses() {
        let args = "\"x\",".repeat(MAX_ARGV_ELEMENTS - 1) + "\"x\"";
        let line = format!(r#"{{"id":"a","op":"search","args":[{args}]}}"#);
        let r = ok(line.as_bytes());
        assert_eq!(r.args.len(), MAX_ARGV_ELEMENTS);
    }

    #[test]
    fn argv_one_over_the_bound_is_rejected() {
        let args = "\"x\",".repeat(MAX_ARGV_ELEMENTS) + "\"x\"";
        let line = format!(r#"{{"id":"a","op":"search","args":[{args}]}}"#);
        assert_eq!(
            err_kind(line.as_bytes()),
            RequestErrorKind::InvalidField("args")
        );
    }

    /// Builds `{"id":"a","op":"search","args":[],"extra":<nested arrays
    /// `depth` deep>}`. `extra` is unknown and ignored, but its value must
    /// still parse as valid, depth-bounded JSON — exercising the bound
    /// independently of the schema's own shape.
    fn nested_extra(depth: u32) -> Vec<u8> {
        let mut extra = String::from("0");
        // `depth` here counts the levels of array nesting on top of the
        // request object itself (depth 1), matching parse_value's depth
        // argument: the object is depth 1, so `extra`'s outermost array is
        // depth 2, and each further nesting level adds one.
        for _ in 0..depth {
            extra = format!("[{extra}]");
        }
        format!(r#"{{"id":"a","op":"search","args":[],"extra":{extra}}}"#).into_bytes()
    }

    #[test]
    fn nesting_at_the_bound_parses() {
        ok(&nested_extra(MAX_NESTING_DEPTH - 1));
    }

    #[test]
    fn nesting_one_level_deeper_is_rejected() {
        assert_eq!(
            err_kind(&nested_extra(MAX_NESTING_DEPTH)),
            RequestErrorKind::NestingTooDeep
        );
    }

    #[test]
    fn trailing_garbage_after_the_object_is_rejected() {
        let line = br#"{"id":"a","op":"search","args":[]} garbage"#;
        assert_eq!(err_kind(line), RequestErrorKind::InvalidJson);
    }

    #[test]
    fn leading_byte_order_mark_is_rejected() {
        let mut line = vec![0xEF, 0xBB, 0xBF];
        line.extend_from_slice(br#"{"id":"a","op":"search","args":[]}"#);
        assert_eq!(err_kind(&line), RequestErrorKind::InvalidJson);
    }

    #[test]
    fn whitespace_everywhere_legal_is_accepted() {
        let line =
            b"  \t\n{ \"id\" : \"a\" ,\n\"op\"\t:\"search\",\"args\"  :[  \"x\" ,  \"y\"  ]  }  \n";
        let r = ok(line);
        assert_eq!(r.args, vec![b"x".to_vec(), b"y".to_vec()]);
    }

    #[test]
    fn cwd_and_limit_round_trip_as_options() {
        let r = ok(br#"{"id":"a","op":"find","args":[],"cwd":"/work","limit":20}"#);
        assert_eq!(r.cwd, Some(b"/work".to_vec()));
        assert_eq!(r.limit, Some(20));
    }

    #[test]
    fn negative_limit_is_rejected() {
        let line = br#"{"id":"a","op":"search","args":[],"limit":-1}"#;
        assert_eq!(err_kind(line), RequestErrorKind::InvalidField("limit"));
    }

    #[test]
    fn fractional_limit_is_rejected() {
        let line = br#"{"id":"a","op":"search","args":[],"limit":1.5}"#;
        assert_eq!(err_kind(line), RequestErrorKind::InvalidField("limit"));
    }

    // ── round-trip property, driving the real json.rs writer ──

    /// Seeded xorshift64*, matching the idiom in
    /// `crates/ferret-verify/src/tests.rs`.
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

        fn bool(&mut self) -> bool {
            self.next() & 1 == 1
        }

        /// Random bytes, including sequences that are not valid UTF-8.
        fn bytes(&mut self, max_len: usize) -> Vec<u8> {
            let len = self.below(max_len + 1);
            (0..len).map(|_| (self.next() & 0xff) as u8).collect()
        }

        fn id(&mut self) -> String {
            let len = 1 + self.below(12);
            (0..len)
                .map(|_| (b'a' + (self.below(26) as u8)) as char)
                .collect()
        }

        fn op(&mut self) -> Op {
            match self.below(4) {
                0 => Op::Search,
                1 => Op::Find,
                2 => Op::Status,
                _ => Op::Reload,
            }
        }

        fn request(&mut self) -> Request {
            let args_len = self.below(6);
            let args = (0..args_len).map(|_| self.bytes(24)).collect();
            Request {
                id: self.id(),
                op: self.op(),
                args,
                cwd: self.bool().then(|| self.bytes(24)),
                limit: self.bool().then(|| self.next() % 1_000_000),
                capabilities: if self.bool() {
                    vec!["local-effects".into(), "interactive".into()]
                } else {
                    vec![]
                },
                start_unix_ns: None,
                child_stdin: self.bool().then(|| {
                    if self.bool() {
                        ChildStdin::Null
                    } else {
                        ChildStdin::Inherit
                    }
                }),
            }
        }
    }

    /// Encodes a [`Request`] with the real production writer
    /// ([`json::Object`]), not a copy of the parser's logic — this is the
    /// writer side of the same schema the parser reads.
    fn encode(req: &Request) -> Vec<u8> {
        let mut out = Vec::new();
        let mut obj = crate::json::Object::new(&mut out);
        obj.str("id", &req.id);
        obj.str(
            "op",
            match req.op {
                Op::Search => "search",
                Op::Find => "find",
                Op::Status => "status",
                Op::Reload => "reload",
            },
        );
        obj.byte_strings("args", req.args.iter().map(|a| a.as_slice()));
        obj.opt_byte_value("cwd", req.cwd.as_deref());
        obj.opt_int("limit", req.limit.map(|l| l as i128));
        obj.byte_strings(
            "capabilities",
            req.capabilities.iter().map(String::as_bytes),
        );
        if let Some(policy) = req.child_stdin {
            obj.str(
                "child_stdin",
                match policy {
                    ChildStdin::Null => "null",
                    ChildStdin::Inherit => "inherit",
                },
            );
        }
        obj.end();
        out
    }

    #[test]
    fn round_trip_through_the_real_writer() {
        let mut rng = Rng(0xfeed_face_dead_beef);
        for _ in 0..4000 {
            let req = rng.request();
            let encoded = encode(&req);
            let parsed = parse_request(&encoded)
                .unwrap_or_else(|e| panic!("{e:?} on {:?}", String::from_utf8_lossy(&encoded)));
            assert_eq!(parsed, req, "line: {:?}", String::from_utf8_lossy(&encoded));
        }
    }

    // ── mutation fuzz ──

    /// Flips, inserts, deletes or truncates bytes in `line` and returns the
    /// result. May produce anything, including invalid UTF-8 and empty
    /// input.
    fn mutate(rng: &mut Rng, line: &[u8]) -> Vec<u8> {
        let mut out = line.to_vec();
        if out.is_empty() {
            out.push(rng.next() as u8);
            return out;
        }
        match rng.below(4) {
            0 => {
                let i = rng.below(out.len());
                out[i] = (rng.next() & 0xff) as u8;
            }
            1 => {
                let i = rng.below(out.len() + 1);
                out.insert(i, (rng.next() & 0xff) as u8);
            }
            2 => {
                let i = rng.below(out.len());
                out.remove(i);
            }
            _ => {
                let i = rng.below(out.len() + 1);
                out.truncate(i);
            }
        }
        out
    }

    fn mutation_fuzz(iterations: u64) {
        let mut rng = Rng(0x0dba_11ca_fef0_0d42);
        for i in 0..iterations {
            let mut req_rng = Rng(0x5eed ^ i);
            let base = encode(&req_rng.request());
            let mutated = mutate(&mut rng, &base);
            // Rejection is always fine; only a panic is a bug. An accepted
            // mutation must re-encode to a request that parses equal: no
            // mutation may be accepted into something the writer itself
            // couldn't produce.
            if let Ok(req) = parse_request(&mutated) {
                let re_encoded = encode(&req);
                let reparsed =
                    parse_request(&re_encoded).expect("re-encoding an accepted request must parse");
                assert_eq!(reparsed, req);
            }
        }
    }

    #[test]
    fn mutation_fuzz_never_panics() {
        mutation_fuzz(2_000);
    }

    /// Longer mutation fuzz run, for ad hoc use:
    /// `FERRET_PROTOCOL_FUZZ_ITERS=200000 cargo test --workspace -- --ignored
    /// mutation_fuzz_long`.
    #[test]
    #[ignore]
    fn mutation_fuzz_long() {
        let iterations = std::env::var("FERRET_PROTOCOL_FUZZ_ITERS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(200_000);
        mutation_fuzz(iterations);
    }
}
