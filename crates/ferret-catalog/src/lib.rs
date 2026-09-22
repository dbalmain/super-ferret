//! Names, inodes and documents: the only mutable state in Super Ferret.
//!
//! `(parent inode, name) → inode → document → content hash`, with dense ids
//! assigned here (DECISIONS.md D4, D5). A rename, move or duplicate changes
//! this crate's tables and nothing else. Also owns the contiguous name heap
//! that filename search scans (D14), and document liveness.
//!
//! Knows nothing about tokens or postings.
