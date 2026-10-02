//! Parse-time compiled GNU printf directives and C-locale UTC formatting.
//! Rendering reuses scratch storage; metadata comes solely through Entry.

use std::collections::HashMap;
use std::fs;
use std::io::{self, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::sync::OnceLock;
use std::time::{SystemTime, UNIX_EPOCH};

use super::{Entry, FileKind, walk};

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Format(Vec<Directive>);

#[derive(Clone, Debug, PartialEq, Eq)]
enum Directive {
    Literal(Vec<u8>),
    Field {
        code: u8,
        time: Option<u8>,
        width: usize,
        precision: Option<usize>,
        left: bool,
        zero: bool,
        alternate: bool,
    },
}

impl Format {
    pub fn path(nul: bool) -> Self {
        Self(vec![
            Directive::Field {
                code: b'p',
                time: None,
                width: 0,
                precision: None,
                left: false,
                zero: false,
                alternate: false,
            },
            Directive::Literal(vec![if nul { 0 } else { b'\n' }]),
        ])
    }

    pub fn compile(bytes: &[u8], warnings: &mut Vec<String>) -> Result<Self, String> {
        let mut directives = Vec::new();
        let mut literal = Vec::new();
        let mut at = 0;
        while let Some(&byte) = bytes.get(at) {
            at += 1;
            match byte {
                b'\\' => {
                    let Some(&escaped) = bytes.get(at) else {
                        warnings.push("escape followed by nothing at all".into());
                        literal.push(b'\\');
                        break;
                    };
                    at += 1;
                    let value = match escaped {
                        b'a' => 7,
                        b'b' => 8,
                        b'f' => 12,
                        b'n' => 10,
                        b'r' => 13,
                        b't' => 9,
                        b'v' => 11,
                        b'\\' => b'\\',
                        b'c' => break,
                        b'0'..=b'7' => {
                            let mut value = u16::from(escaped - b'0');
                            for _ in 0..2 {
                                if let Some(&digit @ b'0'..=b'7') = bytes.get(at) {
                                    value = value * 8 + u16::from(digit - b'0');
                                    at += 1;
                                } else {
                                    break;
                                }
                            }
                            value as u8
                        }
                        _ => {
                            warnings.push(format!("unrecognized escape \\{}", char::from(escaped)));
                            literal.push(b'\\');
                            escaped
                        }
                    };
                    literal.push(value);
                }
                b'%' => {
                    let start = at;
                    let mut left = false;
                    let mut zero = false;
                    let mut alternate = false;
                    while let Some(&flag) = bytes.get(at).filter(|b| b"-+ #0".contains(b)) {
                        left |= flag == b'-';
                        zero |= flag == b'0';
                        alternate |= flag == b'#';
                        at += 1;
                    }
                    let width = number(bytes, &mut at)?;
                    let precision = if bytes.get(at) == Some(&b'.') {
                        at += 1;
                        Some(number(bytes, &mut at)?)
                    } else {
                        None
                    };
                    let code = *bytes
                        .get(at)
                        .ok_or_else(|| "% at end of format string".to_owned())?;
                    at += 1;
                    if code == b'%' {
                        literal.push(b'%');
                        continue;
                    }
                    let time = if b"ABCT".contains(&code) {
                        let value = *bytes
                            .get(at)
                            .ok_or_else(|| "missing time directive".to_owned())?;
                        at += 1;
                        Some(value)
                    } else {
                        None
                    };
                    if !b"ABCTabcdfFgGhHiklmMnpPstSuUyYDZ".contains(&code) {
                        warnings.push(format!(
                            "unrecognized format directive %{}",
                            char::from(code)
                        ));
                        literal.push(b'%');
                        literal.extend_from_slice(&bytes[start..at]);
                        continue;
                    }
                    if !literal.is_empty() {
                        directives.push(Directive::Literal(std::mem::take(&mut literal)));
                    }
                    directives.push(Directive::Field {
                        code,
                        time,
                        width,
                        precision,
                        left,
                        zero,
                        alternate,
                    });
                }
                _ => literal.push(byte),
            }
        }
        if !literal.is_empty() {
            directives.push(Directive::Literal(literal));
        }
        Ok(Self(directives))
    }

    pub fn render(&self, entry: &Entry, output: &mut Vec<u8>) -> io::Result<()> {
        let mut scratch = Vec::with_capacity(64);
        for directive in &self.0 {
            match directive {
                Directive::Literal(bytes) => output.extend_from_slice(bytes),
                Directive::Field {
                    code,
                    time,
                    width,
                    precision,
                    left,
                    zero,
                    alternate,
                } => {
                    scratch.clear();
                    field(entry, *code, *time, &mut scratch)?;
                    if *code == b'm' && *alternate && !scratch.starts_with(b"0") {
                        scratch.insert(0, b'0');
                    }
                    if let Some(precision) = precision {
                        if *code == b'm' || *code == b'd' {
                            let pad = precision.saturating_sub(scratch.len());
                            output.extend(std::iter::repeat_n(b'0', pad));
                        } else {
                            scratch.truncate(*precision);
                        }
                    }
                    let padding = width.saturating_sub(scratch.len());
                    if !left {
                        output.extend(std::iter::repeat_n(
                            if *zero && b"mdS".contains(code) {
                                b'0'
                            } else {
                                b' '
                            },
                            padding,
                        ));
                    }
                    output.extend_from_slice(&scratch);
                    if *left {
                        output.extend(std::iter::repeat_n(b' ', padding));
                    }
                }
            }
        }
        Ok(())
    }
}

fn number(bytes: &[u8], at: &mut usize) -> Result<usize, String> {
    let mut n = 0usize;
    while let Some(&digit) = bytes.get(*at).filter(|b| b.is_ascii_digit()) {
        n = n
            .checked_mul(10)
            .and_then(|n| n.checked_add(usize::from(digit - b'0')))
            .filter(|n| *n <= 1_000_000)
            .ok_or_else(|| "format width too large".to_owned())?;
        *at += 1;
    }
    Ok(n)
}

fn field(entry: &Entry, code: u8, time: Option<u8>, out: &mut Vec<u8>) -> io::Result<()> {
    let path = entry.path().as_os_str().as_bytes();
    match code {
        b'p' => out.extend_from_slice(path),
        b'P' => {
            if entry.depth() != 0 {
                out.extend_from_slice(
                    path.strip_prefix(entry.root().as_os_str().as_bytes())
                        .unwrap_or(path)
                        .strip_prefix(b"/")
                        .unwrap_or_else(|| {
                            path.strip_prefix(entry.root().as_os_str().as_bytes())
                                .unwrap_or(path)
                        }),
                );
            }
        }
        b'H' => out.extend_from_slice(entry.root().as_os_str().as_bytes()),
        b'f' => {
            if path.ends_with(b"/") {
                let end = path.iter().rposition(|&b| b != b'/').map_or(0, |i| i + 1);
                let start = path[..end]
                    .iter()
                    .rposition(|&b| b == b'/')
                    .map_or(0, |i| i + 1);
                out.extend_from_slice(&path[start..(end + 1).min(path.len())]);
            } else {
                out.extend_from_slice(entry.name());
            }
        }
        b'h' => match path.iter().rposition(|&b| b == b'/') {
            Some(at) => out.extend_from_slice(&path[..at]),
            None => out.push(b'.'),
        },
        b'd' => write!(out, "{}", entry.depth())?,
        b'y' => out.push(kind_letter(entry.kind().map_err(walk::copy_error)?)),
        b'Y' => {
            let kind = match fs::metadata(entry.path()) {
                Ok(stat) => kind_letter(walk::kind(stat.file_type())),
                Err(error) if error.raw_os_error() == Some(40) => b'L',
                Err(error) if error.kind() == io::ErrorKind::NotFound => b'N',
                Err(_) => b'?',
            };
            out.push(kind);
        }
        b'l' => {
            if entry.kind().map_err(walk::copy_error)? == FileKind::Symlink {
                out.extend_from_slice(fs::read_link(entry.path())?.as_os_str().as_bytes());
            }
        }
        b'F' => out.extend_from_slice(filesystem(entry.path())?.as_bytes()),
        b'Z' => {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "security contexts are not supported",
            ));
        }
        _ => {
            let stat = entry.metadata().map_err(walk::copy_error)?;
            match code {
                b's' => write!(out, "{}", stat.len())?,
                b'i' => write!(out, "{}", stat.ino())?,
                b'D' => write!(out, "{}", stat.dev())?,
                b'n' => write!(out, "{}", stat.nlink())?,
                b'm' => write!(out, "{:o}", stat.mode() & 0o7777)?,
                b'M' => out.extend_from_slice(&mode(
                    stat.mode(),
                    kind_letter(walk::kind(stat.file_type())),
                )),
                b'U' => write!(out, "{}", stat.uid())?,
                b'G' => write!(out, "{}", stat.gid())?,
                b'u' => out.extend_from_slice(owner(stat.uid(), false).as_bytes()),
                b'g' => out.extend_from_slice(owner(stat.gid(), true).as_bytes()),
                b'b' => write!(out, "{}", stat.blocks())?,
                b'k' => write!(out, "{}", stat.blocks().div_ceil(2))?,
                b'S' => {
                    let ratio = if stat.len() == 0 {
                        1.0
                    } else {
                        stat.blocks() as f64 * 512.0 / stat.len() as f64
                    };
                    general(ratio, out)?;
                }
                b'A' | b'a' => timestamp(stat.atime(), stat.atime_nsec(), time, out)?,
                b'C' | b'c' => timestamp(stat.ctime(), stat.ctime_nsec(), time, out)?,
                b'T' | b't' => timestamp(stat.mtime(), stat.mtime_nsec(), time, out)?,
                b'B' => {
                    if time == Some(b'@') {
                        out.extend_from_slice(b"-1.-000000010");
                    }
                }
                _ => {}
            }
        }
    }
    Ok(())
}

