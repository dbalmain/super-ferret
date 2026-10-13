//! Segments of the folded corpus, a plain suffix array over each, and an
//! FM-index built from it: the BWT in a Huffman-shaped wavelet tree, a mark
//! bitvector over text-position samples, and the samples themselves.
//!
//! Files, per segment `k` under the index directory:
//!
//! ```text
//! seg{k}.docs   doc ids and start offsets (u32 LE), one past the end last
//! seg{k}.text   the ASCII-folded concatenation (SA arm and builds only)
//! seg{k}.sa     the suffix array, u32 LE
//! seg{k}.wt     header, C table, node table, tree words, rank directory
//! seg{k}.m{s}   mark bitvector (words + rank directory) at sampling rate s
//! seg{k}.s{s}   samples (u32 LE) of marked rows, in row order
//! ```

use std::fs::{self, File};
use std::io::{BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};

use memmap2::{Advice, Mmap};

use crate::pop::Docs;

pub const SEGMENT_BYTES: u64 = 1 << 30;

/// Document index ranges into `Docs::ids`, cut at about a GiB each.
pub fn segments(docs: &Docs) -> Vec<std::ops::Range<usize>> {
    let mut out = Vec::new();
    let (mut start, mut bytes) = (0, 0u64);
    for (i, &size) in docs.sizes.iter().enumerate() {
        if bytes > 0 && bytes + size > SEGMENT_BYTES {
            out.push(start..i);
            start = i;
            bytes = 0;
        }
        bytes += size;
    }
    out.push(start..docs.sizes.len());
    out
}

pub fn file(dir: &Path, k: usize, ext: &str) -> PathBuf {
    dir.join(format!("seg{k}.{ext}"))
}

// ── little-endian slices over a mapped file ──

#[inline]
pub fn u32_at(bytes: &[u8], i: usize) -> u32 {
    u32::from_le_bytes(bytes[4 * i..4 * i + 4].try_into().expect("4"))
}

#[inline]
fn u64_at(bytes: &[u8], i: usize) -> u64 {
    u64::from_le_bytes(bytes[8 * i..8 * i + 8].try_into().expect("8"))
}

/// Maps a file read-only. The file is never written while mapped: index
/// files are written once by a build process and only read after.
pub fn map(path: &Path, advice: Advice) -> Mmap {
    let f = File::open(path).expect("open index file");
    #[allow(unsafe_code)]
    // SAFETY: the experiment writes each index file once, in a separate build
    // process, before any query maps it; nothing truncates it while mapped.
    let m = unsafe { Mmap::map(&f) }.expect("mmap");
    m.advise(advice).expect("madvise");
    m
}

pub fn evict(path: &Path) {
    if let Ok(f) = File::open(path) {
        rustix::fs::fadvise(&f, 0, None, rustix::fs::Advice::DontNeed).expect("fadvise");
    }
}

fn write_u32s(path: &Path, values: impl IntoIterator<Item = u32>) {
    let mut w = BufWriter::with_capacity(1 << 22, File::create(path).expect("create"));
    for v in values {
        w.write_all(&v.to_le_bytes()).expect("write");
    }
    w.flush().expect("flush");
}

fn write_u64s(w: &mut impl Write, values: &[u64]) {
    for v in values {
        w.write_all(&v.to_le_bytes()).expect("write");
    }
}

pub fn read_u32s(path: &Path) -> Vec<u32> {
    let len = fs::metadata(path).expect("meta").len() as usize / 4;
    let mut out = Vec::with_capacity(len);
    let mut r = BufReader::with_capacity(1 << 22, File::open(path).expect("open"));
    let mut buf = vec![0u8; 1 << 22];
    loop {
        let n = r.read(&mut buf).expect("read");
        if n == 0 {
            break;
        }
        assert_eq!(n % 4, 0, "short u32 read");
        out.extend(buf[..n].chunks_exact(4).map(|c| u32::from_le_bytes(c.try_into().expect("4"))));
    }
    out
}

// ── the segment table ──

/// Doc ids and their start offsets in the segment text; `starts` has one
/// more entry, the text length.
pub struct Table {
    pub docs: Vec<u32>,
    pub starts: Vec<u32>,
}

