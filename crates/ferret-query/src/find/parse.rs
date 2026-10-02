//! Recursive descent in GNU precedence order: comma, OR, AND, negation.
//! Global options affect traversal even in a branch that never evaluates.

use std::ffi::{OsStr, OsString};
use std::fmt;
use std::os::unix::ffi::OsStrExt;
use std::path::PathBuf;

use super::{Expression, FileKind, Options};

/// A find command; unsupported features are retained after full parsing.
#[derive(Debug)]
pub struct Plan {
    pub(super) expression: Expression,
    pub(super) paths: Vec<PathBuf>,
    pub(super) options: Options,
    pub(super) no_ignore: bool,
    pub(super) unsupported: Option<OsString>,
}

/// Invalid find syntax. The host maps every variant to exit status 1.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ParseError {
    /// A predicate GNU does not recognize.
    Unknown(OsString),
    /// A primary or leading option lacks its operand.
    Missing(OsString),
    /// A recognized primary has an invalid operand.
    Invalid { primary: OsString, value: OsString },
    /// An operator has no operand, parentheses are empty or unbalanced, or a
    /// path follows the beginning of the expression.
    Expression(Option<OsString>),
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unknown(primary) => write!(f, "unknown predicate {}", primary.to_string_lossy()),
            Self::Missing(primary) => {
                write!(f, "missing argument to {}", primary.to_string_lossy())
            }
            Self::Invalid { primary, value } => write!(
                f,
                "invalid argument {} to {}",
                value.to_string_lossy(),
                primary.to_string_lossy()
            ),
            Self::Expression(token) => write!(f, "invalid expression near {token:?}"),
        }
    }
}

impl std::error::Error for ParseError {}

pub(super) fn parse(args: &[OsString]) -> Result<Plan, ParseError> {
    let mut parser = Parser {
        args,
        at: 0,
        options: Options::default(),
        action: false,
        unsupported: None,
    };
    let mut no_ignore = false;
    let mut follow = None;
    while let Some(arg) = parser.peek() {
        match arg {
            b"-I" | b"--no-ignore" => no_ignore = true,
            b"-P" => follow = None,
            b"-H" | b"-L" => follow = Some(args[parser.at].clone()),
            b"--" => {}
            b"-D" => {
                parser.at += 1;
                parser.argument(OsStr::new("-D"))?;
                parser.unsupported(OsStr::new("-D"));
                continue;
            }
            bytes if bytes.starts_with(b"-O") => {
                if decimal(&bytes[2..]).is_none() {
                    return Err(ParseError::Invalid {
                        primary: "-O".into(),
                        value: args[parser.at].clone(),
                    });
                }
            }
            _ => break,
        }
        parser.at += 1;
    }
    if parser.unsupported.is_none() {
        parser.unsupported = follow;
    }
    let mut paths = Vec::new();
    while let Some(bytes) = parser.peek() {
        if bytes.starts_with(b"-") || bytes == b"(" || bytes == b"!" {
            break;
        }
        paths.push(PathBuf::from(&args[parser.at]));
        parser.at += 1;
    }
    if paths.is_empty() {
        paths.push(PathBuf::from("."));
    }
    let mut expression = if parser.at == args.len() {
        Expression::Constant(true)
    } else {
        parser.comma()?
    };
    if parser.at != args.len() {
        return Err(ParseError::Expression(Some(args[parser.at].clone())));
    }
    // The implicit action wraps the entire expression, including disjunctions.
    // An explicit action anywhere suppresses it; prune and quit do not.
    if !parser.action {
        expression = Expression::And(Box::new(expression), Box::new(Expression::Print(false)));
    }
    Ok(Plan {
        expression,
        paths,
        options: parser.options,
        no_ignore,
        unsupported: parser.unsupported,
    })
}

struct Parser<'a> {
    args: &'a [OsString],
    at: usize,
    options: Options,
    action: bool,
    unsupported: Option<OsString>,
}

