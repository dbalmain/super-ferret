//! Replacement records. The same decoder checks writer output and lazy loads.
use super::Family;
use crate::format::{NONE, u32_at, u64_at};
use crate::{ContentState, DecodeError, Hash, Kind, Stat, WorkTreeKind};

/// Complete row replacements and tombstones; ids belong to the expected epoch.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Record {
    LifePut {
        id: u32,
        kind: Kind,
        flags: u8,
        names: u32,
    },
    InodeDelete {
        id: u32,
    },
    NamePut {
        id: u32,
        parent: u32,
        child: u32,
        name: Vec<u8>,
    },
    NameDelete {
        id: u32,
    },
    DirPut {
        id: u32,
        name: Option<u32>,
        entries: Option<u32>,
        flags: u32,
        retained_at: Option<u64>,
    },
    RootPut {
        id: u32,
        path: Vec<u8>,
    },
    RootDelete {
        id: u32,
    },
    PolicyPut {
        hash: Hash,
    },
    InodePut {
        id: u32,
        kind: Kind,
        state: ContentState,
        doc: Option<u32>,
        stat: Stat,
    },
    LinkPut {
        id: u32,
        target: Vec<u8>,
    },
    LinkDelete {
        id: u32,
    },
    WorkTreePut {
        id: u32,
        kind: WorkTreeKind,
        common_id: (u64, u64),
        path: Vec<u8>,
    },
    WorkTreeDelete {
        id: u32,
    },
    DocPut {
        id: u32,
        references: u32,
        hash: Hash,
    },
    DocDelete {
        id: u32,
    },
}

impl Record {
    pub fn family(&self) -> Family {
        match self {
            Self::InodePut { .. } => Family::Inodes,
            Self::LinkPut { .. }
            | Self::LinkDelete { .. }
            | Self::WorkTreePut { .. }
            | Self::WorkTreeDelete { .. } => Family::Aux,
            Self::DocPut { .. } | Self::DocDelete { .. } => Family::Docs,
            _ => Family::Namespace,
        }
    }

    pub(super) fn encode(&self, out: &mut Vec<u8>) -> Result<(), DecodeError> {
        // Some(reserved sentinel) must never silently become None on wire.
        let reserved = match self {
            Self::DirPut {
                name,
                entries,
                retained_at,
                ..
            } => *name == Some(NONE) || *entries == Some(NONE) || *retained_at == Some(u64::MAX),
            Self::InodePut { doc, .. } => *doc == Some(NONE),
            _ => false,
        };
        if reserved {
            return Err(bad());
        }
        let start = out.len();
        out.extend_from_slice(&[0; 8]);
        let op = match self {
            Self::LifePut {
                id,
                kind,
                flags,
                names,
            } => {
                put32(out, *id);
                out.extend_from_slice(&[*kind as u8, *flags, 0, 0]);
                put32(out, *names);
                put32(out, 0);
                1
            }
            Self::InodeDelete { id } => {
                delete(out, *id);
                2
            }
            Self::NamePut {
                id,
                parent,
                child,
                name,
            } => {
                put32(out, *id);
                put32(out, *parent);
                put32(out, *child);
                string(out, name)?;
                3
            }
            Self::NameDelete { id } => {
                delete(out, *id);
                4
            }
            Self::DirPut {
                id,
                name,
                entries,
                flags,
                retained_at,
            } => {
                put32(out, *id);
                put32(out, name.unwrap_or(NONE));
                put32(out, entries.unwrap_or(NONE));
                put32(out, *flags);
                put64(out, retained_at.unwrap_or(u64::MAX));
                5
            }
            Self::RootPut { id, path } => {
                put32(out, *id);
                string(out, path)?;
                6
            }
            Self::RootDelete { id } => {
                delete(out, *id);
                7
            }
            Self::PolicyPut { hash } => {
                out.extend_from_slice(hash);
                8
            }
            Self::InodePut {
                id,
                kind,
                state,
                doc,
                stat,
            } => {
                put32(out, *id);
                out.extend_from_slice(&[*kind as u8, *state as u8, 0, 0]);
                put32(out, doc.unwrap_or(NONE));
                put64(out, stat.dev);
                put64(out, stat.ino);
                put64(out, stat.size);
                put64(out, stat.mtime_sec as u64);
                put32(out, stat.mtime_nsec);
                put64(out, stat.ctime_sec as u64);
                put32(out, stat.ctime_nsec);
                put32(out, stat.mode);
                put32(out, stat.uid);
                put32(out, stat.gid);
                put64(out, stat.nlink);
                9
            }
            Self::LinkPut { id, target } => {
                put32(out, *id);
                string(out, target)?;
                10
            }
            Self::LinkDelete { id } => {
                delete(out, *id);
                11
            }
            Self::WorkTreePut {
                id,
                kind,
                common_id,
                path,
            } => {
                put32(out, *id);
                out.extend_from_slice(&[*kind as u8, 0, 0, 0]);
                put64(out, common_id.0);
                put64(out, common_id.1);
                string(out, path)?;
                12
            }
            Self::WorkTreeDelete { id } => {
                delete(out, *id);
                13
            }
            Self::DocPut {
                id,
                references,
                hash,
            } => {
                put32(out, *id);
                put32(out, *references);
                out.extend_from_slice(hash);
                14
            }
            Self::DocDelete { id } => {
                delete(out, *id);
                15
            }
        };
        while !(out.len() - start).is_multiple_of(8) {
            out.push(0);
        }
        let len = u32::try_from(out.len() - start).map_err(|_| bad())?;
        out[start] = op;
        out[start + 4..start + 8].copy_from_slice(&len.to_le_bytes());
        Ok(())
    }
}

