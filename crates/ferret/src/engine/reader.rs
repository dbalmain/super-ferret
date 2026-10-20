//! Crawl-to-query read adaptation. The crawl owns the stat/read bracket and
//! directory capabilities; this module preserves faults across that boundary.

use ferret_catalog::Catalog;
use ferret_crawl::{ContentFault, Documents};
use ferret_query::{
    CheckedReader, ReadFault, ReadOperation, ReadOutcome, ReadRequest, ReadVersion,
};

pub(crate) fn checked<'a>(
    catalog: &'a Catalog,
    documents: &'a Documents,
) -> Box<CheckedReader<'a>> {
    let mut reader = documents.fork().holding_directories();
    Box::new(move |name, request| {
        let result = match request {
            ReadRequest::Stat => reader.stat_name(catalog, name),
            ReadRequest::Bytes(bytes, stop) => {
                reader.read_search_name_until(catalog, name, bytes, stop)
            }
            ReadRequest::Stream(each, stop) => {
                reader.stream_search_name_until(catalog, name, each, stop)
            }
        };
        match result {
            Ok(observed) => {
                let recorded = catalog.inode(catalog.name(name).child).stat;
                ReadOutcome::Stable(if recorded.same_version(&observed) {
                    ReadVersion::Catalogued
                } else {
                    ReadVersion::Current
                })
            }
            Err(error) => {
                if matches!(error, ContentFault::Changed) {
                    reader = documents.fork().holding_directories();
                }
                fault(error, true)
            }
        }
    })
}

/// Direct paths have no catalog membership: a missing explicit operand is a
/// read error, unlike a vanished snapshot candidate. All use the crawl
/// bracket; zero-size regular files read to actual EOF within that check.
pub(crate) fn direct(
    path: &std::path::Path,
    cap: Option<u64>,
    each: ferret_query::Consumer<'_>,
    cancelled: &dyn Fn() -> bool,
) -> ReadOutcome {
    match ferret_crawl::content::stream_search_file_until(path, cap, each, cancelled) {
        Ok(_) => ReadOutcome::Stable(ReadVersion::Current),
        Err(error) => fault(error, false),
    }
}

/// The directory a walked file was listed in.
#[derive(Debug)]
pub(crate) enum Parent {
    /// Still open, as the walk retained it.
    Open(std::sync::Arc<std::fs::File>),
    /// Closed by a walk past its descriptor budget: its `(dev, ino)`, which
    /// the directory reopened by path must still have.
    Closed(u64, u64),
}

/// A walked file, by its name beneath the directory it was listed in,
/// never through a symlink; otherwise as [`direct`]. A closed parent
/// reopened as another directory reads as changed.
pub(crate) fn listed(
    parent: &Parent,
    path: &std::path::Path,
    cap: Option<u64>,
    each: ferret_query::Consumer<'_>,
    cancelled: &dyn Fn() -> bool,
) -> ReadOutcome {
    use std::os::fd::AsFd;
    use std::os::unix::fs::MetadataExt;
    let Some(name) = path.file_name() else {
        return ReadOutcome::Failed(ReadFault::Io(
            ReadOperation::Open,
            std::io::Error::other("a walked file has no name"),
        ));
    };
    let reopened;
    let directory = match parent {
        Parent::Open(directory) => directory.as_fd(),
        &Parent::Closed(dev, ino) => {
            let spelled = path
                .parent()
                .filter(|parent| !parent.as_os_str().is_empty());
            let opened = std::fs::File::open(spelled.unwrap_or(".".as_ref()))
                .and_then(|directory| Ok((directory.metadata()?, directory)));
            reopened = match opened {
                Ok((stat, directory)) if (stat.dev(), stat.ino()) == (dev, ino) => directory,
                Ok(_) => return ReadOutcome::RetryChanged,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    return ReadOutcome::RetryChanged;
                }
                Err(error) => {
                    return ReadOutcome::Failed(ReadFault::Io(ReadOperation::Open, error));
                }
            };
            reopened.as_fd()
        }
    };
    match ferret_crawl::content::stream_search_listed_until(directory, name, cap, each, cancelled) {
        Ok(_) => ReadOutcome::Stable(ReadVersion::Current),
        Err(error) => fault(error, false),
    }
}

fn fault(error: ContentFault, vanished: bool) -> ReadOutcome {
    let (operation, error) = match error {
        ContentFault::Changed | ContentFault::Alias => return ReadOutcome::RetryChanged,
        ContentFault::Cancelled => return ReadOutcome::Cancelled,
        ContentFault::TooLarge { size, cap } => {
            return ReadOutcome::Failed(ReadFault::TooLarge { size, cap });
        }
        ContentFault::Allocation(error) => {
            return ReadOutcome::Failed(ReadFault::Allocation(error));
        }
        ContentFault::Open(error) => (ReadOperation::Open, error),
        ContentFault::Stat(error) => (ReadOperation::Stat, error),
        ContentFault::Read(error) => (ReadOperation::Read, error),
    };
    if vanished && error.kind() == std::io::ErrorKind::NotFound {
        ReadOutcome::Vanished
    } else {
        ReadOutcome::Failed(ReadFault::Io(operation, error))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io;

    #[test]
    fn crawl_fault_mapping_preserves_operation_size_and_retry() {
        for (faulted, expected) in [
            (
                ContentFault::Open(io::ErrorKind::PermissionDenied.into()),
                ReadOperation::Open,
            ),
            (
                ContentFault::Stat(io::ErrorKind::Other.into()),
                ReadOperation::Stat,
            ),
            (
                ContentFault::Read(io::ErrorKind::UnexpectedEof.into()),
                ReadOperation::Read,
            ),
        ] {
            assert!(
                matches!(fault(faulted, true), ReadOutcome::Failed(ReadFault::Io(operation, _)) if operation == expected)
            );
        }
        assert!(matches!(
            fault(ContentFault::Changed, true),
            ReadOutcome::RetryChanged
        ));
        assert!(matches!(
            fault(ContentFault::Cancelled, true),
            ReadOutcome::Cancelled
        ));
        assert!(matches!(
            fault(ContentFault::Open(io::ErrorKind::NotFound.into()), true),
            ReadOutcome::Vanished
        ));
        assert!(matches!(
            fault(ContentFault::TooLarge { size: 9, cap: 8 }, true),
            ReadOutcome::Failed(ReadFault::TooLarge { size: 9, cap: 8 })
        ));
    }
}
