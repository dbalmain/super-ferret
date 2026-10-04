//! Effective-view checkpoint encoding. Only ids are planned; observations and
//! encoded sections stay in their source view or bounded column buffers.
use std::collections::HashSet;
use std::fs::File;
use std::io::{self, Write};
use std::os::unix::fs::FileExt;

use crate::build::{STAT_COLUMNS, stat_field};
use crate::format::{
    self, At, Bits, COLUMNS, Coding, Column, ColumnWriter, Descriptor, Head, NONE, Range, SECTIONS,
    Section,
};
use crate::packed::BlockSizer;
use crate::{Catalog, DocId, Generation, InoId, Kind, Target};

struct Plan {
    order: Vec<InoId>,
    remap: Vec<u32>,
    dir_names: Vec<u32>,
    docs: Vec<DocId>,
    refs: Vec<u32>,
    roots: Vec<InoId>,
    work_trees: Vec<InoId>,
    dirs: usize,
}

fn invalid(label: &'static str) -> io::Error {
    io::Error::other(crate::DecodeError::Corrupt(label))
}

impl Plan {
    fn new(view: &Catalog) -> io::Result<Self> {
        let mut roots: Vec<_> = view.roots().collect();
        roots.sort_by(|a, b| a.1.cmp(b.1));
        let roots: Vec<_> = roots.into_iter().map(|(id, _)| id).collect();
        let mut order = roots.clone();
        let mut remap = vec![NONE; view.next_inode().0 as usize];
        let mut dir_names = vec![NONE; roots.len()];
        for (new, old) in roots.iter().enumerate() {
            remap[old.0 as usize] = new as u32;
        }
        let mut files = Vec::new();
        let mut name = 0u32;
        let mut at = 0;
        while at < order.len() {
            for id in view.children(order[at]) {
                if let Target::Inode(child) = view.name(id).target() {
                    if view.is_directory(child) {
                        if remap[child.0 as usize] != NONE {
                            return Err(invalid("compaction directory reached twice"));
                        }
                        remap[child.0 as usize] = order.len() as u32;
                        order.push(child);
                        dir_names.push(name);
                    } else if remap[child.0 as usize] == NONE {
                        // Provisional file indices only mark visited aliases.
                        remap[child.0 as usize] = files.len() as u32;
                        files.push(child);
                    }
                }
                name = name.checked_add(1).ok_or_else(|| invalid("name limit"))?;
            }
            at += 1;
        }
        let dirs = order.len();
        order.extend(files);
        for (new, old) in order.iter().enumerate().skip(dirs) {
            remap[old.0 as usize] = new as u32;
        }
        if order.len() != view.inode_count() as usize || name != view.name_count() {
            return Err(invalid("compaction unreachable rows"));
        }
        let mut docs: Vec<_> = view.docs().map(|(id, _)| id).collect();
        docs.sort_unstable();
        let refs = vec![0; docs.len()];
        let mut work_trees: Vec<_> = view.work_trees().map(|row| row.dir).collect();
        work_trees.sort_unstable_by_key(|id| remap[id.0 as usize]);
        Ok(Self {
            order,
            remap,
            dir_names,
            docs,
            refs,
            roots,
            work_trees,
            dirs,
        })
    }

    fn child(&self, target: Target) -> u32 {
        match target {
            Target::Inode(id) => self.remap[id.0 as usize],
            Target::Ignored(kind) => kind.ignored_child(),
        }
    }

    fn doc_row(&self, id: DocId) -> Option<usize> {
        if let (Some(first), Some(last)) = (self.docs.first(), self.docs.last())
            && last.0 - first.0 + 1 == self.docs.len() as u32
        {
            return id
                .0
                .checked_sub(first.0)
                .map(|n| n as usize)
                .filter(|&n| n < self.docs.len());
        }
        self.docs.binary_search(&id).ok()
    }

