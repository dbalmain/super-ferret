//! How a job's bytes are read: explicit and walked files, catalogued
//! documents, the size cap and the binary probe.

use super::*;
use ferret_query::{ReadOperation, ReadVersion};
use std::io::BufRead;

/// Reads an explicit operand by `path`, following symlinks, or, given the
/// `listed` directory a walk found it in, a walked file by its name beneath
/// that directory without following one (D21).
pub(in super::super) fn read_file(
    path: &Path,
    listed: Option<&engine::reader::Parent>,
    plan: &Plan,
    each: ferret_query::Consumer<'_>,
    stopped: &dyn Fn() -> bool,
) -> ReadOutcome {
    read_file_policy(path, listed, plan, each, stopped, false, false)
}

pub(super) fn read_file_policy(
    path: &Path,
    listed: Option<&engine::reader::Parent>,
    plan: &Plan,
    each: ferret_query::Consumer<'_>,
    stopped: &dyn Fn() -> bool,
    rg: bool,
    once: bool,
) -> ReadOutcome {
    let recursive = listed.is_some();
    let cap = (recursive && !plan.traversal.unrestricted && !rg)
        .then(|| ferret_policy::Config::default().size_cap);
    let mut pass = |input: &mut dyn Read, size| {
        if recursive {
            read_content(input, size, plan, rg, each)
        } else {
            each(input, size)
        }
    };
    match listed {
        None => engine::reader::direct(path, cap, &mut pass, stopped),
        Some(parent) => engine::reader::listed(parent, path, !once, cap, &mut pass, stopped),
    }
}

// The size cap bounds content indexing, not grep. Catalogued oversized files
// and candidates that grew past the cap use rg's initial binary window.
pub(super) fn read_catalogued(
    read: &mut CheckedReader<'_>,
    name: NameId,
    plan: &Plan,
    each: ferret_query::Consumer<'_>,
    stopped: &dyn Fn() -> bool,
    unindexed: bool,
) -> ReadOutcome {
    let mut pass = |input: &mut dyn Read, size| {
        if unindexed || size > ferret_policy::DEFAULT_SIZE_CAP {
            read_content(input, size, plan, true, each)
        } else {
            each(input, size)
        }
    };
    read(name, ReadRequest::Stream(&mut pass, stopped))
}

fn read_content(
    input: &mut dyn Read,
    size: u64,
    plan: &Plan,
    rg: bool,
    each: ferret_query::Consumer<'_>,
) -> io::Result<()> {
    if plan.text {
        return each(input, size);
    }
    // Rg recursion probes its first scan window even in quiet/name modes.
    // D13 --live retains the indexing sniffer instead.
    let rg = rg || plan.traversal.unrestricted;
    let probe = if rg { CHUNK } else { ferret_policy::SNIFF_LEN };
    let mut buffered = io::BufReader::with_capacity(probe, input);
    let head = buffered.fill_buf()?;
    let binary = if rg {
        line_grep::binary_window(head)
    } else {
        ferret_policy::sniff(head) == ferret_policy::Content::Binary
    };
    if binary {
        Ok(())
    } else {
        each(&mut buffered, size)
    }
}

pub(in super::super) fn stream(
    input: &mut dyn Read,
    size: u64,
    each: ferret_query::Consumer<'_>,
    stopped: &dyn Fn() -> bool,
) -> ReadOutcome {
    match each(input, size) {
        Ok(()) => ReadOutcome::Stable(ReadVersion::Current),
        Err(_) if stopped() => ReadOutcome::Cancelled,
        Err(error) => ReadOutcome::Failed(ReadFault::Io(ReadOperation::Read, error)),
    }
}
