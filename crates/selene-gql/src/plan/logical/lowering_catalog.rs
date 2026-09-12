//! Catalog, control, and explain lowering for logical plans.
//!
//! Kinds resolve from the statement shape and analyzer category; unchanged
//! DDL payloads stay in source syntax for the row adapter to transport.

use selene_core::{DbString, db_string};

use crate::{
    DdlStatement, GqlType, SourceSpan,
    analyze::{AnalyzedStatement, AnalyzedType},
    plan::{
        BindingTableColumn, BindingTableSchema, PlannerError,
        logical::descriptors::LogicalCatalogKind,
    },
};

/// Map one DDL statement to its logical catalog family.
pub(crate) fn catalog_kind_for_ddl(statement: &DdlStatement) -> LogicalCatalogKind {
    match statement {
        DdlStatement::CreateSchema { .. }
        | DdlStatement::DropSchema { .. }
        | DdlStatement::CreateGraph { .. }
        | DdlStatement::DropGraph { .. }
        | DdlStatement::CreateGraphType { .. }
        | DdlStatement::DropGraphType { .. } => LogicalCatalogKind::DatabaseCatalog,
        DdlStatement::CreateNodeType { .. } => LogicalCatalogKind::CreateNodeType,
        DdlStatement::CreateEdgeType { .. } => LogicalCatalogKind::CreateEdgeType,
        DdlStatement::AlterNodeType { .. } => LogicalCatalogKind::AlterNodeType,
        DdlStatement::AlterEdgeType { .. } => LogicalCatalogKind::AlterEdgeType,
        DdlStatement::DropNodeType { .. } => LogicalCatalogKind::DropNodeType,
        DdlStatement::DropEdgeType { .. } => LogicalCatalogKind::DropEdgeType,
        DdlStatement::TruncateNodeType { .. } | DdlStatement::TruncateEdgeType { .. } => {
            LogicalCatalogKind::Truncate
        }
        DdlStatement::CreateIndex { .. } => LogicalCatalogKind::CreateIndex,
        DdlStatement::DropIndex { .. } => LogicalCatalogKind::DropIndex,
        DdlStatement::ShowNodeTypes(_)
        | DdlStatement::ShowEdgeTypes(_)
        | DdlStatement::ShowIndexes(_)
        | DdlStatement::ShowProcedures(_) => LogicalCatalogKind::Show,
    }
}

/// Build the logical output schema for one DDL statement.
///
/// `SHOW` families expose their static columns; every other family returns
/// no columns. Column names are static identifiers, never semantic
/// decisions, so `db_string` construction here transports no authority.
pub(crate) fn catalog_output_schema(
    statement: &DdlStatement,
) -> Result<BindingTableSchema, PlannerError> {
    match statement {
        DdlStatement::ShowNodeTypes(span) => show_output_schema(
            *span,
            "static SHOW NODE TYPES column 'label'",
            "static SHOW NODE TYPES column 'definition'",
        ),
        DdlStatement::ShowEdgeTypes(span) => show_output_schema(
            *span,
            "static SHOW EDGE TYPES column 'label'",
            "static SHOW EDGE TYPES column 'definition'",
        ),
        DdlStatement::ShowIndexes(span) => named_output_schema(
            *span,
            &[
                ("name", "static SHOW INDEXES column 'name'"),
                ("label", "static SHOW INDEXES column 'label'"),
                ("property", "static SHOW INDEXES column 'property'"),
                ("kind", "static SHOW INDEXES column 'kind'"),
            ],
        ),
        DdlStatement::ShowProcedures(span) => named_output_schema(
            *span,
            &[
                ("name", "static SHOW PROCEDURES column 'name'"),
                ("tier", "static SHOW PROCEDURES column 'tier'"),
                ("mutability", "static SHOW PROCEDURES column 'mutability'"),
                ("signature", "static SHOW PROCEDURES column 'signature'"),
                ("description", "static SHOW PROCEDURES column 'description'"),
                (
                    "since_version",
                    "static SHOW PROCEDURES column 'since_version'",
                ),
            ],
        ),
        _ => Ok(BindingTableSchema {
            columns: Vec::new(),
        }),
    }
}

/// Build the single-column `plan` output schema for `EXPLAIN`.
pub(crate) fn explain_output_schema(span: SourceSpan) -> Result<BindingTableSchema, PlannerError> {
    let name = db_string("plan").map_err(|_err| PlannerError::StaticStringConstructionFailed {
        detail: "static EXPLAIN column 'plan'",
        span,
    })?;
    Ok(BindingTableSchema {
        columns: vec![BindingTableColumn {
            name: Some(name),
            hidden: None,
            ty: AnalyzedType::Resolved(GqlType::String),
        }],
    })
}

fn named_output_schema(
    span: SourceSpan,
    names: &[(&'static str, &'static str)],
) -> Result<BindingTableSchema, PlannerError> {
    let mut columns = Vec::with_capacity(names.len());
    for (name, detail) in names {
        columns.push(BindingTableColumn {
            name: Some(show_column_name(name, detail, span)?),
            hidden: None,
            ty: AnalyzedType::Resolved(GqlType::String),
        });
    }
    Ok(BindingTableSchema { columns })
}

fn show_output_schema(
    span: SourceSpan,
    label_detail: &'static str,
    definition_detail: &'static str,
) -> Result<BindingTableSchema, PlannerError> {
    Ok(BindingTableSchema {
        columns: vec![
            BindingTableColumn {
                name: Some(show_column_name("label", label_detail, span)?),
                hidden: None,
                ty: AnalyzedType::Resolved(GqlType::String),
            },
            BindingTableColumn {
                name: Some(show_column_name("definition", definition_detail, span)?),
                hidden: None,
                ty: AnalyzedType::DYNAMIC,
            },
        ],
    })
}

fn show_column_name(
    value: &'static str,
    detail: &'static str,
    span: SourceSpan,
) -> Result<DbString, PlannerError> {
    db_string(value).map_err(|_err| PlannerError::StaticStringConstructionFailed { detail, span })
}

/// Suppress unused-import warnings until the cutover wires catalog analysis.
#[allow(dead_code)]
pub(crate) fn catalog_dependencies(_analyzed: &AnalyzedStatement) -> usize {
    0
}
