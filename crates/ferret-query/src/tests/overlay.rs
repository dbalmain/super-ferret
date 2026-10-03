//! Generated changes are inputs, not expected query outputs. The oracle is a
//! fresh real checkpoint writer, read through the same public catalog/query
//! APIs.
use super::{DAY, Scratch, dir, file, paths};
use crate::find::{Effects, Plan, WalkError};
use ferret_catalog::log::{ChangeSet, Record, Writer};
use ferret_catalog::{
    Catalog, Content, ContentState, DirToken, Hash, InoId, Kind, NameId, Stat, Target, Transaction,
};
use std::collections::BTreeMap;
use std::ffi::OsString;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

#[derive(Clone, Debug, PartialEq, Eq)]
struct Node {
    stat: Stat,
    kind: Kind,
    hash: Option<Hash>,
    link: Vec<u8>,
    root: Option<Vec<u8>>,
    traversed: bool,
}
#[derive(Clone, Debug, PartialEq, Eq)]
struct Edge {
    parent: u32,
    child: u32,
    name: Vec<u8>,
}
#[derive(Clone, Debug)]
struct State {
    nodes: BTreeMap<u32, Node>,
    edges: BTreeMap<u32, Edge>,
    next_ino: u32,
    next_name: u32,
    next_doc: u32,
    docs: BTreeMap<Hash, u32>,
}
fn node(id: u32, kind: Kind) -> Node {
    let stat = match kind {
        Kind::Dir => dir(u64::from(id) + 100),
        Kind::File => file(u64::from(id) + 100, u64::from(id) * 17 + 11, DAY),
        _ => {
            let mut st = file(u64::from(id) + 100, 19, DAY);
            st.mode = match kind {
                Kind::Symlink => 0o120777,
                Kind::Fifo => 0o010644,
                _ => unreachable!(),
            };
            st
        }
    };
    Node {
        stat,
        kind,
        hash: (kind == Kind::File).then_some([id as u8; 16]),
        link: b"destination".to_vec(),
        root: None,
        traversed: false,
    }
}
impl State {
    fn initial() -> Self {
        let mut s = Self {
            nodes: BTreeMap::new(),
            edges: BTreeMap::new(),
            next_ino: 0,
            next_name: 0,
            next_doc: 0,
            docs: BTreeMap::new(),
        };
        let r = s.root(b"/r");
        let t = s.root(b"/t");
        let a = s.create(r, b"a", Kind::Dir);
        let b = s.create(r, b"b", Kind::Dir);
        let deep = s.create(a, b"deep", Kind::Dir);
        s.create(deep, b"kept.rs", Kind::File);
        s.create(a, b"old.rs", Kind::File);
        s.create(b, b"pipe", Kind::Fifo);
        s.create(t, b"link", Kind::Symlink);
        s.ignored(r, b"ignored", Kind::Dir);
        s.nodes.get_mut(&b).unwrap().traversed = true;
        s.create(b, b"included.rs", Kind::File);
        s
    }
    fn root(&mut self, path: &[u8]) -> u32 {
        let id = self.next_ino;
        self.next_ino += 1;
        let mut n = node(id, Kind::Dir);
        n.root = Some(path.to_vec());
        self.nodes.insert(id, n);
        id
    }
    fn edge(&mut self, parent: u32, child: u32, name: &[u8]) -> u32 {
        let id = self.next_name;
        self.next_name += 1;
        self.edges.insert(
            id,
            Edge {
                parent,
                child,
                name: name.to_vec(),
            },
        );
        id
    }
    fn create(&mut self, parent: u32, name: &[u8], kind: Kind) -> u32 {
        let id = self.next_ino;
        self.next_ino += 1;
        self.nodes.insert(id, node(id, kind));
        self.edge(parent, id, name);
        id
    }
    fn ignored(&mut self, parent: u32, name: &[u8], kind: Kind) {
        self.edge(parent, u32::MAX - 1 - kind as u32, name);
    }
    fn remove(&mut self, id: u32) {
        let e = self.edges.remove(&id).unwrap();
        if !self.edges.values().any(|n| n.child == e.child) {
            self.nodes.remove(&e.child);
        }
    }
    fn own(&self, id: u32) -> Option<u32> {
        self.edges
            .iter()
            .find(|(_, e)| e.child == id)
            .map(|(&id, _)| id)
    }
    fn names(&self, id: u32) -> u32 {
        self.edges.values().filter(|e| e.child == id).count() as u32
    }
    fn entries(&self, id: u32) -> u32 {
        self.edges.values().filter(|e| e.parent == id).count() as u32
    }
    fn doc_refs(&self) -> BTreeMap<Hash, u32> {
        let mut refs = BTreeMap::new();
        for n in self.nodes.values() {
            if let Some(hash) = n.hash {
                *refs.entry(hash).or_default() += 1;
            }
        }
        refs
    }
    fn bind_docs(&mut self) {
        let refs = self.doc_refs();
        self.docs.retain(|hash, _| refs.contains_key(hash));
        for hash in refs.keys() {
            if !self.docs.contains_key(hash) {
                self.docs.insert(*hash, self.next_doc);
                self.next_doc += 1;
            }
        }
    }
    fn materialise(&self, path: &Path) -> Catalog {
        let mut tx = Transaction::begin(path, 1).unwrap();
        let mut b = tx.batch();
        let mut dirs = BTreeMap::<u32, DirToken>::new();
        let mut queue = Vec::new();
        for (&id, n) in &self.nodes {
            if let Some(path) = &n.root {
                let tok = b.root(path, n.stat);
                dirs.insert(id, tok);
                queue.push(id);
            }
        }
        while let Some(id) = queue.pop() {
            let tok = dirs[&id];
            b.entry_count(tok, self.entries(id));
            for e in self.edges.values().filter(|e| e.parent == id) {
                let Some(n) = self.nodes.get(&e.child) else {
                    let kind = match u32::MAX - 1 - e.child {
                        0 => Kind::Dir,
                        1 => Kind::File,
                        2 => Kind::Symlink,
                        _ => unreachable!(),
                    };
                    b.ignored(tok, &e.name, kind);
                    continue;
                };
                match n.kind {
                    Kind::Dir => {
                        let child = if n.traversed {
                            b.traversed_dir(tok, &e.name, n.stat)
                        } else {
                            b.dir(tok, &e.name, n.stat)
                        };
                        dirs.insert(e.child, child);
                        queue.push(e.child);
                    }
                    Kind::Symlink => {
                        b.symlink(tok, &e.name, n.stat, &n.link);
                    }
                    _ => {
                        b.file(
                            tok,
                            &e.name,
                            n.stat,
                            n.hash.map_or(Content::Unindexed, Content::Hashed),
                        );
                    }
                }
            }
        }
        tx.add(b);
        tx.commit().unwrap()
    }
    fn adopt_epoch(&mut self, base: &Catalog) {
        base.load_all().unwrap();
        let ids: BTreeMap<_, _> = base
            .inode_ids()
            .map(|i| (base.inode(i).stat.ino, i.0))
            .collect();
        let map: BTreeMap<_, _> = self
            .nodes
            .iter()
            .map(|(&id, n)| (id, ids[&n.stat.ino]))
            .collect();
        let edges: BTreeMap<_, _> = base
            .name_reader()
            .runs_from(NameId(0))
            .map(|(id, n)| ((n.parent.0, n.bytes.to_vec()), id.0))
            .collect();
        self.edges = self
            .edges
            .values()
            .map(|e| {
                let parent = map[&e.parent];
                let child = map.get(&e.child).copied().unwrap_or(e.child);
                let name = e.name.clone();
                (
                    edges[&(parent, name.clone())],
                    Edge {
                        parent,
                        child,
                        name,
                    },
                )
            })
            .collect();
        self.nodes = self
            .nodes
            .iter()
            .map(|(&id, n)| (map[&id], n.clone()))
            .collect();
        self.next_ino = base.next_inode().0;
        self.next_name = base.next_name().0;
        self.next_doc = base.next_doc().0;
        self.docs = base.docs().map(|(id, h)| (h, id.0)).collect();
    }
    fn delta(&mut self, old: &State) -> ChangeSet {
        self.bind_docs();
        let mut records = Vec::new();
        for &id in old.edges.keys() {
            if !self.edges.contains_key(&id) {
                records.push(Record::NameDelete { id });
            }
        }
        for (&id, e) in &self.edges {
            if old.edges.get(&id) != Some(e) {
                records.push(Record::NamePut {
                    id,
                    parent: e.parent,
                    child: e.child,
                    name: e.name.clone(),
                });
            }
        }
        for (&id, n) in &old.nodes {
            if !self.nodes.contains_key(&id) {
                records.push(Record::InodeDelete { id });
                if n.root.is_some() {
                    records.push(Record::RootDelete { id });
                }
                if n.kind == Kind::Symlink {
                    records.push(Record::LinkDelete { id });
                }
            }
        }
        for (&id, n) in &self.nodes {
            let before = old.nodes.get(&id);
            let names = self.names(id);
            if before.is_none() || old.names(id) != names {
                records.push(Record::LifePut {
                    id,
                    kind: n.kind,
                    flags: 0,
                    names,
                });
            }
            if before.is_none_or(|prev| prev.stat != n.stat || prev.hash != n.hash) {
                records.push(Record::InodePut {
                    id,
                    kind: n.kind,
                    state: if n.hash.is_some() {
                        ContentState::Hashed
                    } else {
                        ContentState::Unindexed
                    },
                    doc: n.hash.map(|h| self.docs[&h]),
                    stat: n.stat,
                });
            }
            if n.kind == Kind::Dir
                && (before.is_none()
                    || old.own(id) != self.own(id)
                    || old.entries(id) != self.entries(id)
                    || before.is_some_and(|p| p.traversed != n.traversed))
            {
                records.push(Record::DirPut {
                    id,
                    name: self.own(id),
                    entries: Some(self.entries(id)),
                    flags: 4 | if n.traversed { 3 } else { 0 },
                    retained_at: None,
                });
            }
            if before.and_then(|p| p.root.as_ref()) != n.root.as_ref()
                && let Some(path) = &n.root
            {
                records.push(Record::RootPut {
                    id,
                    path: path.clone(),
                });
            }
            if n.kind == Kind::Symlink && before.is_none_or(|p| p.link != n.link) {
                records.push(Record::LinkPut {
                    id,
                    target: n.link.clone(),
                });
            }
        }
        let refs = self.doc_refs();
        let prior = old.doc_refs();
        for (hash, &id) in &old.docs {
            if !self.docs.contains_key(hash) {
                records.push(Record::DocDelete { id });
            }
        }
        for (hash, &id) in &self.docs {
            if old.docs.get(hash) != Some(&id) || prior.get(hash) != refs.get(hash) {
                records.push(Record::DocPut {
                    id,
                    references: refs[hash],
                    hash: *hash,
                });
            }
        }
        ChangeSet {
            records,
            counters: [self.next_ino, self.next_name, self.next_doc],
            counts: [
                self.nodes.len() as u32,
                self.edges.len() as u32,
                self.nodes.values().filter(|n| n.kind == Kind::Dir).count() as u32,
                self.docs.len() as u32,
            ],
        }
    }
    fn rename(&mut self, id: u32, parent: u32, name: &[u8]) {
        if let Some(dest) = self
            .edges
            .iter()
            .find(|&(other, e)| *other != id && e.parent == parent && e.name == name)
            .map(|(&id, _)| id)
        {
            self.remove(dest);
        }
        let e = self.edges.get_mut(&id).unwrap();
        e.parent = parent;
        e.name = name.to_vec();
    }
}

