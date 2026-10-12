//! Token policy experiments (docs/TOKENS.md): byte spans whose tokens leave
//! the term index. T1 removes base64-like runs; T2 removes the time-of-day
//! and epoch pieces of recognised timestamps, which a date index holds
//! instead. Trigrams never consult a policy, so `grep:` and regex still see
//! every byte.
//!
//! [`Policy::NONE`] suppresses nothing: [`tokenize_with`] is then exactly
//! [`tokenize`]. A policy is an experiment, not part of the token contract,
//! so [`TOKENIZER_VERSION`](crate::TOKENIZER_VERSION) does not record it.

use std::ops::Range;

use crate::{Scratch, Token, tokenize};

/// Which token classes the term index leaves out.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct Policy {
    /// T1: tokens inside a base64-like run ([`base64_runs`]).
    pub base64: bool,
    /// T2: the time of day and epoch numbers of a [`Timestamp`].
    pub timestamps: bool,
}

impl Policy {
    pub const NONE: Self = Self {
        base64: false,
        timestamps: false,
    };

    pub fn is_none(self) -> bool {
        self == Self::NONE
    }
}

/// [`tokenize`], dropping every token whose run overlaps a span `policy`
/// suppresses. Token order and spellings are otherwise unchanged.
pub fn tokenize_with(
    bytes: &[u8],
    scratch: &mut Scratch,
    policy: Policy,
    mut emit: impl FnMut(Token<'_>),
) {
    if policy.is_none() {
        return tokenize(bytes, scratch, emit);
    }
    let mut spans = std::mem::take(&mut scratch.spans);
    suppressed(bytes, policy, &mut spans);
    let mut at = 0;
    // Token starts never decrease (a whole run, then its parts left to right,
    // then the next run), so a span ending at or before one start overlaps no
    // later token.
    tokenize(bytes, scratch, |token| {
        while spans
            .get(at)
            .is_some_and(|span| span.end <= token.run.start)
        {
            at += 1;
        }
        if !spans.get(at).is_some_and(|span| span.start < token.run.end) {
            emit(token);
        }
    });
    scratch.spans = spans;
}

/// The spans `policy` suppresses in `bytes`, sorted and disjoint, replacing
/// `out`'s contents.
pub fn suppressed(bytes: &[u8], policy: Policy, out: &mut Vec<Range<usize>>) {
    out.clear();
    if policy.base64 {
        base64_runs(bytes, |run| out.push(run));
    }
    if policy.timestamps {
        timestamps(bytes, |stamp| {
            if stamp.time_start < stamp.span.end {
                out.push(stamp.time_start..stamp.span.end);
            }
        });
    }
    out.sort_unstable_by_key(|span| span.start);
    let mut merged = 0;
    for i in 0..out.len() {
        if merged > 0 && out[i].start <= out[merged - 1].end {
            out[merged - 1].end = out[merged - 1].end.max(out[i].end);
        } else {
            out[merged] = out[i].clone();
            merged += 1;
        }
    }
    out.truncate(merged);
}

// ── T1: base64-like runs ──

/// The shortest run, excluding padding, that [`base64_runs`] reports.
pub const BASE64_MIN: usize = 20;

fn base64_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || matches!(b, b'+' | b'/' | b'-' | b'_')
}

