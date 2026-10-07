//! The certainty algebra, against a three-valued evaluator written here as
//! the specification. Random trees over random per-document truth: each
//! leaf's Yes documents are real postings in real segments (with gaps
//! between their ranges) plus bitmaps for the gaps, and its Maybe documents
//! a bitmap, as uncovered documents are. Trees are compiled with the real
//! constructors, so `Or` materialises whenever its estimate says to.

use std::collections::BTreeMap;

use super::*;
use crate::postings::Term;
use crate::segment::{Segment, Writer};

/// Seeded xorshift64*, matching the idiom in `segment/tests.rs`.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
}

// ── The specification ──

/// A truth value: No < Maybe < Yes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Truth {
    No,
    Maybe,
    Yes,
}

#[derive(Clone, Debug)]
enum Expr {
    Leaf(usize),
    And(Vec<Expr>),
    Or(Vec<Expr>),
    Not(Box<Expr>),
    /// The phrase rule: whatever the child says, at best Maybe.
    Maybe(Box<Expr>),
    /// A driver and a probed leaf, as the live filter and S3's
    /// per-document filters are applied: an And whose leaf never drives.
    Filter(Box<Expr>, usize),
}

/// Kleene's logic, from the truth table and nothing else: AND is the
/// minimum, OR the maximum, NOT swaps Yes and No and keeps Maybe.
fn eval(expr: &Expr, leaves: &[Vec<Truth>], doc: usize) -> Truth {
    match expr {
        Expr::Leaf(i) => leaves[*i][doc],
        Expr::And(children) => children
            .iter()
            .map(|c| eval(c, leaves, doc))
            .min()
            .unwrap_or(Truth::Yes),
        Expr::Or(children) => children
            .iter()
            .map(|c| eval(c, leaves, doc))
            .max()
            .unwrap_or(Truth::No),
        Expr::Not(child) => match eval(child, leaves, doc) {
            Truth::Yes => Truth::No,
            Truth::Maybe => Truth::Maybe,
            Truth::No => Truth::Yes,
        },
        Expr::Maybe(child) => eval(child, leaves, doc).min(Truth::Maybe),
        Expr::Filter(child, i) => eval(child, leaves, doc).min(leaves[*i][doc]),
    }
}

fn truth(found: Option<Certainty>) -> Truth {
    match found {
        None => Truth::No,
        Some(Certainty::Maybe) => Truth::Maybe,
        Some(Certainty::Yes) => Truth::Yes,
    }
}

// ── The world: leaves as real cursors ──

/// One generated case: `docs` documents, `leaves[i][d]` the truth of leaf
/// `i` for document `d`, and the structures that answer them.
struct World {
    docs: u32,
    leaves: Vec<Vec<Truth>>,
    segments: Vec<Segment<Vec<u8>>>,
    /// Per leaf: Yes documents outside every segment's range.
    gaps: Vec<DocSet>,
    /// Per leaf: Maybe documents.
    maybes: Vec<DocSet>,
    /// Every document, for NOT with nothing beside it.
    all: DocSet,
    /// What `Cursor::or` sizes its bitmaps with. Larger than `docs` makes
    /// materialising rarer, so both `Or` shapes are drawn.
    bound: u32,
}

impl World {
    fn new(rng: &mut Rng) -> Self {
        let docs = 1 + rng.below(300) as u32;
        let leaf_count = 1 + rng.below(5);
        let leaves: Vec<Vec<Truth>> = (0..leaf_count)
            .map(|_| {
                // Skewed per leaf, so some leaves are sparse and some dense.
                let (yes, maybe) = (rng.below(60), rng.below(30));
                (0..docs)
                    .map(|_| match rng.below(100) {
                        p if p < yes => Truth::Yes,
                        p if p < yes + maybe => Truth::Maybe,
                        _ => Truth::No,
                    })
                    .collect()
            })
            .collect();

        // Up to four segments over disjoint ascending ranges, with gaps.
        let mut ranges = Vec::new();
        let mut at = rng.below(4) as u32;
        while at < docs && ranges.len() < 4 {
            let last = (at + rng.below(docs as usize) as u32).min(docs - 1);
            ranges.push((at, last));
            at = last + 1 + rng.below(8) as u32;
        }
        let covered = |d: u32| ranges.iter().any(|&(f, l)| f <= d && d <= l);
        let docs_of = |leaf: &[Truth], want: Truth, inside: bool| -> Vec<u32> {
            (0..docs)
                .filter(|&d| leaf[d as usize] == want && covered(d) == inside)
                .collect()
        };
        let segments = ranges
            .iter()
            .map(|&(first, last)| {
                let mut terms = BTreeMap::new();
                for (i, leaf) in leaves.iter().enumerate() {
                    let held: Vec<u32> = docs_of(leaf, Truth::Yes, true)
                        .into_iter()
                        .filter(|&d| first <= d && d <= last)
                        .collect();
                    if !held.is_empty() {
                        terms.insert(format!("t{i}").into_bytes(), held);
                    }
                }
                let mut writer = Writer::new(first, last).unwrap();
                for (term, held) in &terms {
                    writer.push(term, held).unwrap();
                }
                let mut bytes = Vec::new();
                writer.finish(&mut bytes).unwrap();
                Segment::open(bytes).unwrap()
            })
            .collect();
        let bound = if rng.below(2) == 0 { docs } else { docs * 64 };
        Self {
            docs,
            gaps: leaves
                .iter()
                .map(|l| DocSet::new(bound, docs_of(l, Truth::Yes, false)))
                .collect(),
            maybes: leaves
                .iter()
                .map(|l| {
                    let all: Vec<u32> = (0..docs)
                        .filter(|&d| l[d as usize] == Truth::Maybe)
                        .collect();
                    DocSet::new(bound, all)
                })
                .collect(),
            all: DocSet::new(bound, 0..docs),
            leaves,
            segments,
            bound,
        }
    }

