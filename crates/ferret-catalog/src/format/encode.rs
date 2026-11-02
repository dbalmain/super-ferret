//! Encoding: the builder streams sections through these.

use std::io::{self, Write};

use super::*;

/// Everything the head of the file records, all known before any section is
/// written.
pub(crate) struct Head {
    pub(crate) generation: Generation,
    pub(crate) sniffer: u32,
    pub(crate) next_doc: u32,
    pub(crate) dirs: u32,
    pub(crate) inodes: u32,
    pub(crate) names: u32,
    pub(crate) docs: u32,
    /// Column descriptors, in [`COLUMNS`] order.
    pub(crate) columns: [Descriptor; COLUMNS.len()],
    /// Every section's length in [`SECTIONS`] order, less its columns: a
    /// column section's length adds its columns' to this (only the docs
    /// section has bytes of its own, its hashes, after its column).
    pub(crate) lens: [u64; SECTIONS.len()],
}

impl Head {
    pub(crate) fn count(&self, rows: Rows) -> u32 {
        match rows {
            Rows::Names => self.names,
            Rows::Dirs => self.dirs,
            Rows::Inodes => self.inodes,
            Rows::Docs => self.docs,
        }
    }

    fn section_lens(&self) -> [u64; SECTIONS.len()] {
        let mut lens = self.lens;
        for (column, desc) in COLUMNS.into_iter().zip(self.columns) {
            lens[column.section() as usize] += desc.len(column.coding(), self.count(column.rows()));
        }
        lens
    }

    /// The head of the file: header, section table and column descriptors,
    /// and the whole file's length.
    pub(crate) fn encode(&self) -> ([u8; TABLE_END], u64) {
        let mut out = Vec::with_capacity(TABLE_END);
        out.extend_from_slice(&MAGIC);
        for v in [
            VERSION,
            self.sniffer,
            self.next_doc,
            SECTIONS.len() as u32,
            self.dirs,
            self.inodes,
            self.names,
            self.docs,
        ] {
            out.extend_from_slice(&v.to_le_bytes());
        }
        out.extend_from_slice(&self.generation.incarnation);
        out.extend_from_slice(&self.generation.checkpoint.to_le_bytes());
        out.extend_from_slice(&self.generation.sequence.to_le_bytes());
        out.extend_from_slice(&self.inodes.to_le_bytes());
        out.extend_from_slice(&self.names.to_le_bytes());
        out.extend_from_slice(&[0; 16]);
        let mut offset = TABLE_END as u64;
        for len in self.section_lens() {
            out.extend_from_slice(&offset.to_le_bytes());
            out.extend_from_slice(&len.to_le_bytes());
            out.extend_from_slice(&[0; 16]);
            offset += len;
        }
        for desc in self.columns {
            out.extend_from_slice(&desc.base.to_le_bytes());
            out.extend_from_slice(&desc.width.to_le_bytes());
            out.extend_from_slice(&desc.dict_len.to_le_bytes());
        }
        let digest = checksum(&out);
        out.extend_from_slice(&digest);
        let mut head = [0; TABLE_END];
        head.copy_from_slice(&out);
        (head, offset)
    }
}

/// Buffered writes at a fixed place in a file, moving on as they go: each
/// section, and each column's values, writes through its own, so the builder
/// can fill several at once (D40: bounded buffers, never a whole column).
pub(crate) struct At<'f> {
    file: &'f File,
    pos: u64,
    buf: Vec<u8>,
}

/// What an [`At`] holds before writing it out.
const AT_BUFFER: usize = 64 << 10;

impl<'f> At<'f> {
    pub(crate) fn new(file: &'f File, pos: u64) -> Self {
        Self {
            file,
            pos,
            buf: Vec::with_capacity(AT_BUFFER),
        }
    }

    /// Writes out what is buffered; returns where the next byte would go.
    pub(crate) fn finish(mut self) -> io::Result<u64> {
        self.flush()?;
        Ok(self.pos)
    }
}

impl Write for At<'_> {
    /// Holds at most [`AT_BUFFER`] bytes: a write that would pass it fills
    /// the buffer and writes it out, and a remainder as large as the buffer
    /// goes straight to the file, so a whole heap written at once is never
    /// copied.
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let room = AT_BUFFER - self.buf.len();
        if bytes.len() < room {
            self.buf.extend_from_slice(bytes);
            return Ok(bytes.len());
        }
        let (fill, rest) = bytes.split_at(room);
        self.buf.extend_from_slice(fill);
        self.flush()?;
        if rest.len() >= AT_BUFFER {
            crate::bulk::write_at(self.file, rest, self.pos)?;
            self.pos += rest.len() as u64;
        } else {
            self.buf.extend_from_slice(rest);
        }
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        crate::bulk::write_at(self.file, &self.buf, self.pos)?;
        self.pos += self.buf.len() as u64;
        self.buf.clear();
        Ok(())
    }
}

