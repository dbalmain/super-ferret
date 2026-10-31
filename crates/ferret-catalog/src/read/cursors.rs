//! Cursors that hold a generation's sections resolved once, for a pass over
//! many names or inodes: [`NameReader`] and its [`NameRuns`], and [`Kinds`].

use super::*;

/// The name columns and the name heap of one generation, resolved once
/// ([`Catalog::name_reader`]), so that a query reading many names looks up
/// no section per name.
#[derive(Clone, Copy)]
pub struct NameReader<'c> {
    pub(super) heap: &'c [u8],
    pub(super) offsets: Option<Blocked<'c>>,
    pub(super) resident: Option<&'c crate::ResidentNames>,
    pub(super) parents: Blocked<'c>,
    pub(super) children: Blocked<'c>,
    pub(super) count: usize,
    pub(super) overlay: Option<(&'c overlay::Namespace, &'c Catalog)>,
}

impl<'c> NameReader<'c> {
    /// One name edge, read by itself: for a sparse hit.
    pub fn get(&self, id: NameId) -> Name<'c> {
        // A view shares its checkpoint's columns, so a name the overlay
        // leaves alone decodes here rather than through a second reader.
        if let Some((ns, _)) = self.overlay {
            if let Some(sparse) = ns.sparse_name(id.0) {
                return sparse.unwrap_or_else(|| panic!("dead name {}", id.0));
            }
            assert!((id.0 as usize) < self.count, "dead name {}", id.0);
        }
        let (parent, bytes) = self.base_edge(id);
        Name {
            parent,
            child: InoId(self.children.get(id.0 as usize) as u32),
            bytes,
        }
    }

    /// A name's directory and bytes, without its child: what a path reads
    /// at each level.
    pub fn edge(&self, id: NameId) -> (InoId, &'c [u8]) {
        if self.overlay.is_some() {
            let n = self.get(id);
            return (n.parent, n.bytes);
        }
        self.base_edge(id)
    }

    /// A checkpoint name's directory and bytes.
    fn base_edge(&self, id: NameId) -> (InoId, &'c [u8]) {
        let i = id.0 as usize;
        if let Some(names) = self.resident {
            return (InoId(self.parents.get(i) as u32), names.name(id));
        }
        (InoId(self.parents.get(i) as u32), self.heap_bytes(id))
    }

    /// A name's bytes from the raw heap, without its parent: what the
    /// resident build reads. Not for an overlay or a resident reader.
    pub(crate) fn heap_bytes(&self, id: NameId) -> &'c [u8] {
        let i = id.0 as usize;
        let offsets = self.offsets.unwrap_or_else(|| unreachable!("raw offsets"));
        // A name runs to the next one's start, less its NUL: decoding checked
        // that the spans tile the heap. One decode is cheaper than a search.
        let end = match i + 1 < self.count {
            true => offsets.get(i + 1) as usize,
            false => self.heap.len(),
        };
        &self.heap[offsets.get(i) as usize..end - 1]
    }

    /// The names from `from` on, in order, decoded a run at a time.
    pub fn runs_from(&self, from: NameId) -> NameRuns<'c> {
        NameRuns {
            names: *self,
            next: from.0 as usize,
            first: 0,
            len: 0,
            offsets: [0; NAME_RUN + 1],
            parents: [0; NAME_RUN],
            children: [0; NAME_RUN],
            delta: 0,
            suppressed: 0,
        }
    }

    /// Every name's child, in name order: a pass over the child column
    /// alone.
    pub fn children(&self) -> impl Iterator<Item = InoId> + 'c {
        self.child_ids().map(|(_, child)| child)
    }
    /// Base name parents in row order, decoding only the parent column.
    pub fn parents(&self) -> impl Iterator<Item = InoId> + 'c {
        let parents = self.parents;
        packed::runs(self.count, move |first, out| parents.decode(first, out))
            .map(|parent| InoId(parent as u32))
    }
    /// Live epoch NameIds and their targets, decoding only the base child
    /// column.
    pub fn child_ids(&self) -> impl Iterator<Item = (NameId, InoId)> + 'c {
        let children = self.children;
        let overlay = self.overlay;
        let base = packed::runs(self.count, move |first, out| children.decode(first, out))
            .enumerate()
            .scan(0usize, move |at, (id, child)| {
                if let Some((ns, _)) = overlay {
                    while *at < ns.suppressed.len() && ns.suppressed[*at] < id as u32 {
                        *at += 1;
                    }
                    if ns.suppressed.get(*at) == Some(&(id as u32)) {
                        *at += 1;
                        return Some(None);
                    }
                }
                Some(Some((NameId(id as u32), InoId(child as u32))))
            })
            .flatten();
        let delta = overlay.into_iter().flat_map(|(ns, base)| {
            ns.names.iter().map(move |&id| {
                let Some(n) = ns.name(base, id) else {
                    unreachable!("live delta name")
                };
                (NameId(id), n.child)
            })
        });
        base.chain(delta)
    }
}

