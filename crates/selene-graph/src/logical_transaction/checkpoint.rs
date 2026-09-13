//! Version-one full semantic image inside the format-2 persistence envelope.
//! All three mandatory sections share one allocation/work/byte budget.

use super::*;
use crate::GraphTypeDef;
use selene_core::Change;

/// Encode one pinned catalog, named type inventory and complete graph inventory.
/// The caller owns the publication reservation and supplies only that immutable view.
/// Runtime references, rows, index contents and callable code are never encoded.
pub fn encode_checkpoint(
    catalog: &CatalogLogicalRecords,
    types: &BTreeMap<GraphTypeId, Arc<GraphTypeDef>>,
    graphs: &[&SeleneGraph],
    limits: Limits,
) -> CodecResult<Vec<u8>> {
    let mut e = Encoder::new(limits)?;
    e.u32(1)?;
    e.u8(1)?;
    selene_catalog::codec::encode_records(catalog, &mut e)?;
    e.u8(2)?;
    e.count(types.len())?;
    for (id, ty) in types {
        e.u64(id.get())?;
        e.graph_definition(&schema::definition(ty)?)?;
    }
    e.u8(3)?;
    e.count(graphs.len())?;
    if graphs
        .windows(2)
        .any(|p| p[0].graph_id() >= p[1].graph_id())
    {
        return Err(E::Invalid("checkpoint graph identity order"));
    }
    let catalog = catalog.reconstruct().map_err(|_| E::Semantic)?;
    for graph in graphs {
        let mut bound = (*graph).clone();
        bound.bind_catalog(&catalog).map_err(|_| E::Semantic)?;
        encode_graph(&bound, &mut e)?;
    }
    Ok(e.finish())
}

fn encode_graph(graph: &SeleneGraph, e: &mut Encoder) -> CodecResult<()> {
    e.u64(graph.graph_id().get())?;
    e.boolean(false)?; // Full graph, not a delta against an implicit seed.
    e.u64(graph.meta.generation)?;
    e.u64(graph.meta.next_node_id)?;
    e.u64(graph.meta.next_edge_id)?;
    e.boolean(graph.meta.bound_type.is_some())?;
    if let Some(ty) = &graph.meta.bound_type {
        e.graph_definition(&schema::definition(ty)?)?;
    }
    let mut backing: Vec<_> = graph
        .catalog_bound_indexes()
        .map(|d| d.id().get())
        .collect();
    backing.sort_unstable();
    e.count(backing.len())?;
    for id in backing {
        e.u64(id)?;
    }
    let rows = graph
        .node_store
        .len()
        .checked_add(graph.edge_store.len())
        .ok_or(E::Limit)?;
    e.budget
        .charge(rows, rows.checked_mul(32).ok_or(E::Limit)?)?;
    let count = graph
        .node_count()
        .checked_add(graph.edge_count())
        .ok_or(E::Limit)?;
    e.count_for::<Change>(count)?;
    let mut nodes: Vec<_> = graph
        .node_store
        .row_to_id
        .iter()
        .copied()
        .filter(|id| graph.is_node_alive(*id))
        .collect();
    nodes.sort_unstable();
    for id in nodes {
        let properties = graph.node_properties(id).ok_or(E::Semantic)?;
        for (_, value) in properties.iter() {
            e.budget.stored_clone(value)?;
        }
        e.graph_change(&Change::NodeCreated {
            id,
            labels: graph.node_labels(id).ok_or(E::Semantic)?.clone(),
            properties: properties.clone(),
        })?;
    }
    let mut edges: Vec<_> = graph
        .edge_store
        .row_to_id
        .iter()
        .copied()
        .filter(|id| graph.is_edge_alive(*id))
        .collect();
    edges.sort_unstable();
    for id in edges {
        let edge = graph.edge_record(id).ok_or(E::Semantic)?;
        let properties = graph.edge_properties(id).ok_or(E::Semantic)?;
        for (_, value) in properties.iter() {
            e.budget.stored_clone(value)?;
        }
        e.graph_change(&Change::EdgeCreated {
            id,
            directionality: edge.directionality,
            label: edge.label,
            source: edge.first,
            target: edge.second,
            properties: properties.clone(),
        })?;
    }
    Ok(())
}

