//! Verification of candidates on a thread pool: whole documents, or only the
//! lines holding hits.

use std::fs::{self, File};
use std::os::unix::fs::FileExt;
use std::path::Path;
use std::time::{Duration, Instant};

use ferret_verify::{Grep, Matcher};
use rayon::prelude::*;

use crate::pop::Docs;
use crate::query::Cand;

#[derive(Default, Clone, Copy)]
pub struct Verified {
    pub time: Duration,
    /// Bytes read from documents (logical).
    pub read: u64,
    /// Bytes the regex ran over.
    pub regex: u64,
    pub device: u64,
    pub whole_docs: u64,
    pub line_docs: u64,
}

/// Documents up to this size are read whole even in line mode; one read
/// beats several positioned ones.
const SMALL: u64 = 64 << 10;

/// The line around `at`, read by growing windows. Returns the line and the
/// bytes read.
fn line_at(file: &File, size: u64, at: u64) -> (u64, Vec<u8>, u64) {
    let mut w = 256u64;
    let mut read = 0;
    loop {
        let s = at.saturating_sub(w);
        let e = (at + w).min(size);
        let mut buf = vec![0u8; (e - s) as usize];
        file.read_exact_at(&mut buf, s).expect("pread");
        read += e - s;
        let rel = (at - s) as usize;
        let ls = match memchr::memrchr(b'\n', &buf[..rel]) {
            Some(i) => Some(i + 1),
            None if s == 0 => Some(0),
            None => None,
        };
        let le = match memchr::memchr(b'\n', &buf[rel..]) {
            Some(i) => Some(rel + i),
            None if e == size => Some(buf.len()),
            None => None,
        };
        if let (Some(ls), Some(le)) = (ls, le) {
            return (s + le as u64, buf[ls..le].to_vec(), read);
        }
        w *= 4;
    }
}

fn line_of(bytes: &[u8], at: usize) -> (usize, &[u8]) {
    let ls = memchr::memrchr(b'\n', &bytes[..at]).map_or(0, |i| i + 1);
    let le = memchr::memchr(b'\n', &bytes[at..]).map_or(bytes.len(), |i| at + i);
    (le, &bytes[ls..le])
}

/// One document: (matched, read bytes, regex bytes).
fn one(grep: &Grep, path: &Path, size: u64, anchors: Option<&[u32]>) -> (bool, u64, u64) {
    match anchors {
        None => {
            let bytes = fs::read(path).expect("doc");
            assert_eq!(bytes.len() as u64, size, "frozen size moved");
            (grep.is_match(&bytes), size, size)
        }
        // Small, or hits dense enough that the windows would cover most of
        // it: one read, then only the hit lines through the regex.
        Some(anchors) if size <= SMALL || anchors.len() as u64 * 2048 >= size => {
            let bytes = fs::read(path).expect("doc");
            assert_eq!(bytes.len() as u64, size, "frozen size moved");
            let (mut done, mut regex) = (0usize, 0u64);
            for &a in anchors {
                let a = a as usize;
                if a < done {
                    continue;
                }
                let (end, line) = line_of(&bytes, a);
                done = end + 1;
                regex += line.len() as u64;
                if grep.is_match(line) {
                    return (true, size, regex);
                }
            }
            (false, size, regex)
        }
        Some(anchors) => {
            let file = File::open(path).expect("doc");
            let (mut done, mut read, mut regex) = (0u64, 0u64, 0u64);
            for &a in anchors {
                let a = a as u64;
                if a < done {
                    continue;
                }
                let (end, line, r) = line_at(&file, size, a);
                done = end + 1;
                read += r;
                regex += line.len() as u64;
                if grep.is_match(&line) {
                    return (true, read, regex);
                }
            }
            (false, read, regex)
        }
    }
}

/// Verifies `cands` (whole documents when `whole`, else as each candidate
/// says) on `pool`. Returns the matching documents, ascending.
pub fn run(
    pool: &rayon::ThreadPool,
    grep: &Grep,
    docs: &Docs,
    sizes: &[u64],
    cands: &[Cand],
    whole: bool,
) -> (Vec<u32>, Verified) {
    let device = crate::util::device_read_bytes();
    let start = Instant::now();
    let results: Vec<(u32, bool, u64, u64, bool)> = pool.install(|| {
        cands
            .par_iter()
            .with_max_len(1)
            .map(|c| {
                let anchors = if whole { None } else { c.anchors.as_deref() };
                let (m, read, regex) = one(grep, &docs.path(c.doc), sizes[c.doc as usize], anchors);
                (c.doc, m, read, regex, anchors.is_none())
            })
            .collect()
    });
    let time = start.elapsed();
    let mut v = Verified {
        time,
        device: crate::util::device_read_bytes() - device,
        ..Verified::default()
    };
    let mut matched = Vec::new();
    for (doc, m, read, regex, whole_doc) in results {
        if m {
            matched.push(doc);
        }
        v.read += read;
        v.regex += regex;
        if whole_doc {
            v.whole_docs += 1;
        } else {
            v.line_docs += 1;
        }
    }
    matched.sort_unstable();
    (matched, v)
}

pub fn evict(docs: &Docs, cands: &[Cand]) {
    for c in cands {
        crate::index::evict(&docs.path(c.doc));
    }
}
