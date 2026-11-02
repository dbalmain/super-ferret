//! The catalog's namespace: names and their bytes, a directory's children,
//! lookups and paths, and the configured roots.

use std::os::unix::fs::FileExt;

use super::*;
use crate::seek::gallop;

impl Catalog {
    /// Every name, NUL-terminated, in `NameId` order: the bytes a filename
    /// scan reads (D14, D28). Needs [`Section::NameHeap`]. A NUL never occurs
    /// inside a name, so a literal match cannot straddle two;
    /// [`Catalog::name_at`] maps a match's offset back to its name.
    pub fn name_heap(&self) -> &[u8] {
        self.resident_names().map_or_else(
            || self.section(Section::NameHeap),
            |names| names.legacy_heap(),
        )
    }

    /// Every name with its id, in heap order. Needs [`Section::Names`].
    pub fn names(&self) -> impl Iterator<Item = (NameId, &[u8])> + '_ {
        self.name_reader()
            .runs_from(NameId(0))
            .map(|(id, name)| (id, name.bytes))
    }

    /// The name columns and the heap, resolved once: what a pass over many
    /// names holds. Needs [`Section::Names`].
    pub fn name_reader(&self) -> NameReader<'_> {
        let rows = self.section(Section::Names);
        NameReader {
            heap: if self.resident_names().is_some() {
                &[]
            } else {
                self.name_heap()
            },
            resident: self.resident_names(),
            offsets: self
                .resident_names()
                .is_none()
                .then(|| self.layout.blocked(Column::NameOffset, rows)),
            parents: self.layout.blocked(Column::NameParent, rows),
            children: self.layout.blocked(Column::NameChild, rows),
            count: self.layout.names,
            overlay: self.overlay.as_ref().map(|o| (o.namespace(), &o.base)),
        }
    }

    /// One name edge. Needs [`Section::Names`].
    pub fn name(&self, id: NameId) -> Name<'_> {
        self.name_reader().get(id)
    }

    /// Whether an epoch name remains live, including sparse births/deaths.
    /// Requires Names. Check the generation before interpreting an external id.
    pub fn is_live_name(&self, id: NameId) -> bool {
        match self.ns_record(overlay::NAME, id.0) {
            Some(Record::NamePut { .. }) => true,
            Some(Record::NameDelete { .. }) => false,
            _ => id.0 < self.base_name_count(),
        }
    }

    /// Where a name's bytes start in [`Catalog::name_heap`]. Needs
    /// [`Section::Names`]; the bytes run to the next name's start, less its
    /// NUL.
    pub fn name_start(&self, id: NameId) -> usize {
        self.resident_names().map_or_else(
            || self.blocked(Column::NameOffset).get(id.0 as usize) as usize,
            |names| names.legacy_start(id),
        )
    }

    /// The raw child column, without reading name bytes. Ignored rows carry
    /// type tags, so use `Name::target()` before reading stat columns. Needs
    /// [`Section::Names`].
    pub fn child(&self, id: NameId) -> InoId {
        self.name_reader().get(id).child
    }

    /// The name whose bytes (or terminator) hold heap offset `offset`, or
    /// `None` past the end of the heap. Needs [`Section::Names`].
    pub fn name_at(&self, offset: usize) -> Option<NameId> {
        if let Some(names) = self.resident_names() {
            return names.legacy_at(offset);
        }
        if offset >= self.name_heap().len() {
            return None;
        }
        let offsets = self.blocked(Column::NameOffset);
        let after = partition_point(self.layout.names, |i| offsets.get(i) as usize <= offset);
        Some(NameId(after as u32 - 1))
    }

    /// The names in directory `dir`, in name order. Needs
    /// [`Section::Names`].
    pub fn children(&self, dir: InoId) -> impl Iterator<Item = NameId> + '_ {
        let (start, end) = self.child_range(dir);
        // Checkpoint children are already sorted by basename. An overlay that
        // touches `dir` merges its sparse names with the surviving base
        // range; an untouched directory, most of them, lists the range.
        let merged = self
            .overlay
            .as_ref()
            .filter(|o| o.namespace().touches(dir.0))
            .map(|o| {
                let mut ids: Vec<_> = (start as u32..end as u32)
                    .map(NameId)
                    .filter(|&id| self.base_name_live(id))
                    .collect();
                ids.extend(o.namespace().children(dir.0).map(NameId));
                ids.sort_unstable_by(|&a, &b| self.name(a).bytes.cmp(self.name(b).bytes));
                ids
            });
        let base = if merged.is_none() {
            start as u32..end as u32
        } else {
            0..0
        };
        base.map(NameId).chain(merged.into_iter().flatten())
    }

    /// Children, visible and ignored, with raw names, kinds and optional stat
    /// rows. Needs Names and Links. Ignored rows never load stat columns.
    pub fn entries(&self, dir: InoId) -> impl Iterator<Item = Entry<'_>> {
        let names = self.name_reader();
        let mut kinds = self.kinds();
        self.children(dir)
            .map(move |id| Self::typed_entry(id, names.child_edge(id), &mut kinds))
    }

    /// One typed name row. Needs Names and Links for a visible non-directory.
    pub fn entry(&self, id: NameId) -> Entry<'_> {
        Self::typed_entry(id, self.name_reader().child_edge(id), &mut self.kinds())
    }

    fn typed_entry<'a>(
        id: NameId,
        (child, bytes): (InoId, &'a [u8]),
        kinds: &mut Kinds<'_>,
    ) -> Entry<'a> {
        let target = Target::from_child(child);
        let kind = match target {
            Target::Inode(inode) => kinds.kind(inode),
            Target::Ignored(kind) => kind,
        };
        Entry {
            name: id,
            bytes,
            kind,
            target,
        }
    }

    /// Where to read a directory's children. Needs Entries for an inode
    /// target. Unknown counts designate unreadable directories.
    pub fn contents(&self, target: Target) -> Option<Contents> {
        match target {
            Target::Ignored(Kind::Dir) => Some(Contents::Ignored),
            Target::Inode(dir) if self.is_directory(dir) => {
                Some(if self.entry_count(dir).is_some() {
                    Contents::Catalogued
                } else {
                    Contents::Unreadable
                })
            }
            _ => None,
        }
    }

    /// Whether a visible directory has a child on disk, ignored names
    /// included. None means unreadable/unknown. Needs Entries.
    pub fn has_children(&self, dir: InoId) -> Option<bool> {
        self.entry_count(dir).map(|count| count != 0)
    }

    /// Resolves an absolute byte path in the innermost configured root.
    /// Needs Roots and Entries (Roots also loads Names). Does not follow
    /// symlinks or `..`. A path below
    /// an opaque marker returns that marker and its unresolved suffix.
    pub fn resolve<'p>(&self, path: &'p [u8]) -> Option<Resolved<'p>> {
        let (root, prefix) = self
            .roots()
            .filter(|&(_, prefix)| {
                path == prefix
                    || path
                        .strip_prefix(prefix)
                        .is_some_and(|rest| prefix == b"/" || rest.starts_with(b"/"))
            })
            .max_by_key(|&(_, prefix)| prefix.len())?;
        let mut target = Target::Inode(root);
        let mut name = None;
        let mut rest = &path[prefix.len()..];
        loop {
            let separator = rest.starts_with(b"/");
            while let Some(next) = rest.strip_prefix(b"/") {
                rest = next;
            }
            if separator
                && !matches!(target, Target::Ignored(Kind::Dir))
                && !matches!(target, Target::Inode(dir) if self.is_directory(dir))
            {
                return None;
            }
            if rest.is_empty() {
                return Some(Resolved {
                    name,
                    target,
                    remainder: rest,
                });
            }
            let end = rest
                .iter()
                .position(|&byte| byte == b'/')
                .unwrap_or(rest.len());
            let part = &rest[..end];
            if part == b".." {
                return None;
            }
            if part != b"." {
                let Target::Inode(dir) = target else {
                    return Some(Resolved {
                        name,
                        target,
                        remainder: rest,
                    });
                };
                if !self.is_directory(dir) {
                    return None;
                }
                if self.entry_count(dir).is_none() {
                    return Some(Resolved {
                        name,
                        target,
                        remainder: rest,
                    });
                }
                let id = self.lookup(dir, part)?;
                name = Some(id);
                target = self.name(id).target();
            }
            rest = &rest[end..];
        }
    }

    fn child_range(&self, dir: InoId) -> (usize, usize) {
        let parents = self.blocked(Column::NameParent);
        let dir = u64::from(dir.0);
        let start = partition_point(self.layout.names, |i| parents.get(i) < dir);
        // Most directories are small: find the end from the start rather
        // than across every name.
        (
            start,
            start + gallop(self.layout.names - start, |i| parents.get(start + i) <= dir),
        )
    }

    /// The entry called `name` in directory `dir`. Needs [`Section::Names`].
    pub fn lookup(&self, dir: InoId, name: &[u8]) -> Option<NameId> {
        if let Some(o) = &self.overlay {
            return o.namespace().lookup(&o.base, dir.0, name).map(NameId);
        }
        let (start, end) = self.child_range(dir);
        let bytes = |i: usize| self.name(NameId((start + i) as u32)).bytes;
        let at = partition_point(end - start, |i| bytes(i) < name);
        (at < end - start && bytes(at) == name).then_some(NameId((start + at) as u32))
    }

    /// A directory's own name edge; `None` for a root (D30). Needs
    /// [`Section::DirNames`].
    pub fn dir_name(&self, dir: InoId) -> Option<NameId> {
        if let Some(Record::DirPut { name, .. }) = self.ns_record(overlay::DIR, dir.0) {
            return name.map(NameId);
        }
        self.column(Column::DirName)
            .nullable(dir.0 as usize)
            .map(|name| NameId(name as u32))
    }

    /// Whether search should suppress this ancestor of a re-included entry
    /// (D29 compatibility). Find sees an ordinary directory inode. Needs
    /// [`Section::Traversed`].
    pub fn is_traversed(&self, dir: InoId) -> bool {
        if let Some(Record::DirPut { flags, .. }) = self.ns_record(overlay::DIR, dir.0) {
            return flags & 1 != 0;
        }
        let bits = self.section(Section::Traversed);
        bits[dir.0 as usize / 8] >> (dir.0 % 8) & 1 == 1
    }

    /// Whether a repository ignore file's `!` line reveals this hidden name
    /// (D127): rg lists it without `--hidden`. A name's overlay row decides,
    /// as for [`Self::is_traversed`]; then the base's sorted list. Needs
    /// [`Section::Revealed`].
    pub fn is_revealed(&self, name: NameId) -> bool {
        match self.ns_record(overlay::NAME, name.0) {
            Some(Record::NamePut { revealed, .. }) => *revealed,
            Some(_) => false,
            None => self
                .section(Section::Revealed)
                .as_chunks::<4>()
                .0
                .binary_search_by(|row| u32::from_le_bytes(*row).cmp(&name.0))
                .is_ok(),
        }
    }

    /// Search suppression is independent of traversal and retained coverage.
    /// Requires Traversed (which loads namespace flags).
    pub fn is_search_suppressed(&self, dir: InoId) -> bool {
        if let Some(Record::DirPut { flags, .. }) = self.ns_record(overlay::DIR, dir.0) {
            return flags & 2 != 0;
        }
        let bits = self.section(Section::Traversed);
        let offset = self.layout.dirs.div_ceil(8);
        if bits.len() == offset * 2 && offset != 0 {
            return bits[offset + dir.0 as usize / 8] >> (dir.0 % 8) & 1 == 1;
        }
        self.is_traversed(dir)
    }
    /// Appends the path of the entry `name` names: its root's path, then each
    /// name below it, joined by `/`. Needs [`Section::Roots`], which loads
    /// every section a path reads.
    pub fn path(&self, name: NameId, out: &mut Vec<u8>) {
        let (parent, bytes) = self.name_reader().edge(name);
        self.dir_path(parent, out);
        push_component(out, bytes);
    }

    /// Appends a directory's path. Needs [`Section::Roots`].
    pub fn dir_path(&self, dir: InoId, out: &mut Vec<u8>) {
        let names = self.name_reader();
        let mut up = Vec::new();
        let mut at = dir;
        while let Some(name) = self.dir_name(at) {
            let (parent, bytes) = names.edge(name);
            up.push(bytes);
            // Decoding checked that the parent's id is lower, so this ends.
            at = parent;
        }
        out.extend_from_slice(self.root_path(at).unwrap_or_default());
        for name in up.iter().rev() {
            push_component(out, name);
        }
    }

    /// Configured root paths for routing, without loading name tables or
    /// building the namespace graph. Checks the root/string checksums and
    /// path bounds and applies committed root changes. This does not prove
    /// inode liveness: use [`Self::load`] and [`Self::roots`] for traversal.
    pub fn configured_paths(&self) -> Result<Vec<Vec<u8>>, OpenError> {
        self.load_one(Section::Strings)?;
        let roots = if let Source::File { file, read, .. } = self.source.as_ref() {
            let (start, end) = self.layout.range(Section::Roots);
            let mut bytes = vec![0; end - start];
            file.read_exact_at(&mut bytes, start as u64)
                .map_err(OpenError::Io)?;
            read.fetch_add(bytes.len() as u64, Ordering::Relaxed);
            bytes
        } else {
            self.section(Section::Roots).to_vec()
        };
        let corrupt = || OpenError::Decode(DecodeError::Corrupt("roots"));
        if crate::generation::checksum(&roots) != self.layout.checksums[Section::Roots as usize] {
            return Err(corrupt());
        }
        let strings = self.section(Section::Strings);
        let mut paths = std::collections::BTreeMap::new();
        let mut last = None;
        for row in roots.chunks_exact(PAIR_ROW) {
            let id = u32_at(row, 0);
            let offset = u32_at(row, 4) as usize;
            if id as usize >= self.layout.dirs || last.is_some_and(|last| id <= last) {
                return Err(corrupt());
            }
            let rest = strings.get(offset..).ok_or_else(corrupt)?;
            let end = rest
                .iter()
                .position(|&byte| byte == 0)
                .ok_or_else(corrupt)?;
            paths.insert(id, rest[..end].to_vec());
            last = Some(id);
        }
        if let Some(overlay) = &self.overlay {
            overlay.load(Family::Namespace)?;
            for record in overlay.projection(Family::Namespace).records(overlay::ROOT) {
                match record {
                    Record::RootPut { id, path } => {
                        paths.insert(*id, path.clone());
                    }
                    Record::RootDelete { id } => {
                        paths.remove(id);
                    }
                    _ => unreachable!("root projection contains only root records"),
                }
            }
        }
        Ok(paths.into_values().collect())
    }

    /// The configured roots, as `(top directory, path)`. Needs
    /// [`Section::Roots`].
    pub fn roots(&self) -> impl Iterator<Item = (InoId, &[u8])> + '_ {
        let base = self
            .section(Section::Roots)
            .chunks_exact(PAIR_ROW)
            .map(|pair| (InoId(u32_at(pair, 0)), self.string(u32_at(pair, 4))))
            .filter(|(id, _)| self.ns_record(overlay::ROOT, id.0).is_none());
        let delta = self
            .overlay
            .iter()
            .flat_map(|o| o.projection(Family::Namespace).records(overlay::ROOT))
            .filter_map(|r| match r {
                Record::RootPut { id, path } => Some((InoId(*id), path.as_slice())),
                _ => None,
            });
        base.chain(delta)
    }

    fn root_path(&self, dir: InoId) -> Option<&[u8]> {
        self.roots()
            .find(|&(root, _)| root == dir)
            .map(|(_, path)| path)
    }
}

fn push_component(out: &mut Vec<u8>, name: &[u8]) {
    if out.last() != Some(&b'/') {
        out.push(b'/');
    }
    out.extend_from_slice(name);
}
