//! File transactions, bounded record caching and worker-owned line rendering.
//! The parent host supplies operands/policy; query's existing scheduler owns
//! parallelism. A bounded completion queue feeds the calling publisher.

mod capture;
mod job;
mod publish;
mod read;
mod render;
mod source;
#[cfg(test)]
mod tests;
mod walk;

use super::*;
use std::sync::{Mutex, MutexGuard, PoisonError};

pub(super) use job::{Observed, observe_streaming};
pub(super) use publish::run;
#[cfg(test)]
pub(super) use read::read_file;
pub(super) use read::stream;
pub(super) use render::{Render, emit};
pub(super) use source::{Candidates, Source};
pub(super) use walk::{Walk, walk};

pub(super) const GROUP_BYTES: usize = 4 << 20;
const ENTRY_BYTES: usize = 64 << 10;
const CACHE_BYTES: usize = 16 << 20;
const CHUNK: usize = 64 << 10;

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}
