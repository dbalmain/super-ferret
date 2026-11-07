//! Local-zone selection and bounded TZif decoding for find's dates and output.
//! The first local-time operation snapshots TZ/TZDIR; ordinary walks never load
//! this OnceLock. No libc globals or process environment are mutated.

use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

pub(super) mod calendar;
mod posix;

use posix::Posix;

const MAX_FILE: u64 = 1_048_576;

#[derive(Debug)]
struct Offset {
    seconds: i32,
    name: Vec<u8>,
    daylight: bool,
}

#[derive(Debug)]
pub(super) struct Zone {
    transitions: Vec<(i64, usize)>,
    offsets: Vec<Offset>,
    future: Option<Posix>,
    leap_seconds: bool,
}

static ZONE: OnceLock<Zone> = OnceLock::new();

#[cfg(test)]
pub(super) fn loaded() -> bool {
    ZONE.get().is_some()
}

pub(super) fn local() -> &'static Zone {
    ZONE.get_or_init(Zone::load)
}

/// Discover the system database without embedding a store-specific path.
pub(super) fn directory() -> PathBuf {
    std::env::var_os("TZDIR").map_or_else(
        || {
            ["/usr/share/zoneinfo", "/etc/zoneinfo", "/usr/lib/zoneinfo"]
                .into_iter()
                .map(PathBuf::from)
                .find(|path| path.is_dir())
                .unwrap_or(PathBuf::from("/usr/share/zoneinfo"))
        },
        PathBuf::from,
    )
}

impl Zone {
    fn utc(name: Vec<u8>) -> Self {
        Self {
            transitions: Vec::new(),
            offsets: vec![Offset {
                seconds: 0,
                name,
                daylight: false,
            }],
            future: None,
            leap_seconds: false,
        }
    }

    fn load() -> Self {
        let Some(tz) = std::env::var_os("TZ") else {
            return Self::file(Path::new("/etc/localtime")).unwrap_or(Self::utc(b"UTC".to_vec()));
        };
        use std::os::unix::ffi::OsStrExt;
        let bytes = tz.as_bytes();
        let bytes = bytes.strip_prefix(b":").unwrap_or(bytes);
        if bytes.is_empty() {
            return Self::utc(b"UTC".to_vec());
        }
        let path = Path::new(std::ffi::OsStr::from_bytes(bytes));
        let path = if path.is_absolute() {
            path.to_owned()
        } else {
            directory().join(path)
        };
        if let Some(zone) = Self::file(&path) {
            return zone;
        }
        if let Some(future) = Posix::parse(bytes) {
            return Self {
                transitions: Vec::new(),
                offsets: Vec::new(),
                future: Some(future),
                leap_seconds: false,
            };
        }
        // glibc preserves the leading alphabetic name when a named zone cannot
        // be loaded, even though its fallback offset is zero.
        let name = bytes
            .iter()
            .copied()
            .take_while(u8::is_ascii_alphabetic)
            .collect();
        Self::utc(name)
    }

    fn file(path: &Path) -> Option<Self> {
        let mut bytes = Vec::new();
        std::fs::File::open(path)
            .ok()?
            .take(MAX_FILE + 1)
            .read_to_end(&mut bytes)
            .ok()?;
        if bytes.len() as u64 > MAX_FILE {
            return None;
        }
        Self::parse(&bytes)
    }

    fn parse(bytes: &[u8]) -> Option<Self> {
        let first = Header::parse(bytes)?;
        let leap_seconds = first.leaps != 0;
        let (header, data, width) = if first.version == 0 {
            (first, bytes.get(44..)?, 4)
        } else {
            let skip = 44usize.checked_add(first.size(4)?)?;
            let header = Header::parse(bytes.get(skip..)?)?;
            if header.version != first.version {
                return None;
            }
            (header, bytes.get(skip.checked_add(44)?..)?, 8)
        };
        let size = header.size(width)?;
        let block = data.get(..size)?;
        let times_end = header.times.checked_mul(width)?;
        let indices_end = times_end.checked_add(header.times)?;
        let types_end = indices_end.checked_add(header.types.checked_mul(6)?)?;
        let names = block.get(types_end..types_end.checked_add(header.chars)?)?;
        let mut offsets = Vec::with_capacity(header.types);
        for record in block.get(indices_end..types_end)?.chunks_exact(6) {
            let seconds = i32::from_be_bytes(record[..4].try_into().ok()?);
            if seconds == i32::MIN || record[4] > 1 {
                return None;
            }
            let name = names.get(usize::from(record[5])..)?;
            let end = name.iter().position(|b| *b == 0)?;
            offsets.push(Offset {
                seconds,
                name: name[..end].to_vec(),
                daylight: record[4] == 1,
            });
        }
        let mut transitions = Vec::with_capacity(header.times);
        for (time, index) in block[..times_end]
            .chunks_exact(width)
            .zip(&block[times_end..indices_end])
        {
            let stamp = if width == 8 {
                i64::from_be_bytes(time.try_into().ok()?)
            } else {
                i64::from(i32::from_be_bytes(time.try_into().ok()?))
            };
            let index = usize::from(*index);
            if index >= offsets.len()
                || transitions
                    .last()
                    .is_some_and(|(previous, _)| *previous >= stamp)
            {
                return None;
            }
            transitions.push((stamp, index));
        }
        let tail = data.get(size..)?;
        let future = if width == 8 {
            let text = tail.strip_prefix(b"\n")?.strip_suffix(b"\n")?;
            if text.is_empty() {
                None
            } else {
                Some(Posix::parse(text)?)
            }
        } else {
            None
        };
        Some(Self {
            transitions,
            offsets,
            future,
            leap_seconds: leap_seconds || header.leaps != 0,
        })
    }