impl Table {
    pub fn read(dir: &Path, k: usize) -> Self {
        let all = read_u32s(&file(dir, k, "docs"));
        let n = all.len() / 2;
        Table {
            docs: all[..n].to_vec(),
            starts: all[n..].to_vec(),
        }
    }
}

/// Builds and writes `seg{k}.text`, `seg{k}.docs` and `seg{k}.sa`.
pub fn build_sa(dir: &Path, docs: &Docs, k: usize, threads: u16) -> String {
    let range = segments(docs)[k].clone();
    let total: u64 = docs.sizes[range.clone()].iter().sum();
    let mut text = Vec::with_capacity(total as usize);
    let mut starts = Vec::new();
    for i in range.clone() {
        starts.push(text.len() as u32);
        let mut bytes = fs::read(docs.path(docs.ids[i])).expect("doc");
        assert_eq!(bytes.len() as u64, docs.sizes[i], "frozen size moved");
        bytes.make_ascii_lowercase();
        text.extend_from_slice(&bytes);
    }
    starts.push(text.len() as u32);
    let ids = docs.ids[range.clone()].to_vec();
    write_u32s(
        &file(dir, k, "docs"),
        ids.iter().copied().chain(starts.iter().copied()),
    );
    fs::write(file(dir, k, "text"), &text).expect("text");
    let read_s = crate::util::wall();
    let sa_start = std::time::Instant::now();
    let sa: Vec<i32> = libsais::SuffixArrayConstruction::for_text(&text)
        .in_owned_buffer32()
        .multi_threaded(libsais::ThreadCount::fixed(threads))
        .run()
        .expect("libsais")
        .into_vec();
    let sa_s = sa_start.elapsed().as_secs_f64();
    write_u32s(&file(dir, k, "sa"), sa.iter().map(|&v| v as u32));
    format!(
        "segment={k} docs={} text_bytes={} sa_bytes={} sais_threads={threads} sais_wall_s={sa_s:.2} since_start_s={read_s:.2}",
        range.len(),
        text.len(),
        4 * sa.len()
    )
}

// ── bitvectors with a rank directory ──

// Words with one u32 count per 512-bit block: the ones before the block
// within its own bitvector. A bitvector starts block-aligned and owns one
// block past its last bit, so `rank1(len)` never reads past it.

fn blocks_for(len: u64) -> usize {
    (len as usize + 1).div_ceil(512)
}

/// The rank directory of `words`, counting from zero again at each of
/// `starts` (word offsets of the bitvectors it holds).
fn directory(words: &[u64], starts: &[u64]) -> Vec<u32> {
    let mut dir = Vec::with_capacity(words.len() / 8);
    let mut count = 0u32;
    for (b, block) in words.chunks(8).enumerate() {
        if starts.binary_search(&(8 * b as u64)).is_ok() {
            count = 0;
        }
        dir.push(count);
        count += block.iter().map(|w| w.count_ones()).sum::<u32>();
    }
    dir
}

/// Ones in `[0, i)` of the bitvector starting at `word0` (block-aligned).
#[inline]
fn rank1(words: &[u8], dir: &[u8], word0: usize, i: u64) -> u64 {
    let w = word0 + (i / 64) as usize;
    let b = w / 8;
    let mut r = u32_at(dir, b) as u64;
    for j in b * 8..w {
        r += u64_at(words, j).count_ones() as u64;
    }
    let bit = i % 64;
    if bit != 0 {
        r += (u64_at(words, w) & ((1u64 << bit) - 1)).count_ones() as u64;
    }
    r
}

#[inline]
fn get(words: &[u8], word0: usize, i: u64) -> bool {
    u64_at(words, word0 + (i / 64) as usize) >> (i % 64) & 1 == 1
}

// ── the wavelet tree ──

const LEAF: u32 = 1 << 31;

/// One internal node: its children (`LEAF | symbol` for a leaf) and where
/// its bits start.
#[derive(Clone, Copy, Default)]
struct Node {
    child: [u32; 2],
    word0: u64,
    len: u64,
}

