//! One parsed gitignore pattern and the cursor steps that apply an anchored
//! pattern one directory at a time.
//!
//! Separators are recognized before component syntax, including `\/`. Each
//! component is a [`glob`], matched by the standard last-star retry loop in
//! O(pattern x name) work without recursion or allocation. An anchored pattern
//! is never matched against a whole path: the walker steps cursors through it
//! per directory ([`Pattern::step`]) and projects each live cursor onto the
//! entry names of one directory ([`Pattern::projections`]). A pattern of k
//! components holds at most k cursors, and a step is O(k) component matches.

use std::sync::OnceLock;

use super::Match;

mod glob;
use glob::{
    Atom, Component, ComponentGlob, PatternByte, Syntax, any_glob, compile_component,
    normalize_star,
};

#[derive(Clone, Debug)]
pub(crate) struct Pattern {
    pub(super) index: usize,
    pub(super) result: Match,
    pub(super) directory_only: bool,
    pub(super) basename_only: bool,
    components: Box<[Component]>,
    /// [`Self::projections`], kept for override matching, which asks once
    /// per walked name: rebuilding them cost about 2.6 µs a name under
    /// `-g '**/*.rs'` on a 305k-name tree.
    projected: OnceLock<Box<[Option<Pattern>]>>,
}

impl Pattern {
    /// Command-line overrides match one path endpoint. Directory descent is
    /// decided by the caller so a later file glob cannot revive a pruned dir.
    pub(crate) fn matches_override(&self, components: &[&[u8]], directory: bool) -> bool {
        let Some((last, parents)) = components.split_last() else {
            return false;
        };
        if self.basename_only {
            return self.matches_name(last, directory);
        }
        let projections = self
            .projected
            .get_or_init(|| self.projections().into_boxed_slice());
        // `**/X` (the Glob tool's usual shape): every cursor projects to X,
        // so the parents need no stepping.
        if self.is_shared_basename() {
            return projections[0]
                .as_ref()
                .is_some_and(|p| p.matches_name(last, directory));
        }
        let mut cursors = vec![(0, false)];
        for parent in parents {
            let mut next = Vec::new();
            self.step(cursors, parent, &mut next);
            cursors = next;
        }
        cursors.into_iter().any(|(pos, fed)| {
            projections[pos * 2 + usize::from(fed)]
                .as_ref()
                .is_some_and(|p| p.matches_name(last, directory))
        })
    }

    /// Whether this override matches the empty path, as rg's globs see cwd
    /// relative to itself. Leading zero-allowing globstars match nothing, then
    /// at most one all-star glob matches the empty name; a mandatory globstar
    /// cannot. Anything after an all-star glob needs a separator the empty
    /// path lacks. So `*`, `**`, `**/*`, `/*` and `/**` match it, and `?*`,
    /// `*/*`, `*/**` and `**\\/**` do not (rg 15.2.0).
    pub(crate) fn matches_empty_path(&self) -> bool {
        let mut first = 0;
        while matches!(
            self.components.get(first),
            Some(Component::Globstar { allow_zero: true })
        ) {
            first += 1;
        }
        if matches!(
            self.components.get(first),
            Some(Component::Globstar { allow_zero: false })
        ) {
            return false;
        }
        match &self.components[first..] {
            [] => true,
            [Component::Glob(glob)] => glob.atoms.iter().all(|atom| *atom == Atom::Star),
            _ => false,
        }
    }

    pub(crate) fn compile(index: usize, original: &str) -> Result<Option<Self>, String> {
        Self::compile_syntax(index, original, Syntax::Ignore)
    }

    pub(crate) fn compile_override(original: &str) -> Result<Option<Self>, String> {
        Self::compile_syntax(0, original, Syntax::Override)
    }

    fn compile_syntax(
        index: usize,
        original: &str,
        syntax: Syntax,
    ) -> Result<Option<Self>, String> {
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
            .map(|part| compile_component(&part, anchored, syntax))
            .collect();
        if syntax == Syntax::Override && components.iter().any(|p| matches!(p, Component::Never)) {
            return Err("invalid glob character class or range".into());
        }
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
            projected: OnceLock::new(),
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
            projected: OnceLock::new(),
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