/// Visits each maximal run of the base64 and base64url alphabets, with up to
/// two `=` of padding, that looks random rather than written:
///
/// - at least [`BASE64_MIN`] characters before the padding;
/// - at least one digit, and upper and lower case each a tenth of the run;
/// - case changes between at least a quarter of adjacent letter pairs (random
///   text: half);
/// - lowercase letters in runs averaging at most 2.6 (random text: about 1.7;
///   camelCase words: 4 or more).
///
/// A short all-lowercase-or-digit word joined to either end by `-`, `_` or
/// `/` (`sha512-…`, `…/index`) is trimmed off the reported span, so it stays
/// a term.
pub fn base64_runs(bytes: &[u8], mut emit: impl FnMut(Range<usize>)) {
    let mut at = 0;
    while at < bytes.len() {
        if !base64_byte(bytes[at]) {
            at += 1;
            continue;
        }
        let start = at;
        while at < bytes.len() && base64_byte(bytes[at]) {
            at += 1;
        }
        let body = start..at;
        let mut end = at;
        while end < bytes.len() && end - at < 2 && bytes[end] == b'=' {
            end += 1;
        }
        if random_looking(&bytes[body.clone()]) {
            let start = trim_word(bytes, body.clone(), true);
            let tail = trim_word(bytes, start..body.end, false);
            let end = if tail < body.end { tail } else { end };
            if end - start >= BASE64_MIN {
                emit(start..end);
            }
        }
        at = end.max(at);
    }
}

fn random_looking(run: &[u8]) -> bool {
    if run.len() < BASE64_MIN {
        return false;
    }
    let (mut upper, mut lower, mut digit) = (0, 0, 0);
    let (mut pairs, mut flips) = (0, 0);
    let mut lower_runs = 0;
    let mut previous = 0u8;
    for &b in run {
        if b.is_ascii_uppercase() {
            upper += 1;
        } else if b.is_ascii_lowercase() {
            lower += 1;
            lower_runs += usize::from(!previous.is_ascii_lowercase());
        } else if b.is_ascii_digit() {
            digit += 1;
        }
        if b.is_ascii_alphabetic() && previous.is_ascii_alphabetic() {
            pairs += 1;
            flips += usize::from(b.is_ascii_uppercase() != previous.is_ascii_uppercase());
        }
        previous = b;
    }
    let tenth = run.len().div_ceil(10);
    digit > 0
        && upper >= tenth
        && lower >= tenth
        && flips * 4 >= pairs
        && lower * 10 <= lower_runs * 26
}

/// Where the run starts (or, when `!leading`, ends) once a short written word
/// at that edge is cut off: at most 16 lowercase letters or digits, joined to
/// the rest by `-`, `_` or `/`.
fn trim_word(bytes: &[u8], run: Range<usize>, leading: bool) -> usize {
    let word = |b: &u8| b.is_ascii_lowercase() || b.is_ascii_digit();
    let joiner = |b: u8| matches!(b, b'-' | b'_' | b'/');
    let slice = &bytes[run.clone()];
    if leading {
        let len = slice.iter().take_while(|b| word(b)).count();
        if (1..=16).contains(&len) && slice.get(len).is_some_and(|&b| joiner(b)) {
            return run.start + len + 1;
        }
        run.start
    } else {
        let len = slice.iter().rev().take_while(|b| word(b)).count();
        if (1..=16).contains(&len) && len < slice.len() && joiner(slice[slice.len() - len - 1]) {
            return run.end - len - 1;
        }
        run.end
    }
}

// ── T2: timestamps ──

/// A recognised timestamp, normalised to UTC.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Timestamp {
    /// The matched text.
    pub span: Range<usize>,
    /// Where the pieces T2 suppresses begin: the separator before the time of
    /// day, or the whole number for an epoch. `span.end` for a date alone,
    /// whose year, month and day stay terms.
    pub time_start: usize,
    /// Seconds since 1970-01-01T00:00:00Z. A time with no offset is read as
    /// UTC; a date alone is its midnight.
    pub seconds: i64,
    /// The fraction of a second, to nine digits, which a date index drops.
    pub nanos: u32,
    pub form: Form,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Form {
    /// `yyyy-mm-dd` or `yyyy/mm/dd`, then `T` or a space, a time, an optional
    /// fraction, and an optional `Z` or offset.
    DateTime,
    /// `yyyy-mm-dd` or `yyyy/mm/dd` alone.
    Date,
    /// A 10-digit number in [`EPOCH_RANGE`].
    EpochSeconds,
    /// A 13-digit number whose seconds are in [`EPOCH_RANGE`].
    EpochMillis,
}

