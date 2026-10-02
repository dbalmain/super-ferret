//! Stat and system-name tests used by GNU find expressions.

use std::ffi::OsStr;
use std::fs;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use super::{Entry, FileKind};

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

/// One parsed metadata predicate. References resolve once against the selected
/// source before execution.
#[derive(Clone, Debug, PartialEq)]
pub(super) enum Test {
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
    Used {
        age: Number,
        now: SystemTime,
    },
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
            Self::Used { .. } => vec![Ctime, CtimeNs],
            Self::Links(_) => vec![Nlink],
            Self::Inum(_) => vec![Dev, Ino],
            Self::Uid(_)
            | Self::Gid(_)
            | Self::User(_)
            | Self::Group(_)
            | Self::NoUser
            | Self::NoGroup => vec![Owner],
            Self::SameFile { .. } | Self::FsType(_) => vec![Dev, Ino],
            Self::Access(_) => vec![],
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
            Self::Used { age, now }
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

    pub(super) fn reference(primary: &[u8], path: PathBuf, follow: bool) -> Option<Self> {
        Some(Self::Reference {
            path,
            follow,
            field: match primary {
                b"-anewer" => TimeField::Access,
                b"-cnewer" => TimeField::Change,
                _ => TimeField::Modify,
            },
            reference_field: TimeField::Modify,
            same_file: primary == b"-samefile",
        })
    }

    pub(super) fn newer_xy(
        x: u8,
        y: u8,
        path: PathBuf,
        _now: SystemTime,
        follow: bool,
    ) -> Option<Self> {
        fn field(code: u8) -> TimeField {
            match code {
                b'a' => TimeField::Access,
                b'B' => TimeField::Birth,
                b'c' => TimeField::Change,
                _ => TimeField::Modify,
            }
        }
        Some(Self::Reference {
            path,
            follow,
            field: field(x),
            reference_field: field(y),
            same_file: false,
        })
    }

