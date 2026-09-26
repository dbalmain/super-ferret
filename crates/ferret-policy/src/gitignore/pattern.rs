//! One parsed gitignore pattern, its component matcher, and the cursor steps
//! that apply an anchored pattern one directory at a time.
//!
//! Separators are recognized before component syntax, including `\/`. A
//! component uses the standard last-star retry loop, O(pattern x name) work
//! without recursion or allocation. An anchored pattern is never matched
//! against a whole path: the walker steps cursors through it per directory
//! ([`Pattern::step`]) and projects each live cursor onto the entry names of
//! one directory ([`Pattern::projections`]). A pattern of k components holds
//! at most k cursors, and a step is O(k) component matches.

use super::Match;

#[derive(Clone, Debug)]
pub(crate) struct Pattern {
    pub(super) index: usize,
    pub(super) result: Match,
    pub(super) directory_only: bool,
    pub(super) basename_only: bool,
    components: Box<[Component]>,
}

#[derive(Clone, Copy, Debug)]
struct PatternByte {
    value: u8,
    escaped: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Component {
    Globstar { allow_zero: bool },
    Glob(ComponentGlob),
    Never,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct ComponentGlob {
    atoms: Box<[Atom]>,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
enum Atom {
    Literal(Box<[u8]>),
    Any,
    Star,
    Class(CharacterClass),
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct CharacterClass {
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
        let mut components: Vec<Component> = parsed
            .parts
            .into_iter()
            .map(|part| compile_component(&part, anchored))
            .collect();
        if parsed.escaped_edge
            && let Some(first) = components.first_mut()
        {
            *first = Component::Never;
        }
        for (at, component) in components.iter_mut().enumerate() {
            if matches!(component, Component::Globstar { .. }) {
                let allow_zero = parsed.separators.get(at).copied().unwrap_or(true);
                *component = Component::Globstar { allow_zero };
            }
        }
        // `**/**` matches exactly what `**` does, so a run of zero-allowing
        // globstars is one. Collapsing keeps a line of a thousand `**/` from
        // costing a thousand cursor positions.
        components.dedup_by(|next, kept| {
            matches!(
                (next, kept),
                (
                    Component::Globstar { allow_zero: true },
                    Component::Globstar { allow_zero: true }
                )
            )
        });
        let components: Box<[Component]> = components.into_boxed_slice();
        Ok(Some(Self {
            index,
            result,
            directory_only: parsed.trailing_separator,
            basename_only,
            components,
        }))
    }

    /// Whether this basename pattern matches entry `name`, honouring
    /// `directory_only`. Only basename patterns reach a list's index; an
    /// anchored pattern is stepped with [`step`](Self::step) and projected
    /// with [`projections`](Self::projections) first.
    pub(super) fn matches_name(&self, name: &[u8], is_dir: bool) -> bool {
        debug_assert!(self.basename_only);
        (!self.directory_only || is_dir) && self.components[0].matches(name)
    }

    /// Source line index, for last-match-wins order within one file.
    pub(crate) fn line(&self) -> usize {
        self.index
    }

    /// A `!` pattern.
    pub(crate) fn is_whitelist(&self) -> bool {
        self.result == Match::Whitelist
    }

    /// How many components an anchored pattern has; a cursor's position is
    /// below this.
    #[cfg(test)]
    pub(crate) fn component_count(&self) -> usize {
        self.components.len()
    }

    pub(super) fn literal_basename(&self) -> Option<Vec<u8>> {
        self.basename_glob()?.literal_bytes()
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
            let Some(consumed) = atom.consumes(&suffix[at..]) else {
                return false;
            };
            at += consumed;
        }
        at == suffix.len()
    }

    fn basename_glob(&self) -> Option<&ComponentGlob> {
        match self.components.as_ref() {
            [Component::Glob(glob)] if self.basename_only => Some(glob),
            _ => None,
        }
    }

    pub(crate) fn is_anchored_reinclude(&self) -> bool {
        self.result == Match::Whitelist
            && !self.basename_only
            && self.components.len() >= 2
            && !matches!(self.components.first(), Some(Component::Globstar { .. }))
            && self.components.iter().all(Component::has_witness)
    }

    /// Whether a later exclusion covers every path this re-include can match.
    /// The comparison is deliberately conservative: parsed component
    /// structure must be identical, and a directory-only exclusion cannot
    /// cancel a re-include that also matches files.
    pub(crate) fn is_superseded_by(&self, later: &Self) -> bool {
        self.result == Match::Whitelist
            && later.result == Match::Ignore
            && self.basename_only == later.basename_only
            && self.components == later.components
            && (!later.directory_only || self.directory_only)
    }

    /// A basename rule is not stepped on enter. A leading `**/` plus one
    /// component matches in every directory, so it is one too; `**\/x`
    /// (`allow_zero: false`) is not, because it cannot match at its own
    /// file's directory.
    pub(crate) fn is_shared_basename(&self) -> bool {
        self.basename_only
            || matches!(
                self.components.as_ref(),
                [Component::Globstar { allow_zero: true }, _]
            )
    }

    /// Steps the cursors `(pos, fed)` of this anchored pattern into child
    /// `name`, writing the child's cursors to `out` in position order, each
    /// position once. Returns the work done, in component visits and
    /// position marks, for tests to bound.
    ///
    /// `fed` means the cursor sits on a globstar that has already consumed
    /// the directory a `**\/` pattern requires, so that globstar may now
    /// match zero further components. A pattern of k components therefore
    /// holds at most k cursors in one directory.
    ///
    /// Iterative, with positions deduplicated in bitsets and each globstar's
    /// zero-width continuation followed at most once per step, so a step is
    /// O(cursors + k / 64) component visits and word operations, however
    /// the globstars are arranged.
    pub(crate) fn step(
        &self,
        cursors: impl IntoIterator<Item = (usize, bool)>,
        name: &[u8],
        out: &mut Vec<(usize, bool)>,
    ) -> usize {
        let count = self.components.len();
        let mut marks = Marks::new(count);
        let mut work = 0;
        for (mut pos, mut fed) in cursors {
            while let Some(component) = self.components.get(pos) {
                work += 1;
                match *component {
                    Component::Globstar { allow_zero } => {
                        // Consuming `name` leaves the globstar fed whether or
                        // not it was allowed to match zero components before.
                        marks.mark(pos, true);
                        if !(allow_zero || fed) || !marks.expand(pos) {
                            break;
                        }
                        pos += 1;
                        fed = false;
                    }
                    Component::Glob(_) | Component::Never => {
                        if pos + 1 < count && component.matches(name) {
                            marks.mark(pos + 1, false);
                        }
                        break;
                    }
                }
            }
        }
        work + marks.drain(out)
    }

    /// The basename rule each cursor projects to, at index `2 * pos + fed`
    /// for every position of this pattern: a basename pattern matching the
    /// same entry names as the cursor does in its directory, or `None` when
    /// nothing in this directory can match and the cursor only constrains
    /// descendants. Computed from the end in one pass, so O(k) for k
    /// components.
    ///
    /// A shared basename pattern projects at `(0, false)` to itself (a
    /// leading `**/` dropped). A lone `*` is rewritten to `?*` because the
    /// basename fast path reads a bare star as a zero-width suffix, which
    /// would match only an empty name.
    pub(crate) fn projections(&self) -> Vec<Option<Self>> {
        let count = self.components.len();
        // `tail[i]`: what a cursor at `i` that is not fed projects to, as the
        // index of the glob it lands on, or `ANY` for a trailing globstar.
        const ANY: usize = usize::MAX;
        let mut tail: Vec<Option<usize>> = vec![None; count];
        for at in (0..count).rev() {
            let last = at + 1 == count;
            tail[at] = match self.components[at] {
                Component::Globstar { .. } if last => Some(ANY),
                Component::Glob(_) if last => Some(at),
                Component::Globstar { allow_zero: true } => tail[at + 1],
                _ => None,
            };
        }
        let project = |target: Option<usize>| {
            let glob = match target? {
                ANY => any_glob(),
                at => match &self.components[at] {
                    Component::Glob(glob) => normalize_star(glob.clone()),
                    Component::Globstar { .. } | Component::Never => return None,
                },
            };
            Some(self.as_basename(glob))
        };
        (0..count)
            .flat_map(|pos| {
                let fed = match self.components[pos] {
                    Component::Globstar { .. } if pos + 1 == count => Some(ANY),
                    Component::Globstar { .. } => tail[pos + 1],
                    _ => None,
                };
                [project(tail[pos]), project(fed)]
            })
            .collect()
    }

    fn as_basename(&self, glob: ComponentGlob) -> Self {
        Self {
            index: self.index,
            result: self.result,
            directory_only: self.directory_only,
            basename_only: true,
            components: Box::from([Component::Glob(glob)]),
        }
    }

    /// This pattern with source index `index`: a list merged from several
    /// files numbers its rules by list position instead of source line.
    pub(crate) fn renumbered(&self, index: usize) -> Self {
        Self {
            index,
            ..self.clone()
        }
    }

    /// What decides a basename pattern's matches: its glob and flags, not
    /// its line. Two rules with equal text are interchangeable in a list.
    pub(crate) fn text(&self) -> RuleText {
        debug_assert!(self.basename_only);
        RuleText {
            glob: match &self.components[0] {
                Component::Glob(glob) => Some(glob.clone()),
                Component::Globstar { .. } | Component::Never => None,
            },
            whitelist: self.is_whitelist(),
            directory_only: self.directory_only,
        }
    }

    #[cfg(test)]
    pub(super) fn basename_match_steps(&self, text: &[u8]) -> (bool, usize) {
        let Some(Component::Glob(glob)) = self.components.first() else {
            return (false, 0);
        };
        let mut steps = 0;
        let matched = glob.matches_observed(text, || steps += 1);
        (matched, steps)
    }
}

/// The rendered text of a basename rule: see [`Pattern::text`].
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) struct RuleText {
    glob: Option<ComponentGlob>,
    whitelist: bool,
    directory_only: bool,
}

/// Positions reached in one step, and which of them are fed, one bit each;
/// plus the globstars whose zero-width continuation has been followed.
struct Marks {
    reached: Vec<u64>,
    fed: Vec<u64>,
    expanded: Vec<u64>,
}

impl Marks {
    fn new(count: usize) -> Self {
        let words = count.div_ceil(64);
        Self {
            reached: vec![0; words],
            fed: vec![0; words],
            expanded: vec![0; words],
        }
    }

