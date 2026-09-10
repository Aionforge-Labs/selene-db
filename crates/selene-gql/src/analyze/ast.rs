//! Immutable source owner and separate semantic tree.

use std::{ops::Deref, sync::Arc};

use selene_core::DbString;

use super::{
    BindingScopeTree, BindingUse, ExprIdLookup, ExprTypeTable, MutationWriteSet, ScopeId,
    StatementCategory,
    semantic::{ResolvedCall, SemanticExpression},
};
use crate::{GqlType, SourceSpan, Statement};

/// An immutable source tree paired with its independently allocated semantics.
///
/// Analysis only borrows the source. Sharing this result or its input cannot
/// change source spelling, declarations, or procedure argument lists. The
/// semantic tree contains resolved scopes and expression edges, never a second
/// mutable syntax tree. Structural type completion belongs to F03-PR02 and
/// logical operations to F03-PR03.
///
/// ```compile_fail
/// let mut analyzed = selene_gql::analyze(
///     selene_gql::parse("RETURN $x").unwrap(),
///     &selene_gql::EmptyProcedureRegistry,
///     None,
/// ).unwrap();
/// analyzed.parameters.clear(); // frozen semantic output has no mutable access
/// ```
#[derive(Clone, Debug)]
pub struct AnalyzedStatement {
    source: Arc<Statement>,
    semantic: Arc<SemanticTree>,
}

/// Resolved lexical and expression trees for one statement.
///
/// Fields remain inspectable for lower-engine consumers, but an
/// [`AnalyzedStatement`] only exposes shared access to this frozen allocation.
#[derive(Clone, Debug)]
pub struct SemanticTree {
    /// Lexical scope tree and stable declaration identities.
    pub scopes: BindingScopeTree,
    /// Resolved binding uses, in traversal order.
    pub references: Vec<BindingUse>,
    /// Parameter uses in source order, with effective inherited declarations.
    pub parameters: Vec<ParameterUse>,
    /// Type cells indexed by semantic expression identity.
    pub expr_types: ExprTypeTable,
    /// Temporary source-expression lookup used by the current-plan adapter.
    pub expr_ids: ExprIdLookup,
    /// Expression nodes with resolved namespace and child identities.
    pub expressions: Vec<SemanticExpression>,
    /// Root source origin.
    pub span: SourceSpan,
    /// Statement category used by transaction enforcement.
    pub category: StatementCategory,
    /// Enumerated mutation writes.
    pub write_set: Option<MutationWriteSet>,
    /// Resolved procedure applications, including semantic-only defaults.
    pub calls: Vec<ResolvedCall>,
    /// Catalog sites and exact descriptor dependencies, when catalog-bound.
    pub catalog: Option<super::catalog::CatalogResolution>,
    /// Exact generated profile contract used during this analysis.
    pub profile: selene_profile::ProfileIdentity,
    /// Registry generation used for procedure resolution.
    pub procedure_registry_version: u64,
}

impl AnalyzedStatement {
    pub(crate) fn new(source: Arc<Statement>, semantic: SemanticTree) -> Self {
        Self {
            source,
            semantic: Arc::new(semantic),
        }
    }

    /// Borrow the exact syntax supplied to analysis, without inferred changes.
    #[must_use]
    pub fn source(&self) -> &Statement {
        &self.source
    }

    /// Borrow the independently allocated immutable semantic tree.
    #[must_use]
    pub fn semantics(&self) -> &SemanticTree {
        &self.semantic
    }

    /// Resolve a source expression to its immutable semantic node.
    #[must_use]
    pub fn expression(&self, source: &crate::ValueExpr) -> Option<&SemanticExpression> {
        let id = self.expr_ids.get(source)?;
        self.expressions
            .get(id.get() as usize)
            .filter(|node| node.id == id)
    }

    /// Return the root lexical scope.
    #[must_use]
    pub fn root_scope(&self) -> ScopeId {
        self.scopes.root()
    }

    // Deliberately absent even from the public test-harness feature. Unit tests
    // of defensive adapter guards can corrupt a private copy; production and
    // external consumers cannot mutate either frozen tree.
    #[cfg(test)]
    pub(crate) fn corrupt_for_test(
        &mut self,
        corrupt: impl FnOnce(&mut Statement, &mut SemanticTree),
    ) {
        corrupt(
            Arc::make_mut(&mut self.source),
            Arc::make_mut(&mut self.semantic),
        );
    }
}

impl Deref for AnalyzedStatement {
    type Target = SemanticTree;

    fn deref(&self) -> &Self::Target {
        &self.semantic
    }
}

/// One parameter reference, resolved in the parameter namespace, not bindings.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ParameterUse {
    /// Exact decoded name without `$`.
    pub name: DbString,
    /// Original occurrence span.
    pub span: SourceSpan,
    /// Effective declaration, including inheritance from another occurrence.
    pub declared_type: Option<GqlType>,
}
