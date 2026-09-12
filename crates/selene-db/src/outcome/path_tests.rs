//! Native selected paths cross the existing §22.4 facade boundary unchanged.

use super::*;
use crate::{
    CreatePolicy, Database, GeneralParameter, ObjectPath, Request, RequestParams, SchemaPath,
};
use selene_gql::runtime::product_path::BoundedPathProgram;
use selene_gql::{
    EmptyProcedureRegistry, ImplDefinedCaps, TxContext, analyze, lower_path_automata_with_defaults,
    parse,
};

#[test]
fn native_selected_path_descriptor_identity_and_deleted_access_survive_facade_conversion() {
    let db = Database::builder().build();
    let schema = SchemaPath::regular("selene", "selected_paths").unwrap();
    let path = ObjectPath::regular("selene", "selected_paths", "main").unwrap();
    db.catalog()
        .create_schema(&schema, CreatePolicy::Strict)
        .unwrap();
    db.catalog()
        .create_graph(&path, None, CreatePolicy::Strict)
        .unwrap();
    let session = db.session(&path).unwrap();
    session
        .execute("INSERT (a:A)-[:E]->(b:B), (a)-[:E]->(b) FINISH")
        .unwrap();
    let owner = session.graph_reference().unwrap();
    let a = analyze(
        parse("MATCH ALL SHORTEST p = (a:A)-[r{1,2}]->(b:B) RETURN p").unwrap(),
        &EmptyProcedureRegistry,
        None,
    )
    .unwrap();
    let set = lower_path_automata_with_defaults(&a).unwrap();
    let program = BoundedPathProgram::compile(&set.automata, &a).unwrap();
    let result = session
        .inner
        .with_reference_graph(owner.graph_id(), |graph| {
            let caps = ImplDefinedCaps::default();
            let tx = TxContext::read_only(
                graph.read(),
                &caps,
                &EmptyProcedureRegistry,
                graph.index_providers(),
            );
            let native = program
                .execute(&tx, Default::default())
                .map_err(crate::Error::from_engine)?;
            let descriptor = selene_gql::BindingTableDescriptor::from_schema(native.table.schema());
            RegularResult::from_engine(&native.table, &descriptor, owner)
        })
        .unwrap();
    assert_eq!(result.row_count(), 2);
    assert_eq!(
        result.descriptor().fields()[0].declared_type(),
        &DeclaredType::Resolved(Type::PATH)
    );
    let values: Vec<_> = result
        .rows()
        .iter()
        .map(|r| r.values()[0].clone())
        .collect();
    let Value::Path(first) = &values[0] else {
        panic!("typed path")
    };
    let Value::Path(second) = &values[1] else {
        panic!("typed path")
    };
    assert_eq!(first.graph(), owner);
    assert_eq!(first.start(), second.start());
    assert_ne!(first.segments()[0].edge(), second.segments()[0].edge());
    assert_eq!(
        first.segments()[0].direction(),
        selene_core::EdgeDirection::Outgoing
    );
    session
        .execute("MATCH ()-[r:E]->() DELETE r FINISH")
        .unwrap();
    let mut params = RequestParams::new();
    params
        .insert(
            "p",
            GeneralParameter::new(Type::PATH, values[0].clone()).unwrap(),
        )
        .unwrap();
    let copied = session
        .execute_request(Request::with_params("RETURN $p", params))
        .into_result()
        .unwrap();
    let ExecutionOutcome::Rows { result, .. } = copied else {
        panic!("rows")
    };
    assert_eq!(result.rows()[0].values()[0], values[0]);
    let mut params = RequestParams::new();
    params
        .insert(
            "e",
            GeneralParameter::new(Type::EDGE, Value::EdgeRef(first.segments()[0].edge())).unwrap(),
        )
        .unwrap();
    let error = session
        .execute_request(Request::with_params("RETURN labels($e)", params))
        .into_result()
        .unwrap_err();
    assert_eq!(error.gqlstatus().unwrap().as_str(), "22G11");
    assert_eq!(
        session
            .path_reference(first.start(), first.segments().to_vec())
            .unwrap_err()
            .gqlstatus()
            .unwrap()
            .as_str(),
        "22G11"
    );
}
