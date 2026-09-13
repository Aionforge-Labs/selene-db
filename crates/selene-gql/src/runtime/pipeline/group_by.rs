//! Aggregate schema and resource diagnostics.
use crate::{
    Aggregate, BindingTableColumn, BindingTableSchema, SourceSpan, runtime::ExecutorError,
};

pub(crate) fn group_by_key_cap_exceeded() -> ExecutorError {
    ExecutorError::ProgramLimitExceeded {
        detail: "GROUP BY distinct-group cap exceeded",
        span: SourceSpan::default(),
    }
}

pub(crate) fn output_schema(
    input: &BindingTableSchema,
    aggregates: &[Aggregate],
) -> BindingTableSchema {
    let mut schema = input.clone();
    schema
        .columns
        .extend(aggregates.iter().flat_map(|aggregate| {
            super::aggregate::output_names(aggregate)
                .into_iter()
                .map(|name| BindingTableColumn {
                    name: Some(name),
                    hidden: None,
                    ty: aggregate.ty.clone(),
                })
                .collect::<Vec<_>>()
        }));
    schema
}
