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
    constraints: Vec<BoundIndex>,
}

impl CompiledBindings {
    fn indexes(&self) -> impl Iterator<Item = &BoundIndex> {
        self.single
            .values()
            .chain(self.composite.values())
            .chain(&self.constraints)
    }
}

/// Derived immutable metadata only. Clones share parsed keys and descriptor
/// revisions; neither physical rows nor historical whole catalogs are retained.
#[derive(Clone, Debug)]
pub(crate) struct CatalogBinding(Arc<CompiledBindings>);

impl SeleneGraph {
    /// Validate logical declaration/backing agreement without constructing an
    /// accelerator or granting query eligibility. Full runtime binding/rebuild
    /// remains required before a recovered database can serve requests.
    pub(crate) fn validate_logical_catalog(
        &self,
        catalog: &CatalogSnapshot,
        backing: &[u64],
    ) -> CatalogResult<()> {
        let owner = GraphId::new(self.graph_id().get())?;
        let mut ids = std::collections::BTreeSet::new();
        let mut targets = std::collections::BTreeSet::new();
        for raw in backing {
            let id = selene_catalog::IndexId::new(*raw)?;
            let descriptor = catalog
                .descriptor(CatalogObjectId::Index(id))
                .ok_or_else(|| invalid("missing_index_declaration"))?;
            if descriptor.parent() != selene_catalog::CatalogParent::Graph(owner)
                || !ids.insert(*raw)
            {
                return Err(invalid("wrong_or_duplicate_index_owner"));
            }
            let CatalogPayload::Index(index) = descriptor.payload() else {
                return Err(invalid("wrong_index_kind"));
            };
            let mut properties = index.target.properties.clone();
            properties.sort();
            if index.configuration.family() != IndexFamily::Constraint
                && !targets.insert((
                    index.target.element,
                    index.configuration.family(),
                    index.target.label.clone(),
                    properties,
                ))
            {
                return Err(invalid("ambiguous_index_implementation"));
            }
        }
        let declarations: Vec<_> = catalog
            .declarations(CatalogObjectId::Graph(owner))
            .cloned()
            .collect();
        for descriptor in &declarations {
            if let CatalogPayload::Index(index) = descriptor.payload()
                && index.metadata.state == DeclarationState::Ready
                && !ids.contains(&descriptor.id().get())
            {
                return Err(invalid("missing_index_implementation"));
            }
        }
        self.validate_constraint_bindings(&declarations)
    }

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
        let prior = self.catalog_binding.replace(binding);
        if let Err(error) = self.rebuild_constraints() {
            self.catalog_binding = prior;
            return Err(CatalogError::InvalidDeclaration {
                reason: match error {
                    crate::GraphError::TypeViolation(_) => "constraint_validation_failed",
                    _ => "constraint_backing_failed",
                },
            });
        }
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
        let mut constraints = Vec::new();
        for (position, descriptor) in declarations.iter().enumerate() {
            let CatalogPayload::Index(index) = descriptor.payload() else {
                continue;
            };
            if let IndexConfiguration::Constraint { declaring_type } = &index.configuration {
                if !declarations.iter().any(|d| {
                    matches!(d.payload(), CatalogPayload::Constraint(rule)
                    if rule.backing_index.map(CatalogObjectId::Index) == Some(descriptor.id())
                    && rule.target == index.target && &rule.declaring_type == declaring_type
                    && rule.metadata.state == DeclarationState::Ready)
                }) {
                    return Err(invalid("unowned_constraint_backing"));
                }
                constraints.push(BoundIndex {
                    descriptor: position,
                    source_name: None,
                    properties: SmallVec::new(),
                });
                continue;
            }
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
            constraints,
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
            self.validate_constraint_target(constraint)?;
            if let Some(backing) = constraint.backing_index {
                if !declarations.iter().any(|d| d.id() == CatalogObjectId::Index(backing)
                && matches!(d.payload(), CatalogPayload::Index(index) if index.target == constraint.target
                    && matches!(&index.configuration, IndexConfiguration::Constraint { declaring_type } if declaring_type == &constraint.declaring_type)
                    && index.metadata.state == DeclarationState::Ready)) {
                return Err(invalid("missing_constraint_backing"));
            }
            } else if constraint.kind != selene_catalog::ConstraintKind::Unique
                || constraint.target.properties.len() != 1
            {
                return Err(invalid("missing_constraint_backing"));
            }
            if let Some(position) = rules.iter().position(|rule| {
                rule.target == constraint.target
                    && rule.declaring_type == constraint.declaring_type
                    && rule.kind == constraint.kind
            }) {
                rules.swap_remove(position);
            } else if constraint.kind == selene_catalog::ConstraintKind::Unique {
                return Err(invalid("unsupported_constraint_activation"));
            }
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

    pub(crate) fn catalog_declarations(&self) -> impl Iterator<Item = &CatalogDescriptor> {
        self.catalog_binding
            .iter()
            .flat_map(|binding| &binding.0.declarations)
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
        self.rebuild_constraints()
            .map_err(|_| invalid("constraint_backing_failed"))?;
        if let Some((named, _)) = &source.named_constraints {
            self.admit_named_constraints(None, named.clone(), &[])
                .map_err(|_| invalid("named_constraint_backing_failed"))?;
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