pub(super) fn kind_letter(kind: FileKind) -> u8 {
    match kind {
        FileKind::File => b'f',
        FileKind::Directory => b'd',
        FileKind::Symlink => b'l',
        FileKind::Fifo => b'p',
        FileKind::Socket => b's',
        FileKind::Block => b'b',
        FileKind::Character => b'c',
    }
}

fn general(value: f64, out: &mut Vec<u8>) -> io::Result<()> {
    let exponent = if value == 0.0 {
        0
    } else {
        value.abs().log10().floor() as i32
    };
    if !(-4..6).contains(&exponent) {
        let text = format!("{value:.5e}");
        let (mantissa, exponent) = text.split_once('e').unwrap_or((&text, "0"));
        let exponent = exponent.parse::<i32>().unwrap_or(0);
        write!(
            out,
            "{}e{exponent:+03}",
            mantissa.trim_end_matches('0').trim_end_matches('.')
        )
    } else {
        let text = format!("{:.*}", (5 - exponent).max(0) as usize, value);
        out.extend_from_slice(if text.contains('.') {
            text.trim_end_matches('0').trim_end_matches('.').as_bytes()
        } else {
            text.as_bytes()
        });
        Ok(())
    }
}

fn mode(value: u32, kind: u8) -> [u8; 10] {
    let mut result = [b'-'; 10];
    result[0] = if kind == b'f' { b'-' } else { kind };
    for (at, bit) in (0..9).zip((0..9).rev()) {
        if value & (1 << bit) != 0 {
            result[at + 1] = b"rwx"[at % 3];
        }
    }
    for (at, bit, yes, no) in [
        (3, 0o4000, b's', b'S'),
        (6, 0o2000, b's', b'S'),
        (9, 0o1000, b't', b'T'),
    ] {
        if value & bit != 0 {
            result[at] = if result[at] == b'x' { yes } else { no };
        }
    }
    result
}

