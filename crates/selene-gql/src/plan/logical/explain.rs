//! Stable logical-plan EXPLAIN rendering.
//!
//! Output contains stable semantic descriptors (binding, scope, and expression
//! identities, procedure effects, source origins) and never contains runtime
//! addresses, pointer values, or hash-dependent ordering.

use super::{
    effect::LogicalEffect,
    operator::{LogicalMultiplicity, LogicalOp, LogicalOrdering, LogicalPlan},
};

/// Render one logical plan as stable multi-line EXPLAIN text.
///
/// Each line names the operator, its semantic descriptors, its output schema,
/// and its source origin as `offset+len`. Effect labels use the logical
/// vocabulary (`query`, `data`, `catalog`, `maintenance`, `session`,
/// `transaction`). The renderer never formats pointers, so output contains no
/// `0x` address fragments.
#[must_use]
pub fn explain(plan: &LogicalPlan) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "logical_plan effect={} operators={} output_columns={}\n",
        effect_label(plan.effects.effect),
        plan.operators.len(),
        plan.output_schema.columns.len()
    ));
    for (index, op) in plan.operators.iter().enumerate() {
        out.push_str(&format!("op[{index}] {}", explain_op(op)));
        if index + 1 < plan.operators.len() {
            out.push('\n');
        }
    }
    if plan.operators.is_empty() {
        out.push_str(&format!(
            "empty effect={} origin={}\n",
            effect_label(plan.effects.effect),
            origin_label(plan.effects.origin)
        ));
    }
    out
}

fn explain_op(op: &LogicalOp) -> String {
    match op {
        LogicalOp::Scan {
            descriptor,
            output_schema,
            scope,
            origin,
        } => format!(
            "scan binding=b{} scope=s{} ty={:?} {} schema=[{}] ordering={} multiplicity={} origin={}",
            descriptor.binding.get(),
            scope.get(),
            descriptor.ty,
            if descriptor.is_node { "node" } else { "edge" },
            schema_label(output_schema),
            ordering_label(op.ordering()),
            multiplicity_label(op.multiplicity()),
            origin_label(*origin),
        ),
        LogicalOp::Filter {
            predicate,
            scope,
            output_schema,
            origin,
        } => format!(
            "filter predicate=e{} scope=s{} schema=[{}] ordering={} multiplicity={} origin={}",
            predicate.get(),
            scope.get(),
            schema_label(output_schema),
            ordering_label(op.ordering()),
            multiplicity_label(op.multiplicity()),
            origin_label(*origin),
        ),
        LogicalOp::Project {
            expressions,
            scope,
            output_schema,
            origin,
        } => {
            let exprs = expressions
                .iter()
                .map(|id| format!("e{}", id.get()))
                .collect::<Vec<_>>()
                .join(",");
            format!(
                "project exprs=[{exprs}] scope=s{} schema=[{}] ordering={} multiplicity={} origin={}",
                scope.get(),
                schema_label(output_schema),
                ordering_label(op.ordering()),
                multiplicity_label(op.multiplicity()),
                origin_label(*origin),
            )
        }
        LogicalOp::Page {
            offset,
            count,
            output_schema,
            origin,
        } => format!(
            "page offset={} count={} schema=[{}] ordering={} multiplicity={} origin={}",
            page_label(offset),
            page_label(count),
            schema_label(output_schema),
            ordering_label(op.ordering()),
            multiplicity_label(op.multiplicity()),
            origin_label(*origin),
        ),
        LogicalOp::Mutate {
            descriptor,
            output_schema,
            scope,
            origin,
        } => format!(
            "mutate writes={} inserts_node={} inserts_edge={} updates={} deletes={} scope=s{} schema=[{}] ordering={} multiplicity={} origin={}",
            descriptor.write_entry_count,
            descriptor.inserts_node,
            descriptor.inserts_edge,
            descriptor.updates_graph,
            descriptor.deletes_target,
            scope.get(),
            schema_label(output_schema),
            ordering_label(op.ordering()),
            multiplicity_label(op.multiplicity()),
            origin_label(*origin),
        ),
        LogicalOp::Call {
            descriptor,
            output_schema,
            scope,
            origin,
        } => format!(
            "call procedure={} effect={} args={} yields={} scope=s{} schema=[{}] ordering={} multiplicity={} origin={}",
            if descriptor.name.is_empty() {
                "<unknown>".to_owned()
            } else {
                descriptor.name.join(".")
            },
            effect_label(descriptor.effect),
            descriptor.argument_count,
            descriptor.yield_count,
            scope.get(),
            schema_label(output_schema),
            ordering_label(op.ordering()),
            multiplicity_label(op.multiplicity()),
            origin_label(*origin),
        ),
    }
}

fn effect_label(effect: LogicalEffect) -> &'static str {
    match effect {
        LogicalEffect::Query => "query",
        LogicalEffect::Data => "data",
        LogicalEffect::Catalog => "catalog",
        LogicalEffect::Maintenance => "maintenance",
        LogicalEffect::Session => "session",
        LogicalEffect::Transaction => "transaction",
    }
}

fn ordering_label(ordering: LogicalOrdering) -> &'static str {
    match ordering {
        LogicalOrdering::Preserved => "preserved",
        LogicalOrdering::Defined => "defined",
        LogicalOrdering::Unordered => "unordered",
    }
}

fn multiplicity_label(multiplicity: LogicalMultiplicity) -> &'static str {
    match multiplicity {
        LogicalMultiplicity::PreservesDuplicates => "preserves_duplicates",
        LogicalMultiplicity::Distinct => "distinct",
    }
}

fn page_label(amount: &super::operator::LogicalPageAmount) -> String {
    match amount {
        super::operator::LogicalPageAmount::Literal(value) => format!("literal({value})"),
        super::operator::LogicalPageAmount::Parameter { name } => {
            format!("param(${})", name.as_str())
        }
    }
}

fn schema_label(schema: &crate::plan::BindingTableSchema) -> String {
    schema
        .columns
        .iter()
        .map(|column| {
            let name = column
                .name
                .as_ref()
                .map_or_else(|| "<anonymous>".to_owned(), |name| name.as_str().to_owned());
            format!("{name}:{:?}", column.ty)
        })
        .collect::<Vec<_>>()
        .join(",")
}

fn origin_label(span: crate::SourceSpan) -> String {
    format!("{}+{}", span.byte_offset, span.byte_len)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explain_contains_no_runtime_addresses() {
        let plan = LogicalPlan {
            operators: Vec::new(),
            effects: crate::plan::logical::effect::EffectSummary {
                effect: LogicalEffect::Query,
                has_data_write: false,
                has_catalog_write: false,
                has_maintenance_write: false,
                write_entry_count: 0,
                call_count: 0,
                registry_version: 0,
                catalog_dependency_count: 0,
                origin: crate::SourceSpan::default(),
            },
            output_schema: crate::plan::BindingTableSchema {
                columns: Vec::new(),
            },
            registry_version: 0,
            input_width: 0,
        };
        let text = explain(&plan);
        assert!(text.contains("effect=query"));
        assert!(!text.contains("0x"));
    }
}
