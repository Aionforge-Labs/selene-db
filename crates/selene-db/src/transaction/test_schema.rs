//! Rich named-definition fixture without widening the property-free facade builder.

use super::*;
use crate::{
    CreatePolicy, Database, GraphTypeDefinition, NodeTypeDefinition, ObjectPath, PathSegment,
    catalog_stage::CatalogStager,
};

pub(crate) fn bind_schema(
    database: &Database,
    graph: &ObjectPath,
    ty: &ObjectPath,
    sources: &[&str],
) {
    database.inner.with_mutation_reservation(|reservation| {
        let base = database.inner.state.load_full();
        let mut draft = DatabaseDraft::new(&base, &reservation);
        let definition = GraphTypeDefinition::builder()
            .with_node_type(
                NodeTypeDefinition::new(
                    PathSegment::regular("Base").unwrap(),
                    vec![PathSegment::regular("Base").unwrap()],
                )
                .unwrap(),
            )
            .build()
            .unwrap();
        CatalogStager::new(&database.inner, &mut draft)
            .create_graph_type(ty, definition, CreatePolicy::Strict)
            .unwrap();
        let id = *draft.graph_types.last_key_value().unwrap().0;
        let mut scratch = SeleneGraph::new(CoreGraphId::new(999));
        scratch.meta.bound_type = Some(draft.graph_types[&id].clone());
        let scratch = SharedGraph::try_from_graph(scratch).unwrap();
        let mut session = selene_gql::Session::new(&scratch);
        for source in sources {
            session
                .execute_source(source, &database.inner.procedures)
                .unwrap();
        }
        let named = scratch.read().meta.bound_type.clone().unwrap();
        draft.graph_types.insert(id, named);
        CatalogStager::new(&database.inner, &mut draft)
            .create_graph(graph, Some(ty), CreatePolicy::OrReplace)
            .unwrap();
        require_committed(
            database
                .inner
                .publish_database_draft(reservation, draft)
                .unwrap(),
        )
        .unwrap();
    });
}
