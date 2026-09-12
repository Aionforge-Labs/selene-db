//! Reusable memory-budget and cancellation seams for batch operators.
//!
//! Later F04 operators share these instead of inventing per-operator limits:
//! [`MemoryBudget`] accounts estimated batch bytes against an optional cap,
//! and [`BatchCancel`] wraps the engine-wide
//! [`CancellationChecker`](selene_core::CancellationChecker) with the
//! executor's error mapping so batch checkpoints report the same GQLSTATUS
//! codes as the row executor (`5GQL2` cancel, `5GQL3` timeout, `5GQL1`
//! scan-budget exhaustion).

use std::time::Instant;

use selene_core::{CancellationCause, CancellationChecker, CancellationToken, NodeScanBudget};

use crate::{SourceSpan, runtime::ExecutorError};

/// Byte budget for batch scratch storage owned by one execution.
///
/// All reservations are estimates (`size_of::<Value>()` plus explicit bitmap
/// bytes), not allocator truth: the budget bounds operator behavior, it does
/// not measure the global heap. `reserve_events`, `used_bytes`, and
/// `peak_bytes` are observed counters the performance probe reports.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct MemoryBudget {
    limit_bytes: Option<usize>,
    used_bytes: usize,
    peak_bytes: usize,
    reserve_events: u64,
}

impl MemoryBudget {
    /// Build a budget capped at `limit_bytes` estimated bytes.
    ///
    /// Test seam: production executions currently run uncapped while budget
    /// wiring from statement limits is follow-up work.
    #[cfg(test)]
    #[must_use]
    pub(crate) const fn new(limit_bytes: usize) -> Self {
        Self {
            limit_bytes: Some(limit_bytes),
            used_bytes: 0,
            peak_bytes: 0,
            reserve_events: 0,
        }
    }

    /// Build a budget with no cap. Accounting counters still run.
    #[must_use]
    pub(crate) const fn unlimited() -> Self {
        Self {
            limit_bytes: None,
            used_bytes: 0,
            peak_bytes: 0,
            reserve_events: 0,
        }
    }

    /// Reserve `bytes` of estimated storage.
    ///
    /// # Errors
    ///
    /// Returns [`MemoryBudgetError`] without recording the reservation when
    /// the cap would be exceeded.
    pub(crate) fn reserve(&mut self, bytes: usize) -> Result<(), MemoryBudgetError> {
        if let Some(limit) = self.limit_bytes {
            let next = self.used_bytes.saturating_add(bytes);
            if next > limit {
                return Err(MemoryBudgetError::Exceeded {
                    limit,
                    requested: bytes,
                    used: self.used_bytes,
                });
            }
            self.used_bytes = next;
        } else {
            self.used_bytes = self.used_bytes.saturating_add(bytes);
        }
        self.reserve_events += 1;
        if self.used_bytes > self.peak_bytes {
            self.peak_bytes = self.used_bytes;
        }
        Ok(())
    }

    /// Release a previous `bytes` reservation. Saturates at zero.
    pub(crate) fn release(&mut self, bytes: usize) {
        self.used_bytes = self.used_bytes.saturating_sub(bytes);
    }

    /// Return currently reserved estimated bytes.
    ///
    /// Test seam for budget-accounting assertions.
    #[cfg(test)]
    #[must_use]
    pub(crate) const fn used_bytes(self) -> usize {
        self.used_bytes
    }

    /// Return the high-water mark of reserved estimated bytes.
    ///
    /// Test seam for the performance probe.
    #[cfg(test)]
    #[must_use]
    pub(crate) const fn peak_bytes(self) -> usize {
        self.peak_bytes
    }

    /// Return the number of successful reservations so far.
    ///
    /// Test seam for the performance probe.
    #[cfg(test)]
    #[must_use]
    pub(crate) const fn reserve_events(self) -> u64 {
        self.reserve_events
    }
}

/// Failed memory-budget reservation.
#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub(crate) enum MemoryBudgetError {
    /// The reservation would exceed the configured cap.
    #[error(
        "batch memory budget exceeded (limit {limit} bytes, {used} used, {requested} requested)"
    )]
    Exceeded {
        /// Configured cap in estimated bytes.
        limit: usize,
        /// Bytes the failed reservation requested.
        requested: usize,
        /// Bytes already reserved.
        used: usize,
    },
}

impl MemoryBudgetError {
    /// Map into the executor error reported at batch boundaries.
    #[must_use]
    pub(crate) const fn into_executor_error(self, span: SourceSpan) -> ExecutorError {
        let _ = self;
        ExecutorError::ProgramLimitExceeded {
            detail: "batch memory budget exceeded",
            span,
        }
    }
}

/// Cooperative cancellation checkpoint shared by batch operators.
///
/// This wraps [`CancellationChecker`] so batch loops observe the same token,
/// deadline, and deterministic node-scan budget as the row executor. Check it
/// at batch boundaries, never per row.
#[derive(Clone, Copy, Debug)]
pub(crate) struct BatchCancel<'a> {
    checker: CancellationChecker<'a>,
    deadline: Option<Instant>,
}