#[path = "../../../ferret-catalog/tests/support/listing.rs"]
mod checkpoint_oracle;
use checkpoint_oracle::listings;

#[derive(Default)]
struct Output {
    paths: Vec<Vec<u8>>,
    bytes: Vec<u8>,
    errors: Vec<String>,
}
impl Effects for Output {
    fn print(&mut self, path: &Path, _nul: bool) -> io::Result<()> {
        self.paths.push(path.as_os_str().as_bytes().to_vec());
        Ok(())
    }
    fn write(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.bytes.extend_from_slice(bytes);
        Ok(())
    }
    fn error(&mut self, e: &WalkError) {
        self.errors.push(format!("{e:?}"));
    }
}
fn find_paths(c: &Catalog, args: &[&str]) -> Vec<Vec<u8>> {
    let roots: Vec<_> = c
        .roots()
        .map(|(_, path)| OsString::from(std::str::from_utf8(path).unwrap()))
        .collect();
    let args: Vec<_> = [OsString::from("-I")]
        .into_iter()
        .chain(roots)
        .chain(args.iter().map(OsString::from))
        .collect();
    let plan = Plan::parse(&args).unwrap();
    c.load(&plan.catalog_sections()).unwrap();
    let mut source = plan.catalog_source(c.clone());
    let mut out = Output::default();
    plan.run(&mut source, &mut out).unwrap();
    assert!(out.errors.is_empty(), "{:?}", out.errors);
    out.paths.extend(
        out.bytes
            .split(|&b| b == b'\n')
            .filter(|s| !s.is_empty())
            .map(<[u8]>::to_vec),
    );
    out.paths.sort();
    out.paths
}
fn oracle(state: &State, effective: &Catalog, scratch: &Scratch, label: &str) {
    if scratch.0.exists() {
        std::fs::remove_dir_all(&scratch.0).unwrap();
    }
    let checkpoint = state.materialise(&scratch.0);
    assert_eq!(
        listings(effective),
        listings(&checkpoint),
        "catalog: {label}"
    );
    for text in [
        "*",
        "rs",
        "ext:rs",
        "path:a",
        "size:>10",
        "size:>1000000",
        "mtime:<2d",
        "type:d",
        "type:l",
        "type:f size:>10",
        "*.rs size:>10",
    ] {
        assert_eq!(
            paths(effective, text),
            paths(&checkpoint, text),
            "query {text}: {label}"
        );
    }
    for args in [
        &["-print"][..],
        &["-depth", "-print"],
        &["-maxdepth", "1", "-print"],
        &["-name", "a", "-prune", "-o", "-print"],
        &["-type", "f", "-print"],
    ] {
        assert_eq!(
            find_paths(effective, args),
            find_paths(&checkpoint, args),
            "find {args:?}: {label}"
        );
    }
}
fn setup(label: &str) -> (Scratch, State, Writer) {
    let scratch = Scratch::new(label);
    let mut state = State::initial();
    let base = state.materialise(&scratch.0);
    state.adopt_epoch(&base);
    let writer = Writer::open(&scratch.0).unwrap();
    (scratch, state, writer)
}
fn publish(state: &mut State, old: &State, w: &mut Writer) -> Catalog {
    let delta = state.delta(old);
    w.commit(w.generation(), &delta).unwrap();
    w.view()
}

