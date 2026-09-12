//! `ALTER NODE TYPE` execution.

use selene_core::DbString;

use crate::{
    BindingTable, ExecutorError, PlannedTypePropertyConstraint, PlannedTypePropertyDef, SourceSpan,
    TxContext,
};

use super::{catalog_graph_error, closed_graph_type, property::property_defs};

pub(super) fn execute(
    label: DbString,
    properties: &[PlannedTypePropertyDef],
    span: SourceSpan,
    table: BindingTable,
    ctx: &mut TxContext<'_, '_>,
) -> Result<BindingTable, ExecutorError> {
    ctx.ensure_write_txn("catalog op invoked without write transaction", span)?;
    // Keep ALTER NODE TYPE on the closed-graph catalog surface even when the
    // mutator could otherwise report the missing type against an open graph.
    let _ = closed_graph_type(ctx.snapshot(), span)?;
    reject_inline_indexes(properties)?;
    let properties = property_defs(properties, false)?;
    reject_required_properties(&label, &properties, span)?;
    ctx.mutator_with_span("catalog op invoked without write transaction", span)?
        .alter_node_type(label, properties)
        .map_err(|source| catalog_graph_error(source, span))?;
    Ok(table)
}

fn reject_inline_indexes(properties: &[PlannedTypePropertyDef]) -> Result<(), ExecutorError> {
    if let Some(span) = properties.iter().find_map(|property| {
        property
            .constraints
            .iter()
            .find_map(|constraint| match constraint {
                PlannedTypePropertyConstraint::Indexed { span, .. } => Some(*span),
                PlannedTypePropertyConstraint::Default(_, _)
                | PlannedTypePropertyConstraint::NotNull(_)
                | PlannedTypePropertyConstraint::Immutable(_)
                | PlannedTypePropertyConstraint::Unique(_) => None,
            })
    }) {
        return Err(ExecutorError::FeatureNotSupportedYet {
            feature: "inline INDEXED on ALTER NODE TYPE properties",
            span,
        });
    }
    Ok(())
}

fn reject_required_properties(
    label: &DbString,
    properties: &[selene_graph::PropertyTypeDef],
    span: SourceSpan,
) -> Result<(), ExecutorError> {
    if let Some(property) = properties.iter().find(|property| property.required) {
        return Err(ExecutorError::GraphTypeViolation {
            message: format!(
                "ALTER NODE TYPE :{label} cannot add required property {}",
                property.name
            ),
            span,
        });
    }
    Ok(())
}
