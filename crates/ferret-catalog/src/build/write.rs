//! Streams a planned generation out: each section at the place the head
//! gives it, each table's columns filled in one pass over its rows (D40).

use std::fs::File;
use std::io::{self, Write};

use super::{Plan, STAT_COLUMNS, stat_field};
use crate::batch::{Batch, Stat};
use crate::format::{self, Bits, COLUMNS, Coding, Column, ColumnWriter, NONE, Output, Section};

/// The inode stat columns, filled a row at a time. A column of width 0 holds
/// nothing per row, so it is only started and finished.
pub(crate) struct StatColumns<'f> {
    live: Vec<(Column, ColumnWriter<'f>)>,
    idle: Vec<ColumnWriter<'f>>,
}

impl<'f> StatColumns<'f> {
    pub(crate) fn start(out: &Output<'f>) -> io::Result<Self> {
        let (mut live, mut idle) = (Vec::new(), Vec::new());
        for field in STAT_COLUMNS {
            let column = out.column(field)?;
            if field.coding().is_blocked() || out.layout.columns[field as usize].desc.width > 0 {
                live.push((field, column));
            } else {
                idle.push(column);
            }
        }
        Ok(Self { live, idle })
    }

    /// One inode's row, a dictionary column storing the value's index in
    /// `dicts`.
    pub(crate) fn row(&mut self, stat: &Stat, dicts: &[Vec<u64>; COLUMNS.len()]) -> io::Result<()> {
        for (field, column) in &mut self.live {
            let value = stat_field(*field, stat);
            match field.coding() {
                Coding::Dictionary => {
                    let dict = &dicts[*field as usize];
                    column.index(dict.partition_point(|&known| known < value))?;
                }
                _ => column.value(value)?,
            }
        }
        Ok(())
    }

    pub(crate) fn finish(self) -> io::Result<()> {
        for (_, column) in self.live {
            column.finish()?;
        }
        for column in self.idle {
            column.finish()?;
        }
        Ok(())
    }
}

/// Writes the planned generation to `out`: the head, then each section at the
/// place the head gives it. Each table's columns fill in one pass over its
/// rows, each column through its own bounded buffer (D40). Frees each batch's
/// names once the name sections are written, and the batches once the inode
/// sections are.
pub(crate) fn write(
    mut plan: Plan,
    mut batches: Vec<Batch>,
    out: &File,
    generation: crate::Generation,
    policy: crate::Hash,
) -> io::Result<()> {
    let (mut head, dicts) = plan.head(&batches);
    head.generation = generation;
    let file = Output::start(out, &head, dicts)?;
    plan.write_names(&batches, &file)?;
    plan.edges = Vec::new();
    plan.edge_start = Vec::new();
    plan.inode_of_file = Vec::new();
    plan.dir_id = Vec::new();
    batches.iter_mut().for_each(Batch::drop_structure);
    plan.write_dirs(&batches, &file)?;
    plan.entry_count = Vec::new();
    plan.write_strings(&file)?;
    let references = plan.write_inodes(&batches, &file)?;
    drop(batches);
    plan.write_tables(&file)?;
    plan.write_docs(&file, references)?;
    let mut fingerprint = file.section(Section::Policy);
    fingerprint.write_all(&policy)?;
    fingerprint.finish()?;
    let mut revealed = file.section(Section::Revealed);
    for &name in &plan.revealed {
        format::put_u32(&mut revealed, name)?;
    }
    revealed.finish()?;
    crate::dormant::write(&plan.dormant, &file)?;
    format::seal(out)
}

/// A column value, or null for [`NONE`].
fn known(id: u32) -> Option<u64> {
    (id != NONE).then_some(u64::from(id))
}

impl Plan {
    /// The name columns and heap, in directory order.
    fn write_names(&self, batches: &[Batch], out: &Output) -> io::Result<()> {
        let (mut parents, mut children) = (
            out.column(Column::NameParent)?,
            out.column(Column::NameChild)?,
        );
        let (mut offsets, mut heap) = (
            out.column(Column::NameOffset)?,
            out.section(Section::NameHeap),
        );
        let mut offset = 0;
        for (id, &dir) in self.order.iter().enumerate() {
            for &entry in self.children(dir) {
                let child = self.child(batches, entry);
                let name = self.name(batches, entry);
                parents.value(id as u64)?;
                children.value(u64::from(child))?;
                offsets.value(offset)?;
                heap.write_all(name)?;
                heap.write_all(&[0])?;
                offset += name.len() as u64 + 1;
            }
        }
        parents.finish()?;
        children.finish()?;
        offsets.finish()?;
        heap.finish()?;
        Ok(())
    }