fn put32(out: &mut Vec<u8>, n: u32) {
    out.extend_from_slice(&n.to_le_bytes());
}
fn put64(out: &mut Vec<u8>, n: u64) {
    out.extend_from_slice(&n.to_le_bytes());
}
fn delete(out: &mut Vec<u8>, id: u32) {
    put32(out, id);
    put32(out, 0);
}
fn string(out: &mut Vec<u8>, bytes: &[u8]) -> Result<(), DecodeError> {
    let count = bytes
        .len()
        .checked_add(1)
        .and_then(|n| u32::try_from(n).ok())
        .ok_or_else(bad)?;
    put32(out, count);
    out.extend_from_slice(bytes);
    out.push(0);
    Ok(())
}
fn bad() -> DecodeError {
    DecodeError::Corrupt("log record")
}
fn kind(n: u8) -> Result<Kind, DecodeError> {
    match n {
        0 => Ok(Kind::Dir),
        1 => Ok(Kind::File),
        2 => Ok(Kind::Symlink),
        3 => Ok(Kind::Fifo),
        4 => Ok(Kind::Socket),
        5 => Ok(Kind::Block),
        6 => Ok(Kind::Character),
        _ => Err(bad()),
    }
}
fn text(bytes: &[u8], count_at: usize, start: usize) -> Result<Vec<u8>, DecodeError> {
    let count = u32_at(bytes, count_at) as usize;
    let end = start
        .checked_add(count)
        .filter(|&n| n <= bytes.len())
        .ok_or_else(bad)?;
    if count < 2
        || bytes[end - 1] != 0
        || bytes[start..end - 1].contains(&0)
        || bytes[end..].iter().any(|&b| b != 0)
        || bytes.len() != end.next_multiple_of(8)
    {
        return Err(bad());
    }
    Ok(bytes[start..end - 1].to_vec())
}