    /// Leap-bearing files retain an unsupported state instead of falling back
    /// to UTC. Every local-time plan must reject that state before execution.
    pub fn check_supported(&self) -> io::Result<()> {
        if self.leap_seconds {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "leap-second TZif zones are unsupported; choose a zone outside right/",
            ));
        }
        Ok(())
    }

    fn at(&self, stamp: i64) -> &Offset {
        if self
            .transitions
            .last()
            .is_none_or(|(last, _)| stamp > *last)
            && let Some(future) = &self.future
        {
            return future.at(stamp);
        }
        let count = self.transitions.partition_point(|(time, _)| *time <= stamp);
        let index = if count == 0 {
            // A DST type zero is not used for timestamps before the table.
            self.offsets
                .iter()
                .position(|offset| !offset.daylight)
                .unwrap_or(0)
        } else {
            self.transitions[count - 1].1
        };
        &self.offsets[index]
    }

    pub fn offset(&self, stamp: i64) -> i32 {
        self.at(stamp).seconds
    }

    pub fn abbreviation(&self, stamp: i64) -> &[u8] {
        &self.at(stamp).name
    }

    pub fn wall(&self, stamp: i64) -> i64 {
        stamp.saturating_add(i64::from(self.offset(stamp)))
    }

    /// Resolve a wall clock, rejecting gaps. Like glibc mktime, date arguments
    /// retain the last successful offset as the next overlap's initial guess.
    pub fn resolve(&self, wall: i64, guess: &mut i32) -> Option<i64> {
        let preferred = self.at(wall.saturating_sub(i64::from(*guess)));
        let offsets = std::iter::once(preferred)
            .chain(&self.offsets)
            .chain(self.future.iter().flat_map(Posix::offsets));
        let stamp = offsets
            .filter_map(|offset| wall.checked_sub(i64::from(offset.seconds)))
            .find(|stamp| self.wall(*stamp) == wall)?;
        *guess = self.offset(stamp);
        Some(stamp)
    }

    /// GNU's day boundary may lie outside the i64 Unix-second range.
    pub fn day_end(&self, stamp: i64) -> i128 {
        let offset = i128::from(self.offset(stamp));
        let midnight = (i128::from(stamp) + offset).div_euclid(86400) * 86400;
        // GNU retains the run-start DST side when establishing daystart,
        // including on a transition day; resolving midnight afresh differs.
        midnight - offset + 86400
    }
}

struct Header {
    version: u8,
    utc: usize,
    standard: usize,
    leaps: usize,
    times: usize,
    types: usize,
    chars: usize,
}
impl Header {
    fn parse(bytes: &[u8]) -> Option<Self> {
        let bytes = bytes.get(..44)?;
        if &bytes[..4] != b"TZif" || ![0, b'2', b'3'].contains(&bytes[4]) {
            return None;
        }
        let counts: Vec<_> = bytes[20..]
            .chunks_exact(4)
            .map(|b| u32::from_be_bytes([b[0], b[1], b[2], b[3]]) as usize)
            .collect();
        let header = Self {
            version: bytes[4],
            utc: counts[0],
            standard: counts[1],
            leaps: counts[2],
            times: counts[3],
            types: counts[4],
            chars: counts[5],
        };
        if !(1..=256).contains(&header.types)
            || header.chars == 0
            || header.chars > 4096
            || header.times > 100_000
            || header.leaps > 100_000
            || ![0, header.types].contains(&header.utc)
            || ![0, header.types].contains(&header.standard)
        {
            return None;
        }
        Some(header)
    }

    fn size(&self, width: usize) -> Option<usize> {
        self.times
            .checked_mul(width + 1)?
            .checked_add(self.types.checked_mul(6)?)?
            .checked_add(self.chars)?
            .checked_add(self.leaps.checked_mul(width + 4)?)?
            .checked_add(self.standard)?
            .checked_add(self.utc)
    }
}

#[cfg(test)]
mod tests;