    /// The directory columns and the traversed bits.
    fn write_dirs(&self, batches: &[Batch], out: &Output) -> io::Result<()> {
        let (mut dir_names, mut entries) =
            (out.column(Column::DirName)?, out.column(Column::Entries)?);
        let mut retained = out.column(Column::RetainedAt)?;
        let (mut traversed, mut bits) = (out.section(Section::Traversed), Bits::new(1));
        for &dir in &self.order {
            dir_names.nullable(known(self.name_of_dir[dir as usize]))?;
            let (b, d) = self.index.dir(dir as usize);
            retained.nullable(batches[b].dirs[d].retained_at)?;
            entries.nullable(known(self.entry_count[dir as usize]))?;
            let (b, d) = self.index.dir(dir as usize);
            bits.push(&mut traversed, u8::from(batches[b].dirs[d].traversed))?;
        }
        dir_names.finish()?;
        retained.finish()?;
        entries.finish()?;
        bits.finish(&mut traversed)?;
        traversed.finish()?;
        Ok(())
    }

    /// The roots and the strings they and the other tables name.
    fn write_strings(&self, out: &Output) -> io::Result<()> {
        let mut roots = out.section(Section::Roots);
        for &(dir, offset) in &self.roots {
            format::put_pair(&mut roots, dir, offset)?;
        }
        roots.finish()?;
        let mut strings = out.section(Section::Strings);
        strings.write_all(&self.strings)?;
        strings.finish()?;
        Ok(())
    }

    /// Every inode column in one pass over the rows, each row's stat found
    /// once. Returns each document's references.
    fn write_inodes(&self, batches: &[Batch], out: &Output) -> io::Result<Vec<u32>> {
        let mut stat_columns = StatColumns::start(out)?;
        let (mut doc, mut states, mut state_bits) = (
            out.column(Column::Doc)?,
            out.section(Section::States),
            Bits::new(2),
        );
        let mut references = vec![0u32; self.docs.len()];
        for (stat, doc_id, state) in self.inode_rows(batches) {
            if doc_id != NONE {
                let row = self.docs.partition_point(|&(_, id)| id < doc_id);
                references[row] += 1;
            }
            stat_columns.row(stat, &out.dicts)?;
            doc.nullable(known(doc_id))?;
            state_bits.push(&mut states, state as u8)?;
        }
        stat_columns.finish()?;
        doc.finish()?;
        state_bits.finish(&mut states)?;
        states.finish()?;
        Ok(references)
    }

    /// The link, work tree and special-file tables.
    fn write_tables(&self, out: &Output) -> io::Result<()> {
        let mut links = out.section(Section::Links);
        for &(inode, offset) in &self.links {
            format::put_pair(&mut links, inode, offset)?;
        }
        links.finish()?;
        let mut work_trees = out.section(Section::WorkTrees);
        for &(dir, offset, common_id, kind) in &self.work_trees {
            format::put_work_tree(&mut work_trees, dir, offset, common_id, kind)?;
        }
        work_trees.finish()?;
        let mut specials = out.section(Section::Specials);
        for &(inode, kind) in &self.specials {
            specials.write_all(&inode.to_le_bytes())?;
            specials.write_all(&kind.to_le_bytes())?;
        }
        specials.finish()?;
        Ok(())
    }

    /// The document ids, hashes and reference counts.
    fn write_docs(&self, out: &Output, references: Vec<u32>) -> io::Result<()> {
        let mut ids = out.column(Column::DocId)?;
        let mut hashes = out.hashes();
        for &((hi, lo), id) in &self.docs {
            ids.value(u64::from(id))?;
            hashes.write_all(&hi.to_be_bytes())?;
            hashes.write_all(&lo.to_be_bytes())?;
        }
        ids.finish()?;
        hashes.finish()?;
        let mut refs = out.section(Section::DocRefs);
        for count in references {
            format::put_u32(&mut refs, count)?;
        }
        refs.finish()?;
        Ok(())
    }
}