/// Epoch seconds [`timestamps`] accepts: 2010-01-01 to 2030-01-01 UTC. An
/// identifier of the right length that lands here is a false positive
/// (TOKENS.md measures how many).
pub const EPOCH_RANGE: Range<i64> = 1_262_304_000..1_893_456_000;

/// Visits each recognised timestamp in `bytes`, left to right, without
/// overlaps. Years run 1970–2099; months, days, hours, minutes and seconds
/// are range-checked (a leap second `:60` is accepted). A match must not
/// continue a longer number or word.
pub fn timestamps(bytes: &[u8], mut emit: impl FnMut(Timestamp)) {
    let mut at = 0;
    while at < bytes.len() {
        if !bytes[at].is_ascii_digit()
            || at > 0 && (bytes[at - 1].is_ascii_alphanumeric() || bytes[at - 1] == b'_')
        {
            at += 1;
            continue;
        }
        match date_time(bytes, at).or_else(|| epoch(bytes, at)) {
            Some(stamp) => {
                at = stamp.span.end;
                emit(stamp);
            }
            None => {
                while at < bytes.len() && bytes[at].is_ascii_digit() {
                    at += 1;
                }
            }
        }
    }
}

/// Whether `bytes` holds a timestamp at an instant in `from..to`, each
/// `(seconds, nanos)`. A date-index hit needs this check whenever the query
/// does not fall on key boundaries, since the index dropped the fraction.
pub fn has_timestamp_in(bytes: &[u8], from: (i64, u32), to: (i64, u32)) -> bool {
    let mut found = false;
    timestamps(bytes, |stamp| {
        found |= (from..to).contains(&(stamp.seconds, stamp.nanos));
    });
    found
}

fn digits(bytes: &[u8], at: usize, n: usize) -> Option<i64> {
    let slice = bytes.get(at..at + n)?;
    slice.iter().try_fold(0i64, |v, &b| {
        b.is_ascii_digit().then(|| v * 10 + i64::from(b - b'0'))
    })
}

fn date_time(bytes: &[u8], start: usize) -> Option<Timestamp> {
    let year = digits(bytes, start, 4)?;
    let sep = *bytes.get(start + 4)?;
    if !(1970..=2099).contains(&year) || !matches!(sep, b'-' | b'/') {
        return None;
    }
    let month = digits(bytes, start + 5, 2)?;
    (bytes.get(start + 7) == Some(&sep)).then_some(())?;
    let day = digits(bytes, start + 8, 2)?;
    if !(1..=12).contains(&month) || day < 1 || day > days_in_month(year, month) {
        return None;
    }
    let mut end = start + 10;
    if bytes.get(end).is_some_and(u8::is_ascii_digit) {
        return None;
    }
    let midnight = days_from_civil(year, month, day) * 86_400;
    let date = Timestamp {
        span: start..end,
        time_start: end,
        seconds: midnight,
        nanos: 0,
        form: Form::Date,
    };
    if !matches!(bytes.get(end), Some(b'T' | b't' | b' ')) {
        return Some(date);
    }
    let Some(hour) = digits(bytes, end + 1, 2) else {
        return Some(date);
    };
    let Some(minute) = (bytes.get(end + 3) == Some(&b':'))
        .then(|| digits(bytes, end + 4, 2))
        .flatten()
    else {
        return Some(date);
    };
    if hour > 23 || minute > 59 {
        return Some(date);
    }
    let time_start = end;
    end += 6;
    let mut second = 0;
    if bytes.get(end) == Some(&b':')
        && let Some(s) = digits(bytes, end + 1, 2)
        && s <= 60
    {
        second = s;
        end += 3;
    }
    let mut nanos = 0u32;
    if matches!(bytes.get(end), Some(b'.' | b','))
        && bytes.get(end + 1).is_some_and(u8::is_ascii_digit)
    {
        end += 1;
        let mut scale = 100_000_000;
        while let Some(&b) = bytes.get(end).filter(|b| b.is_ascii_digit()) {
            nanos += u32::from(b - b'0') * scale;
            scale /= 10;
            end += 1;
        }
    }
    let mut offset = 0;
    match bytes.get(end) {
        Some(b'Z' | b'z') => end += 1,
        Some(&sign @ (b'+' | b'-')) => {
            if let Some(h) = digits(bytes, end + 1, 2).filter(|&h| h <= 14) {
                let colon = usize::from(bytes.get(end + 3) == Some(&b':'));
                let m = digits(bytes, end + 3 + colon, 2).filter(|&m| m <= 59);
                let len = match m {
                    Some(_) => 3 + colon + 2,
                    None if colon == 0 => 3,
                    None => 0,
                };
                if len > 0 && !bytes.get(end + len).is_some_and(u8::is_ascii_digit) {
                    let minutes = h * 60 + m.unwrap_or(0);
                    offset = if sign == b'+' { minutes } else { -minutes } * 60;
                    end += len;
                }
            }
        }
        _ => {}
    }
    if bytes.get(end).is_some_and(u8::is_ascii_digit) {
        return Some(date);
    }
    Some(Timestamp {
        span: start..end,
        time_start,
        seconds: midnight + hour * 3600 + minute * 60 + second - offset,
        nanos,
        form: Form::DateTime,
    })
}

