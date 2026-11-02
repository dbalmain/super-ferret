//! One path component's glob: compiled from a pattern's bytes, and
//! matched with the standard last-star retry loop.

#[derive(Clone, Copy, Debug)]
pub(super) struct PatternByte {
    pub(super) value: u8,
    pub(super) escaped: bool,
}

impl PatternByte {
    /// Whether this is `value` with no backslash before it.
    fn is(self, value: u8) -> bool {
        !self.escaped && self.value == value
    }
}

/// Whether `part[at]` is an unescaped `value`.
fn bare(part: &[PatternByte], at: usize, value: u8) -> bool {
    part.get(at).is_some_and(|byte| byte.is(value))
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum Component {
    Globstar { allow_zero: bool },
    Glob(ComponentGlob),
    Never,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(super) struct ComponentGlob {
    pub(super) atoms: Box<[Atom]>,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(super) enum Atom {
    Literal(Box<[u8]>),
    Any,
    Star,
    Class(CharacterClass),
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(super) struct CharacterClass {
    negated: bool,
    terms: Box<[ClassTerm]>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum ClassTerm {
    Byte(u8),
    Range(u8, u8),
    Posix(PosixClass),
}

#[derive(Clone, Copy, Debug)]
enum ClassMember {
    Byte(PatternByte),
    Posix(PosixClass),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum PosixClass {
    Alnum,
    Alpha,
    Blank,
    Cntrl,
    Digit,
    Graph,
    Lower,
    Print,
    Punct,
    Space,
    Upper,
    Xdigit,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Syntax {
    Ignore,
    Override,
}

/// `?*`: every non-empty name. A path component is never empty.
pub(super) fn any_glob() -> ComponentGlob {
    ComponentGlob {
        atoms: Box::from([Atom::Any, Atom::Star]),
    }
}

pub(super) fn normalize_star(glob: ComponentGlob) -> ComponentGlob {
    if matches!(glob.atoms.as_ref(), [Atom::Star]) {
        any_glob()
    } else {
        glob
    }
}

impl Component {
    pub(super) fn matches(&self, text: &[u8]) -> bool {
        match self {
            Self::Globstar { .. } => true,
            Self::Glob(glob) => glob.matches(text),
            Self::Never => false,
        }
    }

    pub(super) fn has_witness(&self) -> bool {
        match self {
            Self::Globstar { .. } => true,
            Self::Glob(glob) => glob.atoms.iter().all(Atom::has_witness),
            Self::Never => false,
        }
    }
}

impl ComponentGlob {
    pub(super) fn literal_bytes(&self) -> Option<Vec<u8>> {
        let mut literal = Vec::new();
        for atom in &self.atoms {
            let Atom::Literal(run) = atom else {
                return None;
            };
            literal.extend_from_slice(run);
        }
        Some(literal)
    }
}

impl Atom {
    fn has_witness(&self) -> bool {
        match self {
            Self::Class(class) => (0..=u8::MAX).any(|byte| class.matches(byte)),
            _ => true,
        }
    }

    pub(super) fn fixed_width(&self) -> Option<usize> {
        match self {
            Self::Literal(run) => Some(run.len()),
            Self::Any | Self::Class(_) => Some(1),
            Self::Star => None,
        }
    }
}

impl ComponentGlob {
    fn matches(&self, text: &[u8]) -> bool {
        self.matches_observed(text, || {})
    }

    pub(super) fn matches_observed(&self, text: &[u8], mut step: impl FnMut()) -> bool {
        let mut atom_at = 0;
        let mut text_at = 0;
        let mut star: Option<(usize, usize)> = None;

        while text_at < text.len() {
            step();
            if let Some(Atom::Star) = self.atoms.get(atom_at) {
                star = Some((atom_at + 1, text_at));
                atom_at += 1;
                continue;
            }
            if let Some(consumed) = self
                .atoms
                .get(atom_at)
                .and_then(|atom| atom.consumes(&text[text_at..]))
            {
                atom_at += 1;
                text_at += consumed;
                continue;
            }
            let Some((after_star, retry_at)) = star.as_mut() else {
                return false;
            };
            if *retry_at == text.len() {
                return false;
            }
            *retry_at += 1;
            atom_at = *after_star;
            text_at = *retry_at;
        }

        while matches!(self.atoms.get(atom_at), Some(Atom::Star)) {
            atom_at += 1;
        }
        atom_at == self.atoms.len()
    }
}

impl Atom {
    pub(super) fn consumes(&self, text: &[u8]) -> Option<usize> {
        match self {
            Self::Literal(run) => text.starts_with(run).then_some(run.len()),
            Self::Any => (!text.is_empty()).then_some(1),
            Self::Star => None,
            Self::Class(class) => text
                .first()
                .is_some_and(|byte| class.matches(*byte))
                .then_some(1),
        }
    }
}

impl CharacterClass {
    fn matches(&self, byte: u8) -> bool {
        let contained = self.terms.iter().any(|term| term.matches(byte));
        contained != self.negated
    }
}

impl ClassTerm {
    fn matches(self, byte: u8) -> bool {
        match self {
            Self::Byte(want) => byte == want,
            Self::Range(first, last) => first <= byte && byte <= last,
            Self::Posix(class) => class.matches(byte),
        }
    }
}

impl PosixClass {
    fn parse(name: &[u8]) -> Option<Self> {
        Some(match name {
            b"alnum" => Self::Alnum,
            b"alpha" => Self::Alpha,
            b"blank" => Self::Blank,
            b"cntrl" => Self::Cntrl,
            b"digit" => Self::Digit,
            b"graph" => Self::Graph,
            b"lower" => Self::Lower,
            b"print" => Self::Print,
            b"punct" => Self::Punct,
            b"space" => Self::Space,
            b"upper" => Self::Upper,
            b"xdigit" => Self::Xdigit,
            _ => return None,
        })
    }

    fn matches(self, byte: u8) -> bool {
        match self {
            Self::Alnum => byte.is_ascii_alphanumeric(),
            Self::Alpha => byte.is_ascii_alphabetic(),
            Self::Blank => matches!(byte, b' ' | b'\t'),
            Self::Cntrl => byte.is_ascii_control(),
            Self::Digit => byte.is_ascii_digit(),
            Self::Graph => byte.is_ascii_graphic(),
            Self::Lower => byte.is_ascii_lowercase(),
            Self::Print => byte.is_ascii_graphic() || byte == b' ',
            Self::Punct => byte.is_ascii_punctuation(),
            // Git's ASCII class omits form feed (unlike Rust's whitespace
            // predicate); these bytes were checked against git 2.54.
            Self::Space => matches!(byte, b' ' | b'\t' | b'\n' | b'\r'),
            Self::Upper => byte.is_ascii_uppercase(),
            Self::Xdigit => byte.is_ascii_hexdigit(),
        }
    }
}

pub(super) fn compile_component(part: &[PatternByte], anchored: bool, syntax: Syntax) -> Component {
    if anchored && part.len() >= 2 && part.iter().all(|byte| byte.is(b'*')) {
        return Component::Globstar { allow_zero: true };
    }
    if part.is_empty() {
        return Component::Never;
    }

    let mut atoms = Vec::new();
    let mut literal = Vec::new();
    let mut at = 0;
    while at < part.len() {
        let byte = part[at];
        if byte.escaped {
            literal.push(byte.value);
            at += 1;
            continue;
        }
        match byte.value {
            b'*' => {
                flush_literal(&mut atoms, &mut literal);
                if !matches!(atoms.last(), Some(Atom::Star)) {
                    atoms.push(Atom::Star);
                }
                at += 1;
            }
            b'?' => {
                flush_literal(&mut atoms, &mut literal);
                atoms.push(Atom::Any);
                at += 1;
            }
            b'[' => {
                flush_literal(&mut atoms, &mut literal);
                let Some((class, next)) = compile_class(part, at, syntax) else {
                    // Git silently makes malformed/unknown classes unable to
                    // match; it does not diagnose them as bad ignore lines.
                    return Component::Never;
                };
                atoms.push(Atom::Class(class));
                at = next;
            }
            value => {
                literal.push(value);
                at += 1;
            }
        }
    }
    flush_literal(&mut atoms, &mut literal);
    Component::Glob(ComponentGlob {
        atoms: atoms.into_boxed_slice(),
    })
}

fn flush_literal(atoms: &mut Vec<Atom>, literal: &mut Vec<u8>) {
    if !literal.is_empty() {
        atoms.push(Atom::Literal(std::mem::take(literal).into_boxed_slice()));
    }
}

fn compile_class(
    part: &[PatternByte],
    start: usize,
    syntax: Syntax,
) -> Option<(CharacterClass, usize)> {
    let mut at = start + 1;
    let negated = part
        .get(at)
        .is_some_and(|byte| byte.is(b'!') || byte.is(b'^'));
    if negated {
        at += 1;
    }

    let mut members = Vec::new();
    if bare(part, at, b']') {
        members.push(ClassMember::Byte(part[at]));
        at += 1;
    }
    let end = loop {
        let byte = *part.get(at)?;
        if byte.is(b']') {
            break at + 1;
        }
        if syntax == Syntax::Ignore && byte.is(b'[') && bare(part, at + 1, b':') {
            let name_start = at + 2;
            let mut name_end = name_start;
            while !(bare(part, name_end, b':') && bare(part, name_end + 1, b']')) {
                name_end += 1;
                part.get(name_end)?;
            }
            if part[name_start..name_end].iter().any(|item| item.escaped) {
                return None;
            }
            if let Some(ClassMember::Byte(hyphen)) = members.last()
                && hyphen.is(b'-')
            {
                // In Git, a POSIX-looking token after a range hyphen is raw
                // bracket syntax. The first close bracket ends the class;
                // the final close bracket is then an ordinary pattern byte.
                members.pop();
                if members.is_empty() {
                    return None;
                }
                let terms = members.iter().filter_map(|member| match member {
                    ClassMember::Byte(byte) => Some(ClassTerm::Byte(byte.value)),
                    ClassMember::Posix(_) => None,
                });
                return Some((
                    CharacterClass {
                        negated,
                        terms: terms.collect(),
                    },
                    name_end + 2,
                ));
            }
            let name: Vec<u8> = part[name_start..name_end]
                .iter()
                .map(|item| item.value)
                .collect();
            members.push(ClassMember::Posix(PosixClass::parse(&name)?));
            at = name_end + 2;
        } else {
            members.push(ClassMember::Byte(byte));
            at += 1;
        }
    };
    if members.is_empty() {
        return None;
    }
    Some((
        CharacterClass {
            negated,
            terms: class_terms(&members, syntax)?,
        },
        end,
    ))
}

/// A class's members as terms, joining `a-z` into a range. None when an
/// override pattern reverses a range, which Git ignore files tolerate.
fn class_terms(members: &[ClassMember], syntax: Syntax) -> Option<Box<[ClassTerm]>> {
    let mut terms = Vec::new();
    let mut at = 0;
    while at < members.len() {
        if let [
            ClassMember::Byte(first),
            ClassMember::Byte(hyphen),
            ClassMember::Byte(last),
            ..,
        ] = members[at..]
            && hyphen.is(b'-')
        {
            if first.value <= last.value {
                terms.push(ClassTerm::Range(first.value, last.value));
            } else {
                if syntax == Syntax::Override {
                    return None;
                }
                // Git keeps only the first endpoint of a reversed range.
                terms.push(ClassTerm::Byte(first.value));
            }
            at += 3;
            continue;
        }
        terms.push(match members[at] {
            ClassMember::Byte(byte) => ClassTerm::Byte(byte.value),
            ClassMember::Posix(class) => ClassTerm::Posix(class),
        });
        at += 1;
    }
    Some(terms.into_boxed_slice())
}

#[cfg(test)]
mod posix_class_tests {
    use super::PosixClass;

    // Membership observed with git 2.54 for every filesystem-representable
    // single-byte name except slash, dot, and dot-dot, which Unix resolves as
    // path syntax; NUL cannot occur in a path. Hex pairs are the matched bytes.
    const GIT_MEMBERSHIP: &[(&str, &str)] = &[
        (
            "alnum",
            "303132333435363738394142434445464748494a4b4c4d4e4f505152535455565758595a6162636465666768696a6b6c6d6e6f707172737475767778797a",
        ),
        (
            "alpha",
            "4142434445464748494a4b4c4d4e4f505152535455565758595a6162636465666768696a6b6c6d6e6f707172737475767778797a",
        ),
        ("blank", "0920"),
        (
            "cntrl",
            "0102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f7f",
        ),
        ("digit", "30313233343536373839"),
        (
            "graph",
            "2122232425262728292a2b2c2d303132333435363738393a3b3c3d3e3f404142434445464748494a4b4c4d4e4f505152535455565758595a5b5c5d5e5f606162636465666768696a6b6c6d6e6f707172737475767778797a7b7c7d7e",
        ),
        (
            "lower",
            "6162636465666768696a6b6c6d6e6f707172737475767778797a",
        ),
        (
            "print",
            "202122232425262728292a2b2c2d303132333435363738393a3b3c3d3e3f404142434445464748494a4b4c4d4e4f505152535455565758595a5b5c5d5e5f606162636465666768696a6b6c6d6e6f707172737475767778797a7b7c7d7e",
        ),
        (
            "punct",
            "2122232425262728292a2b2c2d3a3b3c3d3e3f405b5c5d5e5f607b7c7d7e",
        ),
        ("space", "090a0d20"),
        (
            "upper",
            "4142434445464748494a4b4c4d4e4f505152535455565758595a",
        ),
        ("xdigit", "30313233343536373839414243444546616263646566"),
    ];

    #[test]
    fn classes_match_the_git_254_byte_tables() {
        for (name, hex) in GIT_MEMBERSHIP {
            let class = PosixClass::parse(name.as_bytes()).unwrap();
            let observed: Vec<u8> = hex
                .as_bytes()
                .chunks_exact(2)
                .map(|pair| u8::from_str_radix(std::str::from_utf8(pair).unwrap(), 16).unwrap())
                .collect();
            for byte in 1..=u8::MAX {
                if byte == b'/' || byte == b'.' {
                    continue;
                }
                assert_eq!(
                    class.matches(byte),
                    observed.contains(&byte),
                    "{name} {byte:#04x}"
                );
            }
            assert_eq!(class.matches(0), *name == "cntrl", "{name} NUL");
            let punctuation = matches!(*name, "graph" | "print" | "punct");
            assert_eq!(class.matches(b'/'), punctuation, "{name} slash");
            assert_eq!(class.matches(b'.'), punctuation, "{name} dot");
        }
    }
}
