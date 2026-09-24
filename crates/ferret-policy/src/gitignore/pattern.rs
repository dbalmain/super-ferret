//! One parsed gitignore pattern and its bounded iterative matchers.
//!
//! Separators are recognized before component syntax, including `\/`. A
//! slash-free component uses the standard last-star retry loop; path components
//! use the same loop with globstar as their star. Both take O(pattern x path)
//! work, without recursion or per-match allocation.

use std::path::Path;

use super::Match;

#[derive(Clone, Debug)]
pub(crate) struct Pattern {
    pub(super) index: usize,
    pub(super) result: Match,
    pub(super) directory_only: bool,
    pub(super) basename_only: bool,
    components: Box<[Component]>,
    flat: Option<ComponentGlob>,
}

#[derive(Clone, Copy, Debug)]
struct PatternByte {
    value: u8,
    escaped: bool,
}

#[derive(Clone, Debug)]
enum Component {
    Globstar,
    Glob(ComponentGlob),
    Never,
}

#[derive(Clone, Debug)]
struct ComponentGlob {
    atoms: Box<[Atom]>,
}

#[derive(Clone, Debug)]
enum Atom {
    Literal(Box<[u8]>),
    Any,
    Star,
    Class(CharacterClass),
}

#[derive(Clone, Debug)]
struct CharacterClass {
    negated: bool,
    terms: Box<[ClassTerm]>,
}

#[derive(Clone, Copy, Debug)]
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

#[derive(Clone, Copy, Debug)]
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

impl Pattern {
    pub(crate) fn compile(index: usize, original: &str) -> Result<Option<Self>, String> {
        let line = trim_trailing_spaces(original);
        if line.is_empty() || line.starts_with('#') {
            return Ok(None);
        }

        let (result, body) = match line.strip_prefix('!') {
            Some("") => return Ok(None),
            Some(rest) => (Match::Whitelist, rest),
            None => (Match::Ignore, line),
        };
        let parsed = split_components(body.as_bytes())?;
        if parsed.parts.is_empty() {
            return Ok(None);
        }

        let basename_only = !parsed.leading_separator && parsed.parts.len() == 1;
        let anchored = !basename_only;
        let components: Box<[Component]> = parsed
            .parts
            .into_iter()
            .map(|part| compile_component(&part, anchored))
            .collect();
        let flat = flatten_components(&components);
        Ok(Some(Self {
            index,
            result,
            directory_only: parsed.trailing_separator,
            basename_only,
            components,
            flat,
        }))
    }

    pub(super) fn matches(&self, path: &[u8], basename: &[u8], is_dir: bool) -> bool {
        if self.directory_only && !is_dir {
            return false;
        }
        if self.basename_only {
            return self.components[0].matches(basename);
        }
        if let Some(prefix) = self.components.first().and_then(Component::literal_slice)
            && (!path.starts_with(prefix) || !matches!(path.get(prefix.len()), None | Some(b'/')))
        {
            return false;
        }
        if let Some(flat) = &self.flat {
            return flat.matches_path(path);
        }
        matches_components(&self.components, path)
    }

    pub(super) fn literal_basename(&self) -> Option<Vec<u8>> {
        self.basename_glob()?.literal_bytes()
    }

    pub(super) fn literal_path(&self) -> Option<Vec<u8>> {
        if self.basename_only {
            return None;
        }
        let mut path = Vec::new();
        for component in &self.components {
            if !path.is_empty() {
                path.push(b'/');
            }
            path.extend(component.literal_bytes()?);
        }
        Some(path)
    }

    pub(super) fn simple_extension(&self) -> Option<Vec<u8>> {
        let glob = self.basename_glob()?;
        let [Atom::Star, Atom::Literal(suffix)] = glob.atoms.as_ref() else {
            return None;
        };
        if suffix.len() < 2 || suffix[0] != b'.' || suffix[1..].contains(&b'.') {
            return None;
        }
        Some(suffix.to_vec())
    }

    pub(super) fn basename_prefix(&self) -> Option<Vec<u8>> {
        let glob = self.basename_glob()?;
        let [Atom::Literal(prefix), Atom::Star] = glob.atoms.as_ref() else {
            return None;
        };
        Some(prefix.to_vec())
    }

