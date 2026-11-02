//! Loading a file's sections on demand, each checked as it arrives, and
//! reading a loaded one.

use std::os::unix::fs::FileExt;

use super::*;
use crate::format::View;
use crate::lock;
use crate::packed::Blocked;

impl Catalog {
    /// Loads `sections`, and each section they depend on, validating each as
    /// it arrives. A section already loaded is not read again. After this
    /// succeeds, every accessor that reads only these sections is
    /// infallible. A failure leaves whatever loaded before it loaded.
    pub fn load(&self, sections: &[Section]) -> Result<(), OpenError> {
        for &section in sections {
            self.load_one(section)?;
            if let Some(o) = &self.overlay {
                o.load(Family::Namespace)?;
                match section {
                    Section::Names
                    | Section::NameHeap
                    | Section::DirNames
                    | Section::Roots
                    | Section::Entries
                    | Section::Traversed
                    | Section::RetainedAt
                    | Section::Revealed => o.load_namespace()?,
                    Section::Links | Section::Specials => {
                        o.load(Family::Aux)?;
                        o.check_aux()?;
                    }
                    Section::WorkTrees => {
                        o.load(Family::Aux)?;
                        o.check_aux()?;
                    }
                    Section::Docs => o.load(Family::Docs)?,
                    Section::DocRefs => {
                        o.load(Family::Inodes)?;
                        o.check_fields()?;
                        o.load(Family::Docs)?;
                    }
                    Section::Strings | Section::Policy => {}
                    _ => {
                        o.load(Family::Inodes)?;
                        o.check_fields()?;
                    }
                }
                o.check_documents(self)?;
            }
        }
        Ok(())
    }

    /// Loads every section: what a caller that reads the whole generation
    /// (the writer's previous generation, a one-shot query, a test) needs.
    /// A large file's sections are read and checked on a few threads first.
    pub fn load_all(&self) -> Result<(), OpenError> {
        // Below this, starting threads costs more than the checks they share.
        const PREFETCH_BYTES: usize = 1 << 20;
        let bytes: usize = SECTIONS
            .iter()
            .map(|&section| {
                let (start, end) = self.layout.range(section);
                end - start
            })
            .sum();
        if bytes >= PREFETCH_BYTES {
            self.prefetch(&[]);
        }
        self.load(&SECTIONS)?;
        if let Some(o) = &self.overlay {
            o.validate_all(self)?;
        }
        Ok(())
    }

    /// Reads and checks the base sections but `skip` on a few threads,
    /// ignoring failures: [`Catalog::load_all`] then finds them loaded, and
    /// loads any that failed again in order, so it reports what a serial
    /// load would. Each section with nothing to wait for heads a chain, and
    /// every other section follows its largest need in that need's chain, at
    /// any depth, so it starts as soon as the need is checked rather than
    /// when every chain is. Longest chains go first.
    pub(super) fn prefetch(&self, skip: &[Section]) {
        const WORKERS: usize = 4;
        if !matches!(self.source.as_ref(), Source::File { .. }) {
            return;
        }
        let size = |section: Section| {
            let (start, end) = self.layout.range(section);
            end - start
        };
        let mut chains: Vec<Vec<Section>> = Vec::new();
        let mut chain_of = [None; SECTIONS.len()];
        let mut pending: Vec<_> = SECTIONS.into_iter().filter(|s| !skip.contains(s)).collect();
        // Needs are acyclic, so each pass places at least one section.
        while !pending.is_empty() {
            let before = pending.len();
            pending.retain(|&section| {
                let needs = section.needs().iter().filter(|n| !skip.contains(n));
                if needs.clone().any(|&n| chain_of[n as usize].is_none()) {
                    return true;
                }
                let lead = needs.max_by_key(|&&n| size(n));
                let index = match lead.and_then(|&n| chain_of[n as usize]) {
                    Some(index) => index,
                    None => {
                        chains.push(Vec::new());
                        chains.len() - 1
                    }
                };
                chains[index].push(section);
                chain_of[section as usize] = Some(index);
                false
            });
            if pending.len() == before {
                break;
            }
        }
        chains
            .sort_by_key(|chain| std::cmp::Reverse(chain.iter().map(|&s| size(s)).sum::<usize>()));
        fan_out(chains, WORKERS, |chain| {
            for section in chain {
                let _ = self.load_one(section);
            }
        });
    }

    pub(super) fn load_one(&self, section: Section) -> Result<(), OpenError> {
        let Source::File {
            file,
            sections,
            loading,
            read,
            facts,
        } = self.source.as_ref()
        else {
            return Ok(());
        };
        let slot = &sections[section as usize];
        if slot.get().is_some() {
            return Ok(());
        }
        for &need in section.needs() {
            self.load_one(need)?;
        }
        // Needs are loaded before this is taken, so no thread holds two.
        let _loading = lock(&loading[section as usize]);
        if slot.get().is_some() {
            return Ok(());
        }
        let (start, end) = self.layout.range(section);
        // Collected from a sized iterator, the slice is allocated once, in
        // place: converting a boxed slice would copy every section again.
        let mut bytes: Arc<[u8]> = std::iter::repeat_n(0, end - start).collect();
        let Some(buffer) = Arc::get_mut(&mut bytes) else {
            unreachable!("a new slice has no other owner");
        };
        file.read_exact_at(buffer, start as u64)
            .map_err(OpenError::Io)?;
        read.fetch_add(bytes.len() as u64, Ordering::Relaxed);
        format::check(section, &self.layout, facts, |s| {
            if s == section {
                &bytes[..]
            } else {
                self.section(s)
            }
        })
        .map_err(OpenError::Decode)?;
        let Ok(()) = slot.set(bytes) else {
            unreachable!("set only under its load lock");
        };
        Ok(())
    }

    /// Whether `section` is loaded. A reader from [`Catalog::from_bytes`]
    /// has every section.
    pub fn is_loaded(&self, section: Section) -> bool {
        let base = match self.source.as_ref() {
            Source::Whole(_) | Source::Resident { .. } => true,
            Source::File { sections, .. } => sections[section as usize].get().is_some(),
        };
        base && self.overlay.as_ref().is_none_or(|o| o.loaded(section))
    }

    /// Bytes read from the file so far: the head of the file and each loaded
    /// section. For a reader from [`Catalog::from_bytes`], the whole file.
    pub fn bytes_read(&self) -> u64 {
        let base = match self.source.as_ref() {
            Source::Whole(bytes) => bytes.len() as u64,
            Source::Resident { read, .. } => *read,
            Source::File { read, .. } => read.load(Ordering::Relaxed),
        };
        base + self.overlay.as_ref().map_or(0, |o| o.bytes_read())
    }

    pub(super) fn column(&self, column: Column) -> View<'_> {
        self.layout.view(column, self.section(column.section()))
    }

    pub(super) fn blocked(&self, column: Column) -> Blocked<'_> {
        self.layout.blocked(column, self.section(column.section()))
    }

    pub(super) fn string(&self, offset: u32) -> &[u8] {
        let rest = &self.section(Section::Strings)[offset as usize..];
        &rest[..rest.iter().position(|&b| b == 0).unwrap_or(0)]
    }

    pub(super) fn section(&self, section: Section) -> &[u8] {
        match self.source.as_ref() {
            Source::Whole(bytes) => self.layout.section(bytes, section),
            Source::Resident { sections, .. } => &sections[section as usize],
            Source::File { sections, .. } => sections[section as usize]
                .get()
                .unwrap_or_else(|| panic!("catalog section {section:?} read before it was loaded")),
        }
    }
}