#[test]
fn discriminating_rename_hides_its_stale_base_key() {
    let (s, mut state, mut w) = setup("overlay-rename");
    let fresh = Scratch::new("oracle-rename");
    let old = state.clone();
    let (&id, e) = state
        .edges
        .iter()
        .find(|(_, e)| e.name == b"old.rs")
        .unwrap();
    let parent = e.parent;
    state.rename(id, parent, b"renamed.rs");
    let view = publish(&mut state, &old, &mut w);
    assert!(view.lookup(InoId(parent), b"old.rs").is_none());
    assert_eq!(view.lookup(InoId(parent), b"renamed.rs"), Some(NameId(id)));
    oracle(&state, &view, &fresh, "stale base key");
    drop(s);
}
#[test]
fn discriminating_delete_then_recreate_same_spelling_gets_new_ids() {
    let (_s, mut state, mut w) = setup("overlay-recreate");
    let fresh = Scratch::new("oracle-recreate");
    let (&edge, e) = state
        .edges
        .iter()
        .find(|(_, e)| e.name == b"old.rs")
        .unwrap();
    let (parent, inode) = (e.parent, e.child);
    let old = state.clone();
    state.remove(edge);
    publish(&mut state, &old, &mut w);
    let old = state.clone();
    let new = state.create(parent, b"old.rs", Kind::File);
    let view = publish(&mut state, &old, &mut w);
    assert_ne!(inode, new);
    assert_ne!(view.lookup(InoId(parent), b"old.rs"), Some(NameId(edge)));
    oracle(&state, &view, &fresh, "delete/recreate");
}
#[test]
fn discriminating_moved_directory_keeps_child_parent_ids_and_reuses_deleted_name() {
    let (_s, mut state, mut w) = setup("overlay-move");
    let fresh = Scratch::new("oracle-move");
    let (&edge, e) = state.edges.iter().find(|(_, e)| e.name == b"deep").unwrap();
    let child = e.child;
    let parent = state
        .nodes
        .iter()
        .find(|(_, n)| n.root.as_deref() == Some(b"/t"))
        .map(|(&id, _)| id)
        .unwrap();
    let before_children: Vec<_> = state
        .edges
        .values()
        .filter(|e| e.parent == child)
        .cloned()
        .collect();
    let old = state.clone();
    let dead = state.create(parent, b"vacated", Kind::File);
    publish(&mut state, &old, &mut w);
    let old = state.clone();
    let dead_edge = state.own(dead).unwrap();
    state.remove(dead_edge);
    publish(&mut state, &old, &mut w);
    let old = state.clone();
    state.rename(edge, parent, b"vacated");
    let view = publish(&mut state, &old, &mut w);
    assert_eq!(
        before_children,
        state
            .edges
            .values()
            .filter(|e| e.parent == child)
            .cloned()
            .collect::<Vec<_>>()
    );
    oracle(&state, &view, &fresh, "moved directory");
}
#[test]
fn directory_cycle_is_refused_by_real_writer_without_publication() {
    let (s, mut state, mut w) = setup("overlay-cycle");
    let old = state.clone();
    let (&edge, e) = state.edges.iter().find(|(_, e)| e.name == b"a").unwrap();
    let a = e.child;
    let deep = state
        .edges
        .values()
        .find(|e| e.parent == a && e.name == b"deep")
        .unwrap()
        .child;
    state.rename(edge, deep, b"a");
    let prior = std::fs::read(s.0.join("current")).unwrap();
    let delta = state.delta(&old);
    assert!(w.commit(w.generation(), &delta).is_err());
    assert_eq!(std::fs::read(s.0.join("current")).unwrap(), prior);
}
/// A deterministic property runner with replayable seed and minimal failing
/// prefix (the oracle runs after every operation). No property crate exists in
/// this workspace, and M3 forbids adding a manifest dependency.
#[test]
fn generated_change_sequences_match_real_materialised_checkpoint_oracle() {
    for seed in 0..16u64 {
        let (_s, mut state, mut writer) = setup(&format!("overlay-property-{seed}"));
        let fresh = Scratch::new(&format!("oracle-property-{seed}"));
        let mut rng = seed + 1;
        for step in 0..24 {
            rng = rng
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            let old = state.clone();
            let dirs: Vec<_> = state
                .nodes
                .iter()
                .filter(|(_, n)| n.kind == Kind::Dir)
                .map(|(&id, _)| id)
                .collect();
            let parent = dirs[(rng >> 8) as usize % dirs.len()];
            let spelling = format!("born-{step}.rs");
            let leaves: Vec<_> = state
                .edges
                .iter()
                .filter(|(_, e)| {
                    state
                        .nodes
                        .get(&e.child)
                        .is_none_or(|n| n.kind != Kind::Dir)
                        || state.entries(e.child) == 0
                })
                .map(|(&id, _)| id)
                .collect();
            let chosen = leaves
                .get((rng >> 16) as usize % leaves.len().max(1))
                .copied();
            match rng % 10 {
                0 => {
                    state.create(parent, spelling.as_bytes(), Kind::File);
                }
                1 => {
                    if let Some(id) = chosen {
                        state.remove(id);
                    }
                }
                2 => {
                    if let Some(id) = chosen {
                        let e = state.edges[&id].clone();
                        if state
                            .nodes
                            .get(&e.child)
                            .is_some_and(|n| n.kind != Kind::Dir)
                        {
                            let new = state.next_ino;
                            state.next_ino += 1;
                            state.nodes.insert(new, node(new, Kind::File));
                            state.edges.get_mut(&id).unwrap().child = new;
                            if state.names(e.child) == 0 {
                                state.nodes.remove(&e.child);
                            }
                        }
                    }
                }
                3 => {
                    if let Some(id) = chosen {
                        let e = state.edges[&id].clone();
                        state.rename(id, e.parent, spelling.as_bytes());
                    }
                }
                4 => {
                    if let Some(id) = chosen
                        && state.edges[&id].child != parent
                    {
                        state.rename(id, parent, spelling.as_bytes());
                    }
                }
                5 => {
                    let files: Vec<_> = state
                        .nodes
                        .iter()
                        .filter(|(_, n)| n.kind == Kind::File)
                        .map(|(&id, _)| id)
                        .collect();
                    if let Some(&child) = files.get((rng >> 24) as usize % files.len().max(1)) {
                        state.edge(parent, child, spelling.as_bytes());
                    }
                }
                6 => {
                    state.ignored(parent, spelling.as_bytes(), Kind::Dir);
                }
                7 => {
                    state.create(
                        parent,
                        spelling.as_bytes(),
                        if step % 2 == 0 {
                            Kind::Fifo
                        } else {
                            Kind::Symlink
                        },
                    );
                }
                8 => {
                    let id = state.create(parent, spelling.as_bytes(), Kind::Dir);
                    state.nodes.get_mut(&id).unwrap().traversed = step % 2 == 0;
                    if step % 2 == 0 {
                        state.create(id, b"re-included.rs", Kind::File);
                    }
                }
                _ => {
                    if step % 2 == 0 {
                        state.root(format!("/root-{step}").as_bytes());
                    } else if let Some(id) = chosen {
                        let child = state.edges[&id].child;
                        if let Some(n) = state.nodes.get_mut(&child)
                            && n.kind == Kind::File
                        {
                            n.stat.size += 100;
                            n.hash = Some([step as u8 + 150; 16]);
                        }
                    }
                }
            }
            // Generate complete catalogued observations: a directory without
            // any retained child is ordinary, rather than a D29 re-inclusion
            // ancestor which the checkpoint writer collapses to an ignored tag.
            let empty: Vec<_> = state
                .nodes
                .iter()
                .filter(|&(id, n)| n.traversed && state.entries(*id) == 0)
                .map(|(&id, _)| id)
                .collect();
            for id in empty {
                state.nodes.get_mut(&id).unwrap().traversed = false;
            }
            let view = publish(&mut state, &old, &mut writer);
            oracle(
                &state,
                &view,
                &fresh,
                &format!("seed={seed} prefix={}", step + 1),
            );
            let disk = Catalog::open(&_s.0).unwrap().unwrap();
            assert_eq!(
                listings(&view),
                listings(&disk),
                "replay seed={seed} prefix={}",
                step + 1
            );
        }
    }
}

