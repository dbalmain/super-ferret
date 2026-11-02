//! [`Search`]'s filter for a line-local pattern: a required run to find
//! first, and the bytes a match can span around it.

use regex_syntax::hir::Class;

use super::*;

/// [`Search`]'s required run and window.
#[derive(Clone, Debug)]
pub(super) struct Filter {
    pub(super) run: Regex,
    /// Every byte a consuming node can admit. Full-span searches keep a
    /// whole line when assertions are present; line selection adds context
    /// around this narrower window instead.
    within: Box<Window>,
}

#[derive(Clone, Debug)]
struct Window {
    bytes: [bool; 256],
    assertions: bool,
}

impl Filter {
    /// Whether the pattern has an assertion, so a match's answer depends
    /// on bytes beside it.
    pub(super) fn assertions(&self) -> bool {
        self.within.assertions
    }

    pub(super) fn start(&self, text: &[u8], at: usize, hit_start: usize) -> usize {
        text[at..hit_start]
            .iter()
            .rposition(|&b| !self.within.bytes[usize::from(b)])
            .map_or(at, |i| at + i + 1)
    }

    pub(super) fn window(
        &self,
        text: &[u8],
        at: usize,
        hit: std::ops::Range<usize>,
    ) -> (usize, usize) {
        #[cfg(test)]
        WINDOW_SCANNED.with(|scanned| scanned.set(scanned.get() + text.len() - hit.end));
        (
            text[at..hit.start]
                .iter()
                .rposition(|&b| !self.within.bytes[usize::from(b)])
                .map_or(at, |i| at + i + 1),
            text[hit.end..]
                .iter()
                .position(|&b| !self.within.bytes[usize::from(b)])
                .map_or(text.len(), |i| hit.end + i),
        )
    }
}

