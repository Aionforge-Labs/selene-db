//! Eager, all-or-error runtime admission from validated logical state.

use super::*;
use crate::graph::{
    CompositePropertyIndexEntry, PropertyIndexEntry, TextIndexEntry, VectorIndexEntry,
};
use crate::{GraphTypeDef, SharedGraph, TypedIndex, TypedIndexKind, VectorIndex, VectorIndexKind};
use selene_catalog::{CatalogSnapshot, ElementKind, IndexConfiguration};
use selene_core::{SchemaPropertyIndexKind as P, SchemaVectorIndexKind as V, db_string};
use smallvec::SmallVec;

/// Fully rebuilt graph runtimes; native callable admission remains the facade's duty.
pub struct ReconstructedRuntime {
    /// Complete authoritative catalog, including retained ineligible declarations.
    pub catalog: CatalogSnapshot,
    /// Validated named type bodies indexed by stable catalog identity.
    pub graph_types: BTreeMap<GraphTypeId, Arc<GraphTypeDef>>,
    /// All graphs, with every retained registered backing eagerly rebuilt.
    pub graphs: BTreeMap<GraphId, SharedGraph>,
    /// Number of rebuilt retained index registrations, including ineligible backing.
    pub rebuilt_indexes: usize,
}

impl ReplayState {
    /// Recreate every registered scalar/edge/composite/vector/text backing before
    /// returning any usable graph. No optional deferral or silent missing index.
    /// The single cumulative budget covers retained primary values, metadata,
    /// reconstruction and conservative all-index allocation/work charges.
    pub fn materialize(&self, limits: Limits) -> CodecResult<ReconstructedRuntime> {
        let mut budget = Budget::new(limits)?;
        selene_catalog::codec::account_records(&self.catalog, &mut budget)?;
        self.validate_coverage()?;
        let catalog = self.catalog.reconstruct().map_err(|_| E::Semantic)?;
        let mut graph_types = BTreeMap::new();
        for (id, definition) in &self.graph_types {
            graph_types.insert(*id, named_types::materialize(definition, &mut budget)?);
        }
        let mut graphs = BTreeMap::new();
        let mut rebuilt_indexes = 0;
        for (id, original) in &self.graphs {
            let rows = original
                .node_store
                .len()
                .checked_add(original.edge_store.len())
                .ok_or(E::Limit)?;
            budget.charge(rows, rows.checked_mul(4096).ok_or(E::Limit)?)?;
            for properties in original
                .node_store
                .properties
                .iter()
                .chain(original.edge_store.properties.iter())
            {
                for (_, value) in properties.iter() {
                    budget.stored_clone(value)?;
                }
            }
            let mut graph = original.as_ref().clone();
            graph
                .validate_logical_catalog(&catalog, &self.backing_indexes[id])
                .map_err(|_| E::Semantic)?;
            for raw in self.backing_indexes[id].iter() {
                let index_id = selene_catalog::IndexId::new(*raw).map_err(|_| E::Semantic)?;
                let descriptor = catalog
                    .descriptor(CatalogObjectId::Index(index_id))
                    .ok_or(E::Semantic)?;
                let CatalogPayload::Index(index) = descriptor.payload() else {
                    return Err(E::Semantic);
                };
                account_index(&graph, index, &mut budget)?;
                register(&mut graph, descriptor)?;
                rebuilt_indexes += 1;
            }
            graph.bind_catalog(&catalog).map_err(|_| E::Semantic)?;
            // This existing owner rebuilds every placeholder before spawning its
            // memory-only committer. No legacy WAL or snapshot decoder is involved.
            let runtime = SharedGraph::try_from_graph(graph).map_err(|_| E::Semantic)?;
            if runtime.read().catalog_bound_indexes().count() != self.backing_indexes[id].len() {
                return Err(E::Admission("incomplete rebuilt runtime"));
            }
            graphs.insert(*id, runtime);
        }
        Ok(ReconstructedRuntime {
            catalog,
            graph_types,
            graphs,
            rebuilt_indexes,
        })
    }
}

fn account_index(
    graph: &SeleneGraph,
    index: &selene_catalog::IndexDeclaration,
    budget: &mut Budget,
) -> CodecResult<()> {
    let rows = graph
        .node_store
        .len()
        .checked_add(graph.edge_store.len())
        .ok_or(E::Limit)?;
    let mut per_row = 4096usize
        .checked_mul(index.target.properties.len())
        .ok_or(E::Limit)?;
    if let IndexConfiguration::Vector {
        dimension,
        hnsw,
        ivf,
        ..
    } = &index.configuration
    {
        per_row = per_row
            .checked_add((*dimension as usize).checked_mul(64).ok_or(E::Limit)?)
            .ok_or(E::Limit)?;
        if let Some(config) = hnsw {
            per_row = per_row
                .checked_add(usize::from(config.max_neighbors) * 1024)
                .ok_or(E::Limit)?;
            budget.charge(
                usize::from(config.ef_construction),
                usize::from(config.ef_construction) * 1024,
            )?;
        }
        if let Some(config) = ivf {
            budget.charge(
                usize::from(config.target_centroids),
                usize::from(config.target_centroids)
                    .checked_mul(*dimension as usize)
                    .and_then(|n| n.checked_mul(64))
                    .ok_or(E::Limit)?,
            )?;
        }
    }
    budget.charge(rows, rows.checked_mul(per_row).ok_or(E::Limit)?)?;
    // Covers variable-width keys, tokenizer/postings work and copies, independently
    // of the fixed per-row allowance. This is enforced accounting, not RSS.
    for properties in graph
        .node_store
        .properties
        .iter()
        .chain(graph.edge_store.properties.iter())
    {
        for (key, value) in properties.iter() {
            if index.target.properties.iter().any(|p| p == key.as_str()) {
                let mut e = Encoder::counting(budget.clone());
                e.value(value, 1)?;
                *budget = e.budget;
                if let Value::String(text) = value {
                    budget.charge(
                        text.as_str().len(),
                        text.as_str().len().checked_mul(256).ok_or(E::Limit)?,
                    )?;
                }
            }
        }
    }
    Ok(())
}