impl ReplayState {
    /// Decode a self-contained image, validating complete catalog/graph/type coverage.
    /// No current-process seed, legacy byte decoder or runtime index activation is used.
    /// The aggregate image shares one budget, not a fresh ceiling per graph/section.
    pub fn from_checkpoint(bytes: &[u8], limits: Limits) -> CodecResult<Self> {
        let mut budget = Budget::new(limits)?;
        let (catalog, graph_types, graphs) = {
            let mut d = Decoder::new(bytes, &mut budget)?;
            if d.u32()? != 1 {
                return Err(E::Unsupported("checkpoint body version"));
            }
            if d.u8()? != 1 {
                return Err(E::Invalid("checkpoint catalog section"));
            }
            let catalog = selene_catalog::codec::decode_records(&mut d)?;
            if d.u8()? != 2 {
                return Err(E::Invalid("checkpoint named type section"));
            }
            let count = d.count()?;
            let mut graph_types: Vec<TypeDelta> = Vec::with_capacity(count);
            for _ in 0..count {
                let id = GraphTypeId::new(d.u64()?).map_err(|_| E::Semantic)?;
                if graph_types.last().is_some_and(|last| last.id >= id) {
                    return Err(E::Invalid("checkpoint named type identity order"));
                }
                graph_types.push(TypeDelta {
                    id,
                    definition: Some(d.graph_definition()?),
                });
            }
            if d.u8()? != 3 {
                return Err(E::Invalid("checkpoint graph section"));
            }
            let count = d.count_for::<GraphDelta>()?;
            let mut graphs: Vec<GraphDelta> = Vec::with_capacity(count);
            for _ in 0..count {
                let graph = GraphDelta::decode(&mut d)?;
                if graph.previous.is_some()
                    || graphs.last().is_some_and(|last| last.id >= graph.id)
                    || graph.changes.iter().any(|change| {
                        !matches!(
                            change,
                            Change::NodeCreated { .. } | Change::EdgeCreated { .. }
                        )
                    })
                {
                    return Err(E::Invalid("checkpoint full graph"));
                }
                graphs.push(graph);
            }
            d.finish()?;
            (catalog, graph_types, graphs)
        };
        let snapshot = catalog.reconstruct().map_err(|_| E::Semantic)?;
        let tx = LogicalTransaction {
            catalog: CatalogDelta {
                previous: snapshot.generation(),
                generation: snapshot.generation(),
                high_water: selene_catalog::codec::DOMAINS
                    .map(|k| catalog.high_water().get(&k).copied().unwrap_or(0)),
                changes: vec![],
            },
            graph_types,
            graphs,
        };
        let mut state = Self {
            catalog,
            graph_types: BTreeMap::new(),
            graphs: BTreeMap::new(),
            backing_indexes: BTreeMap::new(),
        };
        let mut named_runtime = BTreeMap::new();
        for ty in &tx.graph_types {
            let def = ty.definition.as_ref().ok_or(E::Semantic)?;
            let descriptor = snapshot
                .descriptor(CatalogObjectId::GraphType(ty.id))
                .ok_or(E::Semantic)?;
            named_types::check_name(def, descriptor)?;
            named_runtime.insert(ty.id, named_types::materialize(def, &mut budget)?);
            state.graph_types.insert(ty.id, Arc::new(def.clone()));
        }
        for delta in &tx.graphs {
            let bound = delta
                .definition
                .as_ref()
                .map(schema::materialize)
                .transpose()?
                .map(Arc::new);
            let mut graph = super::graph_apply::logical_graph(None, delta, bound, &mut budget)?;
            graph
                .validate_logical_catalog(&snapshot, &delta.backing_indexes)
                .map_err(|_| E::Semantic)?;
            graph
                .admit_replay_constraints(None, &snapshot, &delta.changes)
                .map_err(|_| E::Semantic)?;
            state.graphs.insert(delta.id, Arc::new(graph));
            state
                .backing_indexes
                .insert(delta.id, delta.backing_indexes.clone().into());
        }
        state.validate_coverage()?;
        named_types::validate_bindings(
            &mut state,
            None,
            &snapshot,
            &snapshot,
            &tx,
            &mut named_runtime,
            &mut budget,
        )?;
        Ok(state)
    }
}
