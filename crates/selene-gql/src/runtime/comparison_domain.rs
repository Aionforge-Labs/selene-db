//! GQL diagnostics over the core-owned comparison-domain authority.

use super::{DataExceptionSubclass, ExecutorError};
use crate::SourceSpan;
use selene_core::{
    ComparisonMode, StructuralType, Value, ValueComparisonDomain, ValueComparisonError,
};

#[derive(Default)]
pub(crate) struct ComparisonDomain(Vec<ValueComparisonDomain>);

impl ComparisonDomain {
    pub(crate) fn observe(
        &mut self,
        row: &[Value],
        mode: ComparisonMode,
    ) -> Result<(), ExecutorError> {
        self.0
            .resize_with(row.len(), ValueComparisonDomain::default);
        for (domain, value) in self.0.iter_mut().zip(row) {
            domain
                .observe(value, mode)
                .map_err(|error| diagnostic(error, SourceSpan::default()))?;
        }
        Ok(())
    }
}

pub(crate) fn ensure_pair(
    lhs: &Value,
    rhs: &Value,
    mode: ComparisonMode,
    span: SourceSpan,
) -> Result<(), ExecutorError> {
    let mut domain = ValueComparisonDomain::default();
    domain
        .observe(lhs, mode)
        .and_then(|()| domain.observe(rhs, mode))
        .map_err(|error| diagnostic(error, span))
}

pub(super) fn leaf_type(value: &Value) -> Result<StructuralType, ExecutorError> {
    selene_core::comparison_leaf_type(value)
        .map_err(|error| diagnostic(error, SourceSpan::default()))
}

pub(super) fn incomparable() -> ExecutorError {
    diagnostic(ValueComparisonError::NotComparable, SourceSpan::default())
}

fn diagnostic(error: ValueComparisonError, span: SourceSpan) -> ExecutorError {
    match error {
        ValueComparisonError::NotComparable => ExecutorError::DataException {
            subclass: DataExceptionSubclass::ValuesNotComparable,
            message: "values are not comparable for this operation".into(),
            span,
        },
        ValueComparisonError::TooDeep => ExecutorError::ProgramLimitExceeded {
            detail: "comparison nesting limit exceeded",
            span,
        },
    }
}
