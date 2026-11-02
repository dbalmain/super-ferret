//! Deciding each listed child: skipped by name or policy, stat'd and
//! reported, a link observed, or a directory queued as a job.

use std::ffi::OsString;
use std::os::unix::ffi::OsStringExt;

use ferret_policy::Entry;
use rustix::fs::{openat, readlinkat};

use super::*;

impl<V: EventVisitor> Walker<'_, V> {
    pub(super) fn consider(
        &mut self,
        here: &Here<'_, V::Dir>,
        name: &OsStr,
        kind: FileType,
    ) -> Option<Job<V::Dir>> {
        #[cfg(test)]
        if let Some((op, error)) = inject(&self.root, IoPoint::Child, bytes_path(&self.rel)) {
            self.fail(
                op,
                FaultContext::Child {
                    parent: here.token,
                    name,
                },
                error,
            );
            return None;
        }
        match kind {
            FileType::RegularFile | FileType::Unknown => {
                let stat = self.stat_child(here, name)?;
                self.decide_statted(here, name, stat)
            }
            FileType::Symlink => {
                let decision = here.rules.decide(bytes_path(&self.rel), Entry::Symlink);
                if decision == Decision::Skip {
                    self.emit_skip(here, name, kind);
                    return None;
                }
                self.finish_link(here, name, decision)
            }
            FileType::Directory => self.consider_dir(here, name),
            _ => {
                if here.rules.decide(bytes_path(&self.rel), Entry::Other) == Decision::Skip {
                    self.emit_skip(here, name, kind);
                    return None;
                }
                let stat = self.stat_child(here, name)?;
                self.decide_statted(here, name, stat)
            }
        }
    }

    fn consider_dir(&mut self, here: &Here<'_, V::Dir>, name: &OsStr) -> Option<Job<V::Dir>> {
        let mut decision = here.rules.decide(bytes_path(&self.rel), Entry::Dir);
        // A boundary is checked whatever the policy says here: the inner root
        // owns it (D34). Only a name that could be one is statted when
        // skipped.
        if decision == Decision::Skip && !self.may_be_boundary() {
            self.emit_skip(here, name, FileType::Directory);
            return None;
        }
        let stat = self.stat_child(here, name)?;
        if self.at_boundary(here, name, &stat) {
            return None;
        }
        if decision == Decision::Skip {
            self.emit_skip(here, name, file_type(&stat));
            return None;
        }
        let seen = entry_from_stat(&stat);
        if seen != Entry::Dir {
            decision = here.rules.decide(bytes_path(&self.rel), seen);
            if decision == Decision::Skip {
                self.emit_skip(here, name, file_type(&stat));
                return None;
            }
            if seen == Entry::Symlink {
                return self.finish_link(here, name, decision);
            }
        }
        let token = self.emit_stat(here, name, decision, &stat, None);
        self.follow(here, name, decision, &stat, token)
    }

    /// `stat` came from `statat` (`d_type` was a file, unknown, or disagreed).
    /// A symlink result is re-opened so the published stat and the target are
    /// one observation.
    fn decide_statted(
        &mut self,
        here: &Here<'_, V::Dir>,
        name: &OsStr,
        stat: rustix::fs::Stat,
    ) -> Option<Job<V::Dir>> {
        if self.at_boundary(here, name, &stat) {
            return None;
        }
        let entry = entry_from_stat(&stat);
        let decision = here.rules.decide(bytes_path(&self.rel), entry);
        if decision == Decision::Skip {
            self.emit_skip(here, name, file_type(&stat));
            return None;
        }
        if entry == Entry::Symlink {
            return self.finish_link(here, name, decision);
        }
        let token = self.emit_stat(here, name, decision, &stat, None);
        self.follow(here, name, decision, &stat, token)
    }

    /// Opens `name` with `O_PATH | O_NOFOLLOW`. The stat and, when it is a
    /// symlink, the target come from that descriptor.
    fn finish_link(
        &mut self,
        here: &Here<'_, V::Dir>,
        name: &OsStr,
        decision: Decision,
    ) -> Option<Job<V::Dir>> {
        let (stat, target) = match observe_link(here.fd, name, &self.root, bytes_path(&self.rel)) {
            Ok(pair) => pair,
            Err((op, error)) => {
                self.fail_child(op, here.token, name, error);
                return None;
            }
        };
        let seen = entry_from_stat(&stat);
        if seen != Entry::Symlink {
            if self.at_boundary(here, name, &stat) {
                return None;
            }
            let decision = here.rules.decide(bytes_path(&self.rel), seen);
            if decision == Decision::Skip {
                self.emit_skip(here, name, file_type(&stat));
                return None;
            }
            let token = self.emit_stat(here, name, decision, &stat, None);
            return self.follow(here, name, decision, &stat, token);
        }
        let Some(target) = target else {
            self.fail_child(
                IoOp::Readlink,
                here.token,
                name,
                io::Error::new(io::ErrorKind::InvalidData, "symlink has no target"),
            );
            return None;
        };
        self.emit_stat(here, name, decision, &stat, Some(target.as_os_str()));
        None
    }

    fn stat_child(&mut self, here: &Here<'_, V::Dir>, name: &OsStr) -> Option<rustix::fs::Stat> {
        match statat(here.fd, name, AtFlags::SYMLINK_NOFOLLOW) {
            Ok(stat) => Some(stat),
            Err(error) => {
                self.fail_child(IoOp::Lstat, here.token, name, io::Error::from(error));
                None
            }
        }
    }
}

/// `O_PATH | O_NOFOLLOW`, then `fstat` and, for a symlink, `readlinkat` of
/// that descriptor. `Ok`'s stat is the inode the descriptor refers to, which
/// may no longer be a symlink; then the target is `None`. An error says which
/// step failed.
fn observe_link(
    dir: BorrowedFd<'_>,
    name: &OsStr,
    _root: &Path,
    _relative: &Path,
) -> Result<(rustix::fs::Stat, Option<OsString>), (IoOp, io::Error)> {
    let lstat = |error: Errno| (IoOp::Lstat, io::Error::from(error));
    let fd = openat(dir, name, link_flags(), Mode::empty()).map_err(lstat)?;
    let stat = fstat(&fd).map_err(lstat)?;
    if file_type(&stat) != FileType::Symlink {
        return Ok((stat, None));
    }
    // An empty path reads the link the `O_PATH` descriptor already refers
    // to (Linux 2.6.39), so the target cannot be a different inode from
    // `stat`.
    #[cfg(test)]
    if let Some(error) = inject(_root, IoPoint::Readlink, _relative) {
        return Err(error);
    }
    let raw = readlinkat(&fd, "", Vec::new())
        .map_err(|error| (IoOp::Readlink, io::Error::from(error)))?;
    Ok((stat, Some(OsString::from_vec(raw.into_bytes()))))
}

fn entry_from_stat(stat: &rustix::fs::Stat) -> Entry {
    match file_type(stat) {
        FileType::Symlink => Entry::Symlink,
        FileType::Directory => Entry::Dir,
        FileType::RegularFile => Entry::File {
            size: u64::try_from(stat.st_size).unwrap_or(0),
        },
        _ => Entry::Other,
    }
}