fn owner(id: u32, group: bool) -> String {
    static USERS: OnceLock<HashMap<u32, String>> = OnceLock::new();
    static GROUPS: OnceLock<HashMap<u32, String>> = OnceLock::new();
    let names = if group { &GROUPS } else { &USERS }.get_or_init(|| {
        fs::read_to_string(if group { "/etc/group" } else { "/etc/passwd" })
            .unwrap_or_default()
            .lines()
            .filter_map(|line| {
                let mut fields = line.split(':');
                let name = fields.next()?;
                fields.next()?;
                Some((fields.next()?.parse().ok()?, name.to_owned()))
            })
            .collect()
    });
    names.get(&id).cloned().unwrap_or_else(|| id.to_string())
}

pub(super) fn filesystem(path: &std::path::Path) -> io::Result<String> {
    let stat = rustix::fs::statfs(path).or_else(|_| {
        rustix::fs::statfs(
            path.parent()
                .filter(|p| !p.as_os_str().is_empty())
                .unwrap_or(std::path::Path::new(".")),
        )
    })?;
    let magic = stat.f_type as u64;
    Ok(match magic {
        0xef53 => "ext4",
        0x01021994 => "tmpfs",
        0x794c7630 => "overlay",
        0x6969 => "nfs",
        0x9fa0 => "proc",
        0x62656572 => "sysfs",
        0x9123683e => "btrfs",
        0x58465342 => "xfs",
        0x65735546 => "fuse",
        0x858458f6 => "ramfs",
        0x1cd1 => "devpts",
        _ => return Ok(format!("UNKNOWN ({magic:x})")),
    }
    .to_owned())
}

