//! The sound fix for the term screen, measured. A substring of a literal
//! lies inside one whole-run term of any document holding the literal,
//! unless that run never became a term: T1 dropped it for overlapping a
//! base64 span, or the 64-byte cap cut its tail. Admitting every such
//! document whatever the query (the arm's fallback) is sound and admits
//! 65% of the corpus. The fix admits a document for a query only when its
//! **residue** (the whole runs it lost) could hold one of the query's
//! needles: a trigram index over residue runs, per document.
//!
//! Measured two ways: the trigram residue index a product would build, and
//! an exact check (a needle is inside a residue run), the floor any residue
//! index could reach.

use std::fs;

use ferret_text::{Kind, Scratch, base64_runs, tokenize};

use super::Corpus;

/// Every whole run of `bytes` the term index lost, lowercased, each ending
/// in a newline (no run holds one).
pub(super) fn residue(bytes: &[u8], scratch: &mut Scratch, out: &mut Vec<u8>) {
    out.clear();
    let mut spans = Vec::new();
    base64_runs(bytes, |span| spans.push(span));
    let mut at = 0;
    tokenize(bytes, scratch, |token| {
        if token.kind != Kind::Whole {
            return;
        }
        while spans.get(at).is_some_and(|s| s.end <= token.run.start) {
            at += 1;
        }
        let dropped = spans.get(at).is_some_and(|s| s.start < token.run.end);
        if dropped || token.bytes.len() > ferret_text::MAX_TOKEN_BYTES {
            out.extend_from_slice(token.bytes);
            out.push(b'\n');
        }
    });
}

fn trigram(window: &[u8]) -> usize {
    (usize::from(window[0]) << 16) | (usize::from(window[1]) << 8) | usize::from(window[2])
}

/// Per needle, the fallback documents a residue index would admit: by
/// trigrams, and exactly. Also the residue index's pair count (one per
/// distinct trigram per document) and the residue bytes.
pub(super) struct Admitted {
    pub(super) by_trigrams: Vec<Vec<u32>>,
    pub(super) exact: Vec<Vec<u32>>,
    pub(super) pairs: u64,
    pub(super) residue_bytes: u64,
    pub(super) residue_docs: u64,
}

pub(super) fn admit(corpus: &Corpus, fallback: &[u32], needles: &[Vec<u8>]) -> crate::Result<Admitted> {
    let finders: Vec<_> = needles.iter().map(memchr::memmem::Finder::new).collect();
    let needle_grams: Vec<Vec<usize>> = needles.iter().map(|n| n.windows(3).map(trigram).collect()).collect();
    let mut bits = vec![0u64; 1 << 18];
    let mut touched = Vec::new();
    let mut scratch = Scratch::default();
    let mut lost = Vec::new();
    let mut out = Admitted {
        by_trigrams: vec![Vec::new(); needles.len()],
        exact: vec![Vec::new(); needles.len()],
        pairs: 0,
        residue_bytes: 0,
        residue_docs: 0,
    };
    for &doc in fallback {
        let bytes = fs::read(corpus.snapshot.path(doc))?;
        residue(&bytes, &mut scratch, &mut lost);
        if lost.is_empty() {
            continue;
        }
        out.residue_docs += 1;
        out.residue_bytes += lost.len() as u64;
        for run in lost.split(|&b| b == b'\n') {
            for window in run.windows(3) {
                let key = trigram(window);
                let (word, bit) = (key / 64, 1u64 << (key % 64));
                if bits[word] & bit == 0 {
                    bits[word] |= bit;
                    touched.push(word);
                    out.pairs += 1;
                }
            }
        }
        for (i, grams) in needle_grams.iter().enumerate() {
            if grams.iter().all(|&k| bits[k / 64] & (1 << (k % 64)) != 0) {
                out.by_trigrams[i].push(doc);
                if finders[i].find(&lost).is_some() {
                    out.exact[i].push(doc);
                }
            }
        }
        for word in touched.drain(..) {
            bits[word] = 0;
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn residue_holds_dropped_and_overlong_whole_runs_only() {
        let mut out = Vec::new();
        let long = "a".repeat(70);
        let text = format!("plain words {long} end");
        residue(text.as_bytes(), &mut Scratch::default(), &mut out);
        assert_eq!(out, format!("{long}\n").into_bytes());
        // A base64 span drops its runs from the terms; they land here.
        let blob = b"key = QmFzZTY0X2VuY29kZWRfc2VjcmV0X3ZhbHVlX2hlcmU0Mg== done";
        let mut terms = Vec::new();
        ferret_text::tokenize_terms(blob, &mut Scratch::default(), |t| terms.push(t.bytes.to_vec()));
        residue(blob, &mut Scratch::default(), &mut out);
        let mut all = Vec::new();
        tokenize(blob, &mut Scratch::default(), |t| {
            if t.kind == Kind::Whole {
                all.push(t.bytes.to_vec());
            }
        });
        for run in all {
            let kept = terms.contains(&run);
            let lost = out.split(|&b| b == b'\n').any(|r| r == run.as_slice());
            assert!(kept ^ lost, "{run:?} kept={kept} lost={lost}");
        }
        assert!(!out.is_empty());
    }
}