    fn mark(&mut self, pos: usize, fed: bool) {
        let (word, bit) = (pos / 64, 1u64 << (pos % 64));
        self.reached[word] |= bit;
        if fed {
            self.fed[word] |= bit;
        }
    }

    /// Records that `pos`'s continuation is being followed; false if it
    /// already was this step.
    fn expand(&mut self, pos: usize) -> bool {
        let (word, bit) = (pos / 64, 1u64 << (pos % 64));
        let first = self.expanded[word] & bit == 0;
        self.expanded[word] |= bit;
        first
    }

    /// Appends the marked positions in order; returns the words scanned.
    fn drain(&self, out: &mut Vec<(usize, bool)>) -> usize {
        for (word, &bits) in self.reached.iter().enumerate() {
            let mut rest = bits;
            while rest != 0 {
                let bit = rest.trailing_zeros() as usize;
                rest &= rest - 1;
                out.push((word * 64 + bit, self.fed[word] >> bit & 1 == 1));
            }
        }
        self.reached.len()
    }
}

/// `?*`: every non-empty name. A path component is never empty.
fn any_glob() -> ComponentGlob {
    ComponentGlob {
        atoms: Box::from([Atom::Any, Atom::Star]),
    }
}

fn normalize_star(glob: ComponentGlob) -> ComponentGlob {
    if matches!(glob.atoms.as_ref(), [Atom::Star]) {
        any_glob()
    } else {
        glob
    }
}

impl Component {
    fn matches(&self, text: &[u8]) -> bool {
        match self {
            Self::Globstar { .. } => true,
            Self::Glob(glob) => glob.matches(text),
            Self::Never => false,
        }
    }

