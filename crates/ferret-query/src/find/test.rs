//! Path, metadata and system-name predicates used by GNU find expressions.

use std::ffi::OsStr;
use std::fs;
use std::os::unix::ffi::OsStrExt;
use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use ferret_verify::FindRegex;

use super::printf::account_names;
use super::{Entry, FileKind, glob};

mod date;
use date::parse_date;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Order {
    Equal,
    Greater,
    Less,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) struct Number {
    order: Order,
    value: f64,
}

/// One parsed predicate. References resolve once against the selected
/// source before execution.
#[derive(Clone, Debug, PartialEq)]
pub(super) enum Test {
    Regex(FindRegex),
    Link(glob::Pattern),
    Xtype(Vec<FileKind>),
    Reference {
        path: PathBuf,
        follow: bool,
        field: TimeField,
        reference_field: TimeField,
        same_file: bool,
    },
    Perm {
        mode: u32,
        conditional: u32,
        order: Order,
    },
    Size {
        units: Number,
        bytes: u64,
    },
    Time {
        field: TimeField,
        age: Number,
        now: SystemTime,
        daystart: bool,
        minutes: bool,
    },
    Used(Number),
    Links(Number),
    Inum(Number),
    Uid(Number),
    Gid(Number),
    User(u32),
    Group(u32),
    NoUser,
    NoGroup,
    Newer {
        field: TimeField,
        stamp: i128,
    },
    SameFile {
        dev: u64,
        ino: u64,
    },
    Empty,
    Access(rustix::fs::Access),
    FsType(String),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum TimeField {
    Access,
    Birth,
    Change,
    Modify,
}

impl TimeField {
    fn from_letter(letter: u8) -> Self {
        match letter {
            b'a' => Self::Access,
            b'B' => Self::Birth,
            b'c' => Self::Change,
            _ => Self::Modify,
        }
    }
}

impl Test {
    pub(super) fn sections(&self, out: &mut Vec<ferret_catalog::Section>) {
        use ferret_catalog::Section::*;
        let time = |field| match field {
            TimeField::Modify => vec![Mtime, MtimeNs],
            TimeField::Change => vec![Ctime, CtimeNs],
            _ => vec![],
        };
        out.extend(match self {
            Self::Reference {
                field,
                reference_field,
                same_file,
                ..
            } => {
                let mut fields = time(*field);
                fields.extend(time(*reference_field));
                if *same_file {
                    fields.extend([Dev, Ino]);
                }
                fields
            }
            Self::Perm { .. } => vec![Mode],
            Self::Size { .. } | Self::Empty => vec![Size],
            Self::Time { field, .. } | Self::Newer { field, .. } => time(*field),
            Self::Used(_) => vec![Ctime, CtimeNs],
            Self::Links(_) => vec![Nlink],
            Self::Inum(_) => vec![Dev, Ino],
            Self::Uid(_)
            | Self::Gid(_)
            | Self::User(_)
            | Self::Group(_)
            | Self::NoUser
            | Self::NoGroup => vec![Owner],
            Self::SameFile { .. } | Self::FsType(_) => vec![Dev, Ino],
            Self::Access(_) | Self::Regex(_) | Self::Link(_) | Self::Xtype(_) => vec![],
        });
    }

    pub(super) fn perm(bytes: &[u8]) -> Option<Self> {
        let (order, mode) = match bytes.first() {
            Some(b'-') => (Order::Greater, &bytes[1..]),
            Some(b'/') => (Order::Less, &bytes[1..]),
            _ => (Order::Equal, bytes),
        };
        let (mode, conditional) = parse_mode(mode)?;
        Some(Self::Perm {
            mode,
            conditional,
            order,
        })
    }

    pub(super) fn size(bytes: &[u8]) -> Option<Self> {
        let (number, suffix) = if bytes.last().is_some_and(|b| b"bcwkMG".contains(b)) {
            (&bytes[..bytes.len() - 1], *bytes.last()?)
        } else {
            (bytes, b'b')
        };
        let units = parse_number(number, false)?;
        let multiplier = match suffix {
            b'c' => 1,
            b'w' => 2,
            b'k' => 1024,
            b'M' => 1024 * 1024,
            b'G' => 1024 * 1024 * 1024,
            _ => 512,
        };
        Some(Self::Size {
            units,
            bytes: multiplier,
        })
    }

    pub(super) fn number(primary: &[u8], value: &[u8]) -> Option<Self> {
        match primary {
            b"-links" => Some(Self::Links(parse_number(value, false)?)),
            b"-inum" => Some(Self::Inum(parse_number(value, false)?)),
            b"-uid" => Some(Self::Uid(parse_number(value, false)?)),
            b"-gid" => Some(Self::Gid(parse_number(value, false)?)),
            _ => None,
        }
    }

