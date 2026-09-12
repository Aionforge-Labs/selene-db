//! Configurable batch sizing policy without a release-promised size.
//!
//! The policy derives batch row targets from current workloads (scan-heavy
//! read statements with small-to-medium result sets). The numeric defaults are
//! tuning values, not a public contract: later operators may retune them
//! without a compatibility obligation.

/// Sizing policy for one physical batch.
///
/// `target_rows` caps the logical rows per batch; `max_batch_bytes` caps the
/// estimated resident bytes per batch using `rows_per_batch`. The tighter of
/// the two bounds wins for any given row width estimate.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct BatchPolicy {
    target_rows: usize,
    max_batch_bytes: usize,
}

impl BatchPolicy {
    /// Modest default row target sized for current scan workloads.
    ///
    /// This is a tuning default, not a release promise.
    pub(crate) const DEFAULT_TARGET_ROWS: usize = 1024;

    /// Modest default resident-bytes cap per batch.
    ///
    /// This is a tuning default, not a release promise.
    pub(crate) const DEFAULT_MAX_BATCH_BYTES: usize = 1 << 20;

    /// Return the workload-derived default policy.
    #[must_use]
    pub(crate) const fn default_policy() -> Self {
        Self {
            target_rows: Self::DEFAULT_TARGET_ROWS,
            max_batch_bytes: Self::DEFAULT_MAX_BATCH_BYTES,
        }
    }

    /// Build a policy from explicit bounds, rejecting degenerate inputs.
    ///
    /// Test seam for boundary-cardinality matrices; production uses the
    /// workload-derived default.
    ///
    /// # Errors
    ///
    /// Returns [`BatchPolicyError`] when either bound is zero.
    #[cfg(test)]
    pub(crate) const fn new(
        target_rows: usize,
        max_batch_bytes: usize,
    ) -> Result<Self, BatchPolicyError> {
        if target_rows == 0 {
            return Err(BatchPolicyError::ZeroTargetRows);
        }
        if max_batch_bytes == 0 {
            return Err(BatchPolicyError::ZeroMaxBytes);
        }
        Ok(Self {
            target_rows,
            max_batch_bytes,
        })
    }

    /// Return the configured row target.
    ///
    /// Test seam for policy assertions.
    #[cfg(test)]
    #[must_use]
    pub(crate) const fn target_rows(self) -> usize {
        self.target_rows
    }

    /// Return the configured per-batch byte cap.
    ///
    /// Test seam for policy assertions.
    #[cfg(test)]
    #[must_use]
    pub(crate) const fn max_batch_bytes(self) -> usize {
        self.max_batch_bytes
    }

    /// Return the rows one batch may hold for the given per-row byte estimate.
    ///
    /// The byte cap only ever shrinks the row target; a zero or unknown
    /// estimate keeps the row target unchanged. The result is always at least
    /// one row so progress is guaranteed.
    #[must_use]
    pub(crate) const fn rows_per_batch(self, bytes_per_row_estimate: usize) -> usize {
        if bytes_per_row_estimate == 0 {
            return self.target_rows;
        }
        let by_bytes = self.max_batch_bytes / bytes_per_row_estimate;
        let capped = if by_bytes < self.target_rows {
            by_bytes
        } else {
            self.target_rows
        };
        if capped == 0 { 1 } else { capped }
    }
}

/// Rejected batch-policy construction input.
///
/// Test-only while [`BatchPolicy::new`] is a test seam.
#[cfg(test)]
#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub(crate) enum BatchPolicyError {
    /// The row target was zero, which would stall every operator.
    #[error("batch target rows must be nonzero")]
    ZeroTargetRows,
    /// The byte cap was zero, which would stall every operator.
    #[error("batch max bytes must be nonzero")]
    ZeroMaxBytes,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_nonzero_and_named_as_tuning_values() {
        let policy = BatchPolicy::default_policy();
        assert!(policy.target_rows() > 0);
        assert!(policy.max_batch_bytes() > 0);
    }

    #[test]
    fn zero_bounds_are_rejected() {
        assert_eq!(
            BatchPolicy::new(0, 1024),
            Err(BatchPolicyError::ZeroTargetRows)
        );
        assert_eq!(BatchPolicy::new(16, 0), Err(BatchPolicyError::ZeroMaxBytes));
    }

    #[test]
    fn byte_cap_shrinks_but_never_stalls() {
        let policy = BatchPolicy::new(1024, 4096).unwrap();
        assert_eq!(policy.rows_per_batch(0), 1024);
        assert_eq!(policy.rows_per_batch(4), 1024);
        assert_eq!(policy.rows_per_batch(8), 512);
        assert_eq!(policy.rows_per_batch(usize::MAX), 1);
    }
}
