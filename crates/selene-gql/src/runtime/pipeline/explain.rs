//! EXPLAIN pipeline operator.

use selene_core::{DbString, Value, db_string};

use crate::{
    AnalyzedType, BindingTableColumn, BindingTableSchema, ExecutionPlan, GqlType,
    runtime::{Binding, BindingTable, ExecutorError},
};

pub(crate) fn execute(inner: &ExecutionPlan) -> Result<BindingTable, ExecutorError> {
    let dump = format!("{inner:#?}");
    Ok(BindingTable::new(
        BindingTableSchema {
            columns: vec![BindingTableColumn {
                name: Some(runtime_db_string("plan")?),
                hidden: None,
                ty: AnalyzedType::Resolved(GqlType::String),
            }],
        },
        vec![Binding::new([Value::String(runtime_db_string_owned(
            dump,
        )?)])],
    ))
}

fn runtime_db_string(value: &str) -> Result<DbString, ExecutorError> {
    db_string(value).map_err(|_err| ExecutorError::ImplementationDefined {
        detail: "string construction failed during EXPLAIN rendering",
    })
}

fn runtime_db_string_owned(value: String) -> Result<DbString, ExecutorError> {
    DbString::from_string(value).map_err(|_err| ExecutorError::ImplementationDefined {
        detail: "string construction failed during EXPLAIN rendering",
    })
}
