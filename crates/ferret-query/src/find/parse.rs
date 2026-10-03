//! Recursive descent in GNU precedence order: comma, OR, AND, negation.
//! Global options affect traversal even in a branch that never evaluates.

use std::collections::HashMap;
use std::ffi::{OsStr, OsString};
use std::fmt;
use std::os::unix::ffi::OsStrExt;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use ferret_verify::{Dialect, FindRegex};

use super::action::{Action, Exec, Target};
use super::printf::Format;
use super::{Expression, FileKind, Follow, Options};

/// A find command; unsupported features are retained after full parsing.
#[derive(Debug)]
pub struct Plan {
    pub(super) expression: Expression,
    pub(super) paths: Vec<PathBuf>,
    pub(super) options: Options,
    pub(super) no_ignore: bool,
    pub(super) unsupported: Option<OsString>,
    pub(super) warnings: Vec<String>,
    pub(super) message: Option<String>,
    pub(super) permission_warning: bool,
}

/// Invalid find syntax. The host maps every variant to exit status 1.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ParseError {
    /// A predicate GNU does not recognize.
    Unknown(OsString),
    /// A recognized feature cannot be represented or an output file cannot
    /// open.
    Feature(String),
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
            Self::Feature(message) => f.write_str(message),
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
        warnings: Vec::new(),
        message: None,
        dialect: Dialect::Emacs,
        streams: HashMap::new(),
        exec_id: 0,
        delete: false,
        prune: false,
        explicit_depth: false,
        daystart: false,
        now: std::time::SystemTime::now(),
        permission_warning: false,
        combinators: 0,
        nesting: 0,
    };
    let mut no_ignore = false;
    while let Some(arg) = parser.peek() {
        match arg {
            b"-I" | b"--no-ignore" => no_ignore = true,
            b"-P" => parser.options.follow = Follow::Physical,
            b"-H" => parser.options.follow = Follow::Roots,
            b"-L" => parser.options.follow = Follow::All,
            b"--" => {}
            b"-D" => {
                parser.at += 1;
                let value = parser.argument(OsStr::new("-D"))?.to_owned();
                if value.is_empty() {
                    return Err(invalid(OsStr::new("-D"), &value));
                }
                for flag in value.as_bytes().split(|&b| b == b',') {
                    if flag == b"help" {
                        parser.message = Some(
                            "Debug options: exec opt rates search stat time tree all help\n".into(),
                        );
                    } else if flag != b"exec"
                        && flag != b"stat"
                        && (!value
                            .as_bytes()
                            .split(|&b| b == b',')
                            .any(|part| part == b"help")
                            || ![
                                b"opt".as_slice(),
                                b"rates",
                                b"search",
                                b"time",
                                b"tree",
                                b"all",
                            ]
                            .contains(&flag))
                    {
                        parser
                            .warnings
                            .push(format!("debug flag {}", String::from_utf8_lossy(flag)));
                    }
                }
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
    let mut expression = if parser.at == args.len() || parser.message.is_some() {
        parser.at = args.len();
        Expression::Constant(true)
    } else {
        match parser.comma() {
            _ if parser.message.is_some() => {
                parser.at = args.len();
                Expression::Constant(true)
            }
            result => result?,
        }
    };
    if parser.at != args.len() {
        return Err(ParseError::Expression(Some(args[parser.at].clone())));
    }
    // The implicit action wraps the entire expression, including disjunctions.
    // An explicit action anywhere suppresses it; prune and quit do not.
    if !parser.action {
        expression = Expression::And(Box::new(expression), Box::new(Expression::Print(false)));
    }
    if parser.delete && parser.prune && !parser.explicit_depth {
        return Err(ParseError::Feature(
            "-delete implies -depth; -prune requires an explicit -depth option".into(),
        ));
    }
    parser.options.retain_parent = parser.delete;
    Ok(Plan {
        expression,
        paths,
        options: parser.options,
        no_ignore,
        unsupported: parser.unsupported,
        warnings: parser.warnings,
        message: parser.message,
        permission_warning: parser.permission_warning,
    })
}

struct Parser<'a> {
    args: &'a [OsString],
    at: usize,
    options: Options,
    action: bool,
    unsupported: Option<OsString>,
    warnings: Vec<String>,
    message: Option<String>,
    dialect: Dialect,
    streams: HashMap<PathBuf, super::action::SharedFile>,
    exec_id: usize,
    delete: bool,
    prune: bool,
    explicit_depth: bool,
    daystart: bool,
    now: std::time::SystemTime,
    permission_warning: bool,
    combinators: usize,
    nesting: usize,
}

/// A chain this long would build an AST deep enough to overflow the stack in
/// every recursive pass over it - evaluation foremost, since it runs per
/// entry and can't be rewritten iteratively without an explicit
/// continuation stack for `&&`/`||` short-circuiting. 50,000 `-true`s
/// joined by an implicit `-a` reliably aborted at this depth (#11); this
/// limit sits an order of magnitude below the lowest observed crash
/// (10,000-20,000, debug build, default 8 MiB stack) with headroom for
/// release builds' smaller frames and tighter stacks alike. No real find
/// command line approaches it.
const MAX_COMBINATORS: usize = 2000;
// Parentheses add parser frames even when they add no AST nodes. Negations
// share this budget so alternating parentheses and negations cannot evade it.
const MAX_NESTING: usize = 128;

impl Parser<'_> {
    fn nest(&mut self) -> Result<(), ParseError> {
        self.nesting += 1;
        if self.nesting > MAX_NESTING {
            return Err(ParseError::Feature(format!(
                "expression nesting exceeds {MAX_NESTING} parentheses/negations; split it up"
            )));
        }
        Ok(())
    }

    fn combine(&mut self) -> Result<(), ParseError> {
        self.combinators += 1;
        if self.combinators > MAX_COMBINATORS {
            return Err(ParseError::Feature(format!(
                "expression has more than {MAX_COMBINATORS} -a/-o/-not/, operators; split it up"
            )));
        }
        Ok(())
    }

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
            self.combine()?;
            left = Expression::Comma(Box::new(left), Box::new(self.or()?));
        }
        Ok(left)
    }

    fn or(&mut self) -> Result<Expression, ParseError> {
        let mut left = self.and()?;
        while self.consume(&[b"-o", b"-or"]) {
            self.combine()?;
            left = Expression::Or(Box::new(left), Box::new(self.and()?));
        }
        Ok(left)
    }

    fn and(&mut self) -> Result<Expression, ParseError> {
        let mut left = self.unary()?;
        loop {
            if self.consume(&[b"-a", b"-and"]) {
                self.combine()?;
                left = Expression::And(Box::new(left), Box::new(self.unary()?));
            } else if self
                .peek()
                .is_none_or(|arg| [b")".as_slice(), b",", b"-o", b"-or"].contains(&arg))
            {
                break;
            } else {
                self.combine()?;
                left = Expression::And(Box::new(left), Box::new(self.unary()?));
            }
        }
        Ok(left)
    }

    fn unary(&mut self) -> Result<Expression, ParseError> {
        if self.consume(&[b"!", b"-not"]) {
            self.combine()?;
            self.nest()?;
            let inner = self.unary();
            self.nesting -= 1;
            return inner.map(|inner| Expression::Not(Box::new(inner)));
        }
        if self.consume(&[b"("]) {
            self.nest()?;
            let inner = self.comma();
            self.nesting -= 1;
            let inner = inner?;
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
                    // GNU warns even under -nowarn; a path never ends in '/'.
                    if pattern.ends_with(b"/") {
                        self.warnings.push(format!(
                            "{} {} will not match anything because it ends with /.",
                            primary.to_string_lossy(),
                            String::from_utf8_lossy(&pattern)
                        ));
                    }
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
            // `-d` is GNU's silent BSD spelling of `-depth`.
            b"-depth" | b"-d" => {
                self.explicit_depth = true;
                self.options.depth_first = true;
                constant
            }
            b"-xdev" | b"-mount" => {
                self.options.xdev = true;
                constant
            }
            b"-true" => constant,
            b"-false" => Expression::Constant(false),
            b"-prune" => {
                self.prune = true;
                Expression::Prune
            }
            b"-quit" => Expression::Quit,
            b"-print" | b"-print0" => {
                self.action = true;
                Expression::Print(primary == "-print0")
            }
            b"-exec" | b"-execdir" | b"-ok" | b"-okdir" => {
                let exec = self.exec(&primary)?;
                self.action = true;
                Expression::Action(Action::Exec(exec))
            }
            b"-fprintf" | b"-fprint" | b"-fprint0" | b"-fls" => {
                let target = self.target(&primary)?;
                self.action = true;
                if primary == "-fls" {
                    Expression::Action(Action::List(target))
                } else {
                    let format = if primary == "-fprintf" {
                        self.format(&primary)?
                    } else {
                        Format::path(primary == "-fprint0")
                    };
                    Expression::Action(Action::Output(target, format))
                }
            }
            b"-printf" => {
                let format = self.format(&primary)?;
                self.action = true;
                Expression::Action(Action::Output(Target::Stdout, format))
            }
            b"-ls" => {
                self.action = true;
                Expression::Action(Action::List(Target::Stdout))
            }
            b"-delete" => {
                self.action = true;
                self.delete = true;
                self.options.depth_first = true;
                Expression::Action(Action::Delete)
            }
            b"-follow" => {
                self.options.follow = Follow::All;
                constant
            }
            b"-help" | b"--help" | b"-version" | b"--version" => {
                self.at = self.args.len();
                self.message = Some(if primary.as_bytes().ends_with(b"help") {
                    "Usage: ferret find [-I] [-H|-L|-P] [paths] [expression]\nDefault mode answers from the index, including stored metadata and freshness.\nIt respects ignore rules; read-only starts and siblings may interleave; starts with actions or -quit run in sequence. -I walks live.\nParents precede children; -depth/-delete reverse this; -prune stops descent.\nPasted find ... -delete skips ignored files and still exits 0.\nFailed deletions and traversal errors exit 1.\n".into()
                } else {
                    "ferret find (GNU find compatible syntax)\n".into()
                });
                constant
            }
            b"-regex" | b"-iregex" => {
                let value = self.argument(&primary)?.to_owned();
                let regex = FindRegex::new(value.as_bytes(), self.dialect, primary == "-iregex")
                    .map_err(|error| ParseError::Feature(error.to_string()))?;
                Expression::Test(super::test::Test::Regex(regex))
            }
            b"-lname" | b"-ilname" => {
                let value = self.argument(&primary)?;
                Expression::Test(super::test::Test::Link(super::glob::Pattern::new(
                    value.as_bytes(),
                    primary == "-ilname",
                )))
            }
            b"-daystart" => {
                self.daystart = true;
                constant
            }
            b"-noleaf"
            | b"-ignore_readdir_race"
            | b"-noignore_readdir_race"
            | b"-warn"
            | b"-nowarn" => constant,
            b"-context" | b"-files0-from" => {
                self.argument(&primary)?;
                self.unsupported(&primary)
            }
            b"-regextype" => {
                let value = self.argument(&primary)?.to_owned();
                self.dialect = Dialect::from_name(value.as_bytes())
                    .ok_or_else(|| invalid(&primary, &value))?;
                constant
            }
            b"-xtype" => Expression::Test(super::test::Test::Xtype(kinds(
                &primary,
                self.argument(&primary)?,
            )?)),
            b"-perm" => {
                let value = self.argument(&primary)?;
                if value.as_bytes().starts_with(b"+") || !valid_mode(value.as_bytes()) {
                    return Err(invalid(&primary, value));
                }
                let test = super::test::Test::perm(value.as_bytes())
                    .ok_or_else(|| invalid(&primary, value))?;
                if value.as_bytes().starts_with(b"/")
                    && value.as_bytes()[1..].iter().all(|byte| *byte == b'0')
                {
                    self.permission_warning = true;
                }
                Expression::Test(test)
            }
            b"-size" => {
                let value = self.argument(&primary)?;
                Expression::Test(
                    super::test::Test::size(value.as_bytes())
                        .ok_or_else(|| invalid(&primary, value))?,
                )
            }
            b"-mtime" | b"-atime" | b"-ctime" | b"-mmin" | b"-amin" | b"-cmin" | b"-used"
            | b"-links" | b"-inum" | b"-uid" | b"-gid" => {
                let now = self.now;
                let daystart = self.daystart;
                let value = self.argument(&primary)?;
                let parsed = if matches!(
                    primary.as_bytes(),
                    b"-mtime" | b"-atime" | b"-ctime" | b"-mmin" | b"-amin" | b"-cmin" | b"-used"
                ) {
                    super::test::Test::time(primary.as_bytes(), value.as_bytes(), now, daystart)
                } else {
                    super::test::Test::number(primary.as_bytes(), value.as_bytes())
                };
                Expression::Test(parsed.ok_or_else(|| invalid(&primary, value))?)
            }
            b"-user" | b"-group" => {
                let value = self.argument(&primary)?;
                let id = super::test::identity(primary.as_bytes(), value)
                    .ok_or_else(|| invalid(&primary, value))?;
                Expression::Test(if primary == "-user" {
                    super::test::Test::User(id)
                } else {
                    super::test::Test::Group(id)
                })
            }
            b"-empty" => Expression::Test(super::test::Test::Empty),
            b"-readable" => {
                Expression::Test(super::test::Test::Access(rustix::fs::Access::READ_OK))
            }
            b"-writable" => {
                Expression::Test(super::test::Test::Access(rustix::fs::Access::WRITE_OK))
            }
            b"-executable" => {
                Expression::Test(super::test::Test::Access(rustix::fs::Access::EXEC_OK))
            }
            b"-nouser" => Expression::Test(super::test::Test::NoUser),
            b"-nogroup" => Expression::Test(super::test::Test::NoGroup),
            b"-fstype" => {
                let value = self.argument(&primary)?;
                Expression::Test(super::test::Test::FsType(
                    value.to_string_lossy().into_owned(),
                ))
            }
            b"-samefile" | b"-newer" | b"-anewer" | b"-cnewer" => {
                let follow_references = self.options.follow != Follow::Physical;
                let value = self.argument(&primary)?;
                let test = super::test::Test::reference(
                    primary.as_bytes(),
                    std::path::PathBuf::from(value),
                    follow_references,
                );
                Expression::Test(test.ok_or_else(|| invalid(&primary, value))?)
            }
            b"-newermt" => {
                let now = self.now;
                let value = self.argument(&primary)?;
                let stamp =
                    super::test::parse_date(value, now).ok_or_else(|| invalid(&primary, value))?;
                Expression::Test(super::test::Test::Newer {
                    field: super::test::TimeField::Modify,
                    stamp,
                })
            }
            bytes if bytes.starts_with(b"-newer") && bytes.len() == 8 => {
                if !b"aBcm".contains(&bytes[6]) || !b"aBcmt".contains(&bytes[7]) {
                    return Err(ParseError::Unknown(primary));
                }
                let now = self.now;
                let follow_references = self.options.follow != Follow::Physical;
                let value = self.argument(&primary)?;
                let x = bytes[6];
                let y = bytes[7];
                let test = if y == b't' {
                    super::test::parse_date(value, now).map(|stamp| super::test::Test::Newer {
                        field: match x {
                            b'a' => super::test::TimeField::Access,
                            b'B' => super::test::TimeField::Birth,
                            b'c' => super::test::TimeField::Change,
                            _ => super::test::TimeField::Modify,
                        },
                        stamp,
                    })
                } else {
                    super::test::Test::newer_xy(
                        x,
                        y,
                        std::path::PathBuf::from(value),
                        now,
                        follow_references,
                    )
                };
                Expression::Test(test.ok_or_else(|| invalid(&primary, value))?)
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

    fn target(&mut self, primary: &OsStr) -> Result<Target, ParseError> {
        let path = PathBuf::from(self.argument(primary)?);
        if path == std::path::Path::new("/dev/stdout") {
            return Ok(Target::Stdout);
        }
        // The file itself opens (and truncates) later, during `prepare`'s
        // single ordered pass over the expression - in step with any
        // `-newer`-style reference observation, not here at parse time
        // (#6). Parsing only reserves the shared, as-yet-unopened cell,
        // deduplicated by path so repeated targets share one open.
        let file = match self.streams.get(&path) {
            Some(file) => file.clone(),
            None => {
                let file = Arc::new(Mutex::new(None));
                self.streams.insert(path.clone(), file.clone());
                file
            }
        };
        Ok(Target::File(path, file))
    }

    fn format(&mut self, primary: &OsStr) -> Result<Format, ParseError> {
        let value = self.argument(primary)?.to_owned();
        Format::compile(value.as_bytes(), &mut self.warnings).map_err(ParseError::Feature)
    }

    fn exec(&mut self, primary: &OsStr) -> Result<Exec, ParseError> {
        let start = self.at;
        while let Some(arg) = self.args.get(self.at) {
            if arg == ";" {
                if self.at == start {
                    return Err(ParseError::Missing(primary.to_owned()));
                }
                let args = self.args[start..self.at].to_vec();
                self.at += 1;
                return self.exec_spec(primary, args, false);
            }
            if arg == "+"
                && self.at > start
                && self.args[self.at - 1] == "{}"
                && primary != "-ok"
                && primary != "-okdir"
            {
                let count = self.args[start..self.at]
                    .iter()
                    .filter(|arg| arg.as_bytes().windows(2).any(|pair| pair == b"{}"))
                    .count();
                if count != 1 || primary == "-ok" || primary == "-okdir" {
                    return Err(invalid(primary, arg));
                }
                let args = self.args[start..self.at - 1].to_vec();
                self.at += 1;
                if args.is_empty() {
                    return Err(ParseError::Missing(primary.to_owned()));
                }
                return self.exec_spec(primary, args, true);
            }
            self.at += 1;
        }
        Err(ParseError::Missing(primary.to_owned()))
    }

    fn exec_spec(
        &mut self,
        primary: &OsStr,
        args: Vec<OsString>,
        batch: bool,
    ) -> Result<Exec, ParseError> {
        let directory = primary.as_bytes().ends_with(b"dir");
        if directory
            && std::env::var_os("PATH").is_some_and(|path| {
                path.as_bytes()
                    .split(|&b| b == b':')
                    .any(|part| !part.starts_with(b"/"))
            })
        {
            return Err(ParseError::Feature(
                "relative or empty PATH entries are insecure with -execdir/-okdir".into(),
            ));
        }
        let id = self.exec_id;
        self.exec_id += 1;
        Ok(Exec {
            id,
            args,
            batch,
            directory,
            prompt: primary.as_bytes().starts_with(b"-ok"),
        })
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