/// Rows a [`NameRuns`] decodes at once: one block of the offset column.
const NAME_RUN: usize = packed::BLOCK_ROWS;

/// Names in order from a [`NameReader`], each column decoded a run at a
/// time. A name's end is the next one's start, so each offset is decoded
/// once: the run decodes one offset past its last row.
pub struct NameRuns<'c> {
    names: NameReader<'c>,
    /// The next name to yield.
    next: usize,
    /// The decoded run: rows `first..first + len`.
    first: usize,
    len: usize,
    offsets: [u64; NAME_RUN + 1],
    parents: [u64; NAME_RUN],
    children: [u64; NAME_RUN],
    delta: usize,
    suppressed: usize,
}

impl NameRuns<'_> {
    /// Decodes the run holding `next`, starting at its block's first row.
    fn fill(&mut self) {
        let names = &self.names;
        let first = self.next / NAME_RUN * NAME_RUN;
        let len = NAME_RUN.min(names.count - first);
        let more = first + len < names.count;
        if let Some(offsets) = names.offsets {
            offsets.decode(first, &mut self.offsets[..len + usize::from(more)]);
            if !more {
                self.offsets[len] = names.heap.len() as u64;
            }
        }
        names.parents.decode(first, &mut self.parents[..len]);
        names.children.decode(first, &mut self.children[..len]);
        (self.first, self.len) = (first, len);
    }
}

impl<'c> Iterator for NameRuns<'c> {
    type Item = (NameId, Name<'c>);

    fn next(&mut self) -> Option<Self::Item> {
        if let Some((ns, _)) = self.names.overlay {
            while self.suppressed < ns.suppressed.len()
                && ns.suppressed[self.suppressed] < self.next as u32
            {
                self.suppressed += 1;
            }
            while self.suppressed < ns.suppressed.len()
                && ns.suppressed[self.suppressed] == self.next as u32
            {
                self.next += 1;
                self.suppressed += 1;
            }
        }
        let i = self.next;
        if i >= self.names.count {
            let (ns, _) = self.names.overlay?;
            let id = NameId(*ns.names.get(self.delta)?);
            self.delta += 1;
            return Some((id, self.names.get(id)));
        }
        if i >= self.first + self.len || i < self.first {
            self.fill();
        }
        let j = i - self.first;
        self.next += 1;
        let (start, end) = (self.offsets[j] as usize, self.offsets[j + 1] as usize);
        Some((
            NameId(i as u32),
            Name {
                parent: InoId(self.parents[j] as u32),
                child: InoId(self.children[j] as u32),
                bytes: self.names.resident.map_or_else(
                    || &self.names.heap[start..end - 1],
                    |names| names.name(NameId(i as u32)),
                ),
            },
        ))
    }
}

/// [`Catalog::kind`] with a memory of where the last search ended
/// ([`Catalog::kinds`]): a rising id gallops forwards from there, and one
/// that falls searches again. Names in order ask for rising file ids, since
/// files are numbered by their first name.
pub struct Kinds<'c> {
    pub(super) dirs: u32,
    pub(super) overlay: Option<&'c Overlay>,
    pub(super) links: &'c [u8],
    pub(super) specials: &'c [u8],
    /// The first link row whose inode is at or past the last id asked for.
    pub(super) at: usize,
}

impl Kinds<'_> {
    /// An inode's file type, including FIFO, socket and device kinds.
    pub fn kind(&mut self, id: InoId) -> Kind {
        if let Some(kind) = self.overlay.and_then(|o| o.kind(id.0)) {
            return kind;
        }
        if id.0 < self.dirs {
            return Kind::Dir;
        }
        if let Some(kind) = special_kind(self.specials, id) {
            return kind;
        }
        let links = self.links;
        let n = links.len() / PAIR_ROW;
        let key = |i: usize| u32_at(links, i * PAIR_ROW);
        let below = |i: usize| key(i) < id.0;
        if self.at > 0 && !below(self.at - 1) {
            self.at = partition_point(self.at, below);
        } else {
            // Gallop: `lo` is at or before the answer, `hi` past it or `n`.
            let (mut lo, mut hi, mut step) = (self.at, self.at, 1);
            while hi < n && below(hi) {
                lo = hi + 1;
                hi = (hi + step).min(n);
                step *= 2;
            }
            self.at = lo + partition_point(hi - lo, |i| below(lo + i));
        }
        match self.at < n && key(self.at) == id.0 {
            true => Kind::Symlink,
            false => Kind::File,
        }
    }
}
