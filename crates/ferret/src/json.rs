//! JSON output: one object per line, for `search --json` and the query log.
//!
//! Hand-written because the surface is small: objects of strings, integers,
//! nulls, arrays of strings and nested objects, written straight into a buffer.
//!
//! **Bytes that may not be UTF-8** (paths, query atoms) are written by
//! [`Object::bytes`] as two fields. `KEY` is always present and is the text
//! with each invalid sequence replaced by U+FFFD, so a consumer that only
//! reads strings still gets something readable. When, and only when, the
//! bytes are not valid UTF-8, `KEY_base64` follows with the exact bytes in
//! standard, padded base64 (RFC 4648 § 4). A consumer that needs the real
//! path reads `KEY_base64` when it is present and `KEY` otherwise.

use std::fmt::Write as _;

/// One JSON object being written into `out`. [`Object::end`] closes it.
pub struct Object<'a> {
    out: &'a mut Vec<u8>,
    empty: bool,
}

impl<'a> Object<'a> {
    /// Opens an object at the end of `out`.
    pub fn new(out: &'a mut Vec<u8>) -> Self {
        out.push(b'{');
        Self { out, empty: true }
    }

    fn key(&mut self, key: &str) {
        if !self.empty {
            self.out.push(b',');
        }
        self.empty = false;
        string(self.out, key);
        self.out.push(b':');
    }

    /// `"key":"value"`.
    pub fn str(&mut self, key: &str, value: &str) -> &mut Self {
        self.key(key);
        string(self.out, value);
        self
    }

    /// `"key":"text"`, plus `"key_base64":"…"` when `value` is not UTF-8;
    /// see the module doc.
    pub fn bytes(&mut self, key: &str, value: &[u8]) -> &mut Self {
        self.key(key);
        string(self.out, &String::from_utf8_lossy(value));
        if std::str::from_utf8(value).is_err() {
            self.key(&format!("{key}_base64"));
            self.out.push(b'"');
            base64(self.out, value);
            self.out.push(b'"');
        }
        self
    }

    /// `"key":[…]`, each item written as by [`Object::bytes`] but inside an
    /// array: a string, or `{"base64":"…"}` for an item that is not UTF-8.
    pub fn byte_strings<'b>(
        &mut self,
        key: &str,
        items: impl IntoIterator<Item = &'b [u8]>,
    ) -> &mut Self {
        self.key(key);
        self.out.push(b'[');
        for (i, item) in items.into_iter().enumerate() {
            if i > 0 {
                self.out.push(b',');
            }
            match std::str::from_utf8(item) {
                Ok(text) => string(self.out, text),
                Err(_) => {
                    self.out.extend_from_slice(b"{\"base64\":\"");
                    base64(self.out, item);
                    self.out.extend_from_slice(b"\"}");
                }
            }
        }
        self.out.push(b']');
        self
    }

    /// `"key":N`.
    pub fn int(&mut self, key: &str, value: impl Into<i128>) -> &mut Self {
        self.key(key);
        let _ = write!(Utf8(self.out), "{}", value.into());
        self
    }

    /// `"key":null`, or the number.
    pub fn opt_int(&mut self, key: &str, value: Option<impl Into<i128>>) -> &mut Self {
        match value {
            Some(v) => self.int(key, v),
            None => {
                self.key(key);
                self.out.extend_from_slice(b"null");
                self
            }
        }
    }

    /// `"key":{…}`, filled in by `fill`.
    pub fn object(&mut self, key: &str, fill: impl FnOnce(&mut Object<'_>)) -> &mut Self {
        self.key(key);
        let mut inner = Object::new(self.out);
        fill(&mut inner);
        inner.end();
        self
    }

    /// Closes the object.
    pub fn end(self) {
        self.out.push(b'}');
    }
}

/// `fmt::Write` into a byte buffer; only ever given ASCII here.
struct Utf8<'a>(&'a mut Vec<u8>);

impl std::fmt::Write for Utf8<'_> {
    fn write_str(&mut self, s: &str) -> std::fmt::Result {
        self.0.extend_from_slice(s.as_bytes());
        Ok(())
    }
}

/// A JSON string literal: `"`, `\` and control characters escaped.
fn string(out: &mut Vec<u8>, text: &str) {
    out.push(b'"');
    for &b in text.as_bytes() {
        match b {
            b'"' => out.extend_from_slice(b"\\\""),
            b'\\' => out.extend_from_slice(b"\\\\"),
            b'\n' => out.extend_from_slice(b"\\n"),
            b'\r' => out.extend_from_slice(b"\\r"),
            b'\t' => out.extend_from_slice(b"\\t"),
            0..0x20 => {
                let _ = write!(Utf8(out), "\\u{b:04x}");
            }
            _ => out.push(b),
        }
    }
    out.push(b'"');
}

/// Standard base64 with padding.
fn base64(out: &mut Vec<u8>, bytes: &[u8]) {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    for chunk in bytes.chunks(3) {
        let n = chunk
            .iter()
            .enumerate()
            .fold(0u32, |n, (i, &b)| n | u32::from(b) << (16 - 8 * i));
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(ALPHABET[(n >> (18 - 6 * i) & 63) as usize]);
            } else {
                out.push(b'=');
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn object(fill: impl FnOnce(&mut Object<'_>)) -> String {
        let mut out = Vec::new();
        let mut object = Object::new(&mut out);
        fill(&mut object);
        object.end();
        String::from_utf8(out).unwrap()
    }

    #[test]
    fn base64_matches_rfc_4648_vectors() {
        for (input, expected) in [
            ("", ""),
            ("f", "Zg=="),
            ("fo", "Zm8="),
            ("foo", "Zm9v"),
            ("foob", "Zm9vYg=="),
            ("fooba", "Zm9vYmE="),
            ("foobar", "Zm9vYmFy"),
        ] {
            let mut out = Vec::new();
            base64(&mut out, input.as_bytes());
            assert_eq!(out, expected.as_bytes(), "{input:?}");
        }
        let mut out = Vec::new();
        base64(&mut out, &[0xff, 0xfe, 0x00]);
        assert_eq!(out, b"//4A");
    }

    #[test]
    fn strings_escape_quotes_backslashes_and_controls() {
        assert_eq!(
            object(|o| {
                o.str("k", "a\"b\\c\nd\u{1}é");
            }),
            r#"{"k":"a\"b\\c\nd\u0001é"}"#
        );
    }

    #[test]
    fn bytes_add_base64_only_when_not_utf8() {
        assert_eq!(
            object(|o| {
                o.bytes("path", b"/a/b");
            }),
            r#"{"path":"/a/b"}"#
        );
        assert_eq!(
            object(|o| {
                o.bytes("path", b"/a\xff");
            }),
            r#"{"path":"/a�","path_base64":"L2H/"}"#
        );
    }

    #[test]
    fn numbers_nulls_arrays_and_nesting() {
        assert_eq!(
            object(|o| {
                o.int("a", -3i64)
                    .opt_int("b", None::<u32>)
                    .byte_strings("d", [&b"x"[..], b"\xff"])
                    .object("e", |inner| {
                        inner.int("f", 1u8);
                    });
            }),
            r#"{"a":-3,"b":null,"d":["x",{"base64":"/w=="}],"e":{"f":1}}"#
        );
    }
}