#[test]
fn shared_documents_hard_links_and_root_retirement_match_fresh_checkpoint() {
    let (_s, mut state, mut w) = setup("overlay-shared-docs-roots");
    let fresh = Scratch::new("oracle-shared-docs-roots");
    let old = state.clone();
    let root = *state
        .nodes
        .iter()
        .find(|(_, n)| n.root.as_deref() == Some(b"/r"))
        .unwrap()
        .0;
    let original = *state
        .nodes
        .iter()
        .find(|(_, n)| n.kind == Kind::File)
        .unwrap()
        .0;
    let hash = state.nodes[&original].hash;
    let duplicate = state.create(root, b"duplicate.rs", Kind::File);
    state.nodes.get_mut(&duplicate).unwrap().hash = hash;
    state.edge(root, original, b"hardlink.rs");
    let view = publish(&mut state, &old, &mut w);
    oracle(
        &state,
        &view,
        &fresh,
        "distinct inodes share one document; hard link does not add a doc reference",
    );
    let old = state.clone();
    let retired = *state
        .nodes
        .iter()
        .find(|(_, n)| n.root.as_deref() == Some(b"/t"))
        .unwrap()
        .0;
    let edges: Vec<_> = state
        .edges
        .iter()
        .filter(|(_, e)| e.parent == retired)
        .map(|(&id, _)| id)
        .collect();
    for edge in edges {
        state.remove(edge);
    }
    state.nodes.remove(&retired);
    let view = publish(&mut state, &old, &mut w);
    oracle(&state, &view, &fresh, "root and its symlink retired");
}
#[test]
fn effective_find_delete_traverses_moved_directory_before_its_later_parent() {
    let live = Scratch::new("overlay-delete-live");
    let source = Scratch::new("overlay-delete-source");
    let fresh = Scratch::new("oracle-delete-source");
    let mut state = State {
        nodes: BTreeMap::new(),
        edges: BTreeMap::new(),
        next_ino: 0,
        next_name: 0,
        next_doc: 0,
        docs: BTreeMap::new(),
    };
    let root = state.root(live.0.as_os_str().as_bytes());
    let a = state.create(root, b"a", Kind::Dir);
    state.create(a, b"child", Kind::File);
    let base = state.materialise(&source.0);
    state.adopt_epoch(&base);
    let mut w = Writer::open(&source.0).unwrap();
    let old = state.clone();
    let root = *state
        .nodes
        .iter()
        .find(|(_, n)| n.root.is_some())
        .unwrap()
        .0;
    let a = state
        .edges
        .iter()
        .find(|(_, e)| e.name == b"a")
        .unwrap()
        .1
        .child;
    let later = state.create(root, b"later", Kind::Dir);
    assert!(later > a);
    state.rename(state.own(a).unwrap(), later, b"moved");
    let effective = publish(&mut state, &old, &mut w);
    let checkpoint = state.materialise(&fresh.0);
    for c in [&effective, &checkpoint] {
        std::fs::create_dir_all(live.0.join("later/moved")).unwrap();
        std::fs::write(live.0.join("later/moved/child"), b"data").unwrap();
        let paths = find_paths(c, &["-delete", "-print"]);
        assert_eq!(paths.len(), 4);
        assert!(
            !live.0.exists(),
            "real unlinkat removed every descendant and root"
        );
    }
}
