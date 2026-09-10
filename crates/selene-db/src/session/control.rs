//! Persistent facade session-control resolution and atomic application.

use std::sync::Arc;

use selene_catalog::GraphId as LowerGraphId;
use selene_gql::{PreparedCatalogRequest, PreparedSessionControl, SessionSetGraphTarget};
use selene_graph::SharedGraph;

use crate::{
    CatalogReadSnapshot, Error, ExecutionOutcome, GeneralParameter, GraphDescriptor, Result,
    SchemaDescriptor, TransactionState, database::DatabaseInner, ddl,
    session_context::TransactionCheckout,
};

use super::{Session, execution::lower_session};

pub(crate) enum ResolvedSessionControl {
    NoOp,
    SetValue {
        name: selene_core::DbString,
        parameter: GeneralParameter,
    },
    SetTimeZone {
        zone: jiff::tz::TimeZone,
        displacement_seconds: i32,
    },
    SetSchema(SchemaDescriptor),
    SetGraph(GraphDescriptor),
    ResetAllCharacteristics {
        schema: SchemaDescriptor,
        graph: GraphDescriptor,
    },
    ResetSchema(SchemaDescriptor),
    ResetGraph(GraphDescriptor),
    ResetParameters,
    ResetTimeZone,
    ResetParameter(selene_core::DbString),
    Close,
}

impl ResolvedSessionControl {
    fn changes_selection(&self) -> bool {
        matches!(
            self,
            Self::SetSchema(_)
                | Self::SetGraph(_)
                | Self::ResetAllCharacteristics { .. }
                | Self::ResetSchema(_)
                | Self::ResetGraph(_)
        )
    }
}

impl Session {
    pub(super) fn execute_session_control(
        &self,
        prepared: PreparedCatalogRequest,
        slot: &mut TransactionCheckout<'_>,
    ) -> Result<ExecutionOutcome> {
        let skip_if_exists = prepared
            .session_set_value_target()
            .is_some_and(|(name, guarded)| guarded && self.context.has_parameter(name));
        let control = match slot.as_ref() {
            Some(transaction) if transaction.descriptor().state() == TransactionState::Active => {
                let scratch =
                    SharedGraph::try_from_graph(transaction.draft()?.selected_graph()?.clone())
                        .map_err(Error::invalid_graph_type_source)?;
                resolve_on_graph(
                    &scratch,
                    self.audit_bytes(),
                    prepared,
                    skip_if_exists,
                    &self.inner.procedures,
                )?
            }
            _ => {
                let graph = self.context.current_graph();
                let id = LowerGraphId::new(graph.id.get()).map_err(|source| {
                    Error::invalid_session_reference(Error::from_catalog_invariant(source))
                })?;
                self.inner.resolve_live_session_control(
                    id,
                    &graph,
                    self.audit_bytes(),
                    prepared,
                    skip_if_exists,
                )?
            }
        };
        if matches!(control, PreparedSessionControl::Close) {
            return self.execute_close(slot);
        }
        let snapshot = CatalogReadSnapshot {
            state: self.inner.state.load_full(),
        };
        self.validate_context_snapshot(&snapshot)?;
        let generation = snapshot.generation();
        let resolved = resolve_catalog_control(
            crate::GraphRef::new(self.database_id(), self.context.current_graph().id),
            &snapshot,
            &self.context.current_schema(),
            &self.context.reset_schema_target(),
            &self.context.reset_graph_target(),
            control,
        )?;

        if resolved.changes_selection()
            && slot.as_ref().is_some_and(|transaction| {
                matches!(
                    transaction.descriptor().state(),
                    TransactionState::Active | TransactionState::Failed
                )
            })
        {
            return Err(Error::active_transaction());
        }
        self.context.apply_session_control(resolved, generation);
        Ok(ExecutionOutcome::SUCCESSFUL_OMITTED)
    }

    pub(super) fn execute_graph_independent_session_control(
        &self,
        control: PreparedSessionControl,
        slot: &mut TransactionCheckout<'_>,
    ) -> Result<ExecutionOutcome> {
        if matches!(control, PreparedSessionControl::Close) {
            return self.execute_close(slot);
        }
        let changes_selection = match &control {
            PreparedSessionControl::ResetAllCharacteristics
            | PreparedSessionControl::ResetSchema
            | PreparedSessionControl::ResetGraph => true,
            PreparedSessionControl::ResetParameters
            | PreparedSessionControl::ResetTimeZone
            | PreparedSessionControl::ResetParameter(_) => false,
            _ => {
                return Err(Error::catalog_invariant(
                    "graph-independent session control was not a reset",
                ));
            }
        };
        if changes_selection
            && slot.as_ref().is_some_and(|transaction| {
                matches!(
                    transaction.descriptor().state(),
                    TransactionState::Active | TransactionState::Failed
                )
            })
        {
            return Err(Error::active_transaction());
        }
        let snapshot = CatalogReadSnapshot {
            state: self.inner.state.load_full(),
        };
        let generation = snapshot.generation();
        let resolved = resolve_catalog_control(
            crate::GraphRef::new(self.database_id(), self.context.current_graph().id),
            &snapshot,
            &self.context.current_schema(),
            &self.context.reset_schema_target(),
            &self.context.reset_graph_target(),
            control,
        )?;
        self.context.apply_session_control(resolved, generation);
        Ok(ExecutionOutcome::SUCCESSFUL_OMITTED)
    }