/// Huffman codes by symbol frequency: (code, length), MSB first.
fn huffman(freq: &[u64; 256]) -> (Vec<Node>, [(u64, u32); 256]) {
    // Leaves are `LEAF | sym`; internal nodes are indices into `nodes`.
    let mut heap: std::collections::BinaryHeap<std::cmp::Reverse<(u64, u32)>> = freq
        .iter()
        .enumerate()
        .filter(|(_, f)| **f > 0)
        .map(|(s, &f)| std::cmp::Reverse((f, LEAF | s as u32)))
        .collect();
    assert!(heap.len() >= 2, "need two symbols");
    let mut nodes = Vec::new();
    while heap.len() > 1 {
        let std::cmp::Reverse((fa, a)) = heap.pop().expect("a");
        let std::cmp::Reverse((fb, b)) = heap.pop().expect("b");
        nodes.push(Node {
            child: [a, b],
            ..Node::default()
        });
        heap.push(std::cmp::Reverse((fa + fb, nodes.len() as u32 - 1)));
    }
    // Root is the last node; renumber so the root is 0 (BFS order).
    let root = nodes.len() as u32 - 1;
    let mut order = vec![root];
    let mut i = 0;
    while i < order.len() {
        for c in nodes[order[i] as usize].child {
            if c & LEAF == 0 {
                order.push(c);
            }
        }
        i += 1;
    }
    let mut new_id = vec![0u32; nodes.len()];
    for (new, &old) in order.iter().enumerate() {
        new_id[old as usize] = new as u32;
    }
    let mut out: Vec<Node> = order
        .iter()
        .map(|&old| {
            let mut n = nodes[old as usize];
            for c in &mut n.child {
                if *c & LEAF == 0 {
                    *c = new_id[*c as usize];
                }
            }
            n
        })
        .collect();
    let mut codes = [(0u64, 0u32); 256];
    let mut stack = vec![(0u32, 0u64, 0u32)];
    while let Some((node, code, len)) = stack.pop() {
        for bit in 0..2u64 {
            let c = out[node as usize].child[bit as usize];
            let (code, len) = (code << 1 | bit, len + 1);
            assert!(len < 64, "code too long");
            if c & LEAF != 0 {
                codes[(c & !LEAF) as usize] = (code, len);
            } else {
                stack.push((c, code, len));
            }
        }
    }
    // Lengths per node: symbols routed through it.
    for (s, &(code, len)) in codes.iter().enumerate() {
        if len == 0 {
            continue;
        }
        let mut node = 0usize;
        for d in 0..len {
            out[node].len += freq[s];
            let bit = (code >> (len - 1 - d)) & 1;
            let c = out[node].child[bit as usize];
            if c & LEAF != 0 {
                break;
            }
            node = c as usize;
        }
    }
    let mut word0 = 0u64;
    for n in &mut out {
        n.word0 = word0;
        word0 += 8 * blocks_for(n.len) as u64;
    }
    (out, codes)
}

