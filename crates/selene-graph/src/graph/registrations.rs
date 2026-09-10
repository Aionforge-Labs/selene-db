//! Exact owner-local declaration bindings, compiled once at admission/rebuild.

use rustc_hash::FxHashMap;
use selene_catalog::{
    CatalogDescriptor, CatalogError, CatalogGeneration, CatalogObjectId, CatalogPayload,
    CatalogResult, CatalogSnapshot, DeclarationState, ElementKind, GraphId, IndexConfiguration,
    IndexDeclaration, IndexFamily, generated_index_name,
};
use selene_core::{DbString, SchemaVectorIndexKind, db_string};
use smallvec::SmallVec;
use std::{borrow::Cow, sync::Arc};

use crate::{SeleneGraph, VectorIndexKind, schema_index_kind::schema_kind_from};

type SingleKey = (ElementKind, IndexFamily, DbString, DbString);
type CompositeKey = (DbString, SmallVec<[DbString; 4]>);

#[derive(Debug)]
struct BoundIndex {
    descriptor: usize,
    // Representation observed at admission: an advanced name/config change must
    // invalidate this binding, even if the physical target still exists.
    source_name: Option<DbString>,
    properties: SmallVec<[DbString; 4]>,
}

#[derive(Debug)]
struct CompiledBindings {
    owner: GraphId,
    generation: CatalogGeneration,
    declarations: Vec<CatalogDescriptor>,
    single: FxHashMap<SingleKey, BoundIndex>,
    composite: FxHashMap<CompositeKey, BoundIndex>,
}

impl CompiledBindings {
    fn indexes(&self) -> impl Iterator<Item = &BoundIndex> {
        self.single.values().chain(self.composite.values())
    }
}

/// Derived immutable metadata only. Clones share parsed keys and descriptor
/// revisions; neither physical rows nor historical whole catalogs are retained.
#[derive(Clone, Debug)]
pub(crate) struct CatalogBinding(Arc<CompiledBindings>);

impl SeleneGraph {
    /// Bind a detached runtime view to its authoritative catalog snapshot.
    ///
    /// Every physical registration must match exactly one declaration's owner,
    /// effective name, target and configuration. A ready declaration without a
    /// matching implementation fails admission. Ineligible declarations can
    /// describe retained backing but never become query-eligible as a result.
    #[doc(hidden)]
    pub fn bind_catalog(&mut self, catalog: &CatalogSnapshot) -> CatalogResult<()> {
        let owner = GraphId::new(self.graph_id().get())?;
        if catalog.descriptor(CatalogObjectId::Graph(owner)).is_none() {
            return Err(invalid("missing_runtime_owner"));
        }
        let declarations = catalog
            .declarations(CatalogObjectId::Graph(owner))
            .cloned()
            .collect();
        let binding = self.compile_bindings(owner, catalog.generation(), declarations)?;
        self.catalog_binding = Some(binding);
        Ok(())
    }

