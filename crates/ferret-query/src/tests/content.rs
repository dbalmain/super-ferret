//! `text:ARG` compiled against a real index, followed from a small corpus
//! in several segments, and judged by the verifier over every live
//! document: a Yes must hold without verification, and Yes plus the Maybes
//! that verify must be exactly the documents that hold. Coverage is full,
//! partial (documents past the frontier, and unreadable ones) and mixed
//! with dead documents.

use std::collections::BTreeSet;

use ferret_index::{
    Budget, CatalogView, Certainty, Cursor, DocSet, Fault, IndexWriter, Pinned, Stopped, View,
};
use ferret_verify::{Text, TextMatcher};

use super::Scratch;
use crate::TextAtom;

/// A run of 69 bytes that caps to `a`×61: `𠀀` is a four-byte letter with
/// no case, so it neither ends the run nor splits it.
const CAPPED: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa𠀀tail";
const A61: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

const BASE: &[&str] = &[
    "use serde::Deserialize;",
    "fn requestHandler() {}",
    "request_handler and HttpRequest",
    "the request x handler",
    "my_httprequest",
    "http request",
    "größeÄnderung",
    "Foo bar",
    "foo.bar baz",
    CAPPED,
    A61,
    "FooBar serde",
    "",
];

fn corpus() -> Vec<String> {
    // Three copies, so terms recur across segments.
    (0..3)
        .flat_map(|_| BASE.iter().map(|s| s.to_string()))
        .collect()
}

/// An index over `docs`, covering those below `followed` except the
/// `unreadable` ones, written a few documents per segment.
struct Fixture {
    _dir: Scratch,
    view: std::sync::Arc<View>,
    docs: Vec<String>,
    live: DocSet,
}

impl Fixture {
    fn new(name: &str, followed: u32, dead: &[u32], unreadable: &[u32]) -> Self {
        let docs = corpus();
        let dir = Scratch::new(&format!("content-{name}"));
        let at_follow = DocSet::new(followed, (0..followed).filter(|d| !dead.contains(d)));
        let catalog = CatalogView {
            incarnation: [7; 16],
            live: &at_follow,
        };
        let mut writer = IndexWriter::open(&dir.0, &catalog).unwrap();
        let budget = Budget {
            bytes: 60,
            ..Budget::unbounded()
        };
        loop {
            let followed = writer
                .follow(&catalog, &budget, &mut |doc, out| {
                    if unreadable.contains(&doc) {
                        return Err(Fault::Unreadable);
                    }
                    out.extend_from_slice(docs[doc as usize].as_bytes());
                    Ok(())
                })
                .unwrap();
            if followed.stopped == Stopped::Covered {
                break;
            }
        }
        let view = writer.view();
        assert!(view.segments().len() > 2, "several segments");
        let bound = docs.len() as u32;
        let live = DocSet::new(bound, (0..bound).filter(|d| !dead.contains(d)));
        Self {
            _dir: dir,
            view,
            docs,
            live,
        }
    }

    fn pinned(&self) -> Pinned<'_> {
        Pinned::new(&self.view, &self.live)
    }

    /// The atom's candidates over live documents.
    fn candidates(&self, arg: &str, case: bool) -> Vec<(u32, Certainty)> {
        let pinned = self.pinned();
        let atom = TextAtom::read(Text::new(arg.as_bytes(), case).unwrap(), &pinned).unwrap();
        walk(pinned.top(atom.cursor(&pinned)))
    }

    /// Live documents the verifier says hold the atom.
    fn truth(&self, arg: &str, case: bool) -> Vec<u32> {
        let text = Text::new(arg.as_bytes(), case).unwrap();
        let mut matcher = TextMatcher::new();
        self.live
            .range(0, self.live.bound())
            .filter(|&d| matcher.is_match(&text, self.docs[d as usize].as_bytes()))
            .collect()
    }

    /// Candidates, Yes kept and Maybe verified, must equal the truth.
    fn check(&self, arg: &str, case: bool) -> Vec<(u32, Certainty)> {
        let candidates = self.candidates(arg, case);
        let text = Text::new(arg.as_bytes(), case).unwrap();
        let mut matcher = TextMatcher::new();
        let answered: Vec<u32> = candidates
            .iter()
            .filter(|&&(d, certainty)| {
                certainty == Certainty::Yes
                    || matcher.is_match(&text, self.docs[d as usize].as_bytes())
            })
            .map(|&(d, _)| d)
            .collect();
        assert_eq!(answered, self.truth(arg, case), "text:{arg} case={case}");
        candidates
    }
}

