//! Query syntax, planning, execution and result rows.
//!
//! Content atoms (from S2) are planned over the index's candidate sources
//! by estimate; name and metadata atoms go to the catalog; non-exact
//! candidates go to the verifier. Results are one row per path (D15).
//!
//! Knows nothing about any structure's on-disk format — adding a structure
//! must not require reading this crate.
//!
//! - `content`: [`TextAtom`], a `text:ARG` compiled into the index's cursor
//!   tree (S2).
//! - `query`: [`Query`], parsing and the plan ([`Strategy`],
//!   [`Query::explain`]).
//! - `pattern`: globs lowered to regexes; the literal a glob or regex
//!   guarantees.
//! - `run`: [`Query::run`], which loads what the strategy needs and streams
//!   [`Row`]s.
//! - `expr`: the boolean tree (D62 A) and its three-valued evaluation.
//! - `plan`: [`Query::run_content`], the content planner: driver choice,
//!   `DocId` → rows ([`DocNames`]), verification of Maybe documents.
//!
//! # Grammar
//!
//! A query is a boolean expression over atoms. Adjacent atoms AND; `OR`,
//! `NOT`, `(` and `)` are operators only as whole arguments, as in find(1).
//! AND binds tighter than OR, and NOT tightest. [`Query::from_args`] takes
//! one atom per argument, so a quoted argument with spaces is one atom.
//!
//! ```text
//! query  = expr?
//! expr   = and ("OR" and)*
//! and    = unary+
//! unary  = "NOT" unary | "(" expr ")" | atom
//! atom   = "case:" match | match | meta | "text:" ARG | "case:text:" ARG
//! match  = word | "name:" word | glob | "re:" REGEX | "path:" TEXT | "ext:" EXT
//!        | "name-term:" TOKEN
//! meta   = "size:" [<>] N [kKmMgGtT]   size in bytes, powers of 1024; no
//!                                      comparison means exactly N
//!        | "mtime:" (<|>) N (s|m|h|d|w|y)
//!                                      age: mtime:<1d is modified within a
//!                                      day, mtime:>1y more than a year ago;
//!                                      the comparison is required
//!        | "type:" (f|d|l|file|dir|link)
//! ```
//!
//! - `name-term:TOKEN` matches an exact D9-normalised basename token, including
//!   whole identifiers and camel/snake/digit parts. It always normalises case;
//!   it does not change the substring meaning of a bare word.
//! - A **word** is a substring of the name. A word containing `/` is a
//!   substring of the whole path instead, as is `path:TEXT`. `name:WORD` is a
//!   word whatever it spells, so a file named `OR` is `name:OR` (folded) or
//!   `case:OR` (exact).
//! - `text:ARG` matches file content (docs/S2.md § What `text:ARG` means): ARG
//!   goes through the content tokenizer, one token is a term, several are a
//!   phrase over their parts. `case:text:ARG` compares the original bytes. A
//!   row with no document (a directory, an unindexed file) holds no `text:`
//!   atom, so `NOT text:x` holds there (D4).
//! - A **glob** is a word containing `*`, `?` or `[`. Without a `/` it matches
//!   the whole name (`*.rs`); with one it matches the path's trailing
//!   components (`src/**/*.rs`), or the whole path if it starts with `/`. `*`
//!   and `?` stop at `/`; `**` as a component spans any number.
//! - `re:REGEX` is a regex (the `regex` crate's syntax) searched in the name,
//!   unanchored unless it anchors itself.
//! - `ext:EXT` is a name ending in `.EXT` (a name that is only `.EXT` is not
//!   one: `.rs` has no extension).
//! - **Case:** every match atom folds ASCII case, except under `case:`, which
//!   makes the one atom after it exact: `case:README`, `case:re:^[A-Z]`,
//!   `case:*.C`, `case:ext:C`. There is no smart-case rule, so a query means
//!   the same whatever letters it is typed in. Folding is ASCII-only for words,
//!   globs and `ext:`; `re:` folds with Unicode rules.
//! - A name is bytes; a match atom given bytes that are not UTF-8 matches them
//!   as bytes (a `re:` pattern must be UTF-8).
//!
//! # Planning
//!
//! Every name atom that guarantees a literal offers it: a word its text,
//! `ext:` its `.EXT`, a glob the longest literal run in its last component,
//! a regex the longest run of plain characters before its first group or
//! class (see `pattern::regex_literal` for what counts). The longest offered
//! literal drives a scan of the name heap ([`Strategy::HeapScan`]); every
//! other atom filters the names it hits. A query with metadata atoms and no
//! literal tests the inode rows first ([`Strategy::InodeScan`]); anything
//! else tests every name ([`Strategy::AllNames`]).
//!
//! Only the top-level AND's plain name and metadata atoms plan this way. Every
//! other conjunct (an `OR`, a `NOT`, a `text:` atom) is a tree evaluated per
//! row. A query with a `text:` atom plans in [`Query::run_content`]: the
//! cheapest content conjunct that no document-less row holds drives when its
//! estimate, from the term dictionaries alone, is below the name side's;
//! otherwise the name side drives and the content atoms are probed per
//! document.

mod content;
mod expr;
pub mod find;

pub mod name_index;
mod pattern;
mod plan;
mod query;
mod run;

#[cfg(test)]
mod tests;

pub use content::TextAtom;
pub use name_index::{NameEstimate, NameIndex, NamePlan};
pub use plan::{
    AtomReport, Content, ContentReport, DocNames, ReadRequest, ReadVersion, Reader, Side,
    UNCOVERED_BOUND,
};
pub use query::{ParseError, Query, Strategy};
pub use run::{Row, RunError, Stats};
