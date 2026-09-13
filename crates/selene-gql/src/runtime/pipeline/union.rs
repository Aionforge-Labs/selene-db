//! Set-arm schema and resource diagnostics.
use crate::{
    BindingTableSchema, SourceSpan,
    runtime::{DataExceptionSubclass, ExecutorError},
};

pub(crate) fn set_op_key_cap_exceeded() -> ExecutorError {
    ExecutorError::ProgramLimitExceeded {
        detail: "set-op key cap exceeded",
        span: SourceSpan::default(),
    }
}

pub(crate) fn assert_compatible_schemas(
    op_name: &'static str,
    lhs: &BindingTableSchema,
    rhs: &BindingTableSchema,
) -> Result<(), ExecutorError> {
    let lhs_len = lhs.columns.len();
    let rhs_len = rhs.columns.len();
    if lhs_len != rhs_len {
        return Err(ExecutorError::DataException {
            subclass: DataExceptionSubclass::InvalidValueType,
            message: format!(
                "{op_name} arms have differing column counts: lhs={lhs_len}, rhs={rhs_len}"
            ),
            span: SourceSpan::default(),
        });
    }
    Ok(())
}