impl<'a> BatchCancel<'a> {
    /// Build a checkpoint from the statement's token, deadline, and budget.
    #[must_use]
    pub(crate) const fn new(
        token: Option<&'a CancellationToken>,
        deadline: Option<Instant>,
        node_scan_budget: Option<&'a NodeScanBudget>,
    ) -> Self {
        Self {
            checker: CancellationChecker::new_with_node_scan_budget(
                token,
                deadline,
                node_scan_budget,
            ),
            deadline,
        }
    }

    /// Build a checkpoint that never cancels. Tests use this for the
    /// cancellation-free path; production callers pass real limits.
    #[cfg(test)]
    #[must_use]
    pub(crate) const fn disabled() -> Self {
        Self {
            checker: CancellationChecker::disabled(),
            deadline: None,
        }
    }

    /// Check token and deadline once, mapping to executor errors.
    ///
    /// # Errors
    ///
    /// Returns `Cancelled`, `Timeout`, or `ProgramLimitExceeded` exactly as
    /// the row executor's cancellation mapping does.
    pub(crate) fn check(&self, span: SourceSpan) -> Result<(), ExecutorError> {
        self.checker
            .check()
            .map_err(|cause| self.map_cause(cause, span))
    }

    /// Check cancellation, then account for `nodes` scanned graph nodes.
    ///
    /// Call once per produced batch. Token cancellation wins over deadline
    /// timeout, which wins over scan-budget exhaustion, matching the row
    /// executor's precedence.
    ///
    /// # Errors
    ///
    /// Returns the mapped executor error for the first tripped limit.
    pub(crate) fn note_nodes_scanned(
        &self,
        nodes: usize,
        span: SourceSpan,
    ) -> Result<(), ExecutorError> {
        self.checker
            .note_nodes_scanned(nodes)
            .map_err(|cause| self.map_cause(cause, span))
    }

    fn map_cause(&self, cause: CancellationCause, span: SourceSpan) -> ExecutorError {
        match cause {
            CancellationCause::Cancelled => ExecutorError::Cancelled { span },
            CancellationCause::Timeout { elapsed } => ExecutorError::Timeout {
                deadline: self.deadline.unwrap_or_else(Instant::now),
                elapsed,
                span,
            },
            CancellationCause::NodeScanBudgetExceeded { .. } => {
                ExecutorError::ProgramLimitExceeded {
                    detail: "node scan budget exceeded",
                    span,
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    #[test]
    fn budget_caps_and_accounts() {
        let mut budget = MemoryBudget::new(100);
        budget.reserve(60).unwrap();
        budget.reserve(40).unwrap();
        assert_eq!(budget.used_bytes(), 100);
        assert_eq!(budget.peak_bytes(), 100);
        assert_eq!(budget.reserve_events(), 2);
        let err = budget.reserve(1).unwrap_err();
        assert!(matches!(err, MemoryBudgetError::Exceeded { .. }));
        // Failed reservations are not recorded.
        assert_eq!(budget.used_bytes(), 100);
        assert_eq!(budget.reserve_events(), 2);
        budget.release(30);
        assert_eq!(budget.used_bytes(), 70);
        budget.release(usize::MAX);
        assert_eq!(budget.used_bytes(), 0);
        // Peak survives release.
        assert_eq!(budget.peak_bytes(), 100);
    }

    #[test]
    fn unlimited_budget_still_counts() {
        let mut budget = MemoryBudget::unlimited();
        budget.reserve(usize::MAX).unwrap();
        assert_eq!(budget.reserve_events(), 1);
        assert_eq!(budget.peak_bytes(), usize::MAX);
    }

    #[test]
    fn budget_error_maps_to_program_limit() {
        let err = MemoryBudgetError::Exceeded {
            limit: 8,
            requested: 9,
            used: 0,
        }
        .into_executor_error(SourceSpan::default());
        assert!(matches!(err, ExecutorError::ProgramLimitExceeded { .. }));
        assert_eq!(err.gqlstatus().as_str(), "5GQL1");
    }

    #[test]
    fn cancel_checkpoint_matches_row_executor_codes() {
        let span = SourceSpan::default();
        BatchCancel::disabled().check(span).unwrap();

        let token = CancellationToken::new();
        token.cancel();
        let err = BatchCancel::new(Some(&token), None, None)
            .check(span)
            .unwrap_err();
        assert!(matches!(err, ExecutorError::Cancelled { .. }));
        assert_eq!(err.gqlstatus().as_str(), "5GQL2");

        let past = Instant::now() - Duration::from_secs(1);
        let err = BatchCancel::new(None, Some(past), None)
            .check(span)
            .unwrap_err();
        assert!(matches!(err, ExecutorError::Timeout { .. }));
        assert_eq!(err.gqlstatus().as_str(), "5GQL3");

        let budget = NodeScanBudget::new(2);
        let cancel = BatchCancel::new(None, None, Some(&budget));
        cancel.note_nodes_scanned(2, span).unwrap();
        let err = cancel.note_nodes_scanned(1, span).unwrap_err();
        assert!(matches!(err, ExecutorError::ProgramLimitExceeded { .. }));
        assert_eq!(err.gqlstatus().as_str(), "5GQL1");
    }
}
