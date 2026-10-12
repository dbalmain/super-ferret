//! The term screen as a product could serve it: the dictionary's terms as
//! one flat file, scanned with memmem for every needle, and the residue
//! (whole runs the term index lost, see `residue`) as a second flat file
//! scanned the same way, so a document is admitted past the term screen
//! only when its lost runs hold one of the query's needles. Both files are
//! read in chunks through `pread` inside the timing, so cold reads them from
//! the device and warm copies them from the page cache; mmap would skip the
//! copy, so warm times here are an upper bound.

use std::cell::Cell;
use std::fs::{self, File};
use std::io::Write;
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};

use ferret_index::DocSet;
use ferret_index::segment::{Hit, Segment, TermEntry};
use memchr::memmem::Finder;

use super::super::latency::Meter;
use super::Corpus;
use super::PREFIX_CAP;

const CHUNK: usize = 8 << 20;

struct Flat {
    path: PathBuf,
    file: File,
    len: u64,
}

impl Flat {
    fn write(path: PathBuf, bytes: &[u8]) -> crate::Result<Self> {
        let mut file = File::create(&path)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        Ok(Self {
            file: File::open(&path)?,
            path,
            len: bytes.len() as u64,
        })
    }

    /// Every needle hit, as (needle, absolute offset), reading the file in
    /// chunks cut at record ends. Returns the bytes read.
    fn scan(&self, finders: &[Finder], mut hit: impl FnMut(usize, u64)) -> crate::Result<u64> {
        let mut buf = vec![0u8; CHUNK];
        let (mut offset, mut read) = (0u64, 0u64);
        while offset < self.len {
            let want = buf.len().min((self.len - offset) as usize);
            self.file.read_exact_at(&mut buf[..want], offset)?;
            read += want as u64;
            let end = if offset + want as u64 == self.len {
                want
            } else {
                match memchr::memrchr(b'\n', &buf[..want]) {
                    Some(at) => at + 1,
                    None => {
                        // One record longer than the buffer: grow and reread.
                        buf.resize(buf.len() * 2, 0);
                        continue;
                    }
                }
            };
            for (i, finder) in finders.iter().enumerate() {
                for at in finder.find_iter(&buf[..end]) {
                    hit(i, offset + at as u64);
                }
            }
            offset += end as u64;
        }
        Ok(read)
    }
}

pub(super) struct Resident {
    dictionary: Flat,
    term_starts: Vec<u64>,
    entries: Vec<TermEntry>,
    residue: Flat,
    /// Each residue record's end offset and document, in file order.
    doc_ends: Vec<(u64, u32)>,
    pub(super) bytes: Cell<u64>,
}

impl Drop for Resident {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.dictionary.path);
        let _ = fs::remove_file(&self.residue.path);
    }
}

impl Resident {
    pub(super) fn build(
        term: &Segment<Meter>,
        corpus: &Corpus,
        fallback: &[u32],
        dir: &Path,
    ) -> crate::Result<Self> {
        let (mut flat, mut term_starts, mut entries) = (Vec::new(), Vec::new(), Vec::new());
        let mut terms = term.terms();
        while let Some((bytes, entry)) = terms.next_term()? {
            term_starts.push(flat.len() as u64);
            entries.push(entry);
            flat.extend_from_slice(bytes);
            flat.push(b'\n');
        }
        let pid = std::process::id();
        let dictionary = Flat::write(dir.join(format!("dictionary.{pid}.flat")), &flat)?;
        flat.clear();
        let (mut scratch, mut lost, mut doc_ends) = (Default::default(), Vec::new(), Vec::new());
        for &doc in fallback {
            let bytes = fs::read(corpus.snapshot.path(doc))?;
            super::residue::residue(&bytes, &mut scratch, &mut lost);
            if !lost.is_empty() {
                flat.extend_from_slice(&lost);
                doc_ends.push((flat.len() as u64, doc));
            }
        }
        let residue = Flat::write(dir.join(format!("residue.{pid}.flat")), &flat)?;
        Ok(Self {
            dictionary,
            term_starts,
            entries,
            residue,
            doc_ends,
            bytes: Cell::new(0),
        })
    }

    pub(super) fn sizes(&self) -> (u64, u64, usize, usize) {
        (
            self.dictionary.len,
            self.residue.len,
            self.term_starts.len(),
            self.doc_ends.len(),
        )
    }

    pub(super) fn evict(&self) -> crate::Result<()> {
        for flat in [&self.dictionary, &self.residue] {
            rustix::fs::fadvise(&flat.file, 0, None, rustix::fs::Advice::DontNeed)?;
        }
        Ok(())
    }

    /// Each needle's matching terms and the union of their documents, or
    /// `None` past [`PREFIX_CAP`] terms, as `substring_scan` answers.
    pub(super) fn substrings(
        &self,
        term: &Segment<Meter>,
        needles: &[Vec<u8>],
        bound: u32,
        postings_bytes: &mut u64,
    ) -> crate::Result<Vec<(u64, Option<DocSet>)>> {
        let finders: Vec<_> = needles.iter().map(Finder::new).collect();
        let mut hits: Vec<Vec<usize>> = vec![Vec::new(); needles.len()];
        let read = self.dictionary.scan(&finders, |i, at| {
            let index = self.term_starts.partition_point(|&s| s <= at) - 1;
            if hits[i].last() != Some(&index) {
                hits[i].push(index);
            }
        })?;
        self.bytes.set(self.bytes.get() + read);
        let mut out = Vec::new();
        for terms in hits {
            let named = terms.len() as u64;
            if terms.len() > PREFIX_CAP {
                out.push((named, None));
                continue;
            }
            let mut docs = Vec::new();
            for index in terms {
                match term.read(&self.entries[index])? {
                    Hit::Single(doc) => docs.push(doc),
                    Hit::Postings(list) => {
                        *postings_bytes += list.bytes();
                        list.decode(&mut docs);
                    }
                }
            }
            out.push((named, Some(DocSet::new(bound, docs))));
        }
        Ok(out)
    }

    /// The documents whose lost runs hold any of the needles.
    pub(super) fn residue_docs(&self, needles: &[Vec<u8>], bound: u32) -> crate::Result<DocSet> {
        let finders: Vec<_> = needles.iter().map(Finder::new).collect();
        let mut docs = Vec::new();
        let read = self.residue.scan(&finders, |_, at| {
            let record = self.doc_ends.partition_point(|&(end, _)| end <= at);
            docs.push(self.doc_ends[record].1);
        })?;
        self.bytes.set(self.bytes.get() + read);
        Ok(DocSet::new(bound, docs))
    }
}