    fn compile_bindings(
        &self,
        owner: GraphId,
        generation: CatalogGeneration,
        declarations: Vec<CatalogDescriptor>,
    ) -> CatalogResult<CatalogBinding> {
        if owner.get() != self.graph_id().get() {
            return Err(invalid("wrong_runtime_owner"));
        }
        let mut single = FxHashMap::default();
        let mut composite = FxHashMap::default();
        for (position, descriptor) in declarations.iter().enumerate() {
            let CatalogPayload::Index(index) = descriptor.payload() else {
                continue;
            };
            let label =
                db_string(&index.target.label).map_err(|_| invalid("invalid_property_target"))?;
            let properties = index
                .target
                .properties
                .iter()
                .map(|key| db_string(key))
                .collect::<Result<SmallVec<[DbString; 4]>, _>>()
                .map_err(|_| invalid("invalid_property_target"))?;
            let runtime_name = self.matching_runtime_name(
                index.target.element,
                &label,
                &properties,
                &index.configuration,
            );
            let matching_name = runtime_name.is_some_and(|name| {
                let effective = name
                    .as_ref()
                    .map(|name| Cow::Borrowed(name.as_str()))
                    .unwrap_or_else(|| {
                        Cow::Owned(generated_index_name(
                            index.configuration.family(),
                            &index.target.label,
                            index.target.properties.iter().map(String::as_str),
                        ))
                    });
                effective == descriptor.name().display()
            });
            if !matching_name {
                if index.metadata.state == DeclarationState::Ready {
                    return Err(invalid("missing_index_implementation"));
                }
                continue;
            }
            let binding = BoundIndex {
                descriptor: position,
                source_name: runtime_name.expect("matching runtime name").clone(),
                properties,
            };
            let duplicate = if binding.properties.len() == 1 {
                single
                    .insert(
                        (
                            index.target.element,
                            index.configuration.family(),
                            label,
                            binding.properties[0].clone(),
                        ),
                        binding,
                    )
                    .is_some()
            } else {
                composite
                    .insert(
                        (label, super::composite_property_key(&binding.properties)),
                        binding,
                    )
                    .is_some()
            };
            if duplicate {
                return Err(invalid("ambiguous_index_implementation"));
            }
        }
        // Each inserted key is a distinct existing physical registration. Equal
        // cardinality therefore establishes coverage, not the old match-count
        // heuristic that let multiple aliases conceal an unbound registration.
        if single.len() + composite.len()
            != self.property_index.len()
                + self.edge_property_index.len()
                + self.composite_property_index.len()
                + self.vector_index.len()
                + self.text_index.len()
        {
            return Err(invalid("undeclared_index_implementation"));
        }
        self.validate_constraint_bindings(&declarations)?;
        Ok(CatalogBinding(Arc::new(CompiledBindings {
            owner,
            generation,
            declarations,
            single,
            composite,
        })))
    }

    fn validate_constraint_bindings(
        &self,
        declarations: &[CatalogDescriptor],
    ) -> CatalogResult<()> {
        let mut rules = self.unique_declarations();
        for descriptor in declarations {
            let CatalogPayload::Constraint(constraint) = descriptor.payload() else {
                continue;
            };
            if constraint.metadata.state != DeclarationState::Ready {
                continue;
            }
            let Some(position) = rules.iter().position(|rule| {
                rule.target == constraint.target
                    && rule.declaring_type == constraint.declaring_type
                    && rule.kind == constraint.kind
                    && rule.backing_index == constraint.backing_index
            }) else {
                return Err(invalid("unsupported_constraint_activation"));
            };
            rules.swap_remove(position);
        }
        if !rules.is_empty() {
            return Err(invalid("undeclared_unique_implementation"));
        }
        Ok(())
    }

    /// Logical identities of actual bound physical registrations, including
    /// retained ineligible backing. This metadata-only seam lets the facade
    /// stage trusted drop events by identity, not delete target-wide alternatives.
    #[doc(hidden)]
    pub fn catalog_bound_indexes(&self) -> impl Iterator<Item = &CatalogDescriptor> {
        self.catalog_binding.iter().flat_map(|binding| {
            binding
                .0
                .indexes()
                .map(|index| &binding.0.declarations[index.descriptor])
        })
    }

    /// Carry declaration authority over a rebuilt layout, validating the new
    /// implementations. Compaction must never turn a bound graph into unbound.
    pub(crate) fn rebind_catalog_after_rebuild(&mut self, source: &Self) -> CatalogResult<()> {
        if let Some(binding) = &source.catalog_binding {
            self.catalog_binding = Some(self.compile_bindings(
                binding.0.owner,
                binding.0.generation,
                binding.0.declarations.clone(),
            )?);
        }
        Ok(())
    }

    pub(crate) fn catalog_index_usable(
        &self,
        element: ElementKind,
        label: &DbString,
        properties: &[DbString],
        family: IndexFamily,
    ) -> bool {
        let Some(binding) = &self.catalog_binding else {
            return true;
        };
        if binding.0.owner.get() != self.graph_id().get() {
            return false;
        }
        let index = if properties.len() == 1 {
            binding
                .0
                .single
                .get(&(element, family, label.clone(), properties[0].clone()))
        } else if element == ElementKind::Node && family == IndexFamily::Property {
            binding
                .0
                .composite
                .get(&(label.clone(), super::composite_property_key(properties)))
        } else {
            None
        };
        let Some(index) = index else { return false };
        let CatalogPayload::Index(declaration) = binding.0.declarations[index.descriptor].payload()
        else {
            unreachable!("compiled index descriptor")
        };
        // Profile and revision validation was performed at admission; these
        // descriptors are immutable. Current native name/config and completeness
        // are separate checks, so schema/data changes cannot reuse stale proof.
        declaration.metadata.state == DeclarationState::Ready
            && self.matching_runtime_name(
                element,
                label,
                &index.properties,
                &declaration.configuration,
            ) == Some(&index.source_name)
    }