    fn head(
        &mut self,
        view: &Catalog,
        generation: Generation,
    ) -> io::Result<(Head, [Vec<u64>; COLUMNS.len()])> {
        let mut ranges = [Range::default(); COLUMNS.len()];
        let mut sets: [HashSet<u64>; COLUMNS.len()] = Default::default();
        let mut last = [None; COLUMNS.len()];
        let mut blocks: [BlockSizer; COLUMNS.len()] = Default::default();
        let mut lens = [0u64; SECTIONS.len()];
        for &id in &self.order {
            let inode = view.inode(id);
            for column in STAT_COLUMNS {
                let c = column as usize;
                let value = stat_field(column, &inode.stat);
                match column.coding() {
                    Coding::Dictionary => {
                        if last[c] != Some(value) {
                            sets[c].insert(value);
                            last[c] = Some(value);
                        }
                    }
                    _ => blocks[c].push(value),
                }
            }
            match inode.doc {
                Some(doc) => {
                    blocks[Column::Doc as usize].push(u64::from(doc.0));
                    let row = self
                        .doc_row(doc)
                        .ok_or_else(|| invalid("compaction missing document"))?;
                    self.refs[row] = self.refs[row]
                        .checked_add(1)
                        .ok_or_else(|| invalid("document refcount overflow"))?;
                }
                None => blocks[Column::Doc as usize].push_null(),
            }
            match view.kind(id) {
                Kind::Symlink => {
                    lens[Section::Links as usize] += 8;
                    lens[Section::Strings as usize] += view
                        .link_target(id)
                        .ok_or_else(|| invalid("missing link target"))?
                        .len() as u64
                        + 1;
                }
                Kind::Dir | Kind::File => {}
                _ => lens[Section::Specials as usize] += 8,
            }
        }
        for (&id, &count) in self.docs.iter().zip(&self.refs) {
            if count == 0 || view.doc_references(id) != Some(count) {
                return Err(invalid("compaction document refcounts"));
            }
        }
        let mut offset = 0;
        for (parent, &dir) in self.order[..self.dirs].iter().enumerate() {
            let c = Column::DirName as usize;
            if self.dir_names[parent] != NONE {
                ranges[c].add(u64::from(self.dir_names[parent]));
            }
            for (column, value) in [
                (Column::Entries, view.entry_count(dir).map(u64::from)),
                (Column::RetainedAt, view.retained_at(dir)),
            ] {
                match value {
                    Some(n) => blocks[column as usize].push(n),
                    None => blocks[column as usize].push_null(),
                }
            }
            for id in view.children(dir) {
                let name = view.name(id);
                blocks[Column::NameParent as usize].push(parent as u64);
                blocks[Column::NameChild as usize].push(u64::from(self.child(name.target())));
                blocks[Column::NameOffset as usize].push(offset);
                offset += name.bytes.len() as u64 + 1;
            }
        }
        if offset > u64::from(u32::MAX) {
            return Err(invalid("compaction name heap limit"));
        }
        lens[Section::NameHeap as usize] = offset;
        for (row, &id) in self.docs.iter().enumerate() {
            ranges[Column::DocId as usize].add(u64::from(id.0) - row as u64);
        }
        for (_, path) in view.roots() {
            lens[Section::Strings as usize] += path.len() as u64 + 1;
        }
        for tree in view.work_trees() {
            lens[Section::Strings as usize] += tree.common_dir.len() as u64 + 1;
        }
        if lens[Section::Strings as usize] > u64::from(u32::MAX) {
            return Err(invalid("compaction strings limit"));
        }
        lens[Section::Roots as usize] = self.roots.len() as u64 * 8;
        lens[Section::WorkTrees as usize] = self.work_trees.len() as u64 * 32;
        let independent_suppression = self.order[..self.dirs]
            .iter()
            .any(|&dir| view.is_search_suppressed(dir) != view.is_traversed(dir));
        lens[Section::Traversed as usize] =
            self.dirs.div_ceil(8) as u64 * if independent_suppression { 2 } else { 1 };
        lens[Section::States as usize] = self.order.len().div_ceil(4) as u64;
        lens[Section::Docs as usize] = self.docs.len() as u64 * 16;
        lens[Section::DocRefs as usize] = self.docs.len() as u64 * 4;
        lens[Section::Policy as usize] = 16;
        let mut dicts: [Vec<u64>; COLUMNS.len()] = Default::default();
        let columns = COLUMNS.map(|column| {
            let c = column as usize;
            match column.coding() {
                Coding::Dictionary => {
                    dicts[c] = std::mem::take(&mut sets[c]).into_iter().collect();
                    dicts[c].sort_unstable();
                    Descriptor::dictionary(dicts[c].len() as u32)
                }
                Coding::Nullable => Descriptor::nullable(ranges[c]),
                Coding::Sequence => Descriptor::frame(ranges[c]),
                _ => Descriptor::blocked(std::mem::take(&mut blocks[c]).finish()),
            }
        });
        Ok((
            Head {
                generation,
                sniffer: view.sniffer_version(),
                next_doc: view.next_doc().0,
                dirs: self.dirs as u32,
                inodes: self.order.len() as u32,
                names: view.name_count(),
                docs: self.docs.len() as u32,
                columns,
                lens,
            },
            dicts,
        ))
    }
}

