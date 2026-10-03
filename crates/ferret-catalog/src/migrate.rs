//! Explicit v3 import. Copies packed sections without renumbering any id;
//! the v4 decoder validates the converted layout and every original section.
//! Legacy bytes have no checksums, so import can detect structural corruption
//! but cannot recover checksums that v3 never recorded.

use std::fs::File;
use std::io;
use std::os::unix::fs::FileExt;

use crate::format::{self, COLUMNS, Column, Descriptor, Head, SECTIONS, Section, u32_at, u64_at};
use crate::packed;
use crate::{DecodeError, Generation, Hash};

pub(crate) fn write(
    bytes: &[u8],
    out: &File,
    generation: Generation,
    policy: Hash,
) -> Result<(), crate::CommitError> {
    let fail = || crate::CommitError::Encode(DecodeError::Layout);
    if bytes.len() < 12 || bytes[..8] != format::MAGIC {
        return Err(crate::CommitError::Encode(DecodeError::NotACatalog));
    }
    if u32_at(bytes, 8) != 3 {
        return Err(crate::CommitError::Encode(DecodeError::Version(u32_at(
            bytes, 8,
        ))));
    }
    const OLD_END: usize = 680;
    if bytes.len() < OLD_END || u32_at(bytes, 20) != 23 {
        return Err(fail());
    }
    let mut columns = [Descriptor::default(); COLUMNS.len()];
    for (i, desc) in columns[..17].iter_mut().enumerate() {
        let at = 40 + 23 * 16 + i * 16;
        *desc = Descriptor {
            base: u64_at(bytes, at),
            width: u32_at(bytes, at + 8),
            dict_len: u32_at(bytes, at + 12),
        };
        if desc.width > packed::MAX_WIDTH {
            return Err(fail());
        }
    }
    let dirs = u32_at(bytes, 24);
    if dirs > u32_at(bytes, 28)
        || u32_at(bytes, 28) > format::NONE - 16
        || u32_at(bytes, 32) == format::NONE
        || u32_at(bytes, 36) > u32_at(bytes, 16)
    {
        return Err(fail());
    }
    // All-none blocks have zero value bytes and width, regardless of row count.
    columns[Column::RetainedAt as usize] = Descriptor::blocked((0, 0));
    let mut head = Head {
        generation,
        sniffer: u32_at(bytes, 12),
        next_doc: u32_at(bytes, 16),
        dirs,
        inodes: u32_at(bytes, 28),
        names: u32_at(bytes, 32),
        docs: u32_at(bytes, 36),
        columns,
        lens: [0; SECTIONS.len()],
    };
    let mut old = [(0, 0); 23];
    let mut end = OLD_END;
    for (i, slot) in old.iter_mut().enumerate() {
        let start = usize::try_from(u64_at(bytes, 40 + i * 16)).map_err(|_| fail())?;
        let len = usize::try_from(u64_at(bytes, 48 + i * 16)).map_err(|_| fail())?;
        let next = start.checked_add(len).ok_or_else(fail)?;
        if start != end || next > bytes.len() {
            return Err(fail());
        }
        *slot = (start, next);
        head.lens[i] = len as u64;
        end = next;
    }
    if end != bytes.len() {
        return Err(fail());
    }
    for column in &COLUMNS[..17] {
        let len = columns[*column as usize].len(column.coding(), head.count(column.rows()));
        let section = column.section() as usize;
        head.lens[section] = head.lens[section].checked_sub(len).ok_or_else(fail)?;
    }
    head.lens[Section::DocRefs as usize] = u64::from(head.docs) * 4;
    head.lens[Section::Policy as usize] = 16;
    let (encoded, len) = head.encode();
    let layout = format::decode_table(&encoded, len).map_err(crate::CommitError::Encode)?;
    let write = || -> io::Result<()> {
        out.set_len(len)?;
        out.write_all_at(&encoded, 0)?;
        for (i, &(start, end)) in old.iter().enumerate() {
            out.write_all_at(&bytes[start..end], layout.sections[i].0 as u64)?;
        }
        let mut retained = format::ColumnWriter::start(out, &layout, Column::RetainedAt, &[])?;
        for _ in 0..dirs {
            retained.nullable(None)?;
        }
        retained.finish()?;
        out.write_all_at(&policy, layout.range(Section::Policy).0 as u64)?;
        Ok(())
    };
    write().map_err(crate::CommitError::Write)?;
    // Validate legacy columns before interpreting any document id.
    format::seal(out).map_err(crate::CommitError::Write)?;
    let mut converted = vec![0; len as usize];
    out.read_exact_at(&mut converted, 0)
        .map_err(crate::CommitError::Write)?;
    let checked_layout =
        format::decode_table(&converted, len).map_err(crate::CommitError::Encode)?;
    let facts = format::Facts::default();
    for section in SECTIONS.into_iter().filter(|&s| s != Section::DocRefs) {
        format::check(section, &checked_layout, &facts, |s| {
            layout.section(&converted, s)
        })
        .map_err(crate::CommitError::Encode)?;
    }
    let refs = format::document_references(
        &layout,
        layout.section(&converted, Section::Docs),
        layout.section(&converted, Section::Doc),
    )
    .map_err(crate::CommitError::Encode)?;
    let mut references = format::At::new(out, layout.range(Section::DocRefs).0 as u64);
    for count in refs {
        format::put_u32(&mut references, count).map_err(crate::CommitError::Write)?;
    }
    references.finish().map_err(crate::CommitError::Write)?;
    format::seal(out).map_err(crate::CommitError::Write)
}