fn register(
    graph: &mut SeleneGraph,
    descriptor: &selene_catalog::CatalogDescriptor,
) -> CodecResult<()> {
    let CatalogPayload::Index(index) = descriptor.payload() else {
        return Err(E::Semantic);
    };
    let label = db_string(&index.target.label).map_err(|_| E::Semantic)?;
    let properties: SmallVec<[DbString; 4]> = index
        .target
        .properties
        .iter()
        .map(|p| db_string(p))
        .collect::<Result<_, _>>()
        .map_err(|_| E::Semantic)?;
    let name = Some(db_string(descriptor.name().display()).map_err(|_| E::Semantic)?);
    let property = properties.first().ok_or(E::Semantic)?.clone();
    match &index.configuration {
        IndexConfiguration::Property(kinds) if properties.len() == 1 => {
            let entry = PropertyIndexEntry::new(TypedIndex::new(property_kind(kinds[0])), name);
            let entries = match index.target.element {
                ElementKind::Node => &mut graph.property_index,
                ElementKind::Edge => &mut graph.edge_property_index,
            };
            if entries.insert((label, property), entry).is_some() {
                return Err(E::Semantic);
            }
        }
        IndexConfiguration::Property(kinds) if index.target.element == ElementKind::Node => {
            let key = crate::graph::composite_property_key(&properties);
            let entry = CompositePropertyIndexEntry::new(
                crate::CompositeTypedIndex::new(kinds.iter().copied().map(property_kind).collect()),
                properties,
                name,
            );
            if graph
                .composite_property_index
                .insert((label, key), entry)
                .is_some()
            {
                return Err(E::Semantic);
            }
        }
        IndexConfiguration::Vector {
            kind,
            dimension,
            hnsw,
            ivf,
        } if index.target.element == ElementKind::Node => {
            let entry = VectorIndexEntry::new(
                VectorIndex::new_with_configs(vector_kind(*kind), *dimension, *hnsw, *ivf)
                    .map_err(|_| E::Semantic)?,
                name,
            );
            if graph
                .vector_index
                .insert((label, property), entry)
                .is_some()
            {
                return Err(E::Semantic);
            }
        }
        IndexConfiguration::Text if index.target.element == ElementKind::Node => {
            let entry = TextIndexEntry::new(
                crate::TextIndex::empty(label.clone(), property.clone()),
                name,
            );
            if graph.text_index.insert((label, property), entry).is_some() {
                return Err(E::Semantic);
            }
        }
        _ => return Err(E::Admission("unsupported index reconstruction")),
    }
    Ok(())
}

fn property_kind(kind: P) -> TypedIndexKind {
    match kind {
        P::Bool => TypedIndexKind::Bool,
        P::I64 => TypedIndexKind::I64,
        P::U64 => TypedIndexKind::U64,
        P::I128 => TypedIndexKind::I128,
        P::U128 => TypedIndexKind::U128,
        P::Decimal => TypedIndexKind::Decimal,
        P::F32 => TypedIndexKind::F32,
        P::F64 => TypedIndexKind::F64,
        P::String => TypedIndexKind::String,
        P::Date => TypedIndexKind::Date,
        P::LocalDateTime => TypedIndexKind::LocalDateTime,
        P::ZonedDateTime => TypedIndexKind::ZonedDateTime,
        P::LocalTime => TypedIndexKind::LocalTime,
        P::ZonedTime => TypedIndexKind::ZonedTime,
        P::Duration => TypedIndexKind::Duration,
        P::Uuid => TypedIndexKind::Uuid,
    }
}
fn vector_kind(kind: V) -> VectorIndexKind {
    match kind {
        V::Flat => VectorIndexKind::Flat,
        V::HnswSquaredEuclidean => VectorIndexKind::HnswSquaredEuclidean,
        V::HnswCosine => VectorIndexKind::HnswCosine,
        V::HnswNegativeInnerProduct => VectorIndexKind::HnswNegativeInnerProduct,
        V::IvfSquaredEuclidean => VectorIndexKind::IvfSquaredEuclidean,
        V::IvfCosine => VectorIndexKind::IvfCosine,
        V::IvfNegativeInnerProduct => VectorIndexKind::IvfNegativeInnerProduct,
        V::TurboQuantCosine => VectorIndexKind::TurboQuantCosine,
    }
}
