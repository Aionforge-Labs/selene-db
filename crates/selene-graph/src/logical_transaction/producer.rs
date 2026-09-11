use crate::SeleneGraph;
use selene_core::{
    Change, SchemaChange,
    logical::{CodecError as E, CodecResult, GraphDelta},
};

/// Adapt real mutation-funnel changes plus the validated final graph snapshot.
/// Schema events are represented by complete logical definitions and catalog backing
/// identities, rather than by legacy implicit graph-type IDs or serde discriminants.
pub fn graph_delta(
    original: Option<&SeleneGraph>,
    next: &SeleneGraph,
    changes: &[Change],
) -> CodecResult<GraphDelta> {
    let mut data = Vec::new();
    for change in changes {
        match change {
            Change::SchemaChanged { graph, change } => {
                if *graph != next.graph_id() {
                    return Err(E::Invalid("schema graph identity"));
                }
                match change {
                    SchemaChange::NodeTypeAddedV2 { .. }
                    | SchemaChange::EdgeTypeAddedV2 { .. }
                    | SchemaChange::NodeTypeAlteredV2 { .. }
                    | SchemaChange::EdgeTypeAlteredV2 { .. }
                    | SchemaChange::NodeTypeDropped { .. }
                    | SchemaChange::EdgeTypeDropped { .. }
                    | SchemaChange::PropertyIndexCreated { .. }
                    | SchemaChange::PropertyIndexCreatedNamed { .. }
                    | SchemaChange::PropertyIndexDropped { .. }
                    | SchemaChange::CompositePropertyIndexCreated { .. }
                    | SchemaChange::CompositePropertyIndexDropped { .. }
                    | SchemaChange::VectorIndexCreated { .. }
                    | SchemaChange::VectorIndexDropped { .. }
                    | SchemaChange::TextIndexCreated { .. }
                    | SchemaChange::TextIndexDropped { .. }
                    | SchemaChange::EdgePropertyIndexCreated { .. }
                    | SchemaChange::EdgePropertyIndexDropped { .. } => {}
                    SchemaChange::GraphCreated { .. }
                    | SchemaChange::GraphDropped { .. }
                    | SchemaChange::GraphTypeCreated { .. }
                    | SchemaChange::GraphTypeDropped { .. }
                    | SchemaChange::RecordTypeAdded { .. }
                    | SchemaChange::NodeTypeAdded { .. }
                    | SchemaChange::EdgeTypeAdded { .. } => {
                        return Err(E::Invalid("legacy-only catalog change"));
                    }
                }
            }
            _ => data.push(change.clone()),
        }
    }
    let mut backing_indexes: Vec<_> = next.catalog_bound_indexes().map(|d| d.id().get()).collect();
    if backing_indexes.len()
        != next.property_index.len()
            + next.edge_property_index.len()
            + next.composite_property_index.len()
            + next.vector_index.len()
            + next.text_index.len()
    {
        return Err(E::Invalid(
            "unbound index registrations require catalog metadata",
        ));
    }
    backing_indexes.sort_unstable();
    Ok(GraphDelta {
        id: next.graph_id(),
        previous: original.map(|g| g.meta.generation),
        generation: next.meta.generation,
        next_node_id: next.meta.next_node_id,
        next_edge_id: next.meta.next_edge_id,
        definition: next
            .meta
            .bound_type
            .as_deref()
            .map(super::schema::definition)
            .transpose()?,
        backing_indexes,
        changes: data,
    })
}
