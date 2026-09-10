//! Lexical working-site entry/exit; never mutates session defaults or syntax.

use super::BindContext;
use crate::analyze::{AnalysisError, AnalyzedType, BindingUseKind};
use crate::{GqlType, GraphExpression, SourceSpan, WorkingScopeClause};

impl BindContext<'_> {
    pub(super) fn with_working_scopes<T>(
        &mut self,
        clauses: &[WorkingScopeClause],
        body: impl FnOnce(&mut Self) -> Result<T, AnalysisError>,
    ) -> Result<T, AnalysisError> {
        let saved = self
            .catalog
            .as_ref()
            .map(|catalog| (catalog.schema, catalog.graph));
        let result = (|| {
            for clause in clauses {
                let (kind, origin) = match clause {
                    WorkingScopeClause::Nested(span) => {
                        (crate::ScopeKind::QuerySpecification, *span)
                    }
                    WorkingScopeClause::At { span, .. } => (crate::ScopeKind::WorkingSchema, *span),
                    WorkingScopeClause::Use { span, .. } => (crate::ScopeKind::WorkingGraph, *span),
                };
                self.current = self.scopes.push_scope(self.current, kind, origin, false);
                match clause {
                    WorkingScopeClause::Nested(_) => {}
                    WorkingScopeClause::At { reference, span } => {
                        let catalog = self
                            .catalog
                            .as_mut()
                            .ok_or_else(|| missing_environment(*span))?;
                        catalog.select_schema(reference)?;
                        catalog.record_site(self.current, *span);
                    }
                    WorkingScopeClause::Use { expression, span } => {
                        self.select_working_graph(expression)?;
                        self.use_working_graph(expression.span())?;
                        self.catalog
                            .as_mut()
                            .expect("selected catalog graph")
                            .record_site(self.current, *span);
                    }
                }
            }
            body(self)
        })();
        if let Some((schema, graph)) = saved
            && let Some(catalog) = &mut self.catalog
        {
            catalog.schema = schema;
            catalog.graph = graph;
        }
        result
    }

    fn select_working_graph(&mut self, expression: &GraphExpression) -> Result<(), AnalysisError> {
        // §11.1 SR2: only a regular bare name that is a valid in-scope
        // incoming-working-record binding uses the binding namespace. NEXT's
        // working-table columns are not that record. Explicit ./name and
        // delimited names remain catalog references. Parameters are disjoint.
        let variable = match expression {
            GraphExpression::Variable { name, .. } => Some(name),
            GraphExpression::Reference {
                reference,
                may_reference_binding: true,
            } if self.graph_binding(&reference.leaf().name).is_some() => {
                Some(&reference.leaf().name)
            }
            _ => None,
        };
        if let Some(name) = variable {
            if self.graph_binding(name).is_none() {
                return Err(AnalysisError::undefined_reference(
                    name.clone(),
                    expression.span(),
                ));
            }
            let binding =
                self.resolve(name.clone(), expression.span(), BindingUseKind::Variable)?;
            let ty = self.binding_type(binding);
            if let AnalyzedType::Resolved(found) = ty
                && found.strip_not_null() != &GqlType::GraphRef
            {
                return Err(AnalysisError::TypeMismatch {
                    context: crate::TypeMismatchContext::GraphExpression,
                    expected: crate::ExpectedType::Specific(GqlType::GraphRef),
                    found,
                    span: expression.span(),
                });
            }
            return Err(AnalysisError::NotImplemented {
                message: "graph-valued binding expressions are not implemented (GV60)".into(),
                span: expression.span(),
                hint: None,
            });
        }
        let catalog = self
            .catalog
            .as_mut()
            .ok_or_else(|| missing_environment(expression.span()))?;
        if let GraphExpression::Reference { reference, .. } = expression {
            catalog.graph = catalog.resolve_graph(reference)?;
        }
        Ok(())
    }

    fn graph_binding(&self, name: &selene_core::DbString) -> Option<crate::BindingId> {
        // The bounded surface has no procedure-local definition block. Only
        // a subquery's imported/correlated incoming record can supply a graph
        // binding. Its boundary preserves CALL () and explicit-import rules.
        let mut cursor = Some(self.current);
        while let Some(id) = cursor {
            let scope = self.scopes.scope(id)?;
            if scope.kind == crate::ScopeKind::Subquery {
                return self.scopes.resolve(id, name.clone());
            }
            cursor = scope.parent;
        }
        None
    }

    pub(super) fn use_working_graph(&mut self, span: SourceSpan) -> Result<(), AnalysisError> {
        if let Some(catalog) = &mut self.catalog {
            catalog.use_graph(span)?;
        }
        Ok(())
    }
}

fn missing_environment(span: SourceSpan) -> AnalysisError {
    AnalysisError::NotImplemented {
        message: "working catalog scopes require a catalog-bound analysis environment".into(),
        span,
        hint: None,
    }
}