fn epoch(bytes: &[u8], start: usize) -> Option<Timestamp> {
    let len = bytes[start..]
        .iter()
        .take_while(|b| b.is_ascii_digit())
        .count();
    let end = start + len;
    let continues = bytes
        .get(end)
        .is_some_and(|&b| b.is_ascii_alphanumeric() || b == b'_');
    let decimal = |dot: Option<&u8>, digit: Option<&u8>| {
        dot == Some(&b'.') && digit.is_some_and(u8::is_ascii_digit)
    };
    if continues
        || decimal(bytes.get(end), bytes.get(end + 1))
        || start >= 2 && decimal(bytes.get(start - 1), bytes.get(start - 2))
        || names_identifier(bytes, start)
    {
        return None;
    }
    let value = digits(bytes, start, len)?;
    let (seconds, nanos, form) = match len {
        10 => (value, 0, Form::EpochSeconds),
        13 => (
            value / 1000,
            (value % 1000) as u32 * 1_000_000,
            Form::EpochMillis,
        ),
        _ => return None,
    };
    EPOCH_RANGE.contains(&seconds).then_some(Timestamp {
        span: start..end,
        time_start: start,
        seconds,
        nanos,
        form,
    })
}

/// Whether the key just before a number names an identifier: `"id": `,
/// `userId=`, `msg_ids: `. Up to three quote, colon, equals or space bytes may
/// separate the key from the number.
pub(crate) fn names_identifier(bytes: &[u8], start: usize) -> bool {
    let before = &bytes[start.saturating_sub(40)..start];
    let gap = before
        .iter()
        .rev()
        .take(3)
        .take_while(|&&b| matches!(b, b'"' | b'\'' | b':' | b'=' | b' '))
        .count();
    if gap == 0 {
        return false;
    }
    let key = &before[..before.len() - gap];
    let len = key
        .iter()
        .rev()
        .take_while(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-'))
        .count();
    let key = key[key.len() - len..].to_ascii_lowercase();
    key.ends_with(b"id") || key.ends_with(b"ids")
}

fn days_in_month(year: i64, month: i64) -> i64 {
    match month {
        2 if year % 4 == 0 && (year % 100 != 0 || year % 400 == 0) => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    }
}

/// Days from 1970-01-01 to a proleptic Gregorian date (Howard Hinnant's
/// `days_from_civil`).
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let y = if month <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (month + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

#[cfg(test)]
mod tests;