#[cfg(test)]
thread_local! {
    static WINDOW_SCANNED: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
pub(super) fn reset_window_scanned() {
    WINDOW_SCANNED.with(|scanned| scanned.set(0));
}

#[cfg(test)]
pub(super) fn window_scanned() -> usize {
    WINDOW_SCANNED.with(std::cell::Cell::get)
}

/// [`Search`]'s filter for a line-local pattern; `None` when the engine
/// finds the pattern's matches by a literal prefix of its own (a finite
/// prefix set), or when there is no run of three or more.
pub(super) fn filter(hir: &Hir) -> Option<Filter> {
    use regex_syntax::hir::literal::{ExtractKind, Extractor};
    if Extractor::new()
        .kind(ExtractKind::Prefix)
        .extract(hir)
        .is_finite()
    {
        return None;
    }
    let run = required_run(hir)?;
    // Printing a HIR gives a pattern that parses back to it; it is already
    // case folded, so it compiles with the builder's defaults.
    let run = Regex::build(&regex::bytes::RegexBuilder::new(&run.to_string())).ok()?;
    let mut within = Box::new(Window {
        bytes: [false; 256],
        assertions: false,
    });
    within.assertions = !match_bytes(hir, &mut within.bytes);
    within.bytes[usize::from(b'\n')] = false;
    Some(Filter { run, within })
}

/// Marks in `within` every byte a match of `hir` can contain; false when
/// `hir` has an assertion, whose answer depends on bytes beside the match.
fn match_bytes(hir: &Hir, within: &mut [bool; 256]) -> bool {
    let mut mark = |from: u32, to: u32| {
        for b in from..=to.min(0xFF) {
            within[b as usize] = true;
        }
    };
    match hir.kind() {
        HirKind::Empty => true,
        HirKind::Literal(literal) => {
            for &b in literal.0.iter() {
                within[usize::from(b)] = true;
            }
            true
        }
        // A character above ASCII is two or more bytes, each above it.
        HirKind::Class(Class::Unicode(class)) => {
            for range in class.ranges() {
                let (from, to) = (u32::from(range.start()), u32::from(range.end()));
                if from < 0x80 {
                    mark(from, to.min(0x7F));
                }
                if to >= 0x80 {
                    mark(0x80, 0xFF);
                }
            }
            true
        }
        HirKind::Class(Class::Bytes(class)) => {
            for range in class.ranges() {
                mark(u32::from(range.start()), u32::from(range.end()));
            }
            true
        }
        HirKind::Look(_) => false,
        HirKind::Repetition(repetition) => match_bytes(&repetition.sub, within),
        HirKind::Capture(capture) => match_bytes(&capture.sub, within),
        HirKind::Concat(hirs) | HirKind::Alternation(hirs) => {
            // Assertions require context, but every consuming node still
            // contributes to the selection window's byte set.
            let mut plain = true;
            for hir in hirs {
                if !match_bytes(hir, within) {
                    plain = false;
                }
            }
            plain
        }
    }
}

/// The longest run of consecutive literals and small classes that every
/// match of `hir` contains: at least three long (bytes of a literal, one per
/// class), and short of the whole pattern.
fn required_run(hir: &Hir) -> Option<Hir> {
    Some(run_in(hir)?.1)
}

/// [`required_run`], with its length.
fn run_in(hir: &Hir) -> Option<(usize, Hir)> {
    match hir.kind() {
        HirKind::Capture(capture) => run_in(&capture.sub),
        HirKind::Repetition(repetition) if repetition.min > 0 => run_in(&repetition.sub),
        HirKind::Concat(hirs) => {
            let (mut best, mut length, mut start, mut sum) = (0..0, 0, 0, 0);
            for (i, child) in hirs.iter().enumerate() {
                match literal_length(child) {
                    None => (start, sum) = (i + 1, 0),
                    Some(n) => {
                        sum += n;
                        if sum > length {
                            (best, length) = (start..i + 1, sum);
                        }
                    }
                }
            }
            let own = (length >= 3 && best.len() < hirs.len())
                .then(|| (length, Hir::concat(hirs[best].to_vec())));
            // A run inside a child is required too.
            own.into_iter()
                .chain(hirs.iter().filter_map(run_in))
                .max_by_key(|(length, _)| *length)
        }
        _ => None,
    }
}

/// How many bytes a literal matches, or 1 for a class of
/// at most four characters (the case variants of one); `None` otherwise.
fn literal_length(hir: &Hir) -> Option<usize> {
    let small = |sizes: &mut dyn Iterator<Item = u32>| (sizes.sum::<u32>() <= 4).then_some(1);
    match hir.kind() {
        HirKind::Literal(literal) => Some(literal.0.len()),
        HirKind::Class(Class::Unicode(class)) => small(
            &mut class
                .ranges()
                .iter()
                .map(|r| u32::from(r.end()) - u32::from(r.start()) + 1),
        ),
        HirKind::Class(Class::Bytes(class)) => small(
            &mut class
                .ranges()
                .iter()
                .map(|r| u32::from(r.end()) - u32::from(r.start()) + 1),
        ),
        HirKind::Capture(capture) => literal_length(&capture.sub),
        _ => None,
    }
}

// Line/absolute anchors supply no proof excluding LF. Word assertions do;
// every consuming node and every alternative must conservatively exclude LF.
pub(super) fn lf_excluded(hir: &Hir) -> bool {
    use regex_syntax::hir::Look;
    match hir.kind() {
        HirKind::Look(look) => !matches!(
            look,
            Look::Start | Look::End | Look::StartLF | Look::EndLF | Look::StartCRLF | Look::EndCRLF
        ),
        HirKind::Capture(capture) => lf_excluded(&capture.sub),
        HirKind::Repetition(repetition) => lf_excluded(&repetition.sub),
        HirKind::Concat(parts) | HirKind::Alternation(parts) => parts.iter().all(lf_excluded),
        _ => line_local(hir),
    }
}

/// [`Grep::is_line_local`] of a parsed pattern.
pub(super) fn line_local(hir: &Hir) -> bool {
    use regex_syntax::hir::Look;
    match hir.kind() {
        HirKind::Empty => true,
        HirKind::Literal(literal) => !literal.0.contains(&b'\n'),
        HirKind::Class(Class::Unicode(class)) => !class
            .ranges()
            .iter()
            .any(|range| range.start() <= '\n' && '\n' <= range.end()),
        HirKind::Class(Class::Bytes(class)) => !class
            .ranges()
            .iter()
            .any(|range| range.start() <= b'\n' && b'\n' <= range.end()),
        HirKind::Look(look) => !matches!(
            look,
            Look::Start | Look::End | Look::StartCRLF | Look::EndCRLF
        ),
        HirKind::Repetition(repetition) => line_local(&repetition.sub),
        HirKind::Capture(capture) => line_local(&capture.sub),
        HirKind::Concat(hirs) | HirKind::Alternation(hirs) => hirs.iter().all(line_local),
    }
}
