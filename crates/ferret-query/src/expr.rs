//! The boolean grammar (D62 A): `OR`, `NOT`, `(` and `)` as whole
//! arguments over atoms, parsed into a [`Node`] tree, and the Kleene
//! evaluation of a tree on one row.
//!
//! ```text
//! query = expr?
//! expr  = and ("OR" and)*
//! and   = unary+
//! unary = "NOT" unary | "(" expr ")" | atom
//! ```
//!
//! Adjacent atoms AND, which binds tighter than `OR`; `NOT` binds tightest.
//! [`crate::Query`] flattens the top-level AND: its plain name and metadata
//! atoms become S1's tests, which plan and run exactly as before, and every
//! other conjunct is kept as a tree and evaluated per row.

use crate::query::{MetaTest, NameTest, ParseError, PathTest};

/// Bound NOT and parentheses together before constructing a recursive tree.
/// Each group can add AND and OR frames, so leave ample room on the daemon
/// request thread's 2 MiB stack for evaluation, cursor compilation and Drop.
const NESTING_LIMIT: usize = 32;

/// A boolean tree over leaves `L`: lexed atoms while parsing, [`Test`]s
/// once the query is built.
#[derive(Debug)]
pub(crate) enum Node<L> {
    Leaf(L),
    And(Vec<Node<L>>),
    Or(Vec<Node<L>>),
    Not(Box<Node<L>>),
}

/// One argument, lexed: an operator or an atom.
pub(crate) enum Token<A> {
    Or,
    Not,
    Open,
    Close,
    Atom(A),
}

/// A leaf of a query's per-row tree.
#[derive(Debug)]
pub(crate) enum Test {
    Name(NameTest),
    Path(PathTest),
    Meta(MetaTest),
    /// The query's `texts[i]`, a content atom.
    Content(usize),
}

/// Three-valued truth: Kleene's logic, as the index's certainty algebra
/// uses (docs/S2.md § The Rust shape). Maybe is settled by verification.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Truth {
    No,
    Maybe,
    Yes,
}

impl From<bool> for Truth {
    fn from(holds: bool) -> Self {
        if holds { Truth::Yes } else { Truth::No }
    }
}

/// Parses lexed arguments. No arguments is the empty conjunction, which
/// every row satisfies (S1's "all names").
pub(crate) fn parse<A>(tokens: Vec<Token<A>>) -> Result<Node<A>, ParseError> {
    let mut parser = Parser {
        tokens: tokens.into_iter().peekable(),
    };
    if parser.tokens.peek().is_none() {
        return Ok(Node::And(Vec::new()));
    }
    let tree = parser.expr(After::Start, 0)?;
    match parser.tokens.next() {
        None => Ok(tree),
        // `and` stops only at the end, `OR` or `)`, and `expr` takes every
        // `OR`, so what is left is a `)` no `(` opened.
        Some(_) => Err(ParseError::Unopened),
    }
}

/// What came just before an `and`: where an empty one is an error, and
/// which.
#[derive(Clone, Copy, PartialEq, Eq)]
enum After {
    Start,
    Or,
    Open,
}

struct Parser<A> {
    tokens: std::iter::Peekable<std::vec::IntoIter<Token<A>>>,
}

impl<A> Parser<A> {
    fn expr(&mut self, after: After, depth: usize) -> Result<Node<A>, ParseError> {
        let mut any = vec![self.and(after, depth)?];
        while matches!(self.tokens.peek(), Some(Token::Or)) {
            self.tokens.next();
            any.push(self.and(After::Or, depth)?);
        }
        Ok(one_or(any, Node::Or))
    }

    fn and(&mut self, after: After, depth: usize) -> Result<Node<A>, ParseError> {
        let mut all = Vec::new();
        while !matches!(self.tokens.peek(), None | Some(Token::Or | Token::Close)) {
            all.push(self.unary(depth)?);
        }
        if all.is_empty() {
            return Err(match (self.tokens.peek(), after) {
                (Some(Token::Or), _) | (_, After::Or) => ParseError::Dangling("OR"),
                (_, After::Open) => ParseError::EmptyGroup,
                _ => ParseError::Unopened,
            });
        }
        Ok(one_or(all, Node::And))
    }