/// Builds the FM-index of `seg{k}` from its text and suffix array, at each
/// sampling rate. Returns a report line.
pub fn build_fm(dir: &Path, k: usize, rates: &[u32]) -> String {
    let text = fs::read(file(dir, k, "text")).expect("text");
    let sa = read_u32s(&file(dir, k, "sa"));
    let n = text.len();
    assert_eq!(sa.len(), n);
    // BWT of text$, rows 0..=n; row 0 is the suffix "$".
    let mut bwt = vec![0u8; n + 1];
    bwt[0] = text[n - 1];
    let mut primary = 0usize;
    for (i, &s) in sa.iter().enumerate() {
        if s == 0 {
            primary = i + 1;
        } else {
            bwt[i + 1] = text[s as usize - 1];
        }
    }
    let mut counts = [0u64; 256];
    for &b in &text {
        counts[b as usize] += 1;
    }
    drop(text);
    let mut c_table = [0u64; 257];
    c_table[0] = 1;
    for s in 0..256 {
        c_table[s + 1] = c_table[s] + counts[s];
    }
    let mut freq = [0u64; 256];
    for &b in &bwt {
        freq[b as usize] += 1;
    }
    let (nodes, codes) = huffman(&freq);
    let total_words = nodes.last().map_or(0, |n| n.word0 as usize + 8 * blocks_for(n.len));
    let mut words = vec![0u64; total_words];
    let mut cursor: Vec<u64> = nodes.iter().map(|n| n.word0 * 64).collect();
    for &b in &bwt {
        let (code, len) = codes[b as usize];
        let mut node = 0usize;
        for d in 0..len {
            let bit = (code >> (len - 1 - d)) & 1;
            let at = cursor[node];
            words[(at / 64) as usize] |= bit << (at % 64);
            cursor[node] += 1;
            let c = nodes[node].child[bit as usize];
            if c & LEAF != 0 {
                break;
            }
            node = c as usize;
        }
    }
    drop(bwt);
    let starts: Vec<u64> = nodes.iter().map(|n| n.word0).collect();
    let dir_words = directory(&words, &starts);
    let tree_bits: u64 = nodes.iter().map(|n| n.len).sum();
    {
        let mut w = BufWriter::with_capacity(1 << 22, File::create(file(dir, k, "wt")).expect("wt"));
        let header = [
            n as u64,
            primary as u64,
            nodes.len() as u64,
            words.len() as u64,
        ];
        write_u64s(&mut w, &header);
        write_u64s(&mut w, &c_table);
        for node in &nodes {
            write_u64s(
                &mut w,
                &[
                    node.child[0] as u64 | (node.child[1] as u64) << 32,
                    node.word0,
                    node.len,
                ],
            );
        }
        write_u64s(&mut w, &words);
        for d in &dir_words {
            w.write_all(&d.to_le_bytes()).expect("dir");
        }
        w.flush().expect("flush");
    }
    let wt_bytes = fs::metadata(file(dir, k, "wt")).expect("wt").len();
    let mut out = format!(
        "segment={k} n={n} symbols={} nodes={} tree_bits={tree_bits} bits_per_symbol={:.3} wt_bytes={wt_bytes}",
        codes.iter().filter(|c| c.1 > 0).count(),
        nodes.len(),
        tree_bits as f64 / (n + 1) as f64
    );
    // Marks and samples: row 0 (SA value n) and every row whose SA value is
    // a multiple of the rate.
    let rows = n as u64 + 1;
    for &rate in rates {
        let mut mark = vec![0u64; 8 * blocks_for(rows)];
        let mut samples = Vec::new();
        mark[0] |= 1;
        samples.push(n as u32);
        for (i, &s) in sa.iter().enumerate() {
            if s % rate == 0 {
                let r = i + 1;
                mark[r / 64] |= 1 << (r % 64);
                samples.push(s);
            }
        }
        let mdir = directory(&mark, &[0]);
        {
            let mut w = BufWriter::with_capacity(
                1 << 22,
                File::create(file(dir, k, &format!("m{rate}"))).expect("m"),
            );
            write_u64s(&mut w, &[mark.len() as u64]);
            write_u64s(&mut w, &mark);
            for d in &mdir {
                w.write_all(&d.to_le_bytes()).expect("dir");
            }
            w.flush().expect("flush");
        }
        write_u32s(&file(dir, k, &format!("s{rate}")), samples.iter().copied());
        let m = fs::metadata(file(dir, k, &format!("m{rate}"))).expect("m").len();
        let s = fs::metadata(file(dir, k, &format!("s{rate}"))).expect("s").len();
        out += &format!(" rate{rate}_mark_bytes={m} rate{rate}_sample_bytes={s} rate{rate}_samples={}", samples.len());
    }
    out
}

// ── query-time views ──

/// An FM-index segment, mapped.
pub struct Fm {
    wt: Mmap,
    mark: Mmap,
    samples: Mmap,
    n: u64,
    primary: u64,
    c: [u64; 257],
    nodes: Vec<Node>,
    codes: [(u64, u32); 256],
    words_at: usize,
    dir_at: usize,
    words_len: usize,
    mark_words: usize,
}

impl Fm {
    pub fn files(dir: &Path, k: usize, rate: u32) -> [PathBuf; 3] {
        [
            file(dir, k, "wt"),
            file(dir, k, &format!("m{rate}")),
            file(dir, k, &format!("s{rate}")),
        ]
    }