    pub(super) fn time(
        primary: &[u8],
        value: &[u8],
        now: SystemTime,
        daystart: bool,
    ) -> Option<Self> {
        let field = match primary {
            b"-atime" | b"-amin" => TimeField::Access,
            b"-ctime" | b"-cmin" => TimeField::Change,
            _ => TimeField::Modify,
        };
        let age = parse_number(value, true)?;
        Some(if primary == b"-used" {
            Self::Used(age)
        } else {
            Self::Time {
                field,
                age,
                now,
                daystart,
                minutes: matches!(primary, b"-mmin" | b"-amin" | b"-cmin"),
            }
        })
    }

    pub(super) fn same_file(path: PathBuf, follow: bool) -> Self {
        Self::Reference {
            path,
            follow,
            field: TimeField::Modify,
            reference_field: TimeField::Modify,
            same_file: true,
        }
    }

    /// `-newerXY`, where `-newer`, `-anewer` and `-cnewer` are `mm`, `am` and
    /// `cm`; a `t` for Y reads the value as a date rather than a file.
    pub(super) fn newer(
        x: u8,
        y: u8,
        value: &OsStr,
        now: SystemTime,
        follow: bool,
        date_offset: &mut i32,
    ) -> Option<Self> {
        let field = TimeField::from_letter(x);
        if y == b't' {
            return parse_date(value, now, date_offset).map(|stamp| Self::Newer { field, stamp });
        }
        Some(Self::Reference {
            path: PathBuf::from(value),
            follow,
            field,
            reference_field: TimeField::from_letter(y),
            same_file: false,
        })
    }

    pub(super) fn resolve_reference(
        &mut self,
        catalog: Option<&ferret_catalog::Catalog>,
        options: &super::Options,
    ) -> std::io::Result<()> {
        let Self::Reference {
            path,
            follow,
            field,
            reference_field,
            same_file,
        } = self
        else {
            return Ok(());
        };
        let logical = options.logical_path(path);
        let observed = options.observed_path(path);
        let entry = Entry::new(observed.to_path_buf(), 0, FileKind::File);
        if let Some(catalog) = catalog {
            let resolved = super::walk::resolve_observed(
                catalog,
                logical.as_ref(),
                *follow,
                observed.as_ref(),
            )?
            .ok_or_else(|| {
                std::io::Error::other(
                    "reference is outside the catalog or the index is stale; re-index or use -I",
                )
            })?;
            if let (ferret_catalog::Target::Inode(id), false) = resolved {
                *self = if *same_file {
                    let (dev, ino) = catalog.identity(id);
                    Self::SameFile { dev, ino }
                } else {
                    let stamp = match reference_field {
                        TimeField::Modify => timestamp(catalog.mtime(id), catalog.mtime_nsec(id)),
                        TimeField::Change => timestamp(catalog.ctime(id), catalog.ctime_nsec(id)),
                        TimeField::Access => {
                            let stat = super::walk::metadata(observed.as_ref(), *follow)?;
                            stat_stamp(&stat, TimeField::Access)
                        }
                        TimeField::Birth => birth_stamp(rustix::fs::CWD, entry.path())
                            .ok_or_else(|| std::io::Error::other("birth time unavailable"))?,
                    };
                    Self::Newer {
                        field: *field,
                        stamp,
                    }
                };
                return Ok(());
            }
        }
        let stat = super::walk::metadata(observed.as_ref(), *follow)?;
        *self = if *same_file {
            Self::SameFile {
                dev: stat.dev(),
                ino: stat.ino(),
            }
        } else {
            Self::Newer {
                field: *field,
                stamp: if *reference_field == TimeField::Birth {
                    birth_stamp(rustix::fs::CWD, entry.path())
                        .ok_or_else(|| std::io::Error::other("birth time unavailable"))?
                } else {
                    stat_stamp(&stat, *reference_field)
                },
            }
        };
        Ok(())
    }