    fn unary(&mut self, depth: usize) -> Result<Node<A>, ParseError> {
        match self.tokens.next() {
            Some(Token::Not) => {
                if depth >= NESTING_LIMIT {
                    return Err(ParseError::Nesting {
                        limit: NESTING_LIMIT,
                    });
                }
                if matches!(self.tokens.peek(), None | Some(Token::Or | Token::Close)) {
                    return Err(ParseError::Dangling("NOT"));
                }
                Ok(Node::Not(Box::new(self.unary(depth + 1)?)))
            }
            Some(Token::Open) => {
                if depth >= NESTING_LIMIT {
                    return Err(ParseError::Nesting {
                        limit: NESTING_LIMIT,
                    });
                }
                let inner = self.expr(After::Open, depth + 1)?;
                match self.tokens.next() {
                    Some(Token::Close) => Ok(inner),
                    _ => Err(ParseError::Unclosed),
                }
            }
            Some(Token::Atom(atom)) => Ok(Node::Leaf(atom)),
            // `and` peeks before calling: only these three start a unary.
            Some(Token::Or | Token::Close) | None => Err(ParseError::Unopened),
        }
    }
}

fn one_or<A>(mut nodes: Vec<Node<A>>, many: fn(Vec<Node<A>>) -> Node<A>) -> Node<A> {
    if nodes.len() == 1 {
        nodes.swap_remove(0)
    } else {
        many(nodes)
    }
}

impl<L> Node<L> {
    /// The conjuncts of the top-level AND, nested ANDs (a parenthesised
    /// group) flattened into it.
    pub(crate) fn into_conjuncts(self) -> Vec<Node<L>> {
        match self {
            Node::And(all) => all.into_iter().flat_map(Node::into_conjuncts).collect(),
            other => vec![other],
        }
    }

    /// The same tree with each leaf replaced.
    pub(crate) fn map<M>(self, f: &mut impl FnMut(L) -> M) -> Node<M> {
        match self {
            Node::Leaf(leaf) => Node::Leaf(f(leaf)),
            Node::And(all) => Node::And(all.into_iter().map(|n| n.map(f)).collect()),
            Node::Or(any) => Node::Or(any.into_iter().map(|n| n.map(f)).collect()),
            Node::Not(inner) => Node::Not(Box::new(inner.map(f))),
        }
    }

    /// Whether every leaf satisfies `f`.
    pub(crate) fn all_leaves(&self, f: &impl Fn(&L) -> bool) -> bool {
        match self {
            Node::Leaf(leaf) => f(leaf),
            Node::And(nodes) | Node::Or(nodes) => nodes.iter().all(|n| n.all_leaves(f)),
            Node::Not(inner) => inner.all_leaves(f),
        }
    }
}

impl Node<Test> {
    /// Kleene evaluation, short-circuiting: a leaf after a deciding sibling
    /// is never asked, so a Maybe there costs no verification.
    pub(crate) fn eval(&self, leaf: &mut impl FnMut(&Test) -> Truth) -> Truth {
        match self {
            Node::Leaf(test) => leaf(test),
            Node::And(all) => {
                let mut result = Truth::Yes;
                for node in all {
                    match node.eval(leaf) {
                        Truth::No => return Truth::No,
                        Truth::Maybe => result = Truth::Maybe,
                        Truth::Yes => {}
                    }
                }
                result
            }
            Node::Or(any) => {
                let mut result = Truth::No;
                for node in any {
                    match node.eval(leaf) {
                        Truth::Yes => return Truth::Yes,
                        Truth::Maybe => result = Truth::Maybe,
                        Truth::No => {}
                    }
                }
                result
            }
            Node::Not(inner) => match inner.eval(leaf) {
                Truth::No => Truth::Yes,
                Truth::Maybe => Truth::Maybe,
                Truth::Yes => Truth::No,
            },
        }
    }

    /// Whether every leaf is a content atom.
    pub(crate) fn is_content(&self) -> bool {
        self.all_leaves(&|test| matches!(test, Test::Content(_)))
    }

    /// For a content-only tree, whether a row with no document satisfies
    /// it: every content atom is false there (D4), so `NOT text:x` holds.
    pub(crate) fn holds_without_document(&self) -> bool {
        self.eval(&mut |_| Truth::No) == Truth::Yes
    }

    /// The tree in words, for [`crate::Query::explain`].
    pub(crate) fn describe(&self, leaf: &impl Fn(&Test) -> String) -> String {
        let join = |nodes: &[Node<Test>], with: &str| {
            let parts: Vec<String> = nodes.iter().map(|n| n.describe(leaf)).collect();
            format!("({})", parts.join(with))
        };
        match self {
            Node::Leaf(test) => leaf(test),
            Node::And(all) => join(all, " AND "),
            Node::Or(any) => join(any, " OR "),
            Node::Not(inner) => format!("NOT {}", inner.describe(leaf)),
        }
    }
}