pub(crate) fn write(view: &Catalog, out: &File, generation: Generation) -> io::Result<()> {
    let mut plan = Plan::new(view)?;
    let (head, dicts) = plan.head(view, generation)?;
    let (bytes, len) = head.encode();
    out.write_all_at(&bytes, 0)?;
    let layout = format::decode_table(&bytes, len).map_err(io::Error::other)?;
    let column = |c: Column| ColumnWriter::start(out, &layout, c, &dicts[c as usize]);
    let section = |s: Section| At::new(out, layout.range(s).0 as u64);
    let (mut parents, mut children, mut offsets) = (
        column(Column::NameParent)?,
        column(Column::NameChild)?,
        column(Column::NameOffset)?,
    );
    let mut heap = section(Section::NameHeap);
    let mut offset = 0;
    for (parent, &dir) in plan.order[..plan.dirs].iter().enumerate() {
        for id in view.children(dir) {
            let name = view.name(id);
            parents.value(parent as u64)?;
            children.value(u64::from(plan.child(name.target())))?;
            offsets.value(offset)?;
            heap.write_all(name.bytes)?;
            heap.write_all(&[0])?;
            offset += name.bytes.len() as u64 + 1;
        }
    }
    parents.finish()?;
    children.finish()?;
    offsets.finish()?;
    heap.finish()?;
    let (mut names, mut entries, mut retained) = (
        column(Column::DirName)?,
        column(Column::Entries)?,
        column(Column::RetainedAt)?,
    );
    let (mut traversed, mut bits) = (section(Section::Traversed), Bits::new(1));
    for (row, &dir) in plan.order[..plan.dirs].iter().enumerate() {
        names.nullable((plan.dir_names[row] != NONE).then_some(u64::from(plan.dir_names[row])))?;
        entries.nullable(view.entry_count(dir).map(u64::from))?;
        retained.nullable(view.retained_at(dir))?;
        bits.push(&mut traversed, u8::from(view.is_traversed(dir)))?;
    }
    names.finish()?;
    entries.finish()?;
    retained.finish()?;
    bits.finish(&mut traversed)?;
    if head.lens[Section::Traversed as usize] > plan.dirs.div_ceil(8) as u64 {
        let mut suppressed = Bits::new(1);
        for &dir in &plan.order[..plan.dirs] {
            suppressed.push(&mut traversed, u8::from(view.is_search_suppressed(dir)))?;
        }
        suppressed.finish(&mut traversed)?;
    }
    traversed.finish()?;
    plan.dir_names = Vec::new();

    let (mut roots, mut links, mut specials, mut trees, mut strings) = (
        section(Section::Roots),
        section(Section::Links),
        section(Section::Specials),
        section(Section::WorkTrees),
        section(Section::Strings),
    );
    let mut offset = 0u32;
    let root_paths: std::collections::BTreeMap<_, _> = view.roots().collect();
    for &old in &plan.roots {
        let path = root_paths
            .get(&old)
            .ok_or_else(|| invalid("missing root"))?;
        format::put_pair(&mut roots, plan.remap[old.0 as usize], offset)?;
        strings.write_all(path)?;
        strings.write_all(&[0])?;
        offset += path.len() as u32 + 1;
    }
    for (new, &old) in plan.order.iter().enumerate() {
        match view.kind(old) {
            Kind::Symlink => {
                let target = view
                    .link_target(old)
                    .ok_or_else(|| invalid("missing link target"))?;
                format::put_pair(&mut links, new as u32, offset)?;
                strings.write_all(target)?;
                strings.write_all(&[0])?;
                offset += target.len() as u32 + 1;
            }
            Kind::Dir | Kind::File => {}
            kind => format::put_pair(&mut specials, new as u32, kind as u32)?,
        }
    }
    for &old in &plan.work_trees {
        let row = view
            .work_tree(old)
            .ok_or_else(|| invalid("missing work tree"))?;
        format::put_work_tree(
            &mut trees,
            plan.remap[old.0 as usize],
            offset,
            row.common_id,
            row.kind as u8,
        )?;
        strings.write_all(row.common_dir)?;
        strings.write_all(&[0])?;
        offset += row.common_dir.len() as u32 + 1;
    }
    roots.finish()?;
    links.finish()?;
    specials.finish()?;
    trees.finish()?;
    strings.finish()?;
    plan.remap = Vec::new();
    let mut stat_columns: Vec<_> = STAT_COLUMNS
        .into_iter()
        .map(|field| column(field).map(|writer| (field, writer)))
        .collect::<io::Result<_>>()?;
    let (mut doc, mut states, mut bits) =
        (column(Column::Doc)?, section(Section::States), Bits::new(2));
    for &old in &plan.order {
        let inode = view.inode(old);
        for (field, writer) in &mut stat_columns {
            let value = stat_field(*field, &inode.stat);
            match field.coding() {
                Coding::Dictionary => {
                    writer.index(dicts[*field as usize].partition_point(|&known| known < value))?
                }
                _ => writer.value(value)?,
            }
        }
        doc.nullable(inode.doc.map(|id| u64::from(id.0)))?;
        bits.push(&mut states, inode.state as u8)?;
    }
    for (_, writer) in stat_columns {
        writer.finish()?;
    }
    doc.finish()?;
    bits.finish(&mut states)?;
    states.finish()?;
    plan.order = Vec::new();
    let mut ids = column(Column::DocId)?;
    let mut hashes = At::new(out, (layout.range(Section::Docs).0 + layout.hashes) as u64);
    let mut refs = section(Section::DocRefs);
    for (&id, count) in plan.docs.iter().zip(plan.refs) {
        ids.value(u64::from(id.0))?;
        hashes.write_all(
            &view
                .doc_hash(id)
                .ok_or_else(|| invalid("missing document hash"))?,
        )?;
        format::put_u32(&mut refs, count)?;
    }
    ids.finish()?;
    hashes.finish()?;
    refs.finish()?;
    let mut policy = section(Section::Policy);
    policy.write_all(&view.policy())?;
    policy.finish()?;
    format::seal(out)
}
