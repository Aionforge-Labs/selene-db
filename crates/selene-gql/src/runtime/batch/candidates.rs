//! Typed scan-candidate resolution bound to the pinned snapshot.
//!
//! Batch scans resolve their candidates through the same
//! [`candidate_entities`](super::super::scan::candidate_entities) entry the
//! row executor uses, so index selection, label bitmaps, and typed/composite
//! lookups agree with the row path by construction: no new index semantics
//! live here. The resolution captures the snapshot identity (graph id plus
//! immutable generation) alongside the stable entity ids; operators revalidate
//! that binding against the execution context before producing rows, so a
//! candidate set from another snapshot or generation fails predictably instead
//! of being silently rebound from physical rows. Cached execution plans carry
//! only access paths ([`ScanAccess`](crate::ScanAccess) handles), never
//! candidate ids, so plan-cache reuse across generations always re-resolves
//! here against the current snapshot.

use selene_core::{GraphId, Value};
use selene_graph::SeleneGraph;

use crate::{NodeOrEdgeScan, runtime::ExecutorError};

use super::super::EvalCtx;
use super::super::scan::{ScanEntityId, candidate_entities};

/// Candidates resolved for one batch scan, with their snapshot binding.
///
/// The entity order is the row path's candidate order (deterministic ascending
/// stable-id order from the typed candidate sets, or snapshot order from the
/// linear path), so batch pulls and row iteration visit the same sequence.
#[derive(Clone, Debug)]
pub(crate) struct ResolvedCandidates {
    graph_id: GraphId,
    generation: u64,
    entities: Vec<ScanEntityId>,
}

impl ResolvedCandidates {
    /// Resolve `scan` against the evaluation context's snapshot.
    ///
    /// # Errors
    ///
    /// Returns the row path's candidate errors (index key resolution,
    /// parameter, or graph errors mapped exactly as the row scan maps them).
    pub(crate) fn resolve(
        scan: &NodeOrEdgeScan,
        ctx: &EvalCtx<'_, '_, '_, '_>,
    ) -> Result<Self, ExecutorError> {
        let snapshot = ctx.tx.snapshot();
        let binding = SnapshotBinding::of(snapshot);
        let entities = candidate_entities(scan, ctx)?;
        Ok(Self {
            graph_id: binding.graph_id,
            generation: binding.generation,
            entities,
        })
    }

    /// Return the snapshot binding captured at resolution time.
    #[must_use]
    pub(crate) const fn binding(&self) -> SnapshotBinding {
        SnapshotBinding {
            graph_id: self.graph_id,
            generation: self.generation,
        }
    }

    /// Borrow the resolved entities in row-path candidate order.
    #[must_use]
    pub(crate) fn entities(&self) -> &[ScanEntityId] {
        &self.entities
    }

    /// Return the number of resolved entities.
    #[must_use]
    pub(crate) fn len(&self) -> usize {
        self.entities.len()
    }
}

/// Snapshot identity captured with a candidate resolution.
///
/// Graph id plus immutable generation: both must match the pinned execution
/// snapshot before any row is produced.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct SnapshotBinding {
    graph_id: GraphId,
    generation: u64,
}

impl SnapshotBinding {
    /// Capture the identity of `snapshot`.
    #[must_use]
    pub(crate) fn of(snapshot: &SeleneGraph) -> Self {
        Self {
            graph_id: snapshot.meta.graph_id,
            generation: snapshot.meta.generation,
        }
    }

    /// Revalidate this binding against the execution snapshot.
    ///
    /// # Errors
    ///
    /// Returns a deterministic `ImplementationDefined` error when the
    /// candidates belong to another graph or generation. Stale candidates
    /// never silently rebind: there is intentionally no path that translates
    /// stored physical rows into a new snapshot.
    pub(crate) fn validate(&self, snapshot: &SeleneGraph) -> Result<(), ExecutorError> {
        let current = Self::of(snapshot);
        if current == *self {
            return Ok(());
        }
        Err(ExecutorError::ImplementationDefined {
            detail: "batch scan candidates are stale for the pinned snapshot",
        })
    }
}

/// Convert a resolved entity into its stable graph-identity value.
///
/// Batch positions never appear in output: every scanned value is the stable
/// node/edge identity the row path emits.
#[must_use]
pub(crate) const fn entity_value(entity: ScanEntityId) -> Value {
    match entity {
        ScanEntityId::Node(id) => Value::NodeRef(id),
        ScanEntityId::Edge(id) => Value::EdgeRef(id),
    }
}
