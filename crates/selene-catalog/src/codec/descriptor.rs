use super::{declaration, generation, native};
use crate::*;
use selene_core::logical::{CodecError as E, CodecResult, Decoder, Encoder};

pub(super) fn id_encode(e: &mut Encoder, id: CatalogObjectId) -> CodecResult<()> {
    let tag = match id.kind() {
        CatalogObjectKind::Catalog => 1,
        CatalogObjectKind::Directory => 2,
        CatalogObjectKind::Schema => 3,
        CatalogObjectKind::Graph => 4,
        CatalogObjectKind::GraphType => 5,
        CatalogObjectKind::BindingTable => 6,
        CatalogObjectKind::Procedure => 7,
        CatalogObjectKind::Index => 8,
        CatalogObjectKind::Constraint => 9,
    };
    e.u8(tag)?;
    e.u64(id.get())
}
pub(super) fn id_decode(d: &mut Decoder<'_, '_>) -> CodecResult<CatalogObjectId> {
    let tag = d.u8()?;
    let raw = d.u64()?;
    Ok(match tag {
        1 => CatalogObjectId::Catalog(CatalogId::new(raw).map_err(|_| E::Semantic)?),
        2 => CatalogObjectId::Directory(DirectoryId::new(raw).map_err(|_| E::Semantic)?),
        3 => CatalogObjectId::Schema(SchemaId::new(raw).map_err(|_| E::Semantic)?),
        4 => CatalogObjectId::Graph(GraphId::new(raw).map_err(|_| E::Semantic)?),
        5 => CatalogObjectId::GraphType(GraphTypeId::new(raw).map_err(|_| E::Semantic)?),
        6 => CatalogObjectId::BindingTable(BindingTableId::new(raw).map_err(|_| E::Semantic)?),
        7 => CatalogObjectId::Procedure(ProcedureId::new(raw).map_err(|_| E::Semantic)?),
        8 => CatalogObjectId::Index(IndexId::new(raw).map_err(|_| E::Semantic)?),
        9 => CatalogObjectId::Constraint(ConstraintId::new(raw).map_err(|_| E::Semantic)?),
        _ => return Err(E::Invalid("catalog identity kind")),
    })
}
pub(super) fn encode(e: &mut Encoder, d: &CatalogDescriptor) -> CodecResult<()> {
    d.validate().map_err(|_| E::Semantic)?;
    id_encode(e, d.id())?;
    e.u8(match d.name().form() {
        None => 0,
        Some(IdentifierForm::Regular) => 1,
        Some(IdentifierForm::Delimited) => 2,
    })?;
    e.text(d.name().display())?;
    match d.parent() {
        CatalogParent::None => e.u8(0)?,
        CatalogParent::Catalog(id) => id_encode(e, CatalogObjectId::Catalog(id))?,
        CatalogParent::Directory(id) => id_encode(e, CatalogObjectId::Directory(id))?,
        CatalogParent::Schema(id) => id_encode(e, CatalogObjectId::Schema(id))?,
        CatalogParent::Graph(id) => id_encode(e, CatalogObjectId::Graph(id))?,
        CatalogParent::GraphType(id) => id_encode(e, CatalogObjectId::GraphType(id))?,
    }
    e.u64(d.generation().get())?;
    e.u64(d.creation().generation().get())?;
    optional_text_encode(e, d.creation().principal())?;
    match d.payload() {
        CatalogPayload::Catalog => e.u8(1),
        CatalogPayload::RootDirectory => e.u8(2),
        CatalogPayload::Schema => e.u8(3),
        CatalogPayload::Graph { graph_type } => {
            e.u8(4)?;
            e.boolean(graph_type.is_some())?;
            if let Some(id) = graph_type {
                e.u64(id.get())?;
            }
            Ok(())
        }
        CatalogPayload::GraphType => e.u8(5),
        CatalogPayload::BindingTable => e.u8(6),
        CatalogPayload::Procedure(v) => {
            e.u8(7)?;
            native::encode(e, v)
        }
        CatalogPayload::Index(v) => {
            e.u8(8)?;
            declaration::index_encode(e, v)
        }
        CatalogPayload::Constraint(v) => {
            e.u8(9)?;
            declaration::constraint_encode(e, v)
        }
    }
}
pub(super) fn decode(d: &mut Decoder<'_, '_>) -> CodecResult<CatalogDescriptor> {
    let id = id_decode(d)?;
    let form = d.u8()?;
    let text = d.text()?;
    let name = match form {
        0 if text.is_empty() => CatalogName::synthetic_root(),
        1 => CatalogName::regular(text).map_err(|_| E::Semantic)?,
        2 => CatalogName::delimited(text).map_err(|_| E::Semantic)?,
        _ => return Err(E::Invalid("catalog name form")),
    };
    // Parent zero is a one-byte sentinel; all other parent tags carry a u64.
    let parent = match d.u8()? {
        0 => CatalogParent::None,
        1 => CatalogParent::Catalog(CatalogId::new(d.u64()?).map_err(|_| E::Semantic)?),
        2 => CatalogParent::Directory(DirectoryId::new(d.u64()?).map_err(|_| E::Semantic)?),
        3 => CatalogParent::Schema(SchemaId::new(d.u64()?).map_err(|_| E::Semantic)?),
        4 => CatalogParent::Graph(GraphId::new(d.u64()?).map_err(|_| E::Semantic)?),
        5 => CatalogParent::GraphType(GraphTypeId::new(d.u64()?).map_err(|_| E::Semantic)?),
        _ => return Err(E::Invalid("catalog parent kind")),
    };
    let revision = generation(d)?;
    let creation = CreationMetadata::new(generation(d)?, optional_text_decode(d)?);
    let payload = match d.u8()? {
        1 => CatalogPayload::Catalog,
        2 => CatalogPayload::RootDirectory,
        3 => CatalogPayload::Schema,
        4 => CatalogPayload::Graph {
            graph_type: if d.boolean()? {
                Some(GraphTypeId::new(d.u64()?).map_err(|_| E::Semantic)?)
            } else {
                None
            },
        },
        5 => CatalogPayload::GraphType,
        6 => CatalogPayload::BindingTable,
        7 => CatalogPayload::Procedure(native::decode(d)?),
        8 => CatalogPayload::Index(declaration::index_decode(d)?),
        9 => CatalogPayload::Constraint(declaration::constraint_decode(d)?),
        _ => return Err(E::Unsupported("catalog payload tag")),
    };
    CatalogDescriptor::new(id, id.kind(), name, parent, revision, creation, payload)
        .map_err(|_| E::Semantic)
}
pub(super) fn optional_text_encode(e: &mut Encoder, value: Option<&str>) -> CodecResult<()> {
    e.boolean(value.is_some())?;
    if let Some(value) = value {
        e.text(value)?;
    }
    Ok(())
}
pub(super) fn optional_text_decode(d: &mut Decoder<'_, '_>) -> CodecResult<Option<String>> {
    Ok(if d.boolean()? {
        Some(d.text()?.to_owned())
    } else {
        None
    })
}
