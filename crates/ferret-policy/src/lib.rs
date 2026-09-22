//! Exclusion policy: `should_index(path, meta) -> Decision`.
//!
//! Owns the precedence of `.ferretignore`, `.gitignore`, the user's global
//! ignore file and the built-in defaults (DECISIONS.md D13), plus the size cap
//! and binary check that separate *catalogued* from *content-indexed*.
//!
//! Knows nothing about the catalog or the index: it is a pure decision over a
//! path and its metadata, tested against a golden corpus.