/// Streams one column to its place in the file: its dictionary, then its
/// values as they are pushed, each through a bounded buffer.
pub(crate) struct ColumnWriter<'f> {
    desc: Descriptor,
    coding: Coding,
    /// Rows pushed so far, for a sequence column.
    rows: u64,
    out: At<'f>,
    values: Values<'f>,
    end: u64,
}

enum Values<'f> {
    Packed(packed::Writer),
    /// The writer and the table's output; the blocks go to `out`.
    Blocked(Box<packed::BlockedWriter>, At<'f>),
}

impl<'f> ColumnWriter<'f> {
    /// Starts `column` at its place in `file`, as `layout` (decoded from the
    /// head just written) places it, writing its dictionary, which must have
    /// the descriptor's length.
    pub(crate) fn start(
        file: &'f File,
        layout: &Layout,
        column: Column,
        dict: &[u64],
    ) -> io::Result<Self> {
        let Placed { desc, start } = layout.columns[column as usize];
        let (coding, count) = (column.coding(), layout.count(column.rows()));
        debug_assert_eq!(dict.len(), desc.dict_len as usize);
        let at = (layout.range(column.section()).0 + start) as u64;
        let end = at + desc.len(coding, count as u32);
        // A dictionary, or a blocked column's table, comes first.
        let mut first = At::new(file, at);
        for &value in dict {
            first.write_all(&value.to_le_bytes())?;
        }
        let (out, values) = match coding {
            Coding::Blocked | Coding::NullableBlocked => {
                let table = packed::blocks(count as u32) * packed::BLOCK_ENTRY;
                let writer = Box::new(packed::BlockedWriter::new());
                (At::new(file, at + table), Values::Blocked(writer, first))
            }
            _ => (first, Values::Packed(packed::Writer::new(desc.width))),
        };
        Ok(Self {
            desc,
            coding,
            rows: 0,
            out,
            values,
            end,
        })
    }

    fn push(&mut self, raw: u64) -> io::Result<()> {
        self.rows += 1;
        match &mut self.values {
            Values::Packed(writer) => writer.push(&mut self.out, raw),
            Values::Blocked(writer, table) => writer.push(table, &mut self.out, raw),
        }
    }

    /// A frame-of-reference or blocked value, inside the range the
    /// descriptor was made from.
    pub(crate) fn value(&mut self, value: u64) -> io::Result<()> {
        match self.coding {
            Coding::Blocked | Coding::NullableBlocked => self.push(value),
            Coding::Sequence => self.push(value - self.rows - self.desc.base),
            _ => self.push(value - self.desc.base),
        }
    }

    /// A nullable column's value, or none.
    pub(crate) fn nullable(&mut self, value: Option<u64>) -> io::Result<()> {
        match (value, &mut self.values) {
            (Some(value), _) => self.value(value),
            (None, Values::Blocked(writer, table)) => {
                self.rows += 1;
                writer.push_null(table, &mut self.out)
            }
            (None, Values::Packed(_)) => self.push(packed::mask(self.desc.width)),
        }
    }

    /// A dictionary column's index.
    pub(crate) fn index(&mut self, index: usize) -> io::Result<()> {
        self.push(index as u64)
    }

    /// Writes the padding, and any last block, and flushes.
    pub(crate) fn finish(mut self) -> io::Result<()> {
        match self.values {
            Values::Packed(writer) => writer.finish(&mut self.out)?,
            Values::Blocked(writer, mut table) => {
                writer.finish(&mut table, &mut self.out)?;
                table.finish()?;
            }
        }
        let end = self.out.finish()?;
        debug_assert_eq!(end, self.end, "column ends where the head placed its end");
        Ok(())
    }
}

/// A generation file being filled: each column and section starts where the
/// head already written to it places them.
pub(crate) struct Output<'f> {
    pub(crate) file: &'f File,
    pub(crate) layout: Layout,
    pub(crate) dicts: [Vec<u64>; COLUMNS.len()],
}

