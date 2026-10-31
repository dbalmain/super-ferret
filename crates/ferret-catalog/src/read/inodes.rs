//! The catalog's inode columns: one inode's row, its fields one at a time,
//! and whole runs of sizes and mtimes.

use super::*;

impl Catalog {
    /// One inode's whole row. Needs every section in [`Section::INODE`].
    pub fn inode(&self, id: InoId) -> Inode {
        if let Some(Record::InodePut {
            stat, state, doc, ..
        }) = self.overlay_inode(id)
        {
            return Inode {
                stat: *stat,
                state: *state,
                doc: doc.map(DocId),
            };
        }
        if let Some(overlay) = &self.overlay {
            // The row lookup already proved that all of its fields are
            // inherited. Avoid repeating that lookup in every field getter.
            return overlay.base.inode(id);
        }
        let (owner, i) = (self.owner(id), id.0 as usize);
        let stat = Stat {
            dev: self.column(Column::Dev).lookup(i),
            ino: self.blocked(Column::Ino).get(i),
            size: self.size(id),
            mtime_sec: self.mtime(id),
            mtime_nsec: self.blocked(Column::MtimeNs).get(i) as u32,
            ctime_sec: self.ctime(id),
            ctime_nsec: self.blocked(Column::CtimeNs).get(i) as u32,
            mode: self.mode(id),
            uid: owner.0,
            gid: owner.1,
            nlink: self.nlink(id),
        };
        Inode {
            stat,
            state: self.state(id),
            doc: self.doc(id),
        }
    }

    /// An inode's `(st_dev, st_ino)`. Needs [`Section::Dev`] and
    /// [`Section::Ino`].
    pub fn identity(&self, id: InoId) -> (u64, u64) {
        if let Some(Record::InodePut { stat, .. }) = self.overlay_inode(id) {
            return (stat.dev, stat.ino);
        }
        let i = id.0 as usize;
        (
            self.column(Column::Dev).lookup(i),
            self.blocked(Column::Ino).get(i),
        )
    }

    /// An inode's `st_size`. Needs [`Section::Size`].
    pub fn size(&self, id: InoId) -> u64 {
        if let Some(Record::InodePut { stat, .. }) = self.overlay_inode(id) {
            return stat.size;
        }
        self.blocked(Column::Size).get(id.0 as usize)
    }

    /// An inode's mtime, in whole seconds. Needs [`Section::Mtime`].
    pub fn mtime(&self, id: InoId) -> i64 {
        if let Some(Record::InodePut { stat, .. }) = self.overlay_inode(id) {
            return stat.mtime_sec;
        }
        format::unorder(self.blocked(Column::Mtime).get(id.0 as usize))
    }

