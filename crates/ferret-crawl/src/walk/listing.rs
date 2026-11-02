//! Listing a directory into the worker's `getdents` buffer: its names in
//! one byte buffer with offsets, and whether the listing reached its end.

use rustix::fs::RawDir;

use super::*;

pub(super) struct Child {
    start: usize,
    end: usize,
    pub(super) kind: FileType,
}

pub(super) struct Children {
    names: Vec<u8>,
    pub(super) entries: Vec<Child>,
    /// The listing reached its end. False after an error partway, when
    /// `entries` holds only what was read before it.
    complete: bool,
}

impl Children {
    /// The raw entry count for [`Event::Entered`]: `None` if the listing is
    /// incomplete, and saturated to fit a `u32`.
    pub(super) fn count(&self) -> Option<u32> {
        self.complete
            .then(|| u32::try_from(self.entries.len()).unwrap_or(u32::MAX))
    }

    pub(super) fn name(&self, child: &Child) -> &OsStr {
        OsStr::from_bytes(&self.names[child.start..child.end])
    }

    pub(super) fn contains(&self, name: &str) -> bool {
        self.entries
            .iter()
            .any(|child| self.name(child).as_bytes() == name.as_bytes())
    }
}

impl<V: EventVisitor> Walker<'_, V> {
    /// Lists `dir` into the worker's `getdents` buffer. The listing stops at
    /// the first error. An error with no entry yet is one fault and `None`, so
    /// ignore files are not probed for a directory that could not be listed.
    /// An error after some entries is reported and the entries already read
    /// are kept.
    ///
    /// `ENOENT` is an error here: the kernel returns it for a directory
    /// unlinked since it was opened, and for a `/proc` directory whose process
    /// is gone. (`rustix::fs::Dir` reads it as the end of the listing, which
    /// would report a vanished directory as entered and empty.) Callers treat
    /// it as uncertain coverage like any other listing fault (D26 A′): it says
    /// the opened directory is gone, not that its path is.
    pub(super) fn list(
        &mut self,
        dir: BorrowedFd<'_>,
        context: FaultContext<'_, V::Dir>,
    ) -> Option<Children> {
        self.visit.observing(dir, bytes_path(&self.rel));
        #[cfg(test)]
        if let Some((op, error)) = inject(&self.root, IoPoint::Listing(0), bytes_path(&self.rel)) {
            self.fail(op, context, error);
            return None;
        }
        let mut children = Children {
            names: Vec::new(),
            entries: Vec::new(),
            complete: true,
        };
        let mut dents = std::mem::take(&mut self.dents);
        let mut raw = RawDir::new(dir, dents.spare_capacity_mut());
        while let Some(item) = raw.next() {
            match item {
                Ok(entry) => {
                    let bytes = entry.file_name().to_bytes();
                    if bytes == b"." || bytes == b".." {
                        continue;
                    }
                    let start = children.names.len();
                    children.names.extend_from_slice(bytes);
                    children.entries.push(Child {
                        start,
                        end: children.names.len(),
                        kind: entry.file_type(),
                    });
                    #[cfg(test)]
                    if let Some((op, error)) = inject(
                        &self.root,
                        IoPoint::Listing(children.entries.len()),
                        bytes_path(&self.rel),
                    ) {
                        self.fail(op, context, error);
                        children.complete = false;
                        break;
                    }
                }
                Err(error) => {
                    self.fail(IoOp::List, context, io::Error::from(error));
                    if error == Errno::ACCESS {
                        // A denied directory is a catalogued opaque row,
                        // even if the filesystem returned a partial batch.
                        children.entries.clear();
                        children.names.clear();
                    }
                    children.complete = false;
                    break;
                }
            }
        }
        self.dents = dents;
        if !children.complete && children.entries.is_empty() {
            None
        } else {
            Some(children)
        }
    }
}
