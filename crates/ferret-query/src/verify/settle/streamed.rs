use std::io::Read;
use std::time::{Duration, UNIX_EPOCH};

use ferret_catalog::NameId;
use ferret_verify::TextMatcher;

use super::{Hand, Outcome, stream, streams, verify};
use crate::{Query, ReadRequest, ReadVersion};

/// Hands out `bytes` 1 to `most` at a time.
struct Dribble<'a> {
    bytes: &'a [u8],
    most: usize,
    reads: usize,
}

impl Read for Dribble<'_> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        self.reads += 1;
        let n = buf
            .len()
            .min(1 + self.reads % self.most)
            .min(self.bytes.len());
        buf[..n].copy_from_slice(&self.bytes[..n]);
        self.bytes = &self.bytes[n..];
        Ok(n)
    }
}

/// Random documents from pieces that include CRLF, NUL, two- and
/// three-byte characters (which the dribbled reads split), and
/// malformed UTF-8: a lone continuation byte, a truncated lead, and
/// 0xff.
fn documents() -> Vec<Vec<u8>> {
    let alphabet: [&[u8]; 16] = [
        b"a",
        b"o",
        b" ",
        b"f",
        b"\n",
        b"k",
        b"1",
        b"_",
        b"\n",
        b"\r\n",
        b"\0",
        "\u{e9}".as_bytes(),
        "\u{65e5}".as_bytes(),
        b"\xa9",
        b"\xe6\x97",
        b"\xff",
    ];
    let mut seed = 0x2545_f491_u64;
    let mut docs: Vec<Vec<u8>> = (0..300)
        .map(|_| {
            seed = seed.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1);
            let len = (seed >> 33) as usize % 300;
            (0..len)
                .flat_map(|_| {
                    seed = seed.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1);
                    alphabet[(seed >> 33) as usize % alphabet.len()]
                })
                .copied()
                .collect()
        })
        .collect();
    let mut long = vec![b'a'; 70_000];
    long.extend_from_slice(b"o foo_\n\nk1 a");
    docs.push(long);
    docs.extend([
        b"".to_vec(),
        b"\n".to_vec(),
        b"foo".to_vec(),
        b"a\n\n".to_vec(),
    ]);
    docs
}

fn answers(outcome: Outcome) -> (Vec<Option<usize>>, ferret_verify::MatchStats) {
    match outcome {
        Outcome::Read { answers, work, .. } => (answers, work.matching),
        _ => panic!("not read"),
    }
}

#[test]
fn streamed_answers_are_the_whole_documents() {
    let patterns = [
        "foo",
        "^foo",
        "foo$",
        "^$",
        r"\bfo",
        r"o\b",
        r"\Bo",
        "^a*$",
        "k1",
        "(?i)K",
        "[a-z]+1",
        "f.o",
        "^",
        "$",
        "a{3}",
        "_$",
        r"^\b",
        r"\b$",
        "(?-u)o_",
        r"\r$",
        r"\x00",
        r"a\x00",
        "\u{e9}",
        "(?i)\u{c9}",
        "\u{65e5}",
        r"\w+$",
        "^.$",
        "f.$",
        r"[^a\r\n]+$",
        r"(?-u)\xff",
        r"(?-u)[\x80-\xff]{2}",
        r"(?-u)^[^\x00\n]*$",
    ];
    let docs = documents();
    let never = || false;
    for (i, pattern) in patterns.into_iter().enumerate() {
        // Every other query negates its second atom: the answers are
        // the atoms', whatever the expression does with them.
        let not = if i % 2 == 0 { "" } else { "NOT " };
        let text = format!("grep:{pattern} {not}grep:a");
        let query = Query::parse(&text, UNIX_EPOCH + Duration::from_secs(1)).unwrap();
        assert!(streams(&query.atoms), "{pattern} is line-local");
        for doc in &docs {
            let mut whole = |_, request: ReadRequest<'_>| {
                let ReadRequest::Bytes(out, _) = request else {
                    panic!("whole reads are bytes");
                };
                out.clear();
                out.extend_from_slice(doc);
                Some(ReadVersion::Catalogued)
            };
            let mut bytes = Vec::new();
            let mut matcher = TextMatcher::new();
            let regexes = Hand::new(&mut whole, &query.atoms).regexes;
            let expected = answers(verify(
                &query.atoms,
                &regexes,
                &mut whole,
                &mut matcher,
                &mut bytes,
                NameId(0),
                &never,
            ));
            let mut dribbled = |_, request: ReadRequest<'_>| {
                let ReadRequest::Stream(each, _) = request else {
                    panic!("line-local reads stream");
                };
                // A few bytes at a time; the long line in larger
                // pieces, which still end mid-line.
                let mut source = Dribble {
                    bytes: doc,
                    most: if doc.len() > 1000 { 4093 } else { 7 },
                    reads: 0,
                };
                each(&mut source, doc.len() as u64).ok()?;
                Some(ReadVersion::Catalogued)
            };
            let mut buffer = Vec::new();
            let got = answers(stream(
                &regexes,
                &mut dribbled,
                &mut buffer,
                NameId(0),
                &never,
            ));
            let shown = String::from_utf8_lossy(&doc[..doc.len().min(80)]);
            assert_eq!(got, expected, "{pattern} over {shown:?}");
        }
    }
}

/// Sol round 1, P6: a line as long as the document grows the buffer to
/// the document's length and no further, at the doubling boundaries
/// and at 8 MiB, the size cap, and the answers stay the whole search's.
#[test]
fn a_long_line_grows_the_buffer_to_the_documents_length_at_most() {
    use crate::scan::BUFFER as LINES;
    let query = Query::parse("grep:a$ grep:ab", UNIX_EPOCH + Duration::from_secs(1)).unwrap();
    let never = || false;
    let regexes = Hand::new(&mut |_, _| None, &query.atoms).regexes;
    for size in [
        LINES - 1,
        LINES,
        LINES + 1,
        2 * LINES - 1,
        2 * LINES,
        3 * LINES,
        8 << 20,
    ] {
        for end in [b'a', b'\n'] {
            let mut doc = vec![b'a'; size];
            doc[size - 1] = end;
            let mut read = |_, request: ReadRequest<'_>| {
                let ReadRequest::Stream(each, _) = request else {
                    panic!("line-local reads stream");
                };
                each(&mut doc.as_slice(), doc.len() as u64).ok()?;
                Some(ReadVersion::Catalogued)
            };
            let mut buffer = Vec::new();
            let got = answers(stream(&regexes, &mut read, &mut buffer, NameId(0), &never));
            let at = size - 1 - usize::from(end == b'\n');
            assert_eq!(got.0, [Some(at), None], "{size} ending {end}");
            assert!(
                buffer.capacity() <= size.max(LINES),
                "{size} ending {end}: a {} byte buffer",
                buffer.capacity()
            );
        }
    }
}

#[test]
fn patterns_that_see_a_newline_or_a_text_end_are_read_whole() {
    for pattern in [
        r"a\nb", r"\s", "[^a]", "(?s).", r"\Aa", r"a\z", "(?R)^a", "(?R)a$",
    ] {
        let text = format!("grep:{pattern}");
        let query = Query::parse(&text, UNIX_EPOCH + Duration::from_secs(1)).unwrap();
        assert!(!streams(&query.atoms), "{pattern} is not line-local");
    }
}