    /// The `st_size`s of inodes `64 * run` onwards, up to 64 of them, decoded
    /// together: one word of a bitset over inodes, for a pass that skips the
    /// words it has already cleared. Empty past the last inode. Needs
    /// [`Section::Size`].
    pub fn size_run<'o>(&self, run: usize, out: &'o mut [u64; RUN]) -> &'o [u64] {
        let out = &mut out[..self.run_len(run)];
        self.blocked(Column::Size)
            .decode(run.saturating_mul(RUN), out);
        if let Some(o) = &self.overlay {
            for r in o.projection(Family::Inodes).range(
                overlay::INODE,
                (run * RUN) as u32,
                ((run + 1) * RUN) as u32,
            ) {
                if let Record::InodePut { id, stat, .. } = r
                    && let Some(value) = out.get_mut((*id as usize).wrapping_sub(run * RUN))
                {
                    *value = stat.size;
                }
            }
        }
        out
    }

    /// The mtimes, in whole seconds, of inodes `64 * run` onwards: as
    /// [`Catalog::size_run`]. Needs [`Section::Mtime`].
    pub fn mtime_run<'o>(&self, run: usize, out: &'o mut [i64; RUN]) -> &'o [i64] {
        let mut raw = [0; RUN];
        let raw = &mut raw[..self.run_len(run)];
        self.blocked(Column::Mtime)
            .decode(run.saturating_mul(RUN), raw);
        for (time, &value) in out.iter_mut().zip(&*raw) {
            *time = format::unorder(value);
        }
        if let Some(o) = &self.overlay {
            for r in o.projection(Family::Inodes).range(
                overlay::INODE,
                (run * RUN) as u32,
                ((run + 1) * RUN) as u32,
            ) {
                if let Record::InodePut { id, stat, .. } = r
                    && let Some(value) =
                        out[..raw.len()].get_mut((*id as usize).wrapping_sub(run * RUN))
                {
                    *value = stat.mtime_sec;
                }
            }
        }
        &out[..raw.len()]
    }

    /// How many inodes run `run` of [`Catalog::size_run`] holds.
    fn run_len(&self, run: usize) -> usize {
        (self.base_inode_count() as usize)
            .saturating_sub(run.saturating_mul(RUN))
            .min(RUN)
    }

    /// An inode's ctime, in whole seconds. Needs [`Section::Ctime`].
    pub fn ctime(&self, id: InoId) -> i64 {
        if let Some(Record::InodePut { stat, .. }) = self.overlay_inode(id) {
            return stat.ctime_sec;
        }
        format::unorder(self.blocked(Column::Ctime).get(id.0 as usize))
    }

    /// Subsecond modification time. Needs MtimeNs.
    pub fn mtime_nsec(&self, id: InoId) -> i64 {
        if let Some(Record::InodePut { stat, .. }) = self.overlay_inode(id) {
            return i64::from(stat.mtime_nsec);
        }
        self.blocked(Column::MtimeNs).get(id.0 as usize) as i64
    }

    /// Subsecond change time. Needs CtimeNs.
    pub fn ctime_nsec(&self, id: InoId) -> i64 {
        if let Some(Record::InodePut { stat, .. }) = self.overlay_inode(id) {
            return i64::from(stat.ctime_nsec);
        }
        self.blocked(Column::CtimeNs).get(id.0 as usize) as i64
    }

    /// An inode's `st_mode`: type and permission bits. Needs
    /// [`Section::Mode`].
    pub fn mode(&self, id: InoId) -> u32 {
        if let Some(Record::InodePut { stat, .. }) = self.overlay_inode(id) {
            return stat.mode;
        }
        self.column(Column::Mode).lookup(id.0 as usize) as u32
    }

    /// An inode's `(st_uid, st_gid)`. Needs [`Section::Owner`].
    pub fn owner(&self, id: InoId) -> (u32, u32) {
        if let Some(Record::InodePut { stat, .. }) = self.overlay_inode(id) {
            return (stat.uid, stat.gid);
        }
        let pair = self.column(Column::Owner).lookup(id.0 as usize);
        ((pair >> 32) as u32, pair as u32)
    }

    /// An inode's `st_nlink`: for a directory, as the filesystem counts it,
    /// ignored children included (D47). Needs [`Section::Nlink`].
    pub fn nlink(&self, id: InoId) -> u64 {
        if let Some(Record::InodePut { stat, .. }) = self.overlay_inode(id) {
            return stat.nlink;
        }
        self.blocked(Column::Nlink).get(id.0 as usize)
    }

    /// An inode's document, when its state is `Hashed`. Needs
    /// [`Section::Doc`].
    pub fn doc(&self, id: InoId) -> Option<DocId> {
        if let Some(Record::InodePut { doc, .. }) = self.overlay_inode(id) {
            return doc.map(DocId);
        }
        self.blocked(Column::Doc)
            .nullable(id.0 as usize)
            .map(|doc| DocId(doc as u32))
    }

    /// What the last observation learnt about an inode's content (D37).
    /// Needs [`Section::States`].
    pub fn state(&self, id: InoId) -> ContentState {
        if let Some(Record::InodePut { state, .. }) = self.overlay_inode(id) {
            return *state;
        }
        let byte = self.section(Section::States)[id.0 as usize / 4];
        ContentState::from_bits(byte >> (id.0 % 4 * 2))
    }

    /// The entries the walk's `getdents` returned for directory `dir`, minus
    /// `.` and `..`, counted before ignore rules dropped any (D47). `None`
    /// when the walk did not list it: unreadable, listing failed partway, or
    /// carried from a generation that did not know. Needs
    /// [`Section::Entries`].
    pub fn entry_count(&self, dir: InoId) -> Option<u32> {
        if let Some(Record::DirPut { entries, flags, .. }) = self.ns_record(overlay::DIR, dir.0) {
            return if flags & 8 != 0 { None } else { *entries };
        }
        self.blocked(Column::Entries)
            .nullable(dir.0 as usize)
            .map(|count| count as u32)
    }

    /// The inode's file type. Needs [`Section::Links`] for a non-directory;
    /// it also loads the sparse Specials table. No stat column is read.
    pub fn kind(&self, id: InoId) -> Kind {
        if let Some(kind) = self.overlay.as_ref().and_then(|o| o.kind(id.0)) {
            return kind;
        }
        if let Some(o) = &self.overlay {
            return o.base.kind(id);
        }
        if id.0 < self.base_dir_count() {
            Kind::Dir
        } else if self.link_target(id).is_some() {
            Kind::Symlink
        } else {
            special_kind(self.section(Section::Specials), id).unwrap_or(Kind::File)
        }
    }

    /// [`Catalog::kind`] for inodes asked for mostly in rising order, as a
    /// pass over names or inodes asks: each search starts where the last
    /// ended. Needs [`Section::Links`].
    pub fn kinds(&self) -> Kinds<'_> {
        Kinds {
            dirs: self.base_dir_count(),
            overlay: self.overlay.as_deref(),
            links: self.section(Section::Links),
            specials: self.section(Section::Specials),
            at: 0,
        }
    }

    /// A symlink's target, as `readlink` returned it. Needs
    /// [`Section::Links`], which loads [`Section::Strings`].
    pub fn link_target(&self, id: InoId) -> Option<&[u8]> {
        match self.family_record(Family::Aux, overlay::LINK, id.0) {
            Some(Record::LinkPut { target, .. }) => return Some(target),
            Some(Record::LinkDelete { .. }) => return None,
            _ => {}
        }
        let links = self.section(Section::Links);
        let n = links.len() / PAIR_ROW;
        let at = partition_point(n, |i| u32_at(links, i * PAIR_ROW) < id.0);
        (at < n && u32_at(links, at * PAIR_ROW) == id.0)
            .then(|| self.string(u32_at(links, at * PAIR_ROW + 4)))
    }
}
