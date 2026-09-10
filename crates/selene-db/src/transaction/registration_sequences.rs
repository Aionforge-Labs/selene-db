//! Trusted prepared-list transitions across every existing lower index family.
//! Edge/composite construction here uses the existing advanced mutation funnel;
//! it does not add an activation route to the public facade.

use super::{AuthorityOutcome, DatabaseDraft, fixture, ids};
use crate::*;
use selene_core::db_string;
use selene_graph::{Mutator, SharedGraph, TypedIndexKind};

fn create(m: &mut Mutator<'_, '_>) {
    m.create_property_index(
        db_string("Doc").unwrap(),
        db_string("p").unwrap(),
        TypedIndexKind::I64,
    )
    .unwrap();
    m.create_edge_property_index(
        db_string("Link").unwrap(),
        db_string("p").unwrap(),
        TypedIndexKind::I64,
    )
    .unwrap();
    m.create_composite_property_index_named(
        db_string("Doc").unwrap(),
        [db_string("z").unwrap(), db_string("a").unwrap()]
            .into_iter()
            .collect(),
        [TypedIndexKind::String, TypedIndexKind::I64]
            .into_iter()
            .collect(),
        None,
    )
    .unwrap();
    m.create_vector_index_named(
        db_string("Doc").unwrap(),
        db_string("v").unwrap(),
        selene_graph::VectorIndexKind::Flat,
        2,
        None,
    )
    .unwrap();
    m.create_text_index_named(db_string("Doc").unwrap(), db_string("body").unwrap(), None)
        .unwrap();
}

fn drop_indexes(m: &mut Mutator<'_, '_>) {
    m.drop_property_index(db_string("Doc").unwrap(), db_string("p").unwrap())
        .unwrap();
    m.drop_edge_property_index(db_string("Link").unwrap(), db_string("p").unwrap())
        .unwrap();
    // A canonical lookup order different from the declaration's (z, a).
    m.drop_composite_property_index(
        db_string("Doc").unwrap(),
        [db_string("a").unwrap(), db_string("z").unwrap()]
            .into_iter()
            .collect(),
    )
    .unwrap();
    m.drop_vector_index(db_string("Doc").unwrap(), db_string("v").unwrap())
        .unwrap();
    m.drop_text_index(db_string("Doc").unwrap(), db_string("body").unwrap())
        .unwrap();
}

fn publish(database: &Database, path: &ObjectPath, edit: impl FnOnce(&mut Mutator<'_, '_>)) {
    let (_, id) = ids(database, path);
    let catalog = database.catalog();
    let inner = &catalog.inner;
    inner.with_mutation_reservation(|reservation| {
        let base = inner.state.load_full();
        let mut draft = DatabaseDraft::new(&base, &reservation);
        draft.pin_graph(&base, id).unwrap();
        let scratch = SharedGraph::try_from_graph(draft.selected_graph().unwrap().clone()).unwrap();
        let mut transaction = scratch.begin_write();
        edit(&mut transaction.mutator());
        let prepared = transaction.prepare_unpublished(None, None).unwrap();
        draft.attach_prepared_graph(id, prepared).unwrap();
        assert_eq!(
            inner.publish_database_draft(reservation, draft).unwrap(),
            AuthorityOutcome::Committed
        );
    });
}

fn alternatives(database: &Database, path: &ObjectPath) -> Vec<DeclarationDescriptor> {
    let ready = database.catalog().snapshot().declarations(path).unwrap();
    let mut future = Vec::new();
    for (position, descriptor) in ready.into_iter().enumerate() {
        let DeclarationDefinition::Index(mut index) = descriptor.definition else {
            panic!("index")
        };
        index.metadata.state = match position % 3 {
            0 => DeclarationState::Inactive,
            1 => DeclarationState::Building,
            _ => DeclarationState::Failed,
        };
        let name = PathSegment::regular(format!("future_{position}")).unwrap();
        let CreateOutcome::Created(created) = database
            .catalog()
            .declare(
                path,
                &name,
                DeclarationDefinition::Index(index),
                CreatePolicy::Strict,
            )
            .unwrap()
        else {
            panic!("created")
        };
        future.push(created);
    }
    future
}