impl<'f> Output<'f> {
    /// Writes `head` at the start of `file`, then places the rest as a reader
    /// will look for it: by the table just written.
    pub(crate) fn start(
        file: &'f File,
        head: &Head,
        dicts: [Vec<u64>; COLUMNS.len()],
    ) -> io::Result<Self> {
        let (bytes, len) = head.encode();
        crate::bulk::write_at(file, &bytes, 0)?;
        let layout = decode_table(&bytes, len).map_err(io::Error::other)?;
        Ok(Self {
            file,
            layout,
            dicts,
        })
    }

    pub(crate) fn column(&self, column: Column) -> io::Result<ColumnWriter<'f>> {
        ColumnWriter::start(
            self.file,
            &self.layout,
            column,
            &self.dicts[column as usize],
        )
    }

    pub(crate) fn section(&self, section: Section) -> At<'f> {
        At::new(self.file, self.layout.range(section).0 as u64)
    }

    /// The document hashes, after the docs section's id column.
    pub(crate) fn hashes(&self) -> At<'f> {
        At::new(
            self.file,
            (self.layout.range(Section::Docs).0 + self.layout.hashes) as u64,
        )
    }
}

pub(crate) fn put_u32(out: &mut impl Write, v: u32) -> io::Result<()> {
    out.write_all(&v.to_le_bytes())
}

pub(crate) fn put_pair(out: &mut impl Write, a: u32, b: u32) -> io::Result<()> {
    put_u32(out, a)?;
    put_u32(out, b)
}

/// One work-tree row.
pub(crate) fn put_work_tree(
    out: &mut impl Write,
    dir: u32,
    offset: u32,
    common_id: (u64, u64),
    kind: u8,
) -> io::Result<()> {
    put_pair(out, dir, offset)?;
    out.write_all(&common_id.0.to_le_bytes())?;
    out.write_all(&common_id.1.to_le_bytes())?;
    out.write_all(&[kind, 0, 0, 0, 0, 0, 0, 0])
}

/// Packs `width`-bit values (1 or 2) LSB first as they are pushed.
pub(crate) struct Bits {
    width: usize,
    byte: u8,
    count: usize,
}

impl Bits {
    pub(crate) fn new(width: usize) -> Self {
        Self {
            width,
            byte: 0,
            count: 0,
        }
    }

    pub(crate) fn push(&mut self, out: &mut impl Write, value: u8) -> io::Result<()> {
        let per_byte = 8 / self.width;
        self.byte |= value << ((self.count % per_byte) * self.width);
        self.count += 1;
        if self.count.is_multiple_of(per_byte) {
            out.write_all(&[self.byte])?;
            self.byte = 0;
        }
        Ok(())
    }

    /// Writes a final partial byte.
    pub(crate) fn finish(self, out: &mut impl Write) -> io::Result<()> {
        if !self.count.is_multiple_of(8 / self.width) {
            out.write_all(&[self.byte])?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {

    use std::io::Write;

    use super::{AT_BUFFER, At};

    #[test]
    fn a_positional_writer_never_holds_more_than_its_buffer() {
        let path = std::env::temp_dir().join(format!("ferret-at-{}", std::process::id()));
        let file = std::fs::File::options()
            .create(true)
            .truncate(true)
            .read(true)
            .write(true)
            .open(&path)
            .unwrap();
        // After a 3-byte gap: a small write; one that exactly fills the
        // buffer; one that fills it and leaves a remainder larger than the
        // buffer, written directly; two halves, the second overflowing into a
        // small buffered remainder; a tail. The bytes land in order.
        let pattern = |n: usize, seed: u8| -> Vec<u8> {
            (0..n).map(|i| (i as u8).wrapping_mul(31) ^ seed).collect()
        };
        let parts = [
            pattern(10, 1),
            pattern(AT_BUFFER - 10, 2),
            pattern(3 * AT_BUFFER + 7, 3),
            pattern(AT_BUFFER / 2, 4),
            pattern(AT_BUFFER / 2 + 5, 5),
            pattern(5, 6),
        ];
        let mut at = At::new(&file, 3);
        for part in &parts {
            at.write_all(part).unwrap();
            assert!(at.buf.len() < AT_BUFFER, "{} buffered", at.buf.len());
            assert_eq!(at.buf.capacity(), AT_BUFFER, "the buffer grew");
        }
        let end = at.finish().unwrap();
        let expected: Vec<u8> = [&[0u8; 3][..]]
            .into_iter()
            .chain(parts.iter().map(Vec::as_slice))
            .flatten()
            .copied()
            .collect();
        assert_eq!(end, expected.len() as u64);
        assert_eq!(std::fs::read(&path).unwrap(), expected);
        let _ = std::fs::remove_file(&path);
    }
}