impl Parser<'_> {
    fn peek(&self) -> Option<&[u8]> {
        self.args.get(self.at).map(|arg| arg.as_bytes())
    }

    fn consume(&mut self, values: &[&[u8]]) -> bool {
        if self.peek().is_some_and(|next| values.contains(&next)) {
            self.at += 1;
            true
        } else {
            false
        }
    }

    fn comma(&mut self) -> Result<Expression, ParseError> {
        let mut left = self.or()?;
        while self.consume(&[b","]) {
            left = Expression::Comma(Box::new(left), Box::new(self.or()?));
        }
        Ok(left)
    }

    fn or(&mut self) -> Result<Expression, ParseError> {
        let mut left = self.and()?;
        while self.consume(&[b"-o", b"-or"]) {
            left = Expression::Or(Box::new(left), Box::new(self.and()?));
        }
        Ok(left)
    }

    fn and(&mut self) -> Result<Expression, ParseError> {
        let mut left = self.unary()?;
        loop {
            if self.consume(&[b"-a", b"-and"]) {
                left = Expression::And(Box::new(left), Box::new(self.unary()?));
            } else if self
                .peek()
                .is_none_or(|arg| [b")".as_slice(), b",", b"-o", b"-or"].contains(&arg))
            {
                break;
            } else {
                left = Expression::And(Box::new(left), Box::new(self.unary()?));
            }
        }
        Ok(left)
    }

    fn unary(&mut self) -> Result<Expression, ParseError> {
        if self.consume(&[b"!", b"-not"]) {
            return Ok(Expression::Not(Box::new(self.unary()?)));
        }
        if self.consume(&[b"("]) {
            let inner = self.comma()?;
            if !self.consume(&[b")"]) {
                return Err(ParseError::Expression(self.args.get(self.at).cloned()));
            }
            return Ok(inner);
        }
        let primary = self
            .args
            .get(self.at)
            .ok_or(ParseError::Expression(None))?
            .clone();
        self.at += 1;
        let constant = Expression::Constant(true);
        let expression = match primary.as_bytes() {
            b"-name" | b"-iname" | b"-path" | b"-ipath" | b"-wholename" | b"-iwholename" => {
                let pattern = self.argument(&primary)?.as_bytes().to_vec();
                let fold = primary.as_bytes()[1] == b'i';
                if primary == "-name" || primary == "-iname" {
                    Expression::Name(super::glob::Pattern::new(&pattern, fold))
                } else {
                    Expression::Path(super::glob::Pattern::new(&pattern, fold))
                }
            }
            b"-type" => {
                let value = self.argument(&primary)?;
                Expression::Type(kinds(&primary, value)?)
            }
            b"-maxdepth" | b"-mindepth" => {
                let value = self.argument(&primary)?;
                let number = decimal(value.as_bytes())
                    .filter(|&n| n <= i32::MAX as usize)
                    .ok_or_else(|| invalid(&primary, value))?;
                if primary == "-maxdepth" {
                    self.options.max_depth = Some(number);
                } else {
                    self.options.min_depth = number;
                }
                constant
            }
            b"-depth" => {
                self.options.depth_first = true;
                constant
            }
            b"-xdev" | b"-mount" => {
                self.options.xdev = true;
                constant
            }
            b"-true" => constant,
            b"-false" => Expression::Constant(false),
            b"-prune" => Expression::Prune,
            b"-quit" => Expression::Quit,
            b"-print" | b"-print0" => {
                self.action = true;
                Expression::Print(primary == "-print0")
            }
            b"-exec" | b"-execdir" | b"-ok" | b"-okdir" => {
                self.exec(&primary)?;
                self.action = true;
                self.unsupported(&primary)
            }
            b"-fprintf" => {
                self.argument(&primary)?;
                self.argument(&primary)?;
                self.action = true;
                self.unsupported(&primary)
            }
            b"-fprint" | b"-fprint0" | b"-fls" | b"-printf" => {
                self.argument(&primary)?;
                self.action = true;
                self.unsupported(&primary)
            }
            b"-ls" | b"-delete" => {
                self.action = true;
                self.unsupported(&primary)
            }
            b"-follow"
            | b"-daystart"
            | b"-noleaf"
            | b"-ignore_readdir_race"
            | b"-noignore_readdir_race"
            | b"-warn"
            | b"-nowarn"
            | b"-empty"
            | b"-readable"
            | b"-writable"
            | b"-executable"
            | b"-nouser"
            | b"-nogroup"
            | b"-help"
            | b"--help"
            | b"-version"
            | b"--version" => self.unsupported(&primary),
            b"-lname" | b"-ilname" | b"-regex" | b"-iregex" | b"-fstype" | b"-context"
            | b"-user" | b"-group" | b"-newer" | b"-anewer" | b"-cnewer" | b"-samefile"
            | b"-files0-from" => {
                self.argument(&primary)?;
                self.unsupported(&primary)
            }
            b"-regextype" => {
                let value = self.argument(&primary)?;
                if ![
                    b"findutils-default".as_slice(),
                    b"awk",
                    b"ed",
                    b"egrep",
                    b"emacs",
                    b"gnu-awk",
                    b"grep",
                    b"posix-awk",
                    b"posix-basic",
                    b"posix-egrep",
                    b"posix-extended",
                    b"posix-minimal-basic",
                    b"sed",
                ]
                .contains(&value.as_bytes())
                {
                    return Err(invalid(&primary, value));
                }
                self.unsupported(&primary)
            }
            b"-xtype" => {
                kinds(&primary, self.argument(&primary)?)?;
                self.unsupported(&primary)
            }
            b"-perm" => {
                let value = self.argument(&primary)?;
                if !valid_mode(value.as_bytes()) {
                    return Err(invalid(&primary, value));
                }
                self.unsupported(&primary)
            }
            b"-size" => {
                let value = self.argument(&primary)?;
                let mut bytes = value.as_bytes();
                if bytes.last().is_some_and(|b| b"bcwkMG".contains(b)) {
                    bytes = &bytes[..bytes.len() - 1];
                }
                if !comparison_number(bytes, false) {
                    return Err(invalid(&primary, value));
                }
                self.unsupported(&primary)
            }
            b"-mtime" | b"-atime" | b"-ctime" | b"-mmin" | b"-amin" | b"-cmin" | b"-used"
            | b"-links" | b"-inum" | b"-uid" | b"-gid" => {
                let value = self.argument(&primary)?;
                let fractional = matches!(
                    primary.as_bytes(),
                    b"-mtime" | b"-atime" | b"-ctime" | b"-mmin" | b"-amin" | b"-cmin" | b"-used"
                );
                if !comparison_number(value.as_bytes(), fractional) {
                    return Err(invalid(&primary, value));
                }
                self.unsupported(&primary)
            }
            bytes if bytes.starts_with(b"-newer") && bytes.len() == 8 => {
                if !b"aBcm".contains(&bytes[6]) || !b"aBcmt".contains(&bytes[7]) {
                    return Err(ParseError::Unknown(primary));
                }
                self.argument(&primary)?;
                self.unsupported(&primary)
            }
            b")" | b"," | b"-o" | b"-or" | b"-a" | b"-and" => {
                return Err(ParseError::Expression(Some(primary)));
            }
            bytes if !bytes.starts_with(b"-") => return Err(ParseError::Expression(Some(primary))),
            _ => return Err(ParseError::Unknown(primary)),
        };
        Ok(expression)
    }

    fn argument(&mut self, primary: &OsStr) -> Result<&OsStr, ParseError> {
        let value = self
            .args
            .get(self.at)
            .ok_or_else(|| ParseError::Missing(primary.to_owned()))?;
        self.at += 1;
        Ok(value)
    }

    fn unsupported(&mut self, primary: &OsStr) -> Expression {
        if self.unsupported.is_none() {
            self.unsupported = Some(primary.to_owned());
        }
        Expression::Constant(true)
    }

    fn exec(&mut self, primary: &OsStr) -> Result<(), ParseError> {
        let start = self.at;
        while let Some(arg) = self.args.get(self.at) {
            if arg == ";" {
                if self.at == start {
                    return Err(ParseError::Missing(primary.to_owned()));
                }
                self.at += 1;
                return Ok(());
            }
            if arg == "+" && self.at > start && self.args[self.at - 1] == "{}" {
                let count = self.args[start..self.at]
                    .iter()
                    .filter(|arg| arg.as_bytes().windows(2).any(|pair| pair == b"{}"))
                    .count();
                if count != 1 || primary == "-ok" || primary == "-okdir" {
                    return Err(invalid(primary, arg));
                }
                self.at += 1;
                return Ok(());
            }
            self.at += 1;
        }
        Err(ParseError::Missing(primary.to_owned()))
    }
}