    pub(super) fn resolve_reference(
        &mut self,
        catalog: Option<&ferret_catalog::Catalog>,
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
        let mut entry = Entry::new(path.clone(), 0, FileKind::File);
        if let Some(catalog) = catalog {
            let resolved = super::walk::resolve(catalog, path, *follow)?.ok_or_else(|| {
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
                            let stat = if *follow {
                                fs::metadata(&*path)?
                            } else {
                                fs::symlink_metadata(&*path)?
                            };
                            stat_stamp(&stat, TimeField::Access)
                        }
                        TimeField::Birth => birth_stamp(entry.path())
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
        if *follow {
            entry = Entry::new(fs::canonicalize(&*path)?, 0, FileKind::File);
        }
        let stat = entry.metadata().map_err(super::walk::copy_error)?;
        *self = if *same_file {
            Self::SameFile {
                dev: stat.dev(),
                ino: stat.ino(),
            }
        } else {
            Self::Newer {
                field: *field,
                stamp: if *reference_field == TimeField::Birth {
                    birth_stamp(entry.path())
                        .ok_or_else(|| std::io::Error::other("birth time unavailable"))?
                } else {
                    stat_stamp(stat, *reference_field)
                },
            }
        };
        Ok(())
    }

    pub(super) fn evaluate(&self, entry: &Entry) -> std::io::Result<bool> {
        let stat = entry.stat()?;
        Ok(match self {
            Self::Reference { .. } => {
                let mut test = self.clone();
                test.resolve_reference(None)?;
                return test.evaluate(entry);
            }
            Self::Perm {
                mode,
                conditional,
                order,
            } => {
                let actual = stat.mode() & 0o7777;
                let conditional = if *conditional != 0
                    && entry
                        .kind()
                        .map_err(|error| std::io::Error::new(error.kind(), error.to_string()))?
                        == FileKind::Directory
                {
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
                let size = stat.size().div_ceil(*bytes);
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
                let unit_nanos = if *minutes {
                    60_000_000_000i128
                } else {
                    86_400_000_000_000i128
                };
                let unit = unit_nanos as f64;
                if age.value.fract() != 0.0 {
                    let raw = delta as f64 / unit;
                    match age.order {
                        Order::Equal if *minutes => raw > age.value - 1.0 && raw <= age.value,
                        Order::Equal => raw >= age.value && raw < age.value + 1.0,
                        // GNU: `-mtime +0.5` is older than 1.5 days, the
                        // fractional analogue of `+N` meaning "N+1 or more".
                        Order::Greater if !*minutes => raw > age.value + 1.0,
                        Order::Greater => raw > age.value,
                        Order::Less => raw < age.value,
                    }
                } else if *minutes && age.order == Order::Less {
                    // GNU compares the unrounded age here: a 359 s file is
                    // `-mmin -6` although its whole-minute bucket is 6.
                    delta < age.value as i128 * unit_nanos
                } else {
                    let value = if *minutes {
                        (delta + unit_nanos - 1).div_euclid(unit_nanos)
                    } else {
                        delta.div_euclid(unit_nanos)
                    };
                    compare_float(value as f64, age.value, age.order)
                }
            }
            Self::Used { age, now } => {
                let access = entry_stamp(entry, TimeField::Access)?;
                let changed = entry_stamp(entry, TimeField::Change)?;
                let days = (access - changed) as f64 / 86_400_000_000_000.0;
                let _ = now;
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
            Self::Links(n) => compare(stat.nlink(), n.value as u64, n.order),
            Self::Inum(n) => compare(stat.ino(), n.value as u64, n.order),
            Self::Uid(n) => compare(stat.uid() as u64, n.value as u64, n.order),
            Self::Gid(n) => compare(stat.gid() as u64, n.value as u64, n.order),
            Self::User(uid) => stat.uid() == *uid,
            Self::Group(gid) => stat.gid() == *gid,
            Self::NoUser => !user_exists(stat.uid()),
            Self::NoGroup => !group_exists(stat.gid()),
            Self::Newer { field, stamp } => {
                let actual = if *field == TimeField::Birth {
                    birth_stamp(entry.path()).ok_or_else(|| {
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
            Self::SameFile { dev, ino } => stat.dev() == *dev && stat.ino() == *ino,
            Self::Empty => match entry
                .kind()
                .map_err(|e| std::io::Error::new(e.kind(), e.to_string()))?
            {
                FileKind::File => stat.size() == 0,
                // Raw indexed counts include ignored names. Opaque/live
                // directories have no count and use their live listing.
                FileKind::Directory => match entry.has_children() {
                    Some(has_children) => !has_children,
                    None => fs::read_dir(entry.path())?.next().transpose()?.is_none(),
                },
                _ => false,
            },
            Self::Access(access) => rustix::fs::access(entry.path(), *access).is_ok(),
            Self::FsType(wanted) => filesystem_type(stat.dev()) == Some(wanted.as_str()),
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
            let live = entry.metadata().map_err(super::walk::copy_error)?;
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
        UNIX_EPOCH + Duration::from_secs(duration.as_secs() / 86_400 * 86_400 + 86_400)
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
fn stat_stamp(stat: &fs::Metadata, field: TimeField) -> i128 {
    match field {
        TimeField::Access => timestamp(stat.atime(), stat.atime_nsec()),
        TimeField::Change => timestamp(stat.ctime(), stat.ctime_nsec()),
        _ => timestamp(stat.mtime(), stat.mtime_nsec()),
    }
}
fn birth_stamp(path: &std::path::Path) -> Option<i128> {
    let stat = rustix::fs::statx(
        rustix::fs::CWD,
        path,
        rustix::fs::AtFlags::empty(),
        rustix::fs::StatxFlags::BTIME,
    )
    .ok()?;
    (stat.stx_mask & rustix::fs::StatxFlags::BTIME.bits() != 0)
        .then(|| timestamp(stat.stx_btime.tv_sec, i64::from(stat.stx_btime.tv_nsec)))
}
fn user_exists(id: u32) -> bool {
    fs::read_to_string("/etc/passwd").is_ok_and(|s| {
        s.lines()
            .any(|l| l.split(':').nth(2).and_then(|v| v.parse::<u32>().ok()) == Some(id))
    })
}
fn group_exists(id: u32) -> bool {
    fs::read_to_string("/etc/group").is_ok_and(|s| {
        s.lines()
            .any(|l| l.split(':').nth(2).and_then(|v| v.parse::<u32>().ok()) == Some(id))
    })
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

/// Parses the deliberately bounded GNU date subset used by the find corpus.
pub(super) fn parse_date(value: &OsStr, now: SystemTime) -> Option<i128> {
    let text = std::str::from_utf8(value.as_bytes()).ok()?.trim();
    let now_ns = now.duration_since(UNIX_EPOCH).ok()?.as_nanos() as i128;
    let timestamp_ns = if text == "now" {
        return Some(now_ns);
    } else if text == "today" || text == "yesterday" {
        if text == "today" {
            now_ns
        } else {
            now_ns - 86_400_000_000_000
        }
    } else if let Some(epoch) = text.strip_prefix('@') {
        epoch.parse::<i64>().ok()? as i128 * 1_000_000_000
    } else if let Some(relative) = text.strip_prefix("now-") {
        relative_seconds(relative).map(|delta| now_ns - delta as i128 * 1_000_000_000)?
    } else if let Some(relative) = text.strip_prefix('-') {
        relative_seconds(relative).map(|delta| now_ns - delta as i128 * 1_000_000_000)?
    } else if let Some(relative) = text.strip_suffix(" ago") {
        if let Some(delta) = relative_seconds(relative) {
            now_ns - delta as i128 * 1_000_000_000
        } else {
            calendar_months_ago(relative, now)?
        }
    } else {
        parse_absolute(text, now)? as i128 * 1_000_000_000
    };
    Some(timestamp_ns)
}

fn relative_seconds(value: &str) -> Option<i64> {
    let mut parts = value.split_whitespace();
    let amount: i64 = parts.next()?.parse().ok()?;
    let unit = parts.next()?;
    if parts.next().is_some() {
        return None;
    }
    let unit = unit.trim_end_matches('s');
    let multiplier = match unit {
        "second" | "sec" => 1,
        "minute" | "min" => 60,
        "hour" | "hr" => 3600,
        "day" => 86400,
        _ => return None,
    };
    amount.checked_mul(multiplier)
}

fn parse_absolute(value: &str, now: SystemTime) -> Option<i64> {
    if let Some((date, clock)) = value.split_once('T') {
        let clock = clock.strip_suffix('Z').unwrap_or(clock);
        let zone_at = clock
            .char_indices()
            .skip(1)
            .filter(|(_, c)| *c == '+' || *c == '-')
            .map(|(i, _)| i)
            .last();
        let (clock, offset) = if let Some(i) = zone_at {
            (&clock[..i], Some(&clock[i..]))
        } else {
            (clock, None)
        };
        let base = parse_absolute(&format!("{date} {clock}"), now)?;
        let zone = if let Some(offset) = offset {
            let sign = if offset.starts_with('-') { -1 } else { 1 };
            let mut parts = offset[1..].split(':');
            let hours: i64 = parts.next()?.parse().ok()?;
            let minutes: i64 = parts.next()?.parse().ok()?;
            sign * (hours * 3600 + minutes * 60)
        } else {
            0
        };
        return Some(base - zone);
    }
    if let Some((month, rest)) = value.split_once(' ')
        && month.len() == 3
        && month.as_bytes()[0].is_ascii_alphabetic()
    {
        let month_num = match month.to_ascii_lowercase().as_str() {
            "jan" => 1,
            "feb" => 2,
            "mar" => 3,
            "apr" => 4,
            "may" => 5,
            "jun" => 6,
            "jul" => 7,
            "aug" => 8,
            "sep" => 9,
            "oct" => 10,
            "nov" => 11,
            "dec" => 12,
            _ => return None,
        };
        let rest = rest.trim_start();
        let (day_text, tail) = rest
            .split_once(',')
            .map_or_else(|| rest.split_once(' '), |(d, t)| Some((d, t)))?;
        let day: u32 = day_text.trim().parse().ok()?;
        let (year, time) = if rest.contains(',') {
            let mut parts = tail.split_whitespace();
            let year = parts.next()?.parse().ok()?;
            (year, parts.next())
        } else {
            let year = civil_year(now)?;
            (year, Some(tail.trim()))
        };
        let date_seconds = days_from_civil(year, month_num, day).checked_mul(86_400)?;
        if let Some(time) = time {
            let mut t = time.split(':');
            let h: i64 = t.next()?.parse().ok()?;
            let m: i64 = t.next()?.parse().ok()?;
            let s: i64 = t.next().map(str::parse).transpose().ok()?.unwrap_or(0);
            return Some(date_seconds + h * 3600 + m * 60 + s);
        }
        return Some(date_seconds);
    }
    let (date, time) = value
        .split_once(' ')
        .map_or((value, None), |(d, t)| (d, Some(t)));
    let mut parts = date.split('-');
    let year: i64 = parts.next()?.parse().ok()?;
    let month: u32 = parts.next()?.parse().ok()?;
    let day: u32 = parts.next()?.parse().ok()?;
    if parts.next().is_some() || !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    let days = days_from_civil(year, month, day);
    let mut seconds = days.checked_mul(86_400)?;
    if let Some(time) = time {
        let mut t = time.split(':');
        let hour: i64 = t.next()?.parse().ok()?;
        let minute: i64 = t.next()?.parse().ok()?;
        let second: i64 = t.next().map(str::parse).transpose().ok()?.unwrap_or(0);
        if hour > 23 || minute > 59 || second > 60 {
            return None;
        }
        seconds += hour * 3600 + minute * 60 + second;
        if t.next().is_some() {
            return None;
        }
    }
    Some(seconds)
}

fn days_from_civil(year: i64, month: u32, day: u32) -> i64 {
    let year = year - i64::from(month <= 2);
    let era = year.div_euclid(400);
    let yoe = year - era * 400;
    let month = month as i64;
    let doy = (153 * (month + if month > 2 { -3 } else { 9 }) + 2) / 5 + day as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

fn civil_year(now: SystemTime) -> Option<i64> {
    let days = now.duration_since(UNIX_EPOCH).ok()?.as_secs() as i64 / 86_400;
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let mut year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let month = mp + if mp < 10 { 3 } else { -9 };
    year += i64::from(month <= 2);
    Some(year)
}

fn calendar_months_ago(value: &str, now: SystemTime) -> Option<i128> {
    let mut parts = value.split_whitespace();
    let count: i64 = parts.next()?.parse().ok()?;
    if parts.next()?.trim_end_matches('s') != "month" || parts.next().is_some() {
        return None;
    }
    let seconds = now.duration_since(UNIX_EPOCH).ok()?.as_secs() as i64;
    let days = seconds.div_euclid(86_400);
    let seconds_in_day = seconds.rem_euclid(86_400);
    let (year, month, day) = civil_date(days);
    let month_index = year
        .checked_mul(12)?
        .checked_add(month as i64 - 1)?
        .checked_sub(count)?;
    let new_year = month_index.div_euclid(12);
    let new_month = month_index.rem_euclid(12) as u32 + 1;
    let max_day = match new_month {
        2 if new_year % 4 == 0 && (new_year % 100 != 0 || new_year % 400 == 0) => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    };
    let new_day = day.min(max_day);
    Some(
        (days_from_civil(new_year, new_month, new_day) * 86_400 + seconds_in_day) as i128
            * 1_000_000_000,
    )
}

fn civil_date(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let mut year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = mp + if mp < 10 { 3 } else { -9 };
    year += i64::from(month <= 2);
    (year, month as u32, day as u32)
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
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use std::fs::FileTimes;
    use std::os::unix::fs::{PermissionsExt, symlink};

    struct Fixture(PathBuf);
    impl Fixture {
        fn new() -> Self {
            static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let path = std::env::temp_dir().join(format!(
                "ferret-find-test-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            ));
            let _ = fs::remove_dir_all(&path);
            fs::create_dir_all(&path).unwrap();
            Self(path)
        }
        fn file(&self, name: &str, size: usize, age: u64) -> Entry {
            let path = self.0.join(name);
            fs::write(&path, vec![0u8; size]).unwrap();
            let file = fs::File::options().write(true).open(&path).unwrap();
            file.set_times(
                FileTimes::new()
                    .set_modified(UNIX_EPOCH + Duration::from_secs(2_000_000_000 - age)),
            )
            .unwrap();
            Entry::new(path, 0, FileKind::File)
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn shared_numeric_parser_accepts_comparators_and_fractional_times_and_matches_gnu_overflow() {
        for value in [b"1".as_slice(), b"+1", b"-1", b"1.5"] {
            assert!(parse_number(value, true).is_some());
        }
        for value in [b"".as_slice(), b"++1", b"1  ", b"1.2.3"] {
            assert!(parse_number(value, false).is_none(), "{value:?}");
        }
        assert!(parse_number(b"18446744073709551616", false).is_some());
    }

    #[test]
    fn fstype_types_a_symlink_where_it_lives_not_where_it_points() {
        let fixture = Fixture::new();
        let link = fixture.0.join("into-proc");
        symlink("/proc/self", &link).unwrap();
        let proc = Test::FsType("proc".to_owned());
        assert!(
            proc.evaluate(&Entry::new(
                PathBuf::from("/proc/self"),
                0,
                FileKind::Symlink
            ))
            .unwrap()
        );
        assert!(
            !proc
                .evaluate(&Entry::new(link, 0, FileKind::Symlink))
                .unwrap()
        ); // Resolving the path would say proc.
    }

    #[test]
    fn size_rounds_each_entry_up_before_comparison() {
        let fixture = Fixture::new();
        let empty = fixture.file("empty", 0, 0);
        let one = fixture.file("one", 1, 0);
        let fifteen_hundred = fixture.file("fifteen-hundred", 1500, 0);
        let less_one_k = Test::size(b"-1k").unwrap();
        assert!(less_one_k.evaluate(&empty).unwrap());
        assert!(!less_one_k.evaluate(&one).unwrap()); // Discriminates byte comparison from GNU's rounded unit count.
        assert!(
            Test::size(b"2k")
                .unwrap()
                .evaluate(&fifteen_hundred)
                .unwrap()
        ); // 1500 bytes rounds up to 2 KiB.
        assert!(Test::size(b"0").unwrap().evaluate(&empty).unwrap());
        assert!(!Test::size(b"-1").unwrap().evaluate(&one).unwrap());
    }

    #[test]
    fn minutes_round_up_and_days_round_down_on_the_same_fixed_clock() {
        let fixture = Fixture::new();
        let now = UNIX_EPOCH + Duration::from_secs(2_000_000_000);
        let thirty = fixture.file("thirty", 0, 30);
        let ninety = fixture.file("ninety", 0, 90);
        let hundred_fifty = fixture.file("one-fifty", 0, 150);
        assert!(
            Test::time(b"-mmin", b"1", now, false)
                .unwrap()
                .evaluate(&thirty)
                .unwrap()
        );
        assert!(
            Test::time(b"-mmin", b"2", now, false)
                .unwrap()
                .evaluate(&ninety)
                .unwrap()
        );
        assert!(
            Test::time(b"-mmin", b"+1", now, false)
                .unwrap()
                .evaluate(&ninety)
                .unwrap()
        );
        assert!(
            Test::time(b"-mmin", b"+1", now, false)
                .unwrap()
                .evaluate(&hundred_fifty)
                .unwrap()
        );
        assert!(
            !Test::time(b"-mmin", b"1", now, false)
                .unwrap()
                .evaluate(&ninety)
                .unwrap()
        );
        let twenty = fixture.file("twenty", 0, 20);
        let forty = fixture.file("forty", 0, 40);
        assert!(
            Test::time(b"-mmin", b"0.5", now, false)
                .unwrap()
                .evaluate(&twenty)
                .unwrap()
        );
        assert!(
            !Test::time(b"-mmin", b"0.5", now, false)
                .unwrap()
                .evaluate(&forty)
                .unwrap()
        );
        let day_and_three_quarters = fixture.file("day-and-three-quarters", 0, 86_400 + 64_800);
        let day_and_quarter = fixture.file("day-and-quarter", 0, 86_400 + 21_600);
        assert!(
            Test::time(b"-mtime", b"1.5", now, false)
                .unwrap()
                .evaluate(&day_and_three_quarters)
                .unwrap()
        );
        assert!(
            !Test::time(b"-mtime", b"1.5", now, false)
                .unwrap()
                .evaluate(&day_and_quarter)
                .unwrap()
        );
        assert!(
            Test::time(b"-mtime", b"0", now, false)
                .unwrap()
                .evaluate(&thirty)
                .unwrap()
        );
        let day_old = fixture.file("day-old", 0, 86_400);
        assert!(
            !Test::time(b"-mtime", b"0", now, false)
                .unwrap()
                .evaluate(&day_old)
                .unwrap()
        );
        assert!(
            !Test::time(b"-mtime", b"-1", now, false)
                .unwrap()
                .evaluate(&day_old)
                .unwrap()
        );
    }

    #[test]
    fn daystart_measures_from_the_end_of_today_and_future_timestamps_remain_negative_ages() {
        let fixture = Fixture::new();
        let now = UNIX_EPOCH + Duration::from_secs(2_000_000_000);
        let midnight = 2_000_000_000u64 / 86_400 * 86_400;
        let path = fixture.0.join("last-night");
        fs::write(&path, b"").unwrap();
        fs::File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_times(
                FileTimes::new().set_modified(UNIX_EPOCH + Duration::from_secs(midnight - 3600)),
            )
            .unwrap();
        let entry = Entry::new(path, 0, FileKind::File);
        // 4.5 hours old by the clock, 25 hours old from the end of today.
        let matches = |age: &[u8], daystart| {
            Test::time(b"-mtime", age, now, daystart)
                .unwrap()
                .evaluate(&entry)
                .unwrap()
        };
        assert!(matches(b"0", false));
        assert!(!matches(b"0", true));
        assert!(matches(b"1", true));
        let future_path = fixture.0.join("future");
        fs::write(&future_path, b"").unwrap();
        fs::File::options()
            .write(true)
            .open(&future_path)
            .unwrap()
            .set_times(FileTimes::new().set_modified(now + Duration::from_secs(3600)))
            .unwrap();
        let future = Entry::new(future_path, 0, FileKind::File);
        assert!(
            Test::time(b"-mtime", b"-1", now, false)
                .unwrap()
                .evaluate(&future)
                .unwrap()
        );
        assert!(
            !Test::time(b"-mtime", b"0", now, false)
                .unwrap()
                .evaluate(&future)
                .unwrap()
        );
    }

    #[test]
    fn permission_prefixes_and_zero_start_symbolic_modes_match_find_rules() {
        let fixture = Fixture::new();
        let path = fixture.0.join("mode");
        fs::write(&path, b"").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o744)).unwrap();
        let entry = Entry::new(path, 0, FileKind::File);
        assert!(Test::perm(b"744").unwrap().evaluate(&entry).unwrap());
        assert!(Test::perm(b"-u+r").unwrap().evaluate(&entry).unwrap());
        assert!(Test::perm(b"/000").unwrap().evaluate(&entry).unwrap());
        assert!(Test::perm(b"-000").unwrap().evaluate(&entry).unwrap());
        assert!(Test::perm(b"-u+x").unwrap().evaluate(&entry).unwrap());
        assert!(!Test::perm(b"u+x").unwrap().evaluate(&entry).unwrap());
        let executable_dir = fixture.0.join("x-dir");
        fs::create_dir(&executable_dir).unwrap();
        fs::set_permissions(&executable_dir, fs::Permissions::from_mode(0o111)).unwrap();
        let entry = Entry::new(executable_dir, 0, FileKind::Directory);
        assert!(Test::perm(b"a+X").unwrap().evaluate(&entry).unwrap());
    }

    #[test]
    fn newer_comparison_is_strict_and_preserves_nanoseconds() {
        let fixture = Fixture::new();
        let reference_path = fixture.0.join("reference");
        let equal_path = fixture.0.join("equal");
        let newer_path = fixture.0.join("newer");
        for path in [&reference_path, &equal_path, &newer_path] {
            fs::write(path, b"").unwrap();
        }
        let stamp =
            UNIX_EPOCH + Duration::from_secs(1_700_000_000) + Duration::from_nanos(123_456_789);
        for path in [&reference_path, &equal_path] {
            fs::File::options()
                .write(true)
                .open(path)
                .unwrap()
                .set_times(FileTimes::new().set_modified(stamp))
                .unwrap();
        }
        fs::File::options()
            .write(true)
            .open(&newer_path)
            .unwrap()
            .set_times(FileTimes::new().set_modified(stamp + Duration::from_nanos(1)))
            .unwrap();
        let reference = Test::reference(b"-newer", reference_path.clone(), false).unwrap();
        assert!(
            !reference
                .evaluate(&Entry::new(equal_path, 0, FileKind::File))
                .unwrap()
        );
        assert!(
            reference
                .evaluate(&Entry::new(newer_path.clone(), 0, FileKind::File))
                .unwrap()
        );
        let same = Test::reference(b"-samefile", newer_path.clone(), false).unwrap();
        assert!(
            same.evaluate(&Entry::new(newer_path, 0, FileKind::File))
                .unwrap()
        );
    }

    #[test]
    fn reference_symlinks_are_followed_only_when_requested() {
        let fixture = Fixture::new();
        let target = fixture.0.join("target");
        let link = fixture.0.join("link");
        fs::write(&target, b"").unwrap();
        symlink(&target, &link).unwrap();
        let physical = Test::reference(b"-samefile", link.clone(), false).unwrap();
        let followed = Test::reference(b"-samefile", link, true).unwrap();
        assert!(
            !physical
                .evaluate(&Entry::new(target.clone(), 0, FileKind::File))
                .unwrap()
        );
        assert!(
            followed
                .evaluate(&Entry::new(target, 0, FileKind::File))
                .unwrap()
        );
    }

    #[test]
    fn empty_and_access_tests_use_live_directory_reads_and_access_checks() {
        let fixture = Fixture::new();
        let empty_file = fixture.file("empty", 0, 0);
        assert!(Test::Empty.evaluate(&empty_file).unwrap());
        let directory = fixture.0.join("empty-dir");
        fs::create_dir(&directory).unwrap();
        let empty_dir = Entry::new(directory.clone(), 0, FileKind::Directory);
        assert!(Test::Empty.evaluate(&empty_dir).unwrap());
        fs::write(directory.join("child"), b"x").unwrap();
        assert!(!Test::Empty.evaluate(&empty_dir).unwrap());

        let denied = fixture.0.join("denied");
        fs::create_dir(&denied).unwrap();
        fs::set_permissions(&denied, fs::Permissions::from_mode(0o0)).unwrap();
        let denied_entry = Entry::new(denied.clone(), 0, FileKind::Directory);
        assert!(Test::Empty.evaluate(&denied_entry).is_err());
        assert!(
            !Test::Access(rustix::fs::Access::READ_OK)
                .evaluate(&denied_entry)
                .unwrap()
        );
        fs::set_permissions(&denied, fs::Permissions::from_mode(0o700)).unwrap();
    }

    #[test]
    fn minute_less_than_compares_the_unrounded_age() {
        let fixture = Fixture::new();
        let now = UNIX_EPOCH + Duration::from_secs(2_000_000_000);
        let entry = fixture.file("almost-six", 0, 359);
        let matches = |age: &[u8]| {
            Test::time(b"-mmin", age, now, false)
                .unwrap()
                .evaluate(&entry)
                .unwrap()
        };
        // Bucket 6 by rounding up, yet younger than six minutes.
        assert!(matches(b"6"));
        assert!(matches(b"-6"));
        assert!(!matches(b"-5"));
        assert!(matches(b"+5"));
    }

    #[test]
    fn fractional_days_greater_than_means_a_whole_day_past_the_value() {
        let fixture = Fixture::new();
        let now = UNIX_EPOCH + Duration::from_secs(2_000_000_000);
        let matches = |age_secs, arg: &[u8]| {
            let entry = fixture.file(&format!("age{age_secs}"), 0, age_secs);
            Test::time(b"-mtime", arg, now, false)
                .unwrap()
                .evaluate(&entry)
                .unwrap()
        };
        // GNU: 1.0 and 1.4999 days are not `-mtime +0.5`; 1.5001 days is.
        assert!(!matches(86_400, b"+0.5"));
        assert!(!matches(129_590, b"+0.5"));
        assert!(matches(129_610, b"+0.5"));
        assert!(matches(43_190, b"-0.5"));
        assert!(!matches(43_210, b"-0.5"));
    }

    #[test]
    fn symbolic_perm_clauses_take_several_operators_and_a_leading_operator() {
        assert_eq!(parse_mode(b"+u+x"), Some((0o111, 0)));
        assert_eq!(parse_mode(b"u+x+w"), Some((0o300, 0)));
        assert_eq!(parse_mode(b"u+"), Some((0, 0)));
        assert_eq!(parse_mode(b"u=rwx,g=rx,o=x"), Some((0o751, 0)));
        assert_eq!(parse_mode(b"+066"), None);
        assert_eq!(parse_mode(b"z+x"), None);
    }

    #[test]
    fn cnewer_compares_the_entry_ctime_with_the_reference_mtime() {
        let fixture = Fixture::new();
        // Setting an old mtime moves ctime to now, so the reference is
        // -cnewer than itself and not -newer than itself.
        let path = fixture.0.join("reference");
        fs::write(&path, b"").unwrap();
        fs::File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_times(FileTimes::new().set_modified(UNIX_EPOCH + Duration::from_secs(1_000_000)))
            .unwrap();
        let reference = Entry::new(path, 0, FileKind::File);
        let cnewer = Test::reference(b"-cnewer", reference.path().to_owned(), false).unwrap();
        let newer = Test::reference(b"-newer", reference.path().to_owned(), false).unwrap();
        assert!(cnewer.evaluate(&reference).unwrap());
        assert!(!newer.evaluate(&reference).unwrap());
    }

    #[test]
    fn numeric_identities_need_no_database_entry_but_names_do() {
        assert_eq!(identity(b"-user", OsStr::new("3999999")), Some(3_999_999));
        assert_eq!(identity(b"-group", OsStr::new(" 42")), Some(42));
        assert_eq!(identity(b"-user", OsStr::new("ferret-no-such-user")), None);
    }

    #[test]
    fn all_distinct_corpus_newermt_values_parse() {
        let now = UNIX_EPOCH + Duration::from_secs(1_800_000_000);
        let values = [
            "-1 day",
            "-1 hour",
            "-10 min",
            "-10 minutes",
            "-100 seconds",
            "-12 hours",
            "-120 seconds",
            "-1200 seconds",
            "-14 hours",
            "-15 minutes",
            "-15 seconds",
            "-2 hours",
            "-2 minutes",
            "-20 minutes",
            "-20 seconds",
            "-3 hours",
            "-30 minutes",
            "-300 seconds",
            "-4 minutes",
            "-40 minutes",
            "-45 minutes",
            "-5 minutes",
            "-6 hours",
            "-6 seconds",
            "-60 seconds",
            "1 hour ago",
            "1 minute ago",
            "1 month ago",
            "1 second ago",
            "15 seconds ago",
            "1970-01-01",
            "1971-01-01",
            "2001-01-01",
            "2008-01-01",
            "2010-01-01",
            "2010-06-01",
            "2013-03-01",
            "2020-01-01",
            "2020-01-02",
            "2020-03-03",
            "2020-09-01",
            "2021-02-15 00:00:00",
            "2021-02-16 00:00:00",
            "2022-01-01 00:01",
            "2022-01-31 23:59",
            "2022-10-17T18:00-07:00",
            "2023-06-26 07:27:07",
            "2024-01-01",
            "2024-12-13",
            "2025-01-01",
            "2025-09-01",
            "2025-11-16",
            "2025-11-23",
            "2025-11-26",
            "2026-06-22",
            "2026-07-01",
            "2026-08-26",
            "24 hours ago",
            "3 day ago",
            "30 minutes ago",
            "5 minutes ago",
            "5 seconds ago",
            "6 minutes ago",
            "60 days ago",
            "7 day ago",
            "7 days ago",
            "7 hours ago",
            "@0",
            "@86400",
            "Oct 3 00:00",
            "Oct 4 00:00",
            "mar 03, 2010",
            "mar 03, 2010 09:00",
            "mar 11, 2010",
            "mar 12, 2021 18:50",
            "now",
            "now-1 days",
            "now-7 days",
            "today",
            "yesterday",
        ];
        for value in values {
            assert!(parse_date(OsStr::new(value), now).is_some(), "{value}");
        }
        assert_eq!(values.len(), 80);
        for value in ["nonsense", "2022-13-45"] {
            assert!(parse_date(OsStr::new(value), now).is_none(), "{value}");
        }
    }
}
