//! The batch request reader: parses one S1B batch-protocol request line
//! (`docs/S1B.md` § Batch protocol) into a [`Request`].
//!
//! Hand-written per D58 B: the wire format is JSON lines (D57 A), but the
//! `ferret` crate parses only its own small request schema, never a general
//! `serde_json::Value`. [`json`](crate::json) is the matching writer; a
//! byte-valued field here is the inverse of
//! [`crate::json::Object::byte_strings`]'s per-item encoding: a UTF-8 string,
//! or `{"base64":"..."}` with the exact bytes.
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

mod value;

use value::Parser;
pub(crate) use value::{Value, base64_decode, parse_object};

/// One parsed batch request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Request {
    pub id: String,
    pub op: Op,
    pub grep: Option<GrepOptions>,
    pub args: Vec<Vec<u8>>,
    pub cwd: Option<Vec<u8>>,
    pub limit: Option<u64>,
    pub capabilities: Vec<String>,
    pub child_stdin: Option<ChildStdin>,
    pub start_unix_ns: Option<u64>,
}

/// Client-resolved grep environment. Pattern-file bytes travel with the
/// request so the socket host never opens client-relative files or stdin.
/// `temp_dir` is the client's absolute temporary directory, where
/// before-context windows spill instead of the server's.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct GrepOptions {
    pub color: bool,
    pub heading: bool,
    pub numbers: bool,
    pub tty: bool,
    pub threads: usize,
    pub pattern_files: Vec<Vec<u8>>,
    pub temp_dir: Vec<u8>,
}

impl GrepOptions {
    pub(crate) fn encode(&self, object: &mut crate::json::Object<'_>) {
        object.object("grep", |o| {
            o.bool("color", self.color)
                .bool("heading", self.heading)
                .bool("numbers", self.numbers)
                .bool("tty", self.tty)
                .int("threads", self.threads as u64)
                .byte_strings(
                    "pattern_files",
                    self.pattern_files.iter().map(Vec::as_slice),
                )
                .opt_byte_value("temp_dir", Some(&self.temp_dir));
        });
    }
    fn parse(value: &Value) -> Option<Self> {
        let boolean = |name| match value.field(name)? {
            Value::Bool(b) => Some(*b),
            _ => None,
        };
        let Value::Arr(files) = value.field("pattern_files")? else {
            return None;
        };
        let threads = usize::try_from(value.field("threads")?.number()?).ok()?;
        // The capture ceiling is (2T + 1) * 4 MiB. Reject counts whose
        // size arithmetic is nonsense even though the host grants fewer.
        threads
            .checked_mul(2)?
            .checked_add(1)?
            .checked_mul(4 << 20)?;
        let temp_dir = byte_value(value.field("temp_dir")?)?;
        (threads > 0 && temp_dir.first() == Some(&b'/')).then_some(Self {
            color: boolean("color")?,
            heading: boolean("heading")?,
            numbers: boolean("numbers")?,
            tty: boolean("tty")?,
            threads,
            pattern_files: files.iter().map(byte_value).collect::<Option<_>>()?,
            temp_dir,
        })
    }
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
    Grep,
    Status,
    Reload,
    Index,
    RootsRemove,
}

impl Op {
    fn from_name(name: &str) -> Option<Self> {
        match name {
            "search" => Some(Op::Search),
            "find" => Some(Op::Find),
            "grep" => Some(Op::Grep),
            "status" => Some(Op::Status),
            "reload" => Some(Op::Reload),
            "index" => Some(Op::Index),
            "roots-remove" => Some(Op::RootsRemove),
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

/// A request's byte-string arguments, which every op but a control request
/// must give.
fn request_args(fields: &[(String, Value)], op: &Op) -> Result<Vec<Vec<u8>>, RequestErrorKind> {
    match find_field(fields, "args") {
        Some(Value::Arr(items)) => {
            if items.len() > MAX_ARGV_ELEMENTS {
                return Err(RequestErrorKind::InvalidField("args"));
            }
            let mut out = Vec::with_capacity(items.len());
            for item in items {
                out.push(byte_value(item).ok_or(RequestErrorKind::InvalidField("args"))?);
            }
            Ok(out)
        }
        Some(_) => Err(RequestErrorKind::InvalidField("args")),
        // Control requests take no arguments; queries must say theirs.
        None if matches!(op, Op::Status | Op::Reload) => Ok(Vec::new()),
        None => Err(RequestErrorKind::MissingField("args")),
    }
}

/// Optional unsigned field `name`; a JSON `null` is the same as absent.
fn optional_u64(
    fields: &[(String, Value)],
    name: &'static str,
) -> Result<Option<u64>, RequestErrorKind> {
    match find_field(fields, name) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Num(n)) => n
            .as_u64()
            .map(Some)
            .ok_or(RequestErrorKind::InvalidField(name)),
        Some(_) => Err(RequestErrorKind::InvalidField(name)),
    }
}

fn build_request(fields: &[(String, Value)], id: String) -> Result<Request, RequestErrorKind> {
    let op = match find_field(fields, "op") {
        Some(Value::Str(s)) => Op::from_name(s).ok_or(RequestErrorKind::UnknownOp)?,
        Some(_) => return Err(RequestErrorKind::InvalidField("op")),
        None => return Err(RequestErrorKind::MissingField("op")),
    };

    let args = request_args(fields, &op)?;

    // An explicit JSON `null` is the same as the field being absent — that
    // is what `json.rs`'s `opt_bytes`/`opt_int` write for `None`, and the
    // round-trip property relies on it.
    let cwd = match find_field(fields, "cwd") {
        Some(Value::Null) | None => None,
        Some(v) => Some(byte_value(v).ok_or(RequestErrorKind::InvalidField("cwd"))?),
    };

    let limit = optional_u64(fields, "limit")?;

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
        grep: match find_field(fields, "grep") {
            None | Some(Value::Null) => None,
            Some(value) => {
                Some(GrepOptions::parse(value).ok_or(RequestErrorKind::InvalidField("grep"))?)
            }
        },
        args,
        cwd,
        limit,
        capabilities,
        child_stdin,
        start_unix_ns: optional_u64(fields, "start_unix_ns")?,
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

#[cfg(test)]
mod tests;