// Gregorian civil date from Unix days, with Euclidean division for pre-epoch
// timestamps. No libc dependency or timezone-global mutation is required.
struct Date {
    year: i64,
    month: i64,
    day: i64,
    hour: i64,
    minute: i64,
    second: i64,
    weekday: usize,
    ordinal: i64,
}
impl Date {
    fn new(seconds: i64) -> Self {
        let days = seconds.div_euclid(86400);
        let remainder = seconds.rem_euclid(86400);
        let z = days + 719468;
        let era = z.div_euclid(146097);
        let doe = z - era * 146097;
        let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
        let mut year = yoe + era * 400;
        let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
        let mp = (5 * doy + 2) / 153;
        let day = doy - (153 * mp + 2) / 5 + 1;
        let month = mp + if mp < 10 { 3 } else { -9 };
        year += i64::from(month <= 2);
        let ordinal = [0, 31, 59, 90, 120, 151, 181, 212, 243, 273, 304, 334][(month - 1) as usize]
            + day
            + i64::from(month > 2 && (year % 4 == 0 && (year % 100 != 0 || year % 400 == 0)));
        Self {
            year,
            month,
            day,
            hour: remainder / 3600,
            minute: remainder / 60 % 60,
            second: remainder % 60,
            weekday: (days + 4).rem_euclid(7) as usize,
            ordinal,
        }
    }
}
const MONTHS: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];
const WEEKDAYS: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];

fn timestamp(seconds: i64, nanos: i64, directive: Option<u8>, out: &mut Vec<u8>) -> io::Result<()> {
    let d = Date::new(seconds);
    let month = MONTHS[(d.month - 1) as usize];
    let weekday = WEEKDAYS[d.weekday];
    match directive {
        None => write!(
            out,
            "{weekday} {month} {:2} {:02}:{:02}:{:02}.{nanos:09}0 {}",
            d.day, d.hour, d.minute, d.second, d.year
        )?,
        Some(b'@') => write!(out, "{seconds}.{nanos:09}0")?,
        Some(b'+') => write!(
            out,
            "{:04}-{:02}-{:02}+{:02}:{:02}:{:02}.{nanos:09}0",
            d.year, d.month, d.day, d.hour, d.minute, d.second
        )?,
        Some(b'S') => write!(out, "{:02}.{nanos:09}0", d.second)?,
        Some(b'T' | b'X') => write!(
            out,
            "{:02}:{:02}:{:02}.{nanos:09}0",
            d.hour, d.minute, d.second
        )?,
        Some(b'c') => write!(
            out,
            "{weekday} {month} {:2} {:02}:{:02}:{:02} {}",
            d.day, d.hour, d.minute, d.second, d.year
        )?,
        Some(b'a') => out.extend_from_slice(weekday.as_bytes()),
        Some(b'A') => out.extend_from_slice(
            [
                "Sunday",
                "Monday",
                "Tuesday",
                "Wednesday",
                "Thursday",
                "Friday",
                "Saturday",
            ][d.weekday]
                .as_bytes(),
        ),
        Some(b'b' | b'h') => out.extend_from_slice(month.as_bytes()),
        Some(b'B') => out.extend_from_slice(
            [
                "January",
                "February",
                "March",
                "April",
                "May",
                "June",
                "July",
                "August",
                "September",
                "October",
                "November",
                "December",
            ][(d.month - 1) as usize]
                .as_bytes(),
        ),
        Some(b'C') => write!(out, "{:02}", d.year / 100)?,
        Some(b'd') => write!(out, "{:02}", d.day)?,
        Some(b'e') => write!(out, "{:2}", d.day)?,
        Some(b'F') => write!(out, "{:04}-{:02}-{:02}", d.year, d.month, d.day)?,
        Some(b'D' | b'x') => write!(
            out,
            "{:02}/{:02}/{:02}",
            d.month,
            d.day,
            d.year.rem_euclid(100)
        )?,
        Some(b'H') => write!(out, "{:02}", d.hour)?,
        Some(b'k') => write!(out, "{:2}", d.hour)?,
        Some(b'I') => write!(out, "{:02}", (d.hour + 11) % 12 + 1)?,
        Some(b'l') => write!(out, "{:2}", (d.hour + 11) % 12 + 1)?,
        Some(b'j') => write!(out, "{:03}", d.ordinal)?,
        Some(b'm') => write!(out, "{:02}", d.month)?,
        Some(b'M') => write!(out, "{:02}", d.minute)?,
        Some(b'n') => out.push(b'\n'),
        Some(b't') => out.push(b'\t'),
        Some(b'p') => out.extend_from_slice(if d.hour < 12 { b"AM" } else { b"PM" }),
        Some(b'P') => out.extend_from_slice(if d.hour < 12 { b"am" } else { b"pm" }),
        Some(b'r') => write!(
            out,
            "{:02}:{:02}:{:02} {}",
            (d.hour + 11) % 12 + 1,
            d.minute,
            d.second,
            if d.hour < 12 { "AM" } else { "PM" }
        )?,
        Some(b'R') => write!(out, "{:02}:{:02}", d.hour, d.minute)?,
        Some(b's') => write!(out, "{seconds}")?,
        Some(b'u') => write!(out, "{}", (d.weekday + 6) % 7 + 1)?,
        Some(b'w') => write!(out, "{}", d.weekday)?,
        Some(b'U') => write!(out, "{:02}", (d.ordinal + 6 - d.weekday as i64) / 7)?,
        Some(b'W') => write!(
            out,
            "{:02}",
            (d.ordinal + 6 - ((d.weekday + 6) % 7) as i64) / 7
        )?,
        Some(b'y') => write!(out, "{:02}", d.year.rem_euclid(100))?,
        Some(b'Y') => write!(out, "{:04}", d.year)?,
        Some(b'z') => out.extend_from_slice(b"+0000"),
        Some(b'Z') => out.extend_from_slice(b"UTC"),
        Some(byte) => {
            out.push(b'%');
            out.push(byte);
        }
    }
    Ok(())
}

