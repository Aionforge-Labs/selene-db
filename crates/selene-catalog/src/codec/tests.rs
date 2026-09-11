use super::*;
use crate::*;
use selene_core::logical::{Budget, Limits};

fn metadata() -> DeclarationMetadata {
    DeclarationMetadata::new(DeclarationState::Inactive)
}
fn descriptor(payload: CatalogPayload) -> CatalogDescriptor {
    let generation = CatalogGeneration::new(2).unwrap();
    let (id, parent) = match payload.kind() {
        Kind::Catalog => (Id::Catalog(CatalogId::new(1).unwrap()), CatalogParent::None),
        Kind::Directory => (
            Id::Directory(DirectoryId::new(1).unwrap()),
            CatalogParent::Catalog(CatalogId::new(1).unwrap()),
        ),
        Kind::Schema => (
            Id::Schema(SchemaId::new(1).unwrap()),
            CatalogParent::Directory(DirectoryId::new(1).unwrap()),
        ),
        Kind::Graph => (
            Id::Graph(GraphId::new(1).unwrap()),
            CatalogParent::Schema(SchemaId::new(1).unwrap()),
        ),
        Kind::GraphType => (
            Id::GraphType(GraphTypeId::new(1).unwrap()),
            CatalogParent::Schema(SchemaId::new(1).unwrap()),
        ),
        Kind::BindingTable => (
            Id::BindingTable(BindingTableId::new(1).unwrap()),
            CatalogParent::Schema(SchemaId::new(1).unwrap()),
        ),
        Kind::Procedure => (
            Id::Procedure(ProcedureId::new(1).unwrap()),
            CatalogParent::Graph(GraphId::new(1).unwrap()),
        ),
        Kind::Index => (
            Id::Index(IndexId::new(1).unwrap()),
            CatalogParent::Graph(GraphId::new(1).unwrap()),
        ),
        Kind::Constraint => (
            Id::Constraint(ConstraintId::new(1).unwrap()),
            CatalogParent::Graph(GraphId::new(1).unwrap()),
        ),
    };
    let name = if payload.kind() == Kind::Directory {
        CatalogName::synthetic_root()
    } else {
        CatalogName::delimited("display name").unwrap()
    };
    CatalogDescriptor::new(
        id,
        payload.kind(),
        name,
        parent,
        generation,
        CreationMetadata::new(generation, Some("actor".into())),
        payload,
    )
    .unwrap()
}
fn check(payload: CatalogPayload) {
    let descriptor = descriptor(payload);
    let mut e = Encoder::new(Limits::default()).unwrap();
    super::descriptor::encode(&mut e, &descriptor).unwrap();
    let bytes = e.finish();
    let mut budget = Budget::new(Limits::default()).unwrap();
    let mut d = Decoder::new(&bytes, &mut budget).unwrap();
    assert_eq!(super::descriptor::decode(&mut d).unwrap(), descriptor);
    d.finish().unwrap();
    for cut in 0..bytes.len() {
        let mut budget = Budget::new(Limits::default()).unwrap();
        let mut d = Decoder::new(&bytes[..cut], &mut budget).unwrap();
        assert!(super::descriptor::decode(&mut d).is_err());
    }
}
fn target() -> PropertyTarget {
    PropertyTarget {
        element: ElementKind::Node,
        label: "L".into(),
        properties: vec!["v".into()],
    }
}

