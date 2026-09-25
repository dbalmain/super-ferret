//! The `ferret` binary's own pieces: where its files live and what setup
//! writes. Behaviour lives in the library crates; this is configuration.
//!
//! Seams: `xdg` resolves directories from the environment and touches no
//! files; `setup` writes the files a new install starts with.

pub mod setup;
pub mod xdg;
