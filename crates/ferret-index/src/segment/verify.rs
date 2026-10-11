//! Version-checked document reads for D65 graduation and offset hints. Hosts
//! establish the indexed version before returning any bytes. Window matches
//! require the entire enclosing tokenizer run, so truncation cannot invent a
//! token or change Unicode lowercasing context.

use ferret_text::{Scratch, cap, tokenize};

use super::fingerprint;

/// Where returned bytes sit in a document. `complete` means a full read.
#[derive(Clone, Copy, Debug)]
pub struct Window {
    pub start: u64,
    pub complete: bool,
}

/// Returns only stable bytes of the indexed document version. `offset` asks
/// for a small window around a hint; `None` asks for the whole document.
/// A changed or unreadable document returns `None`, even if it contains the
/// requested term now. Hosts may return the whole document for either request.
pub type DocumentReader<'a> = dyn FnMut(u32, Option<u32>, &mut Vec<u8>) -> Option<Window> + 'a;

/// A verified occurrence and whether its shared record can be removed.
#[derive(Clone, Copy, Debug)]
pub struct Verified {
    pub offset: u32,
    pub removable: bool,
}

/// Confirms a normalised term at an offset, falling back to a full read.
/// `unique_hint` is true only for codec 5 records whose offset survived
/// collision deduplication. Other records need a full-document collision
/// check before their removal could be sound.
pub fn verify_record(
    doc: u32,
    term: &[u8],
    offset: Option<u32>,
    unique_hint: bool,
    read: &mut DocumentReader<'_>,
    bytes: &mut Vec<u8>,
) -> Option<Verified> {
    if let Some(offset) = offset {
        bytes.clear();
        let window = read(doc, Some(offset), bytes)?;
        if token_at(bytes, window, offset, term) {
            let mut collision = false;
            tokenize(bytes, &mut Scratch::default(), |token| {
                if window.start + token.run.start as u64 == u64::from(offset)
                    && cap(token.bytes) != term
                    && fingerprint(cap(token.bytes)) == fingerprint(term)
                {
                    collision = true;
                }
            });
            return Some(Verified {
                offset,
                removable: unique_hint && !collision,
            });
        }
    }
    bytes.clear();
    let window = read(doc, None, bytes)?;
    if !window.complete || window.start != 0 {
        return None;
    }
    let (mut found, mut collision) = (None, false);
    let fp = fingerprint(term);
    tokenize(bytes, &mut Scratch::default(), |token| {
        let value = cap(token.bytes);
        if value == term {
            if found.is_none() {
                found = u32::try_from(token.run.start).ok();
            }
        } else if fingerprint(value) == fp {
            collision = true;
        }
    });
    found.map(|offset| Verified {
        offset,
        removable: !collision,
    })
}

/// A positive check requires a complete enclosing run in the window.
/// A run touching either truncated edge is uncertain and falls back.
pub fn token_at(bytes: &[u8], window: Window, offset: u32, term: &[u8]) -> bool {
    let Some(local) = u64::from(offset)
        .checked_sub(window.start)
        .and_then(|v| usize::try_from(v).ok())
    else {
        return false;
    };
    let mut matched = false;
    let mut enclosing = None;
    tokenize(bytes, &mut Scratch::default(), |token| {
        if token.kind == ferret_text::Kind::Whole {
            enclosing = Some(token.run.clone());
        }
        if token.run.start == local && cap(token.bytes) == term {
            matched |= enclosing.as_ref().is_some_and(|run| {
                let left = window.start == 0 && run.start == 0
                    || bytes[..run.start]
                        .utf8_chunks()
                        .last()
                        .is_some_and(|chunk| {
                            chunk.invalid().is_empty()
                                && chunk.valid().chars().next_back().is_some_and(separator)
                        });
                let right = window.complete && run.end == bytes.len()
                    || bytes[run.end..]
                        .utf8_chunks()
                        .next()
                        .and_then(|chunk| chunk.valid().chars().next())
                        .is_some_and(separator);
                left && right
            });
        }
    });
    matched
}

fn separator(c: char) -> bool {
    !c.is_alphanumeric() && c != '_'
}