#[test]
fn all_catalog_payload_and_native_binding_families() {
    for payload in [
        CatalogPayload::Catalog,
        CatalogPayload::RootDirectory,
        CatalogPayload::Schema,
        CatalogPayload::Graph {
            graph_type: Some(GraphTypeId::new(2).unwrap()),
        },
        CatalogPayload::GraphType,
        CatalogPayload::BindingTable,
    ] {
        check(payload);
    }
    let types = [
        NativeType::Any,
        NativeType::AnyProperty,
        NativeType::Boolean,
        NativeType::Integer,
        NativeType::Int64,
        NativeType::Uint64,
        NativeType::Float,
        NativeType::Float64,
        NativeType::String,
        NativeType::Vector,
        NativeType::Json,
        NativeType::NodeRef,
        NativeType::EdgeRef,
        NativeType::GraphRef,
        NativeType::OpenRecord,
        NativeType::List(Box::new(NativeType::Integer)),
    ];
    let outputs = types
        .into_iter()
        .enumerate()
        .map(|(i, ty)| NativeField {
            name: format!("f{i}"),
            ty,
            nullable: true,
            description: "output".into(),
        })
        .collect();
    let parameters = [
        NativeDefault::Null,
        NativeDefault::Boolean(true),
        NativeDefault::Integer(-9),
        NativeDefault::String("text".into()),
    ]
    .into_iter()
    .enumerate()
    .map(|(i, default)| NativeParameter {
        field: NativeField {
            name: format!("p{i}"),
            ty: NativeType::Any,
            nullable: true,
            description: "parameter".into(),
        },
        default: Some(default),
        default_doc: Some("doc".into()),
    })
    .collect();
    check(CatalogPayload::Procedure(NativeDeclaration {
        metadata: metadata(),
        binding: NativeBinding::Procedure(NativeProcedure {
            binding: vec!["selene".into(), "fixture".into()],
            description: "metadata only".into(),
            since_version: "2".into(),
            parameters,
            outputs,
            effect: NativeEffect::GraphRead,
        }),
    }));
    check(CatalogPayload::Procedure(NativeDeclaration {
        metadata: metadata(),
        binding: NativeBinding::CandidateState(NativeCandidateState {
            required_label: Some("N".into()),
            require_outgoing: vec!["out".into()],
            require_incoming: vec!["in".into()],
            exclude_outgoing: vec!["exclude_out".into()],
            exclude_incoming: vec!["exclude_in".into()],
        }),
    }));
    check(CatalogPayload::Procedure(NativeDeclaration {
        metadata: metadata(),
        binding: NativeBinding::Projection(NativeProjection {
            node_labels: vec!["L".into()],
            edge_labels: vec!["E".into()],
            weight_property: Some("weight".into()),
        }),
    }));
    check(CatalogPayload::Index(IndexDeclaration {
        metadata: metadata(),
        target: target(),
        configuration: IndexConfiguration::Text,
    }));
    for kind in [
        ConstraintKind::Unique,
        ConstraintKind::CompositeUnique,
        ConstraintKind::Key,
    ] {
        check(CatalogPayload::Constraint(ConstraintDeclaration {
            metadata: metadata(),
            target: target(),
            declaring_type: "NamedType".into(),
            kind,
            backing_index: None,
        }));
    }
}

#[test]
fn scalar_and_vector_index_configuration_inventory() {
    use selene_core::{SchemaPropertyIndexKind as P, SchemaVectorIndexKind as V};
    for kind in [
        P::Bool,
        P::I64,
        P::U64,
        P::I128,
        P::U128,
        P::Decimal,
        P::F32,
        P::F64,
        P::String,
        P::Date,
        P::LocalDateTime,
        P::ZonedDateTime,
        P::LocalTime,
        P::ZonedTime,
        P::Duration,
        P::Uuid,
    ] {
        check(CatalogPayload::Index(IndexDeclaration {
            metadata: metadata(),
            target: target(),
            configuration: IndexConfiguration::Property(vec![kind]),
        }));
    }
    for kind in [
        V::Flat,
        V::HnswSquaredEuclidean,
        V::HnswCosine,
        V::HnswNegativeInnerProduct,
        V::IvfSquaredEuclidean,
        V::IvfCosine,
        V::IvfNegativeInnerProduct,
        V::TurboQuantCosine,
    ] {
        let hnsw = matches!(
            kind,
            V::HnswSquaredEuclidean | V::HnswCosine | V::HnswNegativeInnerProduct
        )
        .then(selene_core::HnswIndexConfig::default);
        let ivf = matches!(
            kind,
            V::IvfSquaredEuclidean | V::IvfCosine | V::IvfNegativeInnerProduct
        )
        .then_some(selene_core::IvfIndexConfig {
            target_centroids: 16,
        });
        check(CatalogPayload::Index(IndexDeclaration {
            metadata: metadata(),
            target: target(),
            configuration: IndexConfiguration::Vector {
                kind,
                dimension: 32,
                hnsw,
                ivf,
            },
        }));
    }
    let mut target = target();
    target.properties.push("second".into());
    check(CatalogPayload::Index(IndexDeclaration {
        metadata: metadata(),
        target,
        configuration: IndexConfiguration::Property(vec![P::I64, P::String]),
    }));
}

#[test]
fn independent_descriptor_bytes_and_unknown_tags() {
    // Catalog ID kind 1/value 1; regular "c"; no parent; revision/creation 1;
    // no principal; catalog marker 1. Independently assembled field sequence.
    let mut bytes = vec![1];
    bytes.extend(1u64.to_le_bytes());
    bytes.extend([1, 1, 0, 0, 0, b'c', 0]);
    bytes.extend(1u64.to_le_bytes());
    bytes.extend(1u64.to_le_bytes());
    bytes.extend([0, 1]);
    let mut budget = Budget::new(Limits::default()).unwrap();
    let mut d = Decoder::new(&bytes, &mut budget).unwrap();
    let descriptor = super::descriptor::decode(&mut d).unwrap();
    d.finish().unwrap();
    assert_eq!(descriptor.name().display(), "c");
    let mut e = Encoder::new(Limits::default()).unwrap();
    super::descriptor::encode(&mut e, &descriptor).unwrap();
    assert_eq!(e.finish(), bytes);
    bytes[0] = 255;
    let mut budget = Budget::new(Limits::default()).unwrap();
    let mut d = Decoder::new(&bytes, &mut budget).unwrap();
    assert_eq!(
        super::descriptor::decode(&mut d).unwrap_err(),
        E::Invalid("catalog identity kind")
    );
}