    pub(super) fn basename_suffix(&self) -> Option<Vec<u8>> {
        let glob = self.basename_glob()?;
        let [Atom::Star, Atom::Literal(suffix)] = glob.atoms.as_ref() else {
            return None;
        };
        Some(suffix.to_vec())
    }

    pub(super) fn basename_contains(&self) -> Option<Vec<u8>> {
        let glob = self.basename_glob()?;
        let [Atom::Star, Atom::Literal(needle), Atom::Star] = glob.atoms.as_ref() else {
            return None;
        };
        Some(needle.to_vec())
    }

    pub(super) fn fixed_basename_suffix_width(&self) -> Option<usize> {
        let glob = self.basename_glob()?;
        matches!(glob.atoms.first(), Some(Atom::Star)).then_some(())?;
        glob.atoms[1..].iter().map(Atom::fixed_width).sum()
    }

    pub(super) fn matches_fixed_basename_suffix(
        &self,
        basename: &[u8],
        is_dir: bool,
        width: usize,
    ) -> bool {
        if self.directory_only && !is_dir {
            return false;
        }
        let Some(glob) = self.basename_glob() else {
            return false;
        };
        let Some(suffix) = basename
            .len()
            .checked_sub(width)
            .and_then(|start| basename.get(start..))
        else {
            return false;
        };
        let mut at = 0;
        for atom in &glob.atoms[1..] {
            let Some(consumed) = atom.consumes(&suffix[at..], false) else {
                return false;
            };
            at += consumed;
        }
        at == suffix.len()
    }

    pub(super) fn first_literal_byte(&self) -> Option<u8> {
        if self.basename_only {
            return None;
        }
        let Component::Glob(glob) = self.components.first()? else {
            return None;
        };
        let Atom::Literal(run) = glob.atoms.first()? else {
            return None;
        };
        run.first().copied()
    }

    pub(super) fn first_literal_prefix2(&self) -> Option<u16> {
        if self.basename_only {
            return None;
        }
        let Component::Glob(glob) = self.components.first()? else {
            return None;
        };
        let Atom::Literal(run) = glob.atoms.first()? else {
            return None;
        };
        let prefix = run.get(..2)?;
        Some(u16::from_ne_bytes([prefix[0], prefix[1]]))
    }

    fn basename_glob(&self) -> Option<&ComponentGlob> {
        let component = match self.components.as_ref() {
            [Component::Glob(glob)] if self.basename_only => return Some(glob),
            [Component::Globstar, component] => component,
            _ => return None,
        };
        let Component::Glob(glob) = component else {
            return None;
        };
        Some(glob)
    }

    pub(crate) fn is_anchored_reinclude(&self) -> bool {
        self.result == Match::Whitelist
            && !self.basename_only
            && self.components.len() >= 2
            && !matches!(self.components.first(), Some(Component::Globstar))
            && self.components.iter().all(Component::has_witness)
    }

    pub(crate) fn reaches_below(&self, dir: &Path) -> bool {
        let mut path_at = 0;
        for (component_at, component) in self.components.iter().enumerate() {
            if matches!(component, Component::Globstar) {
                return component_at != 0
                    && self.components[component_at + 1..]
                        .iter()
                        .all(Component::has_witness);
            }
            let Some((path_component, next)) =
                next_component(dir.as_os_str().as_encoded_bytes(), path_at)
            else {
                return self.components[component_at..]
                    .iter()
                    .all(Component::has_witness);
            };
            if !component.matches(path_component) {
                return false;
            }
            path_at = next;
        }
        false
    }

    #[cfg(test)]
    pub(super) fn basename_match_steps(&self, text: &[u8]) -> (bool, usize) {
        let Some(Component::Glob(glob)) = self.components.first() else {
            return (false, 0);
        };
        let mut steps = 0;
        let matched = glob.matches_observed(text, false, || steps += 1);
        (matched, steps)
    }
}

impl Component {
    fn matches(&self, text: &[u8]) -> bool {
        match self {
            Self::Globstar => true,
            Self::Glob(glob) => glob.matches(text),
            Self::Never => false,
        }
    }

    fn literal_bytes(&self) -> Option<Vec<u8>> {
        let Self::Glob(glob) = self else {
            return None;
        };
        glob.literal_bytes()
    }