fn walk(mut cursor: Cursor<'_>) -> Vec<(u32, Certainty)> {
    let mut out = Vec::new();
    let mut target = 0;
    while let Some((doc, certainty)) = cursor.next_geq(target) {
        out.push((doc, certainty));
        target = doc + 1;
    }
    out
}

const QUERIES: &[&str] = &[
    "serde",
    "request handler",
    "requestHandler",
    "HttpRequest",
    "http",
    "foo.bar",
    "Foo",
    "größe änderung",
    "x",
    "absent",
    A61,
    CAPPED,
];

fn certainties(candidates: &[(u32, Certainty)]) -> BTreeSet<Certainty> {
    candidates.iter().map(|&(_, c)| c).collect()
}

#[test]
fn every_query_is_exact_after_verification_whatever_the_coverage() {
    let fixtures = [
        Fixture::new("full", 39, &[], &[]),
        Fixture::new("partial", 20, &[], &[]),
        Fixture::new("unreadable", 39, &[], &[1, 5, 14]),
        Fixture::new("dead", 30, &[0, 2, 11, 13, 31], &[4]),
    ];
    for fixture in &fixtures {
        for arg in QUERIES {
            for case in [false, true] {
                let candidates = fixture.check(arg, case);
                assert!(
                    candidates.iter().all(|&(d, _)| fixture.live.contains(d)),
                    "text:{arg}: a dead document"
                );
            }
        }
    }
}

#[test]
fn a_term_is_exact_and_a_phrase_is_maybe_under_full_coverage() {
    let fixture = Fixture::new("shapes", 39, &[], &[]);
    use Certainty::{Maybe, Yes};
    assert_eq!(certainties(&fixture.check("serde", false)), [Yes].into());
    assert_eq!(certainties(&fixture.check("absent", false)), [].into());
    assert_eq!(
        certainties(&fixture.check("request handler", false)),
        [Maybe].into()
    );
    // `HttpRequest`: its whole token is Yes, its parts in sequence Maybe.
    let found = fixture.check("HttpRequest", false);
    assert!(found.contains(&(4, Yes)), "my_httprequest emits it");
    assert!(found.contains(&(2, Yes)), "HttpRequest itself");
    assert!(found.contains(&(5, Maybe)), "http request, verified");
    // Under `case:` nothing is Yes.
    assert_eq!(certainties(&fixture.check("serde", true)), [Maybe].into());
}

/// A query token at the cap's reach is Maybe: `a`×61 is stored for both
/// `A61` and `CAPPED`, and only the verifier can tell them apart.
#[test]
fn a_term_a_longer_token_caps_to_is_verified() {
    let fixture = Fixture::new("cap", 39, &[], &[]);
    let found = fixture.check(A61, false);
    assert!(found.contains(&(9, Certainty::Maybe)), "{found:?}");
    assert!(found.contains(&(10, Certainty::Maybe)), "{found:?}");
    assert_eq!(fixture.truth(A61, false), [10, 23, 36]);
}

#[test]
fn uncovered_documents_are_maybe_for_every_atom() {
    let fixture = Fixture::new("uncovered", 20, &[], &[3]);
    let found = fixture.check("absent", false);
    let uncovered: Vec<(u32, Certainty)> = std::iter::once(3)
        .chain(20..39)
        .map(|d| (d, Certainty::Maybe))
        .collect();
    assert_eq!(found, uncovered);
}

/// `NOT text:x`, where x's only document is uncovered: Maybe there, not
/// No. A covered document holding x is No.
#[test]
fn not_of_a_term_only_an_uncovered_document_holds_is_maybe() {
    let fixture = Fixture::new("not", 38, &[], &[]);
    let pinned = fixture.pinned();
    // `größe` is in documents 6, 19 and 32; 38 is uncovered and holds
    // nothing at all.
    for (arg, maybe, no) in [("absent", vec![38], vec![]), ("größe", vec![38], vec![6, 19, 32])] {
        let atom = TextAtom::read(Text::new(arg.as_bytes(), false).unwrap(), &pinned).unwrap();
        let found = walk(pinned.top(pinned.not(atom.cursor(&pinned))));
        for d in 0..39 {
            let expected = if no.contains(&d) {
                None
            } else if maybe.contains(&d) {
                Some(Certainty::Maybe)
            } else {
                Some(Certainty::Yes)
            };
            let got = found.iter().find(|&&(f, _)| f == d).map(|&(_, c)| c);
            assert_eq!(got, expected, "NOT text:{arg}, document {d}");
        }
    }
}