    pub fn open(dir: &Path, k: usize, rate: u32) -> Self {
        let [w, m, s] = Self::files(dir, k, rate);
        let wt = map(&w, Advice::Random);
        let mark = map(&m, Advice::Random);
        let samples = map(&s, Advice::Random);
        let n = u64_at(&wt, 0);
        let primary = u64_at(&wt, 1);
        let node_count = u64_at(&wt, 2) as usize;
        let words_len = u64_at(&wt, 3) as usize;
        let mut c = [0u64; 257];
        for (i, v) in c.iter_mut().enumerate() {
            *v = u64_at(&wt, 4 + i);
        }
        let base = 4 + 257;
        let nodes: Vec<Node> = (0..node_count)
            .map(|i| {
                let ch = u64_at(&wt, base + 3 * i);
                Node {
                    child: [ch as u32, (ch >> 32) as u32],
                    word0: u64_at(&wt, base + 3 * i + 1),
                    len: u64_at(&wt, base + 3 * i + 2),
                }
            })
            .collect();
        let words_at = 8 * (base + 3 * node_count);
        let dir_at = words_at + 8 * words_len;
        let mut codes = [(0u64, 0u32); 256];
        let mut stack = vec![(0u32, 0u64, 0u32)];
        while let Some((node, code, len)) = stack.pop() {
            for bit in 0..2u64 {
                let ch = nodes[node as usize].child[bit as usize];
                let (code, len) = (code << 1 | bit, len + 1);
                if ch & LEAF != 0 {
                    codes[(ch & !LEAF) as usize] = (code, len);
                } else {
                    stack.push((ch, code, len));
                }
            }
        }
        let mark_words = u64_at(&mark, 0) as usize;
        Fm {
            wt,
            mark,
            samples,
            n,
            primary,
            c,
            nodes,
            codes,
            words_at,
            dir_at,
            words_len,
            mark_words,
        }
    }

    fn words(&self) -> &[u8] {
        &self.wt[self.words_at..self.words_at + 8 * self.words_len]
    }

    fn dir(&self) -> &[u8] {
        &self.wt[self.dir_at..]
    }

    /// Occurrences of `sym` in BWT rows `[0, i)`.
    #[inline]
    fn rank(&self, sym: u8, mut i: u64) -> u64 {
        let (code, len) = self.codes[sym as usize];
        if len == 0 {
            return 0;
        }
        let orig = i;
        let (words, dir) = (self.words(), self.dir());
        let mut node = 0usize;
        for d in 0..len {
            let bit = (code >> (len - 1 - d)) & 1;
            let nd = &self.nodes[node];
            let ones = rank1(words, dir, nd.word0 as usize, i);
            i = if bit == 1 { ones } else { i - ones };
            let ch = nd.child[bit as usize];
            if ch & LEAF != 0 {
                break;
            }
            node = ch as usize;
        }
        // The "$" row holds a placeholder 0 byte.
        if sym == 0 && self.primary < orig {
            i -= 1;
        }
        i
    }

    /// The BWT row range of `pattern` (already folded), as [sp, ep).
    pub fn range(&self, pattern: &[u8]) -> (u64, u64) {
        let (mut sp, mut ep) = (0u64, self.n + 1);
        for &c in pattern.iter().rev() {
            sp = self.c[c as usize] + self.rank(c, sp);
            ep = self.c[c as usize] + self.rank(c, ep);
            if sp >= ep {
                return (0, 0);
            }
        }
        (sp, ep)
    }

    /// LF of a row other than the primary: the row of the suffix one to the
    /// left. Walks the tree once, reading the symbol as it ranks.
    #[inline]
    fn lf(&self, row: u64) -> u64 {
        let (words, dir) = (self.words(), self.dir());
        let mut node = 0usize;
        let mut i = row;
        loop {
            let nd = &self.nodes[node];
            let w0 = nd.word0 as usize;
            let bit = get(words, w0, i) as usize;
            let ones = rank1(words, dir, w0, i);
            i = if bit == 1 { ones } else { i - ones };
            let ch = nd.child[bit];
            if ch & LEAF != 0 {
                let sym = (ch & !LEAF) as u8;
                if sym == 0 && self.primary < row {
                    i -= 1;
                }
                return self.c[sym as usize] + i;
            }
            node = ch as usize;
        }
    }

