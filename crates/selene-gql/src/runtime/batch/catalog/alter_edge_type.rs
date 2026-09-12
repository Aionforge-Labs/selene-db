//! `ALTER EDGE TYPE` execution.

use crate::{
    BindingTable, EdgeEndpointSpec, ExecutorError, PlannedTypePropertyDef, SourceSpan, TxContext,
};
use selene_core::DbString;

use super::{
    catalog_graph_error, closed_graph_type, endpoints::resolve_endpoints, property::property_defs,
};

pub(super) fn execute(
    label: DbString,
    endpoints: Option<&EdgeEndpointSpec>,
    properties: &[PlannedTypePropertyDef],
    span: SourceSpan,
    table: BindingTable,
    ctx: &mut TxContext<'_, '_>,
) -> Result<BindingTable, ExecutorError> {
    ctx.ensure_write_txn("catalog op invoked without write transaction", span)?;
    let graph_type = closed_graph_type(ctx.snapshot(), span)?;
    let (source, target) = endpoints
        .map(|endpoints| resolve_endpoints(endpoints, &graph_type, span))
        .transpose()?
        .map(|(source, target)| (Some(source), Some(target)))
        .unwrap_or((None, None));
    let properties = property_defs(properties, false)?;
    reject_required_properties(&label, &properties, span)?;
    ctx.mutator_with_span("catalog op invoked without write transaction", span)?
        .alter_edge_type(label, source, target, properties)
        .map_err(|source| catalog_graph_error(source, span))?;
    Ok(table)
}

fn reject_required_properties(
    label: &DbString,
    properties: &[selene_graph::PropertyTypeDef],
    span: SourceSpan,
) -> Result<(), ExecutorError> {
    if let Some(property) = properties.iter().find(|property| property.required) {
        return Err(ExecutorError::GraphTypeViolation {
            message: format!(
                "ALTER EDGE TYPE :{label} cannot add required property {}",
                property.name
            ),
            span,
        });
    }
    Ok(())
}
