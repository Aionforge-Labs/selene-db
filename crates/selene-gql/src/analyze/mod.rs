//! Semantic analyzer entry points.
//!
//! The analyzer turns parsed statements into a closed semantic model: every
//! reference resolves to a `BindingDecl`, every `ValueExpr` has an expression
//! type cell, closed-graph mutations are statically validated when a schema is
//! supplied, and CALL arguments/YIELD bindings are checked against registry
//! metadata. It defers physical access selection and row-shape execution to the
//! planner/runtime boundary, and it treats dynamic expression cells as explicit
//! unknowns rather than implicit success. See Spec 08 §5.

pub mod ast;
pub mod binding;
pub mod catalog;
pub mod category;
pub mod error;
pub mod index_expression;
pub mod scope;
pub mod semantic;
pub mod types;
pub mod write_set;

pub(crate) mod bind;
pub(crate) mod infer;
pub(crate) mod schema;

use selene_graph::GraphTypeDef;

use crate::{ProcedureRegistry, Statement};

pub use ast::{AnalyzedStatement, ParameterUse, SemanticTree};
pub use binding::{BindingDecl, BindingDeclKind, BindingId, BindingUse, BindingUseKind};
pub use category::StatementCategory;
pub use error::{
    AnalysisError, ConditionClause, ExpectedType, InvalidLabelForm, PatternElementKind, Side,
    TypeMismatchContext,
};
pub use scope::{BindingScope, BindingScopeTree, ScopeId, ScopeKind};
pub use types::{AnalyzedType, ExprId, ExprIdLookup, ExprTypeTable};
pub use write_set::{ElementKind, MutationWriteSet, WriteKind, WriteSetEntry};

/// Analyze a parsed GQL statement.
///
/// Resolves every binding reference, allocates [`BindingId`]s for every
/// declaration site, resolves procedure signatures through `registry`, applies
/// closed-graph static validation when `schema` is present, and returns an
/// [`AnalyzedStatement`] suitable for the planner stage.
///
/// # Errors
///
/// Returns the first [`AnalysisError`] detected by the fail-fast bind pass.
#[tracing::instrument(
    name = "selene.gql.analyze",
    skip(stmt, registry, schema),
    fields(schema_bound = schema.is_some())
)]
pub fn analyze(
    stmt: impl Into<std::sync::Arc<Statement>>,
    registry: &dyn ProcedureRegistry,
    schema: Option<&GraphTypeDef>,
) -> Result<AnalyzedStatement, AnalysisError> {
    let analyzed = bind::bind_statement(stmt.into(), registry, None, &Default::default())?;
    if let Some(graph_type) = schema {
        self::schema::validate(&analyzed, graph_type)?;
    }
    Ok(analyzed)
}

/// Resolve one immutable source tree against a catalog and lexical defaults.
///
/// # Errors
/// Returns binding, catalog-reference, or single-graph transaction diagnostics
/// with original source spans. The selected runtime schema is validated by the
/// same schema pass when the owner supplies the selected execution snapshot.
pub fn analyze_catalog(
    stmt: impl Into<std::sync::Arc<Statement>>,
    registry: &dyn ProcedureRegistry,
    environment: catalog::CatalogEnvironment,
) -> Result<AnalyzedStatement, AnalysisError> {
    bind::bind_statement(
        stmt.into(),
        registry,
        Some(environment),
        &Default::default(),
    )
}

/// Analyze immutable source with explicit request-parameter structural types.
/// Inline source declarations remain separate and are validated at preflight.
pub fn analyze_with_parameters(
    stmt: impl Into<std::sync::Arc<Statement>>,
    registry: &dyn ProcedureRegistry,
    environment: Option<catalog::CatalogEnvironment>,
    parameters: &std::collections::BTreeMap<selene_core::DbString, selene_core::StructuralType>,
) -> Result<AnalyzedStatement, AnalysisError> {
    bind::bind_statement(stmt.into(), registry, environment, parameters)
}
