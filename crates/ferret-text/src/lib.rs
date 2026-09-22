//! The tokenizer: alphanumeric-and-underscore runs, lowercased, plus the
//! camelCase / TitleCase / snake / digit-boundary parts (DECISIONS.md D9).
//!
//! Versioned, because segments record the version that wrote them. The one
//! place tokens are defined: indexing and query parsing both call it.
//!
//! Knows nothing about files or ids.
