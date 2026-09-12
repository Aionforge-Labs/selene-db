//! Explicit declaration backing for the parse corpus's mock-only selene.labels.

use selene_catalog::*;
use selene_core::{DbString, Value};
use selene_gql::{
    ProcedureContext, ProcedureError, ProcedureHandle, ProcedureMetadata, ProcedureRegistry,
    ProcedureResult, analyze::catalog::CatalogEnvironment,
};
use std::sync::Arc;

struct CorpusRegistry {
    mock: selene_testing::MockProcedureRegistry,
    declaration: Arc<CatalogDescriptor>,
}

impl ProcedureRegistry for CorpusRegistry {
    fn lookup(&self, name: &[DbString]) -> Option<ProcedureMetadata> {
        self.mock.lookup(name).map(|mut metadata| {
            metadata.declaration = Some(Arc::clone(&self.declaration));
            metadata
        })
    }
    fn registry_version(&self) -> u64 {
        self.mock.registry_version()
    }
    fn execute(
        &self,
        handle: ProcedureHandle,
        args: &[Value],
        ctx: &mut ProcedureContext<'_, '_>,
    ) -> Result<ProcedureResult, ProcedureError> {
        self.mock.execute(handle, args, ctx)
    }
}

pub(super) fn fixture() -> (impl ProcedureRegistry, CatalogEnvironment) {
    let generation = CatalogGeneration::new(1).unwrap();
    let creation = CreationMetadata::new(generation, None);
    let catalog_id = CatalogId::new(1).unwrap();
    let directory = DirectoryId::new(1).unwrap();
    let schema = SchemaId::new(1).unwrap();
    let graph = GraphId::new(1).unwrap();
    let catalog = CatalogDescriptor::catalog(
        catalog_id,
        CatalogName::regular("selene").unwrap(),
        generation,
        creation.clone(),
    )
    .unwrap();
    let root =
        CatalogDescriptor::root_directory(directory, catalog_id, generation, creation.clone())
            .unwrap();
    let mut builder = CatalogSnapshotBuilder::new(generation, catalog, root).unwrap();
    builder
        .insert(
            CatalogDescriptor::schema(
                schema,
                CatalogName::regular("memory").unwrap(),
                directory,
                generation,
                creation.clone(),
            )
            .unwrap(),
        )
        .unwrap();
    builder
        .insert(
            CatalogDescriptor::graph(
                graph,
                CatalogName::regular("main").unwrap(),
                schema,
                generation,
                creation.clone(),
                None,
            )
            .unwrap(),
        )
        .unwrap();
    let declaration = Arc::new(
        CatalogDescriptor::procedure(
            ProcedureId::new(1).unwrap(),
            CatalogName::delimited("selene.labels").unwrap(),
            CatalogParent::Catalog(catalog_id),
            generation,
            creation,
            NativeDeclaration {
                metadata: DeclarationMetadata::new(DeclarationState::Ready),
                binding: NativeBinding::Procedure(NativeProcedure {
                    binding: vec!["selene".into(), "labels".into()],
                    description: String::new(),
                    since_version: "1.0.0".into(),
                    parameters: vec![],
                    outputs: vec![NativeField {
                        name: "label".into(),
                        ty: NativeType::String,
                        nullable: false,
                        description: String::new(),
                    }],
                    effect: NativeEffect::GraphRead,
                }),
            },
        )
        .unwrap(),
    );
    builder.insert(declaration.as_ref().clone()).unwrap();
    (
        CorpusRegistry {
            mock: selene_testing::default_corpus_registry(),
            declaration,
        },
        CatalogEnvironment::new(builder.build().unwrap(), schema, graph, None),
    )
}
