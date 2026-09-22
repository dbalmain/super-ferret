//! Index structures over document ids: postings, filters, trigram
//! structures — each a source of *candidate* documents for a query atom,
//! with an exactness flag and a cost estimate (DECISIONS.md D6).
//!
//! Knows doc ids and byte strings, never files, paths or inodes, so it stays
//! reusable outside desktop search. The planner sees these structures only
//! through the candidate-source interface, never their formats.
