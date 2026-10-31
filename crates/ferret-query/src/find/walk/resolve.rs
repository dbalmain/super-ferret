//! Resolving a path, symlinks and `..` included, against the catalog or a
//! host's [`Namespace`] that agrees with it.

use super::*;

// Resolve links from the snapshot, including intermediate components. A ..
// after a link applies to the target's parent, not to the link's lexical
// parent.
//
// `..` is collapsed lexically against `prefix` only while every component
// resolved so far is exact: a real, catalogued directory, not an opaque
// (ignored) directory and not a symlink still waiting to be followed. Once
// resolution crosses an opaque boundary, a later `..` no longer has a
// trustworthy lexical parent (the opaque directory's real parent, after any
// symlinks inside it are followed, is something only the live filesystem
// knows), so the walk stops there and hands the whole reference to the
// caller's live fallback (#5) rather than guessing a lexical answer.
pub(in crate::find) fn resolve(
    catalog: &Catalog,
    path: &Path,
    follow: bool,
) -> io::Result<Option<(Target, bool)>> {
    resolve_observed(catalog, path, follow, path)
}

pub(in crate::find) fn resolve_observed(
    catalog: &Catalog,
    path: &Path,
    follow: bool,
    observed: &Path,
) -> io::Result<Option<(Target, bool)>> {
    let resolved = match resolve_catalog(catalog, path, follow) {
        Err(error) if follow && error.kind() == io::ErrorKind::NotFound => None,
        result => result?,
    };
    if resolved.is_some() || !follow {
        return Ok(resolved);
    }
    let physical = resolve_catalog(catalog, path, false)?;
    let Some((target, remainder)) = physical else {
        return Ok(None);
    };
    match fs::metadata(observed) {
        Ok(_) => Ok(Some((target, true))),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(Some((target, remainder))),
        Err(error) => Err(error),
    }
}

fn resolve_catalog(
    catalog: &Catalog,
    path: &Path,
    follow: bool,
) -> io::Result<Option<(Target, bool)>> {
    resolve_in(catalog, path, follow)
}

/// A namespace [`resolve_in`] walks: the catalog, or a host's view that must
/// agree with it on which spellings resolve.
pub trait Namespace {
    /// What a resolved path names.
    type Node;
    /// The node at absolute `path`, and whether it lies below an opaque node
    /// with an unresolved suffix; `None` when the namespace has no such path.
    fn resolve(&self, path: &Path) -> Option<(Self::Node, bool)>;
    /// The target of a symlink node; `None` for any other node.
    fn link_target(&self, node: &Self::Node) -> Option<PathBuf>;
    /// Whether the node is a directory `..` may be collapsed against.
    fn is_directory(&self, node: &Self::Node) -> bool;
}

impl Namespace for Catalog {
    type Node = Target;
    fn resolve(&self, path: &Path) -> Option<(Target, bool)> {
        Catalog::resolve(self, path.as_os_str().as_bytes())
            .map(|resolved| (resolved.target, !resolved.remainder.is_empty()))
    }
    fn link_target(&self, node: &Target) -> Option<PathBuf> {
        match *node {
            Target::Inode(id) if self.kind(id) == Kind::Symlink => Some(PathBuf::from(
                OsStr::from_bytes(Catalog::link_target(self, id).unwrap_or_default()),
            )),
            _ => None,
        }
    }
    fn is_directory(&self, node: &Target) -> bool {
        matches!(*node, Target::Inode(dir) if Catalog::is_directory(self, dir))
    }
}

/// Resolves `path` the way a catalogued start is resolved: lexically outside
/// the namespace, following its recorded symlinks inside it, and collapsing
/// `..` only after exact directories. `Ok(None)` means the spelling is
/// outside the namespace; `NotFound` is a missing child inside it.
pub fn resolve_in<N: Namespace>(
    namespace: &N,
    path: &Path,
    follow: bool,
) -> io::Result<Option<(N::Node, bool)>> {
    let mut pending = std::path::absolute(path)?;
    for _ in 0..40 {
        let mut prefix = PathBuf::new();
        let parts: Vec<_> = pending.components().collect();
        let mut redirected = None;
        // Whether `prefix` so far is an exact, catalogued directory - safe
        // to pop a later `..` against lexically.
        let mut exact = true;
        let mut opaque = None;
        // Whether the walk is currently inside catalog coverage. A `..` that
        // steps back out of a catalogued directory onto an uncatalogued
        // prefix is leaving coverage, not failing to find a child inside it,
        // so it clears `inside` instead of erroring; a miss on any other
        // component while `inside` is a genuinely missing child and errors.
        let mut inside = false;
        for (at, part) in parts.iter().enumerate() {
            let via_parent = matches!(part, std::path::Component::ParentDir);
            match part {
                std::path::Component::ParentDir => {
                    if !exact {
                        return Ok(opaque.map(|target| (target, true)));
                    }
                    prefix.pop();
                }
                std::path::Component::CurDir => {}
                part => prefix.push(part.as_os_str()),
            }
            let Some((target, below_opaque)) = namespace.resolve(&prefix) else {
                if inside && !via_parent {
                    return Err(rustix::io::Errno::NOENT.into());
                }
                inside = false;
                continue;
            };
            inside = true;
            if below_opaque {
                exact = false;
                opaque = Some(target);
                continue;
            }
            if let Some(link) = namespace.link_target(&target)
                && (follow || at + 1 < parts.len())
            {
                let mut next = prefix.parent().unwrap_or(Path::new("/")).join(link);
                for part in &parts[at + 1..] {
                    next.push(part.as_os_str());
                }
                redirected = Some(std::path::absolute(next)?);
                break;
            }
            exact = namespace.is_directory(&target);
            opaque = Some(target);
        }
        match redirected {
            Some(next) => pending = next,
            None => {
                return Ok(namespace.resolve(&prefix));
            }
        }
    }
    Err(rustix::io::Errno::LOOP.into())
}
