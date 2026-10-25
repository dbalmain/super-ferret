//! Neutral checked-reader outcomes. Hosts retain their filesystem brackets
//! and reset their own output per attempt; [`retry_read`] owns the retry
//! policy, and grep is its caller. Document search reads through the same
//! checked reader but adapts its outcomes to the [`Reader`](crate::Reader)
//! boundary, where anything but a stable read is a miss, without retrying.

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
    use std::cell::Cell;

    use super::*;

    fn fault() -> ReadOutcome {
        ReadOutcome::Failed(ReadFault::Io(
            ReadOperation::Read,
            io::ErrorKind::PermissionDenied.into(),
        ))
    }

    /// Runs `retry_read` over a scripted sequence of attempt outcomes and
    /// returns its result with the number of attempts it made.
    fn run(mut script: Vec<ReadOutcome>, stopped: &dyn Fn() -> bool) -> (ReadOutcome, usize) {
        script.reverse();
        let mut attempts = 0;
        let outcome = retry_read(
            || {
                attempts += 1;
                script.pop().expect("no more attempts than scripted")
            },
            stopped,
        );
        (outcome, attempts)
    }

    #[test]
    fn retries_one_changed_observation_and_fails_a_second() {
        let stable = || ReadOutcome::Stable(ReadVersion::Current);
        let (outcome, attempts) = run(vec![ReadOutcome::RetryChanged, stable()], &|| false);
        assert!(matches!(outcome, ReadOutcome::Stable(ReadVersion::Current)));
        assert_eq!(attempts, 2);
        let script = vec![ReadOutcome::RetryChanged, ReadOutcome::RetryChanged];
        let (outcome, attempts) = run(script, &|| false);
        assert!(matches!(outcome, ReadOutcome::Failed(ReadFault::Unstable)));
        assert_eq!(attempts, 2);
    }

    #[test]
    fn faults_and_vanished_files_are_not_retried() {
        let (outcome, attempts) = run(vec![fault()], &|| false);
        assert!(matches!(outcome, ReadOutcome::Failed(ReadFault::Io(..))));
        assert_eq!(attempts, 1);
        let (outcome, attempts) = run(vec![ReadOutcome::Vanished], &|| false);
        assert!(matches!(outcome, ReadOutcome::Vanished));
        assert_eq!(attempts, 1);
    }

    #[test]
    fn cancellation_is_checked_before_each_attempt_and_before_acceptance() {
        let (outcome, attempts) = run(vec![], &|| true);
        assert!(matches!(outcome, ReadOutcome::Cancelled));
        assert_eq!(attempts, 0);

        // Stopped while the stable attempt ran: its result is not accepted.
        let stop = Cell::new(false);
        let mut attempts = 0;
        let outcome = retry_read(
            || {
                attempts += 1;
                stop.set(true);
                ReadOutcome::Stable(ReadVersion::Current)
            },
            &|| stop.get(),
        );
        assert!(matches!(outcome, ReadOutcome::Cancelled));
        assert_eq!(attempts, 1);

        // Stopped after a changed observation: no second attempt.
        let stop = Cell::new(false);
        let mut attempts = 0;
        let outcome = retry_read(
            || {
                attempts += 1;
                stop.set(true);
                ReadOutcome::RetryChanged
            },
            &|| stop.get(),
        );
        assert!(matches!(outcome, ReadOutcome::Cancelled));
        assert_eq!(attempts, 1);
    }
}