pub(super) fn list(entry: &Entry, out: &mut Vec<u8>) -> io::Result<()> {
    let stat = entry.metadata().map_err(walk::copy_error)?;
    let d = Date::new(stat.mtime());
    let mut date = Vec::new();
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64;
    if stat.mtime() > now + 3600 || stat.mtime() < now - 15_778_476 {
        write!(
            &mut date,
            "{} {:2}  {:04}",
            MONTHS[(d.month - 1) as usize],
            d.day,
            d.year
        )?;
    } else {
        write!(
            &mut date,
            "{} {:2} {:02}:{:02}",
            MONTHS[(d.month - 1) as usize],
            d.day,
            d.hour,
            d.minute
        )?;
    }
    let permissions = mode(stat.mode(), kind_letter(walk::kind(stat.file_type())));
    write!(
        out,
        "{:9} {:6} {} {:3} {:8} {:8} {:8} {} ",
        stat.ino(),
        stat.blocks().div_ceil(2),
        String::from_utf8_lossy(&permissions),
        stat.nlink(),
        owner(stat.uid(), false),
        owner(stat.gid(), true),
        stat.len(),
        String::from_utf8_lossy(&date)
    )?;
    escaped(entry.path().as_os_str().as_bytes(), out)?;
    if stat.file_type().is_symlink() {
        out.extend_from_slice(b" -> ");
        escaped(fs::read_link(entry.path())?.as_os_str().as_bytes(), out)?;
    }
    out.push(b'\n');
    Ok(())
}

fn escaped(bytes: &[u8], out: &mut Vec<u8>) -> io::Result<()> {
    for &b in bytes {
        match b {
            b' ' | b'\\' | b'"' => {
                out.push(b'\\');
                out.push(b);
            }
            b'\n' => out.extend_from_slice(b"\\n"),
            b'\t' => out.extend_from_slice(b"\\t"),
            b'\r' => out.extend_from_slice(b"\\r"),
            b'\x08' => out.extend_from_slice(b"\\b"),
            b'\x0c' => out.extend_from_slice(b"\\f"),
            b'\x0b' => out.extend_from_slice(b"\\v"),
            33..=126 => out.push(b),
            _ => write!(out, "\\{b:03o}")?,
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests;
