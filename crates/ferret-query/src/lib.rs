//! Query syntax, planning, execution and result rows.
//!
//! Content atoms are planned over the index's candidate sources by estimate;
//! name and metadata atoms go to the catalog; non-exact candidates go to the
//! verifier. Results are one row per path (DECISIONS.md D15).
//!
//! Knows nothing about any structure's on-disk format — adding a structure
//! must not require reading this crate.
