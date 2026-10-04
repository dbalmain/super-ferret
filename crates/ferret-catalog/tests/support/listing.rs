//! Shared M3/M4 semantic comparison with a materialised checkpoint.
use ferret_catalog::{Catalog, Hash, Kind, NameId, Stat, Target};

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct Listing {
    pub(crate) path: Vec<u8>,
    kind: u8,
    state: Option<u8>,
    stat: Option<StatKey>,
    hash: Option<Hash>,
    refs: Option<u32>,
    link: Option<Vec<u8>>,
    traversed: bool,
    pub(crate) entries: Option<u32>,
    suppressed: bool,
    pub(crate) retained_at: Option<u64>,
    work_tree: Option<(u8, (u64, u64), Vec<u8>)>,
}
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct StatKey([u64; 12]);
fn stat_key(s: Stat) -> StatKey {
    StatKey([
        s.dev,
        s.ino,
        s.size,
        s.mtime_sec as u64,
        u64::from(s.mtime_nsec),
        s.ctime_sec as u64,
        u64::from(s.ctime_nsec),
        u64::from(s.mode),
        u64::from(s.uid),
        u64::from(s.gid),
        s.nlink,
        0,
    ])
}
pub(crate) fn listings(c: &Catalog) -> Vec<Listing> {
    c.load_all().unwrap();
    let mut rows = Vec::new();
    let mut add = |path: Vec<u8>, target: Target| {
        let (kind, stat, hash, refs, link, traversed, entries) = match target {
            Target::Ignored(kind) => (kind, None, None, None, None, false, None),
            Target::Inode(id) => {
                let kind = c.kind(id);
                let inode = c.inode(id);
                let hash = inode.doc.and_then(|doc| c.doc_hash(doc));
                let refs = inode.doc.and_then(|doc| c.doc_references(doc));
                (
                    kind,
                    Some(stat_key(inode.stat)),
                    hash,
                    refs,
                    c.link_target(id).map(<[u8]>::to_vec),
                    kind == Kind::Dir && c.is_traversed(id),
                    if kind == Kind::Dir {
                        c.entry_count(id)
                    } else {
                        None
                    },
                )
            }
        };
        rows.push(Listing {
            path,
            kind: kind as u8,
            state: match target {
                Target::Inode(id) => Some(c.state(id) as u8),
                Target::Ignored(_) => None,
            },
            stat,
            hash,
            refs,
            link,
            traversed,
            entries,
            suppressed: match target {
                Target::Inode(id) if kind == Kind::Dir => c.is_search_suppressed(id),
                _ => false,
            },
            retained_at: match target {
                Target::Inode(id) if kind == Kind::Dir => c.retained_at(id),
                _ => None,
            },
            work_tree: match target {
                Target::Inode(id) if kind == Kind::Dir => c
                    .work_tree(id)
                    .map(|w| (w.kind as u8, w.common_id, w.common_dir.to_vec())),
                _ => None,
            },
        });
    };
    for (id, path) in c.roots() {
        add(path.to_vec(), Target::Inode(id));
    }
    for (id, n) in c.name_reader().runs_from(NameId(0)) {
        let mut path = Vec::new();
        c.path(id, &mut path);
        add(path, n.target());
    }
    rows.sort();
    rows
}