    pub(super) fn execute_close(
        &self,
        slot: &mut TransactionCheckout<'_>,
    ) -> Result<ExecutionOutcome> {
        if slot.as_ref().is_some_and(|transaction| {
            matches!(
                transaction.descriptor().state(),
                TransactionState::Active | TransactionState::Failed
            )
        }) {
            self.rollback_transaction_checked(slot)?;
        }
        drop(slot.take());
        self.context.apply_session_control(
            ResolvedSessionControl::Close,
            self.context.catalog_generation(),
        );
        Ok(ExecutionOutcome::SUCCESSFUL_OMITTED)
    }
}

impl DatabaseInner {
    fn resolve_live_session_control(
        &self,
        id: LowerGraphId,
        graph: &GraphDescriptor,
        audit: Option<Arc<[u8]>>,
        prepared: PreparedCatalogRequest,
        skip_if_exists: bool,
    ) -> Result<PreparedSessionControl> {
        self.with_graph_request(id, &graph.path, |runtime| {
            resolve_on_graph(runtime, audit, prepared, skip_if_exists, &self.procedures)
        })
    }
}

fn resolve_on_graph(
    graph: &SharedGraph,
    audit: Option<Arc<[u8]>>,
    prepared: PreparedCatalogRequest,
    skip_if_exists: bool,
    procedures: &selene_gql::BuiltinProcedureRegistry,
) -> Result<PreparedSessionControl> {
    lower_session(graph, audit)
        .resolve_prepared_session_control(prepared, procedures, skip_if_exists)
        .map_err(Error::from_engine)
}

fn resolve_catalog_control(
    graph: crate::GraphRef,
    snapshot: &CatalogReadSnapshot,
    current_schema: &SchemaDescriptor,
    reset_schema: &SchemaDescriptor,
    reset_graph: &GraphDescriptor,
    control: PreparedSessionControl,
) -> Result<ResolvedSessionControl> {
    Ok(match control {
        PreparedSessionControl::NoOp => ResolvedSessionControl::NoOp,
        PreparedSessionControl::SetValue {
            param,
            declared_type,
            value,
        } => ResolvedSessionControl::SetValue {
            name: param,
            parameter: GeneralParameter::from_session_value(
                selene_gql::normalize_value_type(&declared_type).map_err(|source| {
                    Error::from_engine(selene_gql::ExecutorError::Analysis {
                        source: selene_gql::AnalysisError::StructuralType {
                            source,
                            span: selene_gql::SourceSpan::default(),
                        },
                    })
                })?,
                crate::Value::from_lower(&value, graph)?,
            )?,
        },
        PreparedSessionControl::SetTimeZone {
            zone,
            displacement_seconds,
        } => ResolvedSessionControl::SetTimeZone {
            zone,
            displacement_seconds,
        },
        PreparedSessionControl::SetSchema(reference) => {
            let path = ddl::resolve_schema(&current_schema.path, &reference)?;
            ResolvedSessionControl::SetSchema(snapshot.resolve_schema(&path)?)
        }
        PreparedSessionControl::SetGraph(SessionSetGraphTarget::CatalogReference(reference)) => {
            let path = ddl::resolve_graph(&current_schema.path, &reference)?;
            ResolvedSessionControl::SetGraph(snapshot.resolve_graph(&path)?)
        }
        PreparedSessionControl::SetGraph(
            SessionSetGraphTarget::CurrentGraph
            | SessionSetGraphTarget::CurrentPropertyGraph
            | SessionSetGraphTarget::SchemaReference(_),
        ) => ResolvedSessionControl::NoOp,
        PreparedSessionControl::ResetAllCharacteristics => {
            if !snapshot.matches_schema_reference(reset_schema)
                || !snapshot.matches_graph_reference(reset_graph)
            {
                return Err(Error::stale_session_reference());
            }
            ResolvedSessionControl::ResetAllCharacteristics {
                schema: reset_schema.clone(),
                graph: reset_graph.clone(),
            }
        }
        PreparedSessionControl::ResetSchema => {
            if !snapshot.matches_schema_reference(reset_schema) {
                return Err(Error::stale_session_reference());
            }
            ResolvedSessionControl::ResetSchema(reset_schema.clone())
        }
        PreparedSessionControl::ResetGraph => {
            if !snapshot.matches_graph_reference(reset_graph) {
                return Err(Error::stale_session_reference());
            }
            ResolvedSessionControl::ResetGraph(reset_graph.clone())
        }
        PreparedSessionControl::ResetParameters => ResolvedSessionControl::ResetParameters,
        PreparedSessionControl::ResetTimeZone => ResolvedSessionControl::ResetTimeZone,
        PreparedSessionControl::ResetParameter(name) => {
            ResolvedSessionControl::ResetParameter(name)
        }
        PreparedSessionControl::Close => ResolvedSessionControl::Close,
    })
}
