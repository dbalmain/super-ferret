//! Neutral checked-reader outcomes. Hosts retain their filesystem brackets;
//! retry policy belongs here so grep and document search use the same rules.

use std::io;

use ferret_catalog::NameId;

use crate::{ReadRequest, ReadVersion};

/// The operation that failed while observing a candidate.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReadOperation {
    Open,
    Stat,
    Read,
}

/// A failure never establishes a negative content fact.
#[derive(Debug)]
pub enum ReadFault {
    Io(ReadOperation, io::Error),
    TooLarge { size: u64, cap: u64 },
    Allocation(std::collections::TryReserveError),
    Unstable,
}

impl std::fmt::Display for ReadFault {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(operation, error) => write!(f, "{operation:?}: {error}"),
            Self::TooLarge { size, cap } => write!(f, "size {size} exceeds cap {cap}"),
            Self::Allocation(error) => write!(f, "allocation: {error}"),
            Self::Unstable => f.write_str("changed during both read attempts"),
        }
    }
}

impl std::error::Error for ReadFault {}

/// The result of one stat or bracketed read attempt.
#[derive(Debug)]
pub enum ReadOutcome {
    Stable(ReadVersion),
    Vanished,
    /// The host must discard its observation before the next attempt.
    RetryChanged,
    Failed(ReadFault),
    Cancelled,
}

/// One checked observation; partial failed bytes are never cacheable.
pub type CheckedReader<'a> = dyn FnMut(NameId, ReadRequest<'_>) -> ReadOutcome + 'a;

/// Reads stable bytes, retrying a changed observation once. Every attempt
/// replaces `bytes`; failures clear partial data before returning to a caller.
pub fn read_bytes(
    reader: &mut CheckedReader<'_>,
    name: NameId,
    bytes: &mut Vec<u8>,
    stopped: &dyn Fn() -> bool,
) -> ReadOutcome {
    let outcome = retry_read(
        || {
            bytes.clear();
            reader(name, ReadRequest::Bytes(bytes, stopped))
        },
        stopped,
    );
    if !matches!(outcome, ReadOutcome::Stable(_)) {
        bytes.clear();
    }
    outcome
}

/// Retry one changed observation, checking cancellation before every attempt
/// and before accepting a stable result. The consumer resets its output for
/// each attempt; a failed attempt must never publish partial results.
pub fn retry_read(
    mut attempt: impl FnMut() -> ReadOutcome,
    stopped: &dyn Fn() -> bool,
) -> ReadOutcome {
    for at in 0..2 {
        if stopped() {
            return ReadOutcome::Cancelled;
        }
        match attempt() {
            ReadOutcome::RetryChanged if at == 0 => continue,
            ReadOutcome::RetryChanged => return ReadOutcome::Failed(ReadFault::Unstable),
            ReadOutcome::Stable(_) if stopped() => return ReadOutcome::Cancelled,
            outcome => return outcome,
        }
    }
    unreachable!("the second attempt always returns")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retries_once_and_discards_partial_failed_bytes() {
        let mut attempts = 0;
        let mut reader = |_, request: ReadRequest<'_>| {
            attempts += 1;
            let ReadRequest::Bytes(bytes, _) = request else {
                panic!("bytes request")
            };
            assert!(bytes.is_empty());
            bytes.extend_from_slice(b"fresh");
            if attempts == 1 {
                ReadOutcome::RetryChanged
            } else {
                ReadOutcome::Stable(ReadVersion::Current)
            }
        };
        let mut bytes = Vec::new();
        assert!(matches!(
            read_bytes(&mut reader, NameId(0), &mut bytes, &|| false),
            ReadOutcome::Stable(ReadVersion::Current)
        ));
        assert_eq!(bytes, b"fresh");
        assert_eq!(attempts, 2);
    }

    #[test]
    fn repeated_instability_and_faults_never_return_bytes() {
        for fail in [false, true] {
            let mut attempts = 0;
            let mut reader = |_, request: ReadRequest<'_>| {
                attempts += 1;
                let ReadRequest::Bytes(bytes, _) = request else {
                    panic!("bytes request")
                };
                bytes.extend_from_slice(b"partial");
                if fail {
                    ReadOutcome::Failed(ReadFault::Io(
                        ReadOperation::Read,
                        io::ErrorKind::PermissionDenied.into(),
                    ))
                } else {
                    ReadOutcome::RetryChanged
                }
            };
            let mut bytes = Vec::new();
            assert!(matches!(
                read_bytes(&mut reader, NameId(0), &mut bytes, &|| false),
                ReadOutcome::Failed(_)
            ));
            assert!(bytes.is_empty());
            assert_eq!(attempts, if fail { 1 } else { 2 });
        }
    }

    #[test]
    fn vanished_and_cancelled_are_distinct_and_do_not_retry() {
        for cancelled in [false, true] {
            let mut attempts = 0;
            let mut reader = |_, _: ReadRequest<'_>| {
                attempts += 1;
                ReadOutcome::Vanished
            };
            let mut bytes = b"old".to_vec();
            let outcome = read_bytes(&mut reader, NameId(0), &mut bytes, &|| cancelled);
            assert!(matches!(
                (cancelled, outcome),
                (true, ReadOutcome::Cancelled) | (false, ReadOutcome::Vanished)
            ));
            assert!(bytes.is_empty());
            assert_eq!(attempts, usize::from(!cancelled));
        }
    }
}
