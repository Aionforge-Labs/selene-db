//! Scalar ordinal/offset value contract (profile evidence EVID-ORDINALITY).
//!
//! The row-expansion executor was deleted at F04-PR09. Only this checked
//! numerical conversion remains; physical batch expansion calls it directly.

use crate::{
    RowExpansionPositionKind, SourceSpan,
    runtime::{DataExceptionSubclass, ExecutorError},
};
use selene_core::Value;

pub(crate) fn position_value(
    kind: RowExpansionPositionKind,
    index: usize,
    span: SourceSpan,
) -> Result<Value, ExecutorError> {
    let error = || {
        ExecutorError::data_exception(
            DataExceptionSubclass::NumericValueOutOfRange,
            "row expansion position exceeds INTEGER range",
            span,
        )
    };
    let offset = i64::try_from(index).map_err(|_| error())?;
    Ok(Value::Int(match kind {
        RowExpansionPositionKind::Offset => offset,
        RowExpansionPositionKind::Ordinality => offset.checked_add(1).ok_or_else(error)?,
    }))
}