    /// Text position of a row, and the LF steps it took.
    #[inline]
    pub fn locate(&self, mut row: u64) -> (u32, u32) {
        let mark = &self.mark[8..8 + 8 * self.mark_words];
        let mdir = &self.mark[8 + 8 * self.mark_words..];
        let mut steps = 0u32;
        while !get(mark, 0, row) {
            row = self.lf(row);
            steps += 1;
        }
        let r = rank1(mark, mdir, 0, row) as usize;
        (u32_at(&self.samples, r) + steps, steps)
    }
}

/// A plain suffix array segment, mapped with its text.
pub struct Sa {
    pub text: Mmap,
    pub sa: Mmap,
}

impl Sa {
    pub fn files(dir: &Path, k: usize) -> [PathBuf; 2] {
        [file(dir, k, "text"), file(dir, k, "sa")]
    }

    pub fn open(dir: &Path, k: usize) -> Self {
        let [t, s] = Self::files(dir, k);
        Sa {
            text: map(&t, Advice::Random),
            sa: map(&s, Advice::Normal),
        }
    }

    fn len(&self) -> usize {
        self.sa.len() / 4
    }

    fn suffix(&self, i: usize) -> &[u8] {
        &self.text[u32_at(&self.sa, i) as usize..]
    }

    /// The SA range of suffixes starting with `pattern`, as [lo, hi).
    pub fn range(&self, pattern: &[u8]) -> (usize, usize) {
        let prefix = |i: usize| {
            let s = self.suffix(i);
            &s[..s.len().min(pattern.len())]
        };
        let (mut lo, mut hi) = (0, self.len());
        while lo < hi {
            let mid = (lo + hi) / 2;
            if prefix(mid) < pattern {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }
        let start = lo;
        hi = self.len();
        while lo < hi {
            let mid = (lo + hi) / 2;
            if prefix(mid) <= pattern {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }
        (start, lo)
    }

    pub fn positions(&self, (lo, hi): (usize, usize), out: &mut Vec<u32>) {
        out.extend(
            self.sa[4 * lo..4 * hi]
                .chunks_exact(4)
                .map(|c| u32::from_le_bytes(c.try_into().expect("4"))),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The FM-index counts and locates exactly what a naive scan finds, over
    /// a text with every byte value, repeats, and the placeholder byte 0.
    #[test]
    fn fm_matches_naive_scan() {
        let dir = std::env::temp_dir().join(format!("fm-screen-test-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let mut text = Vec::new();
        let mut x = 12345u64;
        for i in 0..20000u32 {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            text.push(if i % 7 == 0 { b"abcab"[(x % 5) as usize] } else { (x % 256) as u8 });
        }
        text.extend_from_slice(b"\0\0abcabcabc\0");
        let sa: Vec<i32> = libsais::SuffixArrayConstruction::for_text(&text)
            .in_owned_buffer32()
            .single_threaded()
            .run()
            .unwrap()
            .into_vec();
        fs::write(file(&dir, 0, "text"), &text).unwrap();
        write_u32s(&file(&dir, 0, "sa"), sa.iter().map(|&v| v as u32));
        build_fm(&dir, 0, &[1, 4, 32]);
        let sa_view = Sa::open(&dir, 0);
        for rate in [1, 4, 32] {
            let fm = Fm::open(&dir, 0, rate);
            for pat in [&b"a"[..], b"ab", b"abc", b"\0", b"\0\0a", b"cab", b"\xff", b"zzzz"] {
                let mut naive: Vec<u32> = (0..text.len())
                    .filter(|&i| text[i..].starts_with(pat))
                    .map(|i| i as u32)
                    .collect();
                let (sp, ep) = fm.range(pat);
                let mut got: Vec<u32> = (sp..ep).map(|r| fm.locate(r).0).collect();
                got.sort_unstable();
                naive.sort_unstable();
                assert_eq!(got, naive, "rate {rate} pattern {pat:?}");
                let mut from_sa = Vec::new();
                sa_view.positions(sa_view.range(pat), &mut from_sa);
                from_sa.sort_unstable();
                assert_eq!(from_sa, naive, "sa pattern {pat:?}");
            }
        }
        fs::remove_dir_all(&dir).unwrap();
    }
}