    fn literal_slice(&self) -> Option<&[u8]> {
        let Self::Glob(glob) = self else {
            return None;
        };
        let [Atom::Literal(run)] = glob.atoms.as_ref() else {
            return None;
        };
        Some(run)
    }

    fn has_witness(&self) -> bool {
        match self {
            Self::Globstar => true,
            Self::Glob(glob) => glob.atoms.iter().all(Atom::has_witness),
            Self::Never => false,
        }
    }
}

impl ComponentGlob {
    fn literal_bytes(&self) -> Option<Vec<u8>> {
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

    fn fixed_width(&self) -> Option<usize> {
        match self {
            Self::Literal(run) => Some(run.len()),
            Self::Any | Self::Class(_) => Some(1),
            Self::Star => None,
        }
    }
}

impl ComponentGlob {
    fn matches(&self, text: &[u8]) -> bool {
        self.matches_observed(text, false, || {})
    }

    fn matches_path(&self, text: &[u8]) -> bool {
        self.matches_observed(text, true, || {})
    }

    fn matches_observed(&self, text: &[u8], path_mode: bool, mut step: impl FnMut()) -> bool {
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
                .and_then(|atom| atom.consumes(&text[text_at..], path_mode))
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
            if path_mode && text[*retry_at] == b'/' {
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
    fn consumes(&self, text: &[u8], path_mode: bool) -> Option<usize> {
        match self {
            Self::Literal(run) => text.starts_with(run).then_some(run.len()),
            Self::Any => text
                .first()
                .is_some_and(|byte| !path_mode || *byte != b'/')
                .then_some(1),
            Self::Star => None,
            Self::Class(class) => text
                .first()
                .is_some_and(|byte| (!path_mode || *byte != b'/') && class.matches(*byte))
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
            Self::Space => byte.is_ascii_whitespace(),
            Self::Upper => byte.is_ascii_uppercase(),
            Self::Xdigit => byte.is_ascii_hexdigit(),
        }
    }
}

struct SplitPattern {
    leading_separator: bool,
    trailing_separator: bool,
    parts: Vec<Vec<PatternByte>>,
}

fn split_components(pattern: &[u8]) -> Result<SplitPattern, String> {
    let mut parts = vec![Vec::new()];
    let mut at = 0;
    while at < pattern.len() {
        match pattern[at] {
            b'\\' => {
                let Some(&escaped) = pattern.get(at + 1) else {
                    return Err("trailing backslash".to_owned());
                };
                if escaped == b'/' {
                    parts.push(Vec::new());
                } else {
                    let Some(current) = parts.last_mut() else {
                        return Err("pattern has no component".to_owned());
                    };
                    current.push(PatternByte {
                        value: escaped,
                        escaped: true,
                    });
                }
                at += 2;
            }
            b'/' => {
                parts.push(Vec::new());
                at += 1;
            }
            value => {
                let Some(current) = parts.last_mut() else {
                    return Err("pattern has no component".to_owned());
                };
                current.push(PatternByte {
                    value,
                    escaped: false,
                });
                at += 1;
            }
        }
    }

    let leading_separator = parts.first().is_some_and(Vec::is_empty) && parts.len() > 1;
    if leading_separator {
        parts.remove(0);
    }
    let trailing_separator = parts.last().is_some_and(Vec::is_empty) && parts.len() > 1;
    if trailing_separator {
        parts.pop();
    }
    Ok(SplitPattern {
        leading_separator,
        trailing_separator,
        parts,
    })
}

fn compile_component(part: &[PatternByte], anchored: bool) -> Component {
    if anchored && part.len() >= 2 && part.iter().all(|byte| !byte.escaped && byte.value == b'*') {
        return Component::Globstar;
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
                let Some((class, next)) = compile_class(part, at) else {
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

fn flatten_components(components: &[Component]) -> Option<ComponentGlob> {
    let mut atoms = Vec::new();
    for (component_at, component) in components.iter().enumerate() {
        if component_at != 0 {
            atoms.push(Atom::Literal(Box::from(&b"/"[..])));
        }
        let Component::Glob(glob) = component else {
            return None;
        };
        atoms.extend(glob.atoms.iter().cloned());
    }
    Some(ComponentGlob {
        atoms: atoms.into_boxed_slice(),
    })
}

fn flush_literal(atoms: &mut Vec<Atom>, literal: &mut Vec<u8>) {
    if !literal.is_empty() {
        atoms.push(Atom::Literal(std::mem::take(literal).into_boxed_slice()));
    }
}

fn compile_class(part: &[PatternByte], start: usize) -> Option<(CharacterClass, usize)> {
    let mut at = start + 1;
    let negated = part
        .get(at)
        .is_some_and(|byte| !byte.escaped && matches!(byte.value, b'!' | b'^'));
    if negated {
        at += 1;
    }

    let mut members = Vec::new();
    if part
        .get(at)
        .is_some_and(|byte| !byte.escaped && byte.value == b']')
    {
        members.push(ClassMember::Byte(part[at]));
        at += 1;
    }
    let end = loop {
        let byte = *part.get(at)?;
        if !byte.escaped && byte.value == b']' {
            break at + 1;
        }
        if !byte.escaped
            && byte.value == b'['
            && part
                .get(at + 1)
                .is_some_and(|colon| !colon.escaped && colon.value == b':')
        {
            let name_start = at + 2;
            let mut name_end = name_start;
            while !(part
                .get(name_end)
                .is_some_and(|colon| !colon.escaped && colon.value == b':')
                && part
                    .get(name_end + 1)
                    .is_some_and(|close| !close.escaped && close.value == b']'))
            {
                name_end += 1;
                part.get(name_end)?;
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

    let mut terms = Vec::new();
    let mut member_at = 0;
    while member_at < members.len() {
        if let (
            Some(ClassMember::Byte(first)),
            Some(ClassMember::Byte(hyphen)),
            Some(ClassMember::Byte(last)),
        ) = (
            members.get(member_at),
            members.get(member_at + 1),
            members.get(member_at + 2),
        ) && !hyphen.escaped
            && hyphen.value == b'-'
        {
            if first.value <= last.value {
                terms.push(ClassTerm::Range(first.value, last.value));
            } else {
                // Git keeps only the first endpoint of a reversed range.
                terms.push(ClassTerm::Byte(first.value));
            }
            member_at += 3;
            continue;
        }
        terms.push(match members[member_at] {
            ClassMember::Byte(byte) => ClassTerm::Byte(byte.value),
            ClassMember::Posix(class) => ClassTerm::Posix(class),
        });
        member_at += 1;
    }
    Some((
        CharacterClass {
            negated,
            terms: terms.into_boxed_slice(),
        },
        end,
    ))
}

fn matches_components(pattern: &[Component], path: &[u8]) -> bool {
    let mut pattern_at = 0;
    let mut path_at = 0;
    let mut globstar: Option<(usize, usize)> = None;

    while let Some((path_component, next_path)) = next_component(path, path_at) {
        if let Some(Component::Globstar) = pattern.get(pattern_at) {
            if pattern_at + 1 == pattern.len() {
                return true;
            }
            globstar = Some((pattern_at + 1, path_at));
            pattern_at += 1;
            continue;
        }
        if pattern
            .get(pattern_at)
            .is_some_and(|component| component.matches(path_component))
        {
            pattern_at += 1;
            path_at = next_path;
            continue;
        }
        let Some((after_globstar, retry_at)) = globstar.as_mut() else {
            return false;
        };
        let Some((_, next_retry)) = next_component(path, *retry_at) else {
            return false;
        };
        *retry_at = next_retry;
        pattern_at = *after_globstar;
        path_at = next_retry;
    }

    while matches!(pattern.get(pattern_at), Some(Component::Globstar)) {
        if pattern_at + 1 == pattern.len() {
            return false;
        }
        pattern_at += 1;
    }
    pattern_at == pattern.len()
}

fn next_component(path: &[u8], at: usize) -> Option<(&[u8], usize)> {
    if at >= path.len() {
        return None;
    }
    let end = path[at..]
        .iter()
        .position(|byte| *byte == b'/')
        .map_or(path.len(), |offset| at + offset);
    let next = if end < path.len() { end + 1 } else { end };
    Some((&path[at..end], next))
}

fn trim_trailing_spaces(mut line: &str) -> &str {
    while line.ends_with(' ') {
        let before = &line.as_bytes()[..line.len() - 1];
        let backslashes = before
            .iter()
            .rev()
            .take_while(|byte| **byte == b'\\')
            .count();
        if backslashes % 2 == 1 {
            break;
        }
        line = &line[..line.len() - 1];
    }
    line
}
