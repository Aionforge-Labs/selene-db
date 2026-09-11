use super::{
    declaration::*,
    descriptor::{optional_text_decode, optional_text_encode},
};
use crate::*;
use selene_core::logical::{CodecError as E, CodecResult, Decoder, Encoder};

pub(super) fn encode(e: &mut Encoder, v: &NativeDeclaration) -> CodecResult<()> {
    metadata_encode(e, &v.metadata)?;
    match &v.binding {
        NativeBinding::Procedure(p) => {
            e.u8(1)?;
            strings_encode(e, &p.binding)?;
            e.text(&p.description)?;
            e.text(&p.since_version)?;
            e.count_for::<NativeParameter>(p.parameters.len())?;
            for parameter in &p.parameters {
                field_encode(e, &parameter.field)?;
                match &parameter.default {
                    None => e.u8(0)?,
                    Some(NativeDefault::Null) => e.u8(1)?,
                    Some(NativeDefault::Boolean(v)) => {
                        e.u8(2)?;
                        e.boolean(*v)?;
                    }
                    Some(NativeDefault::Integer(v)) => {
                        e.u8(3)?;
                        e.fixed(&v.to_le_bytes())?;
                    }
                    Some(NativeDefault::String(v)) => {
                        e.u8(4)?;
                        e.text(v)?;
                    }
                }
                optional_text_encode(e, parameter.default_doc.as_deref())?;
            }
            e.count_for::<NativeField>(p.outputs.len())?;
            for field in &p.outputs {
                field_encode(e, field)?;
            }
            e.u8(match p.effect {
                NativeEffect::GraphRead => 1,
                NativeEffect::SchemaWrite => 2,
                NativeEffect::MaintenanceWrite => 3,
            })
        }
        NativeBinding::CandidateState(v) => {
            e.u8(2)?;
            optional_text_encode(e, v.required_label.as_deref())?;
            strings_encode(e, &v.require_outgoing)?;
            strings_encode(e, &v.require_incoming)?;
            strings_encode(e, &v.exclude_outgoing)?;
            strings_encode(e, &v.exclude_incoming)
        }
        NativeBinding::Projection(v) => {
            e.u8(3)?;
            strings_encode(e, &v.node_labels)?;
            strings_encode(e, &v.edge_labels)?;
            optional_text_encode(e, v.weight_property.as_deref())
        }
    }
}
pub(super) fn decode(d: &mut Decoder<'_, '_>) -> CodecResult<NativeDeclaration> {
    let metadata = metadata_decode(d)?;
    let binding = match d.u8()? {
        1 => {
            let binding = strings_decode(d)?;
            let description = d.text()?.to_owned();
            let since_version = d.text()?.to_owned();
            let count = d.count_for::<NativeParameter>()?;
            if count > 256 {
                return Err(E::Limit);
            }
            let mut parameters = Vec::with_capacity(count);
            for _ in 0..count {
                let field = field_decode(d)?;
                let default = match d.u8()? {
                    0 => None,
                    1 => Some(NativeDefault::Null),
                    2 => Some(NativeDefault::Boolean(d.boolean()?)),
                    3 => Some(NativeDefault::Integer(d.u64()? as i64)),
                    4 => Some(NativeDefault::String(d.text()?.into())),
                    _ => return Err(E::Invalid("native default")),
                };
                parameters.push(NativeParameter {
                    field,
                    default,
                    default_doc: optional_text_decode(d)?,
                });
            }
            let count = d.count_for::<NativeField>()?;
            if count > 256 {
                return Err(E::Limit);
            }
            let mut outputs = Vec::with_capacity(count);
            for _ in 0..count {
                outputs.push(field_decode(d)?);
            }
            let effect = match d.u8()? {
                1 => NativeEffect::GraphRead,
                2 => NativeEffect::SchemaWrite,
                3 => NativeEffect::MaintenanceWrite,
                _ => return Err(E::Invalid("native effect")),
            };
            NativeBinding::Procedure(NativeProcedure {
                binding,
                description,
                since_version,
                parameters,
                outputs,
                effect,
            })
        }
        2 => NativeBinding::CandidateState(NativeCandidateState {
            required_label: optional_text_decode(d)?,
            require_outgoing: strings_decode(d)?,
            require_incoming: strings_decode(d)?,
            exclude_outgoing: strings_decode(d)?,
            exclude_incoming: strings_decode(d)?,
        }),
        3 => NativeBinding::Projection(NativeProjection {
            node_labels: strings_decode(d)?,
            edge_labels: strings_decode(d)?,
            weight_property: optional_text_decode(d)?,
        }),
        _ => return Err(E::Invalid("native binding kind")),
    };
    Ok(NativeDeclaration { metadata, binding })
}
fn field_encode(e: &mut Encoder, f: &NativeField) -> CodecResult<()> {
    e.budget.metadata(1)?;
    e.text(&f.name)?;
    ty_encode(e, &f.ty, 1)?;
    e.boolean(f.nullable)?;
    e.text(&f.description)
}
fn field_decode(d: &mut Decoder<'_, '_>) -> CodecResult<NativeField> {
    d.budget.metadata(1)?;
    Ok(NativeField {
        name: d.text()?.into(),
        ty: ty_decode(d, 1)?,
        nullable: d.boolean()?,
        description: d.text()?.into(),
    })
}
fn ty_encode(e: &mut Encoder, ty: &NativeType, depth: usize) -> CodecResult<()> {
    e.budget.depth(depth)?;
    e.budget.charge(1, 64)?;
    if depth > 65 {
        return Err(E::Limit);
    }
    e.u8(match ty {
        NativeType::Any => 0,
        NativeType::AnyProperty => 1,
        NativeType::Boolean => 2,
        NativeType::Integer => 3,
        NativeType::Int64 => 4,
        NativeType::Uint64 => 5,
        NativeType::Float => 6,
        NativeType::Float64 => 7,
        NativeType::String => 8,
        NativeType::Vector => 9,
        NativeType::Json => 10,
        NativeType::NodeRef => 11,
        NativeType::EdgeRef => 12,
        NativeType::GraphRef => 13,
        NativeType::OpenRecord => 14,
        NativeType::List(inner) => {
            e.u8(15)?;
            return ty_encode(e, inner, depth + 1);
        }
    })
}
fn ty_decode(d: &mut Decoder<'_, '_>, depth: usize) -> CodecResult<NativeType> {
    d.budget.depth(depth)?;
    d.budget.charge(1, 64)?;
    if depth > 65 {
        return Err(E::Limit);
    }
    Ok(match d.u8()? {
        0 => NativeType::Any,
        1 => NativeType::AnyProperty,
        2 => NativeType::Boolean,
        3 => NativeType::Integer,
        4 => NativeType::Int64,
        5 => NativeType::Uint64,
        6 => NativeType::Float,
        7 => NativeType::Float64,
        8 => NativeType::String,
        9 => NativeType::Vector,
        10 => NativeType::Json,
        11 => NativeType::NodeRef,
        12 => NativeType::EdgeRef,
        13 => NativeType::GraphRef,
        14 => NativeType::OpenRecord,
        15 => NativeType::List(Box::new(ty_decode(d, depth + 1)?)),
        _ => return Err(E::Invalid("native type")),
    })
}