    pub(super) fn evaluate(&self, entry: &Entry) -> std::io::Result<bool> {
        Ok(match self {
            Self::Regex(regex) => regex
                .try_is_match(entry.path().as_os_str().as_bytes())
                .map_err(std::io::Error::other)?,
            Self::Link(pattern) => {
                entry.kind()? == FileKind::Symlink && pattern.matches(&entry.link_target()?)
            }
            Self::Xtype(kinds) => kinds.contains(&entry.opposite_kind()?),
            Self::Reference { .. } => {
                let mut test = self.clone();
                test.resolve_reference(None, &super::Options::default())?;
                return test.evaluate(entry);
            }
            Self::Perm {
                mode,
                conditional,
                order,
            } => {
                let actual = entry.stat()?.mode() & 0o7777;
                let conditional = if *conditional != 0 && entry.kind()? == FileKind::Directory {
                    *conditional
                } else {
                    0
                };
                let required = *mode | conditional;
                match order {
                    Order::Equal => actual == required,
                    Order::Greater => actual & required == required,
                    Order::Less => required == 0 || actual & required != 0,
                }
            }
            Self::Size { units, bytes } => {
                let size = entry.stat()?.size().div_ceil(*bytes);
                compare(size, units.value as u64, units.order)
            }
            Self::Time {
                field,
                age,
                now,
                daystart,
                minutes,
            } => {
                let delta = age_nanos(entry_stamp(entry, *field)?, *now, *daystart);
                age_matches(delta, *age, *minutes)
            }
            Self::Used(age) => {
                let access = entry_stamp(entry, TimeField::Access)?;
                let changed = entry_stamp(entry, TimeField::Change)?;
                let days = (access - changed) as f64 / 86_400_000_000_000.0;
                if days < 0.0 {
                    false
                } else {
                    match age.order {
                        Order::Equal => days.floor() + 1.0 == age.value,
                        Order::Greater => days > age.value,
                        Order::Less => days < age.value,
                    }
                }
            }
            Self::Links(n) => compare(entry.stat()?.nlink(), n.value as u64, n.order),
            Self::Inum(n) => compare(entry.stat()?.ino(), n.value as u64, n.order),
            Self::Uid(n) => compare(entry.stat()?.uid() as u64, n.value as u64, n.order),
            Self::Gid(n) => compare(entry.stat()?.gid() as u64, n.value as u64, n.order),
            Self::User(uid) => entry.stat()?.uid() == *uid,
            Self::Group(gid) => entry.stat()?.gid() == *gid,
            Self::NoUser => !account_names(false).contains_key(&entry.stat()?.uid()),
            Self::NoGroup => !account_names(true).contains_key(&entry.stat()?.gid()),
            Self::Newer { field, stamp } => {
                let actual = if *field == TimeField::Birth {
                    entry.with_observed_at(birth_stamp).ok_or_else(|| {
                        std::io::Error::new(
                            std::io::ErrorKind::Unsupported,
                            "birth time unavailable",
                        )
                    })?
                } else {
                    entry_stamp(entry, *field)?
                };
                actual > *stamp
            }
            Self::SameFile { dev, ino } => {
                let stat = entry.stat()?;
                stat.dev() == *dev && stat.ino() == *ino
            }
            Self::Empty => match entry.kind()? {
                FileKind::File => entry.stat()?.size() == 0,
                // Raw indexed counts include ignored names. Opaque/live
                // directories have no count and use their live listing.
                FileKind::Directory => match entry.has_children() {
                    Some(has_children) => !has_children,
                    None => !entry.with_observed_at(has_entries)?,
                },
                _ => false,
            },
            Self::Access(access) => entry
                .with_observed_at(|dir, path| {
                    rustix::fs::accessat(dir, path, *access, rustix::fs::AtFlags::empty())
                })
                .is_ok(),
            Self::FsType(wanted) => filesystem_type(entry.stat()?.dev()) == Some(wanted.as_str()),
        })
    }
}

fn parse_number(bytes: &[u8], fractional: bool) -> Option<Number> {
    let (order, digits) = match bytes.first() {
        Some(b'+') => (Order::Greater, &bytes[1..]),
        Some(b'-') => (Order::Less, &bytes[1..]),
        _ => (Order::Equal, bytes),
    };
    if digits.is_empty()
        || !digits
            .iter()
            .all(|b| b.is_ascii_digit() || (fractional && *b == b'.'))
    {
        return None;
    }
    let value = if fractional && digits.contains(&b'.') {
        std::str::from_utf8(digits).ok()?.parse::<f64>().ok()?
    } else {
        let text = std::str::from_utf8(digits).ok()?;
        text.parse::<u64>()
            .map_or_else(|_| text.parse::<f64>(), |n| Ok(n as f64))
            .ok()?
    };
    let value = if value.is_infinite() { f64::MAX } else { value };
    value.is_finite().then_some(Number { order, value })
}

fn compare(actual: u64, expected: u64, order: Order) -> bool {
    match order {
        Order::Equal => actual == expected,
        Order::Greater => actual > expected,
        Order::Less => actual < expected,
    }
}
/// Whether an age of `delta` nanoseconds is `age` days, or minutes, in
/// GNU's rounding for each.
fn age_matches(delta: i128, age: Number, minutes: bool) -> bool {
    let unit_nanos = if minutes {
        60_000_000_000i128
    } else {
        86_400_000_000_000i128
    };
    let unit = unit_nanos as f64;
    if age.value.fract() != 0.0 {
        let raw = delta as f64 / unit;
        match age.order {
            Order::Equal if minutes => raw > age.value - 1.0 && raw <= age.value,
            Order::Equal => raw >= age.value && raw < age.value + 1.0,
            // GNU: `-mtime +0.5` is older than 1.5 days, the fractional
            // analogue of `+N` meaning "N+1 or more".
            Order::Greater if !minutes => raw > age.value + 1.0,
            Order::Greater => raw > age.value,
            Order::Less => raw < age.value,
        }
    } else if minutes && age.order == Order::Less {
        // GNU compares the unrounded age here: a 359 s file is `-mmin -6`
        // although its whole-minute bucket is 6.
        delta < age.value as i128 * unit_nanos
    } else {
        let value = if minutes {
            (delta + unit_nanos - 1).div_euclid(unit_nanos)
        } else {
            delta.div_euclid(unit_nanos)
        };
        compare_float(value as f64, age.value, age.order)
    }
}

fn compare_float(actual: f64, expected: f64, order: Order) -> bool {
    match order {
        Order::Equal => actual == expected,
        Order::Greater => actual > expected,
        Order::Less => actual < expected,
    }
}

fn parse_mode(bytes: &[u8]) -> Option<(u32, u32)> {
    if bytes.is_empty() {
        return None;
    }
    if bytes.iter().all(|b| (b'0'..=b'7').contains(b)) {
        return u32::from_str_radix(std::str::from_utf8(bytes).ok()?, 8)
            .ok()
            .filter(|v| *v <= 0o7777)
            .map(|mode| (mode, 0));
    }
    let mut mode = 0u32;
    let mut conditional = 0u32;
    for clause in bytes.split(|b| *b == b',') {
        // chmod grammar: who* (op perm*)+, applied to a zero starting mode.
        let op_at = clause.iter().position(|b| b"+-=".contains(b))?;
        let who = &clause[..op_at];
        if !who.iter().all(|w| b"ugoa".contains(w)) {
            return None;
        }
        let who = if who.is_empty() || who.contains(&b'a') {
            b"ugo".as_slice()
        } else {
            who
        };
        let selected = who.iter().fold(0, |acc, w| {
            acc | match w {
                b'u' => 0o4700,
                b'g' => 0o2070,
                _ => 0o1007,
            }
        });
        let class = |u, g, o| {
            who.iter().fold(0u32, |acc, w| {
                acc | match w {
                    b'u' => u,
                    b'g' => g,
                    _ => o,
                }
            })
        };
        let mut rest = &clause[op_at..];
        while let Some((&op, tail)) = rest.split_first() {
            let end = tail
                .iter()
                .position(|b| b"+-=".contains(b))
                .unwrap_or(tail.len());
            let (perms, next) = tail.split_at(end);
            rest = next;
            let mut bits = 0u32;
            let mut xbits = 0u32;
            for c in perms {
                match c {
                    b'r' => bits |= class(0o400, 0o040, 0o004),
                    b'w' => bits |= class(0o200, 0o020, 0o002),
                    b'x' => bits |= class(0o100, 0o010, 0o001),
                    b'X' => xbits |= class(0o100, 0o010, 0o001),
                    b's' => bits |= class(0o4000, 0o2000, 0),
                    b't' => bits |= 0o1000,
                    // Copying a class from the zero starting mode adds nothing.
                    b'u' | b'g' | b'o' => {}
                    _ => return None,
                }
            }
            match op {
                b'+' => {
                    mode |= bits;
                    conditional |= xbits;
                }
                b'-' => {
                    mode &= !(bits | xbits);
                    conditional &= !(bits | xbits);
                }
                _ => {
                    mode = (mode & !selected) | bits;
                    conditional = (conditional & !selected) | xbits;
                }
            }
        }
    }
    Some((mode, conditional))
}

fn entry_stamp(entry: &Entry, field: TimeField) -> std::io::Result<i128> {
    let stat = entry.stat()?;
    Ok(match field {
        TimeField::Access => {
            let live = entry.metadata()?;
            timestamp(live.atime(), live.atime_nsec())
        }
        TimeField::Change => timestamp(stat.ctime(), stat.ctime_nsec()),
        _ => timestamp(stat.mtime(), stat.mtime_nsec()),
    })
}

fn age_nanos(stamp: i128, now: SystemTime, daystart: bool) -> i128 {
    let now = if daystart {
        let duration = now.duration_since(UNIX_EPOCH).unwrap_or_default();
        // GNU measures -daystart ages from the end of today (observed: a file
        // from 23:00 the day before yesterday is `-daystart -mtime 1`).
        UNIX_EPOCH
            + Duration::from_secs(super::zone::local().day_end(duration.as_secs() as i64) as u64)
    } else {
        now
    };
    let now_ns = now
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos() as i128;
    now_ns - stamp
}

fn timestamp(sec: i64, nsec: i64) -> i128 {
    sec as i128 * 1_000_000_000 + nsec as i128
}
fn stat_stamp(stat: &super::walk::Metadata, field: TimeField) -> i128 {
    match field {
        TimeField::Access => timestamp(stat.atime(), stat.atime_nsec()),
        TimeField::Change => timestamp(stat.ctime(), stat.ctime_nsec()),
        _ => timestamp(stat.mtime(), stat.mtime_nsec()),
    }
}
fn birth_stamp(dir: std::os::fd::BorrowedFd<'_>, path: &std::path::Path) -> Option<i128> {
    let stat = rustix::fs::statx(
        dir,
        path,
        rustix::fs::AtFlags::NO_AUTOMOUNT,
        rustix::fs::StatxFlags::BTIME,
    )
    .ok()?;
    (stat.stx_mask & rustix::fs::StatxFlags::BTIME.bits() != 0)
        .then(|| timestamp(stat.stx_btime.tv_sec, i64::from(stat.stx_btime.tv_nsec)))
}
// Whether the directory at `path` beneath `dir` lists anything besides `.`
// and `..`.
fn has_entries(dir: std::os::fd::BorrowedFd<'_>, path: &std::path::Path) -> std::io::Result<bool> {
    use rustix::fs::{Dir, Mode, OFlags, openat};
    let flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC;
    // `Dir::new` lists the fresh descriptor itself; `read_from` would reopen
    // `.` beneath it, which needs search permission GNU does not.
    let mut listing = Dir::new(openat(dir, path, flags, Mode::empty())?)?;
    while let Some(entry) = listing.read() {
        if !matches!(entry?.file_name().to_bytes(), b"." | b"..") {
            return Ok(true);
        }
    }
    Ok(false)
}

pub(super) fn identity(primary: &[u8], name: &OsStr) -> Option<u32> {
    let path = if primary == b"-user" {
        "/etc/passwd"
    } else {
        "/etc/group"
    };
    let bytes = name.as_bytes();
    // A name first, then a number, which names an id even without a database
    // entry (GNU accepts `-user 99999`).
    fs::read_to_string(path)
        .ok()
        .and_then(|contents| {
            contents.lines().find_map(|line| {
                let mut fields = line.split(':');
                let entry_name = fields.next()?;
                let id = fields.nth(1)?.parse().ok()?;
                (entry_name.as_bytes() == bytes).then_some(id)
            })
        })
        .or_else(|| std::str::from_utf8(bytes).ok()?.trim_start().parse().ok())
}

/// The type of the filesystem holding a device, from the mount whose
/// `major:minor` (mountinfo field 3) is the entry's own `st_dev`. Matching on
/// the device rather than the path means a symlink is typed where it lives, not
/// where it points, and costs no path resolution per entry.
pub(super) fn filesystem_type(dev: u64) -> Option<&'static str> {
    static MOUNTS: std::sync::OnceLock<Vec<(u64, String)>> = std::sync::OnceLock::new();
    let mounts = MOUNTS.get_or_init(|| {
        fs::read_to_string("/proc/self/mountinfo")
            .unwrap_or_default()
            .lines()
            .filter_map(|line| {
                let (left, right) = line.split_once(" - ")?;
                let (major, minor) = left.split_whitespace().nth(2)?.split_once(':')?;
                let dev = device(major.parse().ok()?, minor.parse().ok()?);
                Some((dev, right.split_whitespace().next()?.to_owned()))
            })
            .collect()
    });
    mounts
        .iter()
        .find(|(mount, _)| *mount == dev)
        .map(|(_, kind)| kind.as_str())
}

/// Linux's `makedev`: the `dev_t` encoding of a major and minor number.
fn device(major: u64, minor: u64) -> u64 {
    (major & 0xfff) << 8 | (major & !0xfff) << 32 | (minor & 0xff) | (minor & !0xff) << 12
}

#[cfg(test)]
mod tests;
