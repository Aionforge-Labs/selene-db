//! One-way normalization of the canonical built-in specifications into catalog data.

use selene_catalog::{
    CatalogDescriptor, CatalogGeneration, CatalogId, CatalogName, CatalogParent, CreationMetadata,
    DeclarationMetadata, DeclarationState, NativeBinding, NativeDeclaration, NativeDefault,
    NativeEffect, NativeField, NativeParameter, NativeProcedure, NativeType, ProcedureId,
};
use selene_core::DbString;

use crate::{
    GqlType, ProcedureDefaultValue, ProcedureMetadata, ProcedureMutability, ProcedureTier,
};

pub(super) fn descriptor(
    id: u64,
    name: &[DbString],
    metadata: &ProcedureMetadata,
) -> CatalogDescriptor {
    let generation = CatalogGeneration::new(1).expect("static generation");
    CatalogDescriptor::procedure(
        ProcedureId::new(id).expect("static nonzero ID"),
        CatalogName::delimited(
            name.iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join("."),
        )
        .expect("static name"),
        CatalogParent::Catalog(CatalogId::new(1).expect("initial catalog")),
        generation,
        CreationMetadata::new(generation, None),
        NativeDeclaration {
            metadata: DeclarationMetadata::new(DeclarationState::Ready),
            binding: NativeBinding::Procedure(NativeProcedure {
                binding: name.iter().map(ToString::to_string).collect(),
                description: metadata.description.to_owned(),
                since_version: metadata.signature.since_version.to_owned(),
                parameters: metadata
                    .signature
                    .parameters
                    .iter()
                    .map(|parameter| NativeParameter {
                        field: NativeField {
                            name: parameter.name.to_string(),
                            ty: native_type(&parameter.ty),
                            nullable: parameter.nullable,
                            description: parameter.description.to_owned(),
                        },
                        default: parameter.default.map(|default| match default {
                            ProcedureDefaultValue::Null => NativeDefault::Null,
                            ProcedureDefaultValue::Boolean(value) => NativeDefault::Boolean(value),
                            ProcedureDefaultValue::Integer(value) => NativeDefault::Integer(value),
                            ProcedureDefaultValue::String(value) => {
                                NativeDefault::String(value.to_owned())
                            }
                        }),
                        default_doc: parameter.default_doc.map(ToOwned::to_owned),
                    })
                    .collect(),
                outputs: metadata
                    .output_schema
                    .columns
                    .iter()
                    .map(|column| NativeField {
                        name: column.name.to_string(),
                        ty: native_type(&column.ty),
                        nullable: column.nullable,
                        description: column.description.to_owned(),
                    })
                    .collect(),
                effect: match (metadata.tier, metadata.mutability) {
                    (ProcedureTier::Graph, ProcedureMutability::Read) => NativeEffect::GraphRead,
                    (ProcedureTier::Mutation, ProcedureMutability::SchemaWrite) => {
                        NativeEffect::SchemaWrite
                    }
                    (ProcedureTier::Maintenance, ProcedureMutability::MaintenanceWrite) => {
                        NativeEffect::MaintenanceWrite
                    }
                    _ => panic!("static built-in tier/effect mismatch"),
                },
            }),
        },
    )
    .unwrap_or_else(|error| panic!("canonical native inventory {name:?}: {error}"))
}

fn native_type(ty: &GqlType) -> NativeType {
    let native = match ty {
        GqlType::Any => NativeType::Any,
        GqlType::AnyProperty => NativeType::AnyProperty,
        GqlType::Boolean => NativeType::Boolean,
        GqlType::Integer => NativeType::Integer,
        GqlType::Int64 => NativeType::Int64,
        GqlType::Uint64 => NativeType::Uint64,
        GqlType::Float => NativeType::Float,
        GqlType::Float64 => NativeType::Float64,
        GqlType::String => NativeType::String,
        GqlType::Vector => NativeType::Vector,
        GqlType::Json => NativeType::Json,
        GqlType::NodeRef => NativeType::NodeRef,
        GqlType::EdgeRef => NativeType::EdgeRef,
        GqlType::GraphRef => NativeType::GraphRef,
        GqlType::Record(crate::RecordType::Open) => NativeType::OpenRecord,
        GqlType::List(element) => NativeType::List(Box::new(native_type(element))),
        _ => panic!("native inventory requires a storage-neutral representation for {ty:?}"),
    };
    assert_eq!(
        native.structural_type().expect("valid native descriptor"),
        crate::normalize_value_type(ty).expect("supported native source type"),
        "native signature and runtime structural meaning must agree"
    );
    native
}