fn invalid(primary: &OsStr, value: &OsStr) -> ParseError {
    ParseError::Invalid {
        primary: primary.to_owned(),
        value: value.to_owned(),
    }
}

fn decimal(bytes: &[u8]) -> Option<usize> {
    if bytes.is_empty() || !bytes.iter().all(u8::is_ascii_digit) {
        return None;
    }
    std::str::from_utf8(bytes).ok()?.parse().ok()
}

fn comparison_number(bytes: &[u8], fractional: bool) -> bool {
    let bytes = bytes
        .strip_prefix(b"+")
        .or_else(|| bytes.strip_prefix(b"-"))
        .unwrap_or(bytes);
    if fractional {
        std::str::from_utf8(bytes)
            .ok()
            .and_then(|s| s.parse::<f64>().ok())
            .is_some_and(|n| n.is_finite() && n >= 0.0)
    } else {
        decimal(bytes).is_some()
    }
}

fn kinds(primary: &OsStr, value: &OsStr) -> Result<Vec<FileKind>, ParseError> {
    let mut kinds = Vec::new();
    for part in value.as_bytes().split(|&b| b == b',') {
        let kind = match part {
            b"f" => FileKind::File,
            b"d" => FileKind::Directory,
            b"l" => FileKind::Symlink,
            b"p" => FileKind::Fifo,
            b"s" => FileKind::Socket,
            b"b" => FileKind::Block,
            b"c" => FileKind::Character,
            _ => return Err(invalid(primary, value)),
        };
        if kinds.contains(&kind) {
            return Err(invalid(primary, value));
        }
        kinds.push(kind);
    }
    Ok(kinds)
}

fn valid_mode(mut bytes: &[u8]) -> bool {
    if bytes.first().is_some_and(|b| b"-/".contains(b)) {
        bytes = &bytes[1..];
    }
    if !bytes.is_empty() && bytes.iter().all(|b| (b'0'..=b'7').contains(b)) {
        return u32::from_str_radix(std::str::from_utf8(bytes).unwrap_or(""), 8)
            .is_ok_and(|n| n <= 0o7777);
    }
    bytes.split(|&b| b == b',').all(|mut clause| {
        while clause.first().is_some_and(|b| b"ugoa".contains(b)) {
            clause = &clause[1..];
        }
        let mut operations = 0;
        while clause.first().is_some_and(|b| b"+-=".contains(b)) {
            operations += 1;
            clause = &clause[1..];
            if clause.first().is_some_and(|b| b"ugo".contains(b)) {
                clause = &clause[1..];
                continue;
            }
            while clause.first().is_some_and(|b| b"rwxXst".contains(b)) {
                clause = &clause[1..];
            }
        }
        operations > 0 && clause.is_empty()
    })
}