    fn terms(&self) -> Vec<Term> {
        (0..self.leaves.len())
            .map(|i| Term::read(&self.segments, format!("t{i}").as_bytes()).unwrap())
            .collect()
    }

    fn leaf<'a>(&'a self, terms: &'a [Term], i: usize) -> Cursor<'a> {
        Cursor::or(
            vec![
                Cursor::Postings(Box::new(terms[i].cursor())),
                Cursor::bits(&self.gaps[i], Certainty::Yes),
                Cursor::bits(&self.maybes[i], Certainty::Maybe),
            ],
            self.bound,
        )
    }

    /// The planner's shapes: a NOT beside positive conjuncts becomes
    /// `AndNot` on their conjunction; a NOT alone is relative to every
    /// document.
    fn compile<'a>(&'a self, terms: &'a [Term], expr: &Expr) -> Cursor<'a> {
        match expr {
            Expr::Leaf(i) => self.leaf(terms, *i),
            Expr::And(children) => {
                let (negated, positive): (Vec<&Expr>, Vec<&Expr>) =
                    children.iter().partition(|c| matches!(c, Expr::Not(_)));
                let mut cursor = if positive.is_empty() {
                    Cursor::bits(&self.all, Certainty::Yes)
                } else {
                    Cursor::and(positive.iter().map(|c| self.compile(terms, c)).collect())
                };
                for not in negated {
                    let Expr::Not(inner) = not else {
                        unreachable!("partitioned on Not")
                    };
                    cursor = Cursor::and_not(cursor, self.compile(terms, inner));
                }
                cursor
            }
            Expr::Or(children) => Cursor::or(
                children.iter().map(|c| self.compile(terms, c)).collect(),
                self.bound,
            ),
            Expr::Not(inner) => Cursor::and_not(
                Cursor::bits(&self.all, Certainty::Yes),
                self.compile(terms, inner),
            ),
            Expr::Maybe(inner) => Cursor::maybe(self.compile(terms, inner)),
            Expr::Filter(inner, i) => Cursor::filter(
                self.compile(terms, inner),
                Probe::Cursor(self.leaf(terms, *i)),
            ),
        }
    }
}

fn expr(rng: &mut Rng, leaves: usize, depth: usize) -> Expr {
    if depth == 0 || rng.below(4) == 0 {
        return Expr::Leaf(rng.below(leaves));
    }
    let children = |rng: &mut Rng| -> Vec<Expr> {
        (0..1 + rng.below(3))
            .map(|_| expr(rng, leaves, depth - 1))
            .collect()
    };
    let child = |rng: &mut Rng| Box::new(expr(rng, leaves, depth - 1));
    match rng.below(10) {
        0..=2 => Expr::And(children(rng)),
        3..=5 => Expr::Or(children(rng)),
        6 | 7 => Expr::Not(child(rng)),
        8 => Expr::Maybe(child(rng)),
        _ => Expr::Filter(child(rng), rng.below(leaves)),
    }
}

/// One case: a full walk, then a walk with random skips and repeated
/// targets, each against the evaluator.
fn check(seed: u64) {
    let mut rng = Rng(seed | 1);
    let world = World::new(&mut rng);
    let tree = expr(&mut rng, world.leaves.len(), 4);
    let expected: Vec<Truth> = (0..world.docs as usize)
        .map(|d| eval(&tree, &world.leaves, d))
        .collect();
    let terms = world.terms();

    let mut cursor = world.compile(&terms, &tree);
    let mut found = vec![Truth::No; world.docs as usize];
    let mut target = 0;
    while let Some((doc, certainty)) = cursor.next_geq(target) {
        assert!(doc >= target, "seed {seed}: {doc} below target {target}");
        found[doc as usize] = truth(Some(certainty));
        target = doc + 1;
    }
    assert_eq!(found, expected, "seed {seed}: {tree:?}");

    let mut cursor = world.compile(&terms, &tree);
    let mut target = 0;
    loop {
        let want = (target as usize..expected.len())
            .find(|&d| expected[d] != Truth::No)
            .map(|d| (d as u32, expected[d]));
        let got = cursor.next_geq(target);
        assert_eq!(
            got.map(|(d, c)| (d, truth(Some(c)))),
            want,
            "seed {seed}: next_geq({target}) on {tree:?}"
        );
        let Some((doc, _)) = got else { break };
        // Stays on its document for a repeated or lower target.
        assert_eq!(cursor.next_geq(target).map(|(d, _)| d), Some(doc));
        target = doc + 1 + rng.below(6) as u32;
    }
}