    /// Compare logical target/configuration with native registration metadata.
    /// This cold inspection is not binding proof: it intentionally ignores the
    /// declaration name/state. Query access uses the exact keyed binding above.
    #[doc(hidden)]
    #[must_use]
    pub fn matches_index_declaration(&self, declaration: &IndexDeclaration) -> bool {
        let Ok(label) = db_string(&declaration.target.label) else {
            return false;
        };
        let Ok(properties) = declaration
            .target
            .properties
            .iter()
            .map(|key| db_string(key))
            .collect::<Result<SmallVec<[DbString; 4]>, _>>()
        else {
            return false;
        };
        self.matching_runtime_name(
            declaration.target.element,
            &label,
            &properties,
            &declaration.configuration,
        )
        .is_some()
    }

    fn matching_runtime_name(
        &self,
        element: ElementKind,
        label: &DbString,
        properties: &[DbString],
        configuration: &IndexConfiguration,
    ) -> Option<&Option<DbString>> {
        let property = properties.first()?;
        let key = (label.clone(), property.clone());
        match configuration {
            IndexConfiguration::Property(kinds) if properties.len() == 1 => {
                let entries = match element {
                    ElementKind::Node => &self.property_index,
                    ElementKind::Edge => &self.edge_property_index,
                };
                entries
                    .get(&key)
                    .filter(|entry| kinds.as_slice() == [schema_kind_from(entry.kind())])
                    .map(|entry| &entry.name)
            }
            IndexConfiguration::Property(kinds) if element == ElementKind::Node => self
                .composite_property_index
                .get(&(label.clone(), super::composite_property_key(properties)))
                .filter(|entry| {
                    entry.declared_properties.as_slice() == properties
                        && entry
                            .index
                            .kinds()
                            .iter()
                            .copied()
                            .map(schema_kind_from)
                            .eq(kinds.iter().copied())
                })
                .map(|entry| &entry.name),
            IndexConfiguration::Vector {
                kind,
                dimension,
                hnsw,
                ivf,
            } if element == ElementKind::Node => self
                .vector_index
                .get(&key)
                .filter(|entry| {
                    vector_kind(entry.kind()) == *kind
                        && entry.dimension() == *dimension
                        && entry.hnsw_config() == *hnsw
                        && entry.ivf_config() == *ivf
                })
                .map(|entry| &entry.name),
            IndexConfiguration::Text if element == ElementKind::Node => {
                self.text_index.get(&key).map(|entry| &entry.name)
            }
            _ => None,
        }
    }
}

fn invalid(reason: &'static str) -> CatalogError {
    CatalogError::InvalidDeclaration { reason }
}

fn vector_kind(kind: VectorIndexKind) -> SchemaVectorIndexKind {
    match kind {
        VectorIndexKind::Flat => SchemaVectorIndexKind::Flat,
        VectorIndexKind::HnswSquaredEuclidean => SchemaVectorIndexKind::HnswSquaredEuclidean,
        VectorIndexKind::HnswCosine => SchemaVectorIndexKind::HnswCosine,
        VectorIndexKind::HnswNegativeInnerProduct => {
            SchemaVectorIndexKind::HnswNegativeInnerProduct
        }
        VectorIndexKind::IvfSquaredEuclidean => SchemaVectorIndexKind::IvfSquaredEuclidean,
        VectorIndexKind::IvfCosine => SchemaVectorIndexKind::IvfCosine,
        VectorIndexKind::IvfNegativeInnerProduct => SchemaVectorIndexKind::IvfNegativeInnerProduct,
        VectorIndexKind::TurboQuantCosine => SchemaVectorIndexKind::TurboQuantCosine,
    }
}
