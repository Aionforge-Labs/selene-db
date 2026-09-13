use super::{
    descriptor::{id_decode, id_encode},
    generation,
};
use crate::*;
use selene_core::logical::{CodecError as E, CodecResult, Decoder, Encoder};

#[path = "expression.rs"]
mod expression;

pub(super) fn metadata_encode(e: &mut Encoder, m: &DeclarationMetadata) -> CodecResult<()> {
    e.budget.metadata(1)?;
    e.u8(match m.state {
        DeclarationState::Inactive => 0,
        DeclarationState::Building => 1,
        DeclarationState::Ready => 2,
        DeclarationState::Failed => 3,
    })?;
    e.text(&m.profile.id)?;
    e.text(&m.profile.hash)?;
    e.u32(m.profile.semantics)?;
    e.count(m.dependencies.len())?;
    // Dependency order is canonical typed-ID order, not source Vec order.
    let mut deps: Vec<_> = m.dependencies.iter().collect();
    deps.sort_by_key(|dep| dep.id);
    for dep in deps {
        id_encode(e, dep.id)?;
        e.u64(dep.generation.get())?;
    }
    Ok(())
}
pub(super) fn metadata_decode(d: &mut Decoder<'_, '_>) -> CodecResult<DeclarationMetadata> {
    d.budget.metadata(1)?;
    let state = match d.u8()? {
        0 => DeclarationState::Inactive,
        1 => DeclarationState::Building,
        2 => DeclarationState::Ready,
        3 => DeclarationState::Failed,
        _ => return Err(E::Invalid("declaration state")),
    };
    let profile = DeclarationProfile {
        id: d.text()?.into(),
        hash: d.text()?.into(),
        semantics: d.u32()?,
    };
    let count = d.count()?;
    if count > 256 {
        return Err(E::Limit);
    }
    let mut dependencies: Vec<DeclarationDependency> = Vec::with_capacity(count);
    for _ in 0..count {
        let id = id_decode(d)?;
        if dependencies.last().is_some_and(|dep| dep.id >= id) {
            return Err(E::Invalid("dependency order"));
        }
        dependencies.push(DeclarationDependency {
            id,
            generation: generation(d)?,
        });
    }
    Ok(DeclarationMetadata {
        state,
        profile,
        dependencies,
    })
}
pub(super) fn strings_encode(e: &mut Encoder, values: &[String]) -> CodecResult<()> {
    e.count(values.len())?;
    for value in values {
        e.text(value)?;
    }
    Ok(())
}
pub(super) fn strings_decode(d: &mut Decoder<'_, '_>) -> CodecResult<Vec<String>> {
    let count = d.count()?;
    let mut values = Vec::with_capacity(count);
    for _ in 0..count {
        values.push(d.text()?.to_owned());
    }
    Ok(values)
}
fn target_encode(e: &mut Encoder, target: &PropertyTarget) -> CodecResult<()> {
    e.u8(match target.element {
        ElementKind::Node => 1,
        ElementKind::Edge => 2,
    })?;
    e.text(&target.label)?;
    strings_encode(e, &target.properties)
}
fn target_decode(d: &mut Decoder<'_, '_>) -> CodecResult<PropertyTarget> {
    let element = match d.u8()? {
        1 => ElementKind::Node,
        2 => ElementKind::Edge,
        _ => return Err(E::Invalid("element kind")),
    };
    Ok(PropertyTarget {
        element,
        label: d.text()?.into(),
        properties: strings_decode(d)?,
    })
}
pub(super) fn index_encode(e: &mut Encoder, v: &IndexDeclaration) -> CodecResult<()> {
    metadata_encode(e, &v.metadata)?;
    target_encode(e, &v.target)?;
    match &v.configuration {
        IndexConfiguration::Property(kinds) => {
            e.u8(1)?;
            e.count(kinds.len())?;
            for kind in kinds {
                e.index_kind(*kind)?;
            }
            Ok(())
        }
        IndexConfiguration::Vector {
            kind,
            dimension,
            hnsw,
            ivf,
        } => {
            e.u8(2)?;
            e.vector_kind(*kind)?;
            e.u32(*dimension)?;
            e.boolean(hnsw.is_some())?;
            if let Some(v) = hnsw {
                e.u32(u32::from(v.max_neighbors))?;
                e.u32(u32::from(v.ef_construction))?;
            }
            e.boolean(ivf.is_some())?;
            if let Some(v) = ivf {
                e.u32(u32::from(v.target_centroids))?;
            }
            Ok(())
        }
        IndexConfiguration::Text => e.u8(3),
        IndexConfiguration::Constraint { declaring_type } => {
            e.u8(4)?;
            e.text(declaring_type)
        }
        IndexConfiguration::Expression {
            expression: target,
            kind,
        } => {
            e.u8(5)?;
            expression::encode(e, target)?;
            e.index_kind(*kind)
        }
    }
}
pub(super) fn index_decode(d: &mut Decoder<'_, '_>) -> CodecResult<IndexDeclaration> {
    let metadata = metadata_decode(d)?;
    let target = target_decode(d)?;
    let configuration = match d.u8()? {
        1 => {
            let count = d.count()?;
            let mut kinds = Vec::with_capacity(count);
            for _ in 0..count {
                kinds.push(d.index_kind()?);
            }
            IndexConfiguration::Property(kinds)
        }
        2 => IndexConfiguration::Vector {
            kind: d.vector_kind()?,
            dimension: d.u32()?,
            hnsw: if d.boolean()? {
                Some(selene_core::HnswIndexConfig {
                    max_neighbors: d.u32()?.try_into().map_err(|_| E::Limit)?,
                    ef_construction: d.u32()?.try_into().map_err(|_| E::Limit)?,
                })
            } else {
                None
            },
            ivf: if d.boolean()? {
                Some(selene_core::IvfIndexConfig {
                    target_centroids: d.u32()?.try_into().map_err(|_| E::Limit)?,
                })
            } else {
                None
            },
        },
        3 => IndexConfiguration::Text,
        4 => IndexConfiguration::Constraint {
            declaring_type: d.text()?.into(),
        },
        5 => IndexConfiguration::Expression {
            expression: expression::decode(d)?,
            kind: d.index_kind()?,
        },
        _ => return Err(E::Invalid("index configuration")),
    };
    Ok(IndexDeclaration {
        metadata,
        target,
        configuration,
    })
}
pub(super) fn constraint_encode(e: &mut Encoder, v: &ConstraintDeclaration) -> CodecResult<()> {
    metadata_encode(e, &v.metadata)?;
    target_encode(e, &v.target)?;
    e.text(&v.declaring_type)?;
    e.u8(match v.kind {
        ConstraintKind::Unique => 1,
        ConstraintKind::CompositeUnique => 2,
        ConstraintKind::Key => 3,
    })?;
    e.boolean(v.backing_index.is_some())?;
    if let Some(id) = v.backing_index {
        e.u64(id.get())?;
    }
    Ok(())
}
pub(super) fn constraint_decode(d: &mut Decoder<'_, '_>) -> CodecResult<ConstraintDeclaration> {
    Ok(ConstraintDeclaration {
        metadata: metadata_decode(d)?,
        target: target_decode(d)?,
        declaring_type: d.text()?.into(),
        kind: match d.u8()? {
            1 => ConstraintKind::Unique,
            2 => ConstraintKind::CompositeUnique,
            3 => ConstraintKind::Key,
            _ => return Err(E::Invalid("constraint kind")),
        },
        backing_index: if d.boolean()? {
            Some(IndexId::new(d.u64()?).map_err(|_| E::Semantic)?)
        } else {
            None
        },
    })
}