#[test]
fn ordered_create_drop_and_recreate_bind_fresh_ids_in_one_prepared_list() {
    let (database, _, path) = fixture();
    publish(&database, &path, |m| {
        create(m);
        drop_indexes(m);
    });
    assert!(
        database
            .catalog()
            .snapshot()
            .declarations(&path)
            .unwrap()
            .is_empty()
    );
    assert_eq!(database.catalog().snapshot().state.high_water.index, 5);
    publish(&database, &path, |m| {
        create(m);
        drop_indexes(m);
        create(m);
    });
    let snapshot = database.catalog().snapshot();
    assert_eq!(snapshot.declarations(&path).unwrap().len(), 5);
    assert_eq!(snapshot.state.high_water.index, 15);
    assert!(
        snapshot
            .declarations(&path)
            .unwrap()
            .iter()
            .all(|descriptor| matches!(descriptor.id, DeclarationId::Index(id) if id > 10))
    );
}

#[test]
fn all_family_drops_preserve_unbound_alternatives_and_canonical_names() {
    let (database, _, path) = fixture();
    publish(&database, &path, create);
    let snapshot = database.catalog().snapshot();
    let declared: std::collections::BTreeSet<_> = snapshot
        .declarations(&path)
        .unwrap()
        .into_iter()
        .map(|d| d.name.display().to_owned())
        .collect();
    let expected = std::collections::BTreeSet::from([
        "idx:3:Doc:1:p".to_owned(),
        "idx:4:Link:1:p".to_owned(),
        "idx:3:Doc:c2:1:z:1:a".to_owned(),
        "vidx:3:Doc:1:v".to_owned(),
        "tidx:3:Doc:4:body".to_owned(),
    ]);
    assert_eq!(declared, expected);
    let physical = SharedGraph::try_from_graph(
        snapshot
            .state
            .graphs
            .values()
            .next()
            .unwrap()
            .graph
            .read()
            .as_ref()
            .clone(),
    )
    .unwrap();
    let registry = selene_gql::BuiltinProcedureRegistry::new();
    let mut observed = std::collections::BTreeSet::new();
    for query in [
        "CALL selene.property_index_stats() YIELD name",
        "CALL selene.vector_index_stats() YIELD name",
        "CALL selene.text_index_stats() YIELD name",
    ] {
        let selene_gql::StatementOutput::Rows(rows) = selene_gql::Session::new(&physical)
            .execute_source(query, &registry)
            .unwrap()
        else {
            panic!("diagnostic rows")
        };
        for row in rows.rows() {
            let selene_core::Value::String(name) = &row.values()[0] else {
                panic!("name")
            };
            observed.insert(name.to_string());
        }
    }
    assert_eq!(observed, declared);
    let selene_gql::StatementOutput::Rows(show) = selene_gql::Session::new(&physical)
        .execute_source("SHOW INDEXES", &registry)
        .unwrap()
    else {
        panic!("show rows")
    };
    // Preserve the current SHOW surface (scalar-node and vector), not invent a
    // new edge/composite/text SHOW grammar/surface as part of a binding repair.
    for row in show.rows() {
        let selene_core::Value::String(name) = &row.values()[0] else {
            panic!("name")
        };
        assert!(declared.contains(name.as_str()));
    }
    assert_eq!(show.row_count(), 2);
    let future = alternatives(&database, &path);
    publish(&database, &path, drop_indexes);
    assert_eq!(
        database.catalog().snapshot().declarations(&path).unwrap(),
        future
    );
}

#[test]
fn explicit_native_create_drop_recreate_and_rollback_keep_identity_local() {
    let (database, _, path) = fixture();
    let session = database.session(&path).unwrap();
    let before = database.catalog().snapshot();
    session.execute("START TRANSACTION").unwrap();
    session
        .execute("CALL selene.create_index('Doc', 'p', 'i64')")
        .unwrap();
    session
        .execute("CALL selene.drop_index('Doc', 'p')")
        .unwrap();
    session
        .execute("CALL selene.create_index('Doc', 'p', 'i64')")
        .unwrap();
    assert!(before.shares_state_with(&database.catalog().snapshot()));
    session.execute("ROLLBACK").unwrap();
    assert_eq!(database.catalog().snapshot().state.high_water.index, 0);
    session.execute("START TRANSACTION").unwrap();
    session
        .execute("CALL selene.create_index('Doc', 'p', 'i64')")
        .unwrap();
    session
        .execute("CALL selene.drop_index('Doc', 'p')")
        .unwrap();
    session
        .execute("CALL selene.create_index('Doc', 'p', 'i64')")
        .unwrap();
    session.execute("COMMIT").unwrap();
    assert_eq!(
        database.catalog().snapshot().declarations(&path).unwrap()[0].id,
        DeclarationId::Index(2)
    );
}