pub(super) fn decode(
    bytes: &[u8],
    count: u32,
    family: Family,
    counters: [u32; 3],
    sequence: u64,
) -> Result<Vec<Record>, DecodeError> {
    let mut records = Vec::new();
    let mut rest = bytes;
    while !rest.is_empty() {
        if rest.len() < 8 || rest[1..4] != [0; 3] {
            return Err(bad());
        }
        let len = u32_at(rest, 4) as usize;
        if len < 16 || !len.is_multiple_of(8) || len > rest.len() {
            return Err(bad());
        }
        let b = &rest[..len];
        let id = u32_at(b, 8);
        let exact = |n| if len == n { Ok(()) } else { Err(bad()) };
        let deleted = || -> Result<(), DecodeError> {
            exact(16)?;
            if b[12..16] != [0; 4] {
                return Err(bad());
            }
            Ok(())
        };
        let record = match b[0] {
            1 => {
                exact(24)?;
                if b[14..16] != [0; 2]
                    || b[20..24] != [0; 4]
                    || b[13] != 0
                    || u32_at(b, 16) > counters[1]
                    || (u32_at(b, 16) == 0 && kind(b[12])? != Kind::Dir)
                {
                    return Err(bad());
                }
                Record::LifePut {
                    id,
                    kind: kind(b[12])?,
                    flags: b[13],
                    names: u32_at(b, 16),
                }
            }
            2 => {
                deleted()?;
                Record::InodeDelete { id }
            }
            3 => {
                if len < 32 {
                    return Err(bad());
                }
                let parent = u32_at(b, 12);
                let child = u32_at(b, 16);
                let name = text(b, 20, 24)?;
                if parent >= counters[0]
                    || (child >= counters[0] && Kind::from_ignored_child(child).is_none())
                    || name.contains(&b'/')
                    || name == b"."
                    || name == b".."
                {
                    return Err(bad());
                }
                Record::NamePut {
                    id,
                    parent,
                    child,
                    name,
                }
            }
            4 => {
                deleted()?;
                Record::NameDelete { id }
            }
            5 => {
                exact(32)?;
                let name = u32_at(b, 12);
                let flags = u32_at(b, 20);
                let retained = u64_at(b, 24);
                if (name != NONE && name >= counters[1])
                    || flags & !15 != 0
                    || (retained != u64::MAX && retained > sequence)
                    || ((flags & 8 != 0) != (retained != u64::MAX))
                {
                    return Err(bad());
                }
                Record::DirPut {
                    id,
                    name: (name != NONE).then_some(name),
                    entries: (u32_at(b, 16) != NONE).then_some(u32_at(b, 16)),
                    flags,
                    retained_at: (retained != u64::MAX).then_some(retained),
                }
            }
            6 => {
                let path = text(b, 12, 16)?;
                if !path.starts_with(b"/") {
                    return Err(bad());
                }
                Record::RootPut { id, path }
            }
            7 => {
                deleted()?;
                Record::RootDelete { id }
            }
            8 => {
                exact(24)?;
                let mut hash = [0; 16];
                hash.copy_from_slice(&b[8..24]);
                Record::PolicyPut { hash }
            }
            9 => {
                exact(88)?;
                let k = kind(b[12])?;
                let state = b[13];
                let doc = u32_at(b, 16);
                let stat = Stat {
                    dev: u64_at(b, 20),
                    ino: u64_at(b, 28),
                    size: u64_at(b, 36),
                    mtime_sec: u64_at(b, 44) as i64,
                    mtime_nsec: u32_at(b, 52),
                    ctime_sec: u64_at(b, 56) as i64,
                    ctime_nsec: u32_at(b, 64),
                    mode: u32_at(b, 68),
                    uid: u32_at(b, 72),
                    gid: u32_at(b, 76),
                    nlink: u64_at(b, 80),
                };
                if b[14..16] != [0; 2]
                    || state > 3
                    || (state == 2) != (doc != NONE)
                    || (doc != NONE && doc >= counters[2])
                    || stat.mtime_nsec >= 1_000_000_000
                    || stat.ctime_nsec >= 1_000_000_000
                    || Kind::from_mode(stat.mode) != k
                    || (k != Kind::File && state != 0)
                {
                    return Err(bad());
                }
                Record::InodePut {
                    id,
                    kind: k,
                    state: ContentState::from_bits(state),
                    doc: (doc != NONE).then_some(doc),
                    stat,
                }
            }
            10 => Record::LinkPut {
                id,
                target: text(b, 12, 16)?,
            },
            11 => {
                deleted()?;
                Record::LinkDelete { id }
            }
            12 => {
                if len < 40 || b[13..16] != [0; 3] {
                    return Err(bad());
                }
                let k = WorkTreeKind::from_byte(b[12]).ok_or_else(bad)?;
                let path = text(b, 32, 36)?;
                if !path.starts_with(b"/") {
                    return Err(bad());
                }
                Record::WorkTreePut {
                    id,
                    kind: k,
                    common_id: (u64_at(b, 16), u64_at(b, 24)),
                    path,
                }
            }
            13 => {
                deleted()?;
                Record::WorkTreeDelete { id }
            }
            14 => {
                exact(32)?;
                let references = u32_at(b, 12);
                if references == 0 || references > counters[0] {
                    return Err(bad());
                }
                let mut hash = [0; 16];
                hash.copy_from_slice(&b[16..32]);
                Record::DocPut {
                    id,
                    references,
                    hash,
                }
            }
            15 => {
                deleted()?;
                Record::DocDelete { id }
            }
            _ => return Err(bad()),
        };
        if record.family() != family {
            return Err(bad());
        }
        let limit = match record {
            Record::PolicyPut { .. } => None,
            Record::NamePut { .. } | Record::NameDelete { .. } => Some(counters[1]),
            Record::DocPut { .. } | Record::DocDelete { .. } => Some(counters[2]),
            _ => Some(counters[0]),
        };
        if limit.is_some_and(|n| id >= n) {
            return Err(bad());
        }
        records.push(record);
        if records.len() > count as usize {
            return Err(bad());
        }
        rest = &rest[len..];
    }
    if records.len() != count as usize {
        return Err(bad());
    }
    Ok(records)
}
