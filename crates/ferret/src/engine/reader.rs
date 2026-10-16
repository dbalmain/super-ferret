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
                reader.read_current_name_until(catalog, name, bytes, stop)
            }
            ReadRequest::Stream(each, stop) => {
                reader.stream_current_name_until(catalog, name, each, stop)
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
                fault(error)
            }
        }
    })
}

fn fault(error: ContentFault) -> ReadOutcome {
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
    if error.kind() == std::io::ErrorKind::NotFound {
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
                matches!(fault(faulted), ReadOutcome::Failed(ReadFault::Io(operation, _)) if operation == expected)
            );
        }
        assert!(matches!(
            fault(ContentFault::Changed),
            ReadOutcome::RetryChanged
        ));
        assert!(matches!(
            fault(ContentFault::Cancelled),
            ReadOutcome::Cancelled
        ));
        assert!(matches!(
            fault(ContentFault::Open(io::ErrorKind::NotFound.into())),
            ReadOutcome::Vanished
        ));
        assert!(matches!(
            fault(ContentFault::TooLarge { size: 9, cap: 8 }),
            ReadOutcome::Failed(ReadFault::TooLarge { size: 9, cap: 8 })
        ));
    }
}