#[test]
fn the_algebra_matches_kleenes_logic_on_random_trees() {
    for seed in 0..400 {
        check(seed);
    }
}

#[test]
#[ignore = "long property run; run with --ignored"]
fn the_algebra_matches_kleenes_logic_on_many_random_trees() {
    for seed in 0..100_000 {
        check(seed);
    }
}

// ── Discriminating cases ──

fn walk(mut cursor: Cursor<'_>) -> Vec<(u32, Certainty)> {
    let mut out = Vec::new();
    let mut target = 0;
    while let Some((doc, certainty)) = cursor.next_geq(target) {
        out.push((doc, certainty));
        target = doc + 1;
    }
    out
}

/// `NOT x`, where x's only document is uncovered: x is Maybe there, so NOT
/// x is Maybe, not No. A per-source exact flag would have dropped it.
#[test]
fn not_of_an_uncovered_only_document_is_maybe() {
    let live = DocSet::new(4, 0..4);
    let uncovered = DocSet::new(4, [2]);
    // x's postings are empty: its tree is the uncovered bitmap alone, as
    // `Pinned::atom` builds it.
    let x = Cursor::or(
        vec![Cursor::Empty, Cursor::bits(&uncovered, Certainty::Maybe)],
        4,
    );
    let not_x = Cursor::and_not(Cursor::bits(&live, Certainty::Yes), x);
    use Certainty::{Maybe, Yes};
    assert_eq!(walk(not_x), [(0, Yes), (1, Yes), (2, Maybe), (3, Yes)]);
}

/// `a AND NOT b`, where b is Maybe for a document that is Yes for a: the
/// document survives as Maybe. Dropping it, or keeping a's Yes, are the
/// two plausible wrong answers.
#[test]
fn a_and_not_maybe_b_is_maybe() {
    let a = DocSet::new(8, [1, 3, 5]);
    let b_yes = DocSet::new(8, [1]);
    let b_maybe = DocSet::new(8, [3]);
    let b = Cursor::Or(Or::new(vec![
        Cursor::bits(&b_yes, Certainty::Yes),
        Cursor::bits(&b_maybe, Certainty::Maybe),
    ]));
    let cursor = Cursor::and_not(Cursor::bits(&a, Certainty::Yes), b);
    use Certainty::{Maybe, Yes};
    assert_eq!(walk(cursor), [(3, Maybe), (5, Yes)]);
}

#[test]
fn a_wide_or_materialises_and_keeps_certainties() {
    let sets: Vec<DocSet> = (0..=OR_WIDTH as u32)
        .map(|i| DocSet::new(1000, [i, 500 + i]))
        .collect();
    let children = sets
        .iter()
        .enumerate()
        .map(|(i, s)| {
            let certainty = if i % 2 == 0 {
                Certainty::Yes
            } else {
                Certainty::Maybe
            };
            Cursor::bits(s, certainty)
        })
        .collect();
    // 65 children of two documents each: wide, though not dense.
    let cursor = Cursor::or(children, 1000);
    let Cursor::Or(or) = &cursor else {
        panic!("expected the materialised pair")
    };
    assert!(or.children.iter().all(|c| matches!(
        c,
        Cursor::Bits(Bits {
            docs: Cow::Owned(_),
            ..
        })
    )));
    let got = walk(cursor);
    assert_eq!(got.len(), 2 * (OR_WIDTH + 1));
    assert_eq!(got[0], (0, Certainty::Yes));
    assert_eq!(got[1], (1, Certainty::Maybe));
}

#[test]
fn materialisation_checks_cancellation_between_documents() {
    let set = DocSet::new(100_000, 0..100_000);
    let checks = std::cell::Cell::new(0);
    let cancelled = || {
        checks.set(checks.get() + 1);
        checks.get() == 10
    };
    assert!(matches!(
        Cursor::or_until(
            vec![
                Cursor::bits(&set, Certainty::Yes),
                Cursor::bits(&set, Certainty::Maybe)
            ],
            set.bound(),
            &cancelled
        ),
        Err(crate::ReadError::Cancelled)
    ));
    assert_eq!(checks.get(), 10);
}