    fn has_witness(&self) -> bool {
        match self {
            Self::Globstar { .. } => true,
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
        self.matches_observed(text, || {})
    }

    fn matches_observed(&self, text: &[u8], mut step: impl FnMut()) -> bool {
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
    fn consumes(&self, text: &[u8]) -> Option<usize> {
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

struct SplitPattern {
    leading_separator: bool,
    trailing_separator: bool,
    parts: Vec<Vec<PatternByte>>,
    separators: Vec<bool>,
    escaped_edge: bool,
}

fn split_components(pattern: &[u8]) -> Result<SplitPattern, String> {
    let mut parts = vec![Vec::new()];
    let mut separators = Vec::new();
    let mut at = 0;
    while at < pattern.len() {
        match pattern[at] {
            b'\\' => {
                let Some(&escaped) = pattern.get(at + 1) else {
                    return Err("trailing backslash".to_owned());
                };
                if escaped == b'/' {
                    separators.push(false);
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
                separators.push(true);
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

    let leading_separator = parts.first().is_some_and(Vec::is_empty)
        && parts.len() > 1
        && separators.first().copied().unwrap_or(false);
    let escaped_edge = pattern.starts_with(b"\\/") || pattern.ends_with(b"\\/");
    if leading_separator {
        parts.remove(0);
        separators.remove(0);
    }
    let trailing_separator = parts.last().is_some_and(Vec::is_empty)
        && parts.len() > 1
        && separators.last().copied().unwrap_or(false);
    if trailing_separator {
        parts.pop();
        separators.pop();
    }
    Ok(SplitPattern {
        leading_separator,
        trailing_separator,
        parts,
        separators,
        escaped_edge,
    })
}

fn compile_component(part: &[PatternByte], anchored: bool) -> Component {
    if anchored && part.len() >= 2 && part.iter().all(|byte| !byte.escaped && byte.value == b'*') {
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
            if part[name_start..name_end].iter().any(|item| item.escaped) {
                return None;
            }
            if members.last().is_some_and(|member| {
                matches!(
                    member,
                    ClassMember::Byte(PatternByte {
                        value: b'-',
                        escaped: false
                    })
                )
            }) {
                // In Git, a POSIX-looking token after a range hyphen is raw
                // bracket syntax. The first close bracket ends the class;
                // the final close bracket is then an ordinary pattern byte.
                members.pop();
                let prior = members.pop()?;
                members.push(prior);
                let class_end = name_end + 2;
                let mut terms = Vec::new();
                for member in members {
                    if let ClassMember::Byte(byte) = member {
                        terms.push(ClassTerm::Byte(byte.value));
                    }
                }
                return Some((
                    CharacterClass {
                        negated,
                        terms: terms.into_boxed_slice(),
                    },
                    class_end,
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
