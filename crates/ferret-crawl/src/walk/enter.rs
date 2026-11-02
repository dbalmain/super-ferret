//! Opening directories: a spilled continuation's reopen, the boundary
//! check before a directory is entered, and entering or traversing it with
//! its listing. Every open below the root is `O_NOFOLLOW` and checked
//! against the inode the walk already reported.

use super::*;

impl<V: EventVisitor> Walker<'_, V> {
    // ── reopening ──

    /// Reopen a spilled continuation one component at a time. Only the
    /// configured root is opened by path; every component below it uses
    /// `O_NOFOLLOW`. The root and final inode must match the first pass.
    pub(super) fn reopen(
        &mut self,
        rel: &Path,
        expected: (u64, u64),
        token: V::Dir,
    ) -> Option<OwnedFd> {
        let context = FaultContext::Dir(token);
        let root = match open_path(&self.root, root_dir_flags(), Mode::empty()) {
            Ok(fd) => fd,
            Err(error) => {
                self.fail(IoOp::Reopen, context, io::Error::from(error));
                return None;
            }
        };
        let Some(root_id) = self.root_id else {
            self.fail(
                IoOp::Reopen,
                context,
                io::Error::new(io::ErrorKind::InvalidData, "missing root identity"),
            );
            return None;
        };
        let mut fd = root;
        if !self.same_dir(&fd, root_id, token) {
            return None;
        }
        for name in rel.iter() {
            fd = match openat(&fd, name, child_dir_flags(), Mode::empty()) {
                Ok(next) => next,
                Err(error) => {
                    self.fail(IoOp::Reopen, context, io::Error::from(error));
                    return None;
                }
            };
        }
        self.same_dir(&fd, expected, token).then_some(fd)
    }

    fn same_dir(&mut self, fd: &OwnedFd, expected: (u64, u64), token: V::Dir) -> bool {
        let context = FaultContext::Dir(token);
        match fstat(fd) {
            Ok(stat) if (stat.st_dev, stat.st_ino) == expected => true,
            Ok(_) => {
                self.fail(
                    IoOp::Reopen,
                    context,
                    io::Error::new(
                        io::ErrorKind::InvalidData,
                        "directory changed before resume",
                    ),
                );
                false
            }
            Err(error) => {
                self.fail(IoOp::Reopen, context, io::Error::from(error));
                false
            }
        }
    }

    // ── boundaries ──

    /// Whether the current path is some boundary's, before its identity is
    /// known. Free when there are no boundaries.
    pub(super) fn may_be_boundary(&self) -> bool {
        self.boundaries.ids(&self.rel).is_some()
    }

    /// Reports [`Event::Boundary`] when the current path is a boundary and
    /// `stat` is a directory with that boundary's identity, if it gives one.
    pub(super) fn at_boundary(
        &mut self,
        here: &Here<'_, V::Dir>,
        name: &OsStr,
        stat: &rustix::fs::Stat,
    ) -> bool {
        if self.boundaries.is_empty() || file_type(stat) != FileType::Directory {
            return false;
        }
        let id = (stat.st_dev, stat.st_ino);
        let hit = self
            .boundaries
            .ids(&self.rel)
            .is_some_and(|ids| ids.iter().any(|want| want.is_none_or(|want| want == id)));
        if hit {
            let path = bytes_path(&self.rel);
            self.visit.visit(Event::Boundary {
                parent: here.token,
                name,
                path,
            });
        }
        hit
    }

    // ── entering ──

    /// `token` is what the visitor returned for this directory's `Decided`;
    /// `None` prunes it.
    pub(super) fn follow(
        &mut self,
        here: &Here<'_, V::Dir>,
        name: &OsStr,
        decision: Decision,
        expected: &rustix::fs::Stat,
        token: Option<V::Dir>,
    ) -> Option<Job<V::Dir>> {
        match decision {
            Decision::Descend => self.enter_and_list(here, name, expected, token?),
            Decision::Traverse => self.traverse_and_list(here, name, expected, token?),
            Decision::Skip | Decision::Catalog(_) | Decision::Index => None,
        }
    }

    fn enter_and_list(
        &mut self,
        here: &Here<'_, V::Dir>,
        name: &OsStr,
        expected: &rustix::fs::Stat,
        token: V::Dir,
    ) -> Option<Job<V::Dir>> {
        let child = self.open_child(here, name, expected)?;
        #[cfg(test)]
        after_open(&self.rel);
        let children = self.list(child.as_fd(), FaultContext::Dir(token))?;
        let loaded = self.load_ignores(
            child.as_fd(),
            |name| children.contains(name),
            here.rules.in_work_tree(),
            token,
        );
        let (rules, errors) = here.rules.enter(name, loaded.files());
        self.patterns(errors);
        self.entered(token, loaded.work_tree.as_ref(), children.count());
        Some(Job::new(
            child,
            rules,
            children,
            self.rel.clone(),
            (expected.st_dev, expected.st_ino),
            token,
        ))
    }

    fn traverse_and_list(
        &mut self,
        here: &Here<'_, V::Dir>,
        name: &OsStr,
        expected: &rustix::fs::Stat,
        token: V::Dir,
    ) -> Option<Job<V::Dir>> {
        let child = self.open_child(here, name, expected)?;
        #[cfg(test)]
        after_open(&self.rel);
        let children = self.list(child.as_fd(), FaultContext::Dir(token))?;
        let rules = here.rules.traverse(name);
        self.entered(token, None, children.count());
        Some(Job::new(
            child,
            rules,
            children,
            self.rel.clone(),
            (expected.st_dev, expected.st_ino),
            token,
        ))
    }

    /// `O_DIRECTORY | O_NOFOLLOW`, then `fstat`. A symlink is `ELOOP`. A
    /// different inode than `expected` is a fault either way: the `Decided`
    /// event already described the inode `decide` saw.
    fn open_child(
        &mut self,
        here: &Here<'_, V::Dir>,
        name: &OsStr,
        expected: &rustix::fs::Stat,
    ) -> Option<OwnedFd> {
        #[cfg(test)]
        if let Some((op, error)) = inject(&self.root, IoPoint::OpenDirectory, bytes_path(&self.rel))
        {
            self.fail_child(op, here.token, name, error);
            return None;
        }
        let fd = match openat(here.fd, name, child_dir_flags(), Mode::empty()) {
            Ok(fd) => fd,
            Err(error) => {
                self.fail_child(IoOp::OpenDir, here.token, name, io::Error::from(error));
                return None;
            }
        };
        match fstat(&fd) {
            Ok(stat) if stat.st_dev == expected.st_dev && stat.st_ino == expected.st_ino => {}
            Ok(_) => {
                self.fail_child(
                    IoOp::OpenDir,
                    here.token,
                    name,
                    io::Error::new(
                        io::ErrorKind::InvalidData,
                        "directory changed between stat and open",
                    ),
                );
                return None;
            }
            Err(error) => {
                self.fail_child(IoOp::OpenDir, here.token, name, io::Error::from(error));
                return None;
            }
        }
        Some(fd)
    }
}
