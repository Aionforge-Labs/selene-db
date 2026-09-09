//! Public facade GQL insertion, matching and path-request integration.

use selene_core::{EdgeDirection, EdgeId, GraphId, NodeId, Path, PathSegment, Value};
use selene_db::{
    CreatePolicy, Database, GeneralParameter, GqlType, ObjectPath, Request, RequestOutcome,
    RequestParams, SchemaPath,
};

fn session() -> selene_db::Session {
    let database = Database::builder().build();
    let schema = SchemaPath::regular("selene", "mixed").unwrap();
    let graph = ObjectPath::regular("selene", "mixed", "g").unwrap();
    database
        .catalog()
        .create_schema(&schema, CreatePolicy::Strict)
        .unwrap();
    database
        .catalog()
        .create_graph(&graph, None, CreatePolicy::Strict)
        .unwrap();
    database.session(&graph).unwrap()
}

#[test]
fn facade_insert_and_all_orientations_observe_intrinsic_undirected_semantics() {
    let s = session();
    s.execute("INSERT (a:A {n:1})~[:E {weight:7}]~(b:B {n:2}) FINISH")
        .unwrap();
    for (full, abbreviated, expected) in [
        ("-[e]->", "->", 0),
        ("<-[e]-", "<-", 0),
        ("~[e]~", "~", 1),
        ("<~[e]~", "<~", 1),
        ("~[e]~>", "~>", 1),
        ("<-[e]->", "<->", 0),
        ("-[e]-", "-", 1),
    ] {
        for (left, right) in [("A", "B"), ("B", "A")] {
            assert_eq!(s.execute(&format!("MATCH (a:{left}){full}(b:{right}) WHERE e.weight = 7 AND e IS NOT DIRECTED AND a IS NOT SOURCE OF e AND b IS NOT DESTINATION OF e RETURN e")).unwrap().row_count(), Some(expected));
            assert_eq!(
                s.execute(&format!(
                    "MATCH (a:{left}){abbreviated}(b:{right}) RETURN a"
                ))
                .unwrap()
                .row_count(),
                Some(expected)
            );
        }
    }
    s.execute("INSERT (a:C)<-[:D {weight:9}]-(b:D) FINISH")
        .unwrap();
    assert_eq!(s.execute("MATCH (a:D)-[e:D]->(b:C) WHERE e IS DIRECTED AND a IS SOURCE OF e AND b IS DESTINATION OF e RETURN e").unwrap().row_count(), Some(1));
    // A statement that staged new nodes before failing must not publish them.
    assert!(
        s.execute("INSERT (:RolledBack)~[:E {weight:1 / 0}]~(:RolledBack) FINISH")
            .is_err()
    );
    assert_eq!(
        s.execute("MATCH (n:RolledBack) RETURN n")
            .unwrap()
            .row_count(),
        Some(0)
    );
    assert_eq!(
        s.execute("MATCH (a:A)~[e]~(b:B) RETURN e")
            .unwrap()
            .row_count(),
        Some(1)
    );
}

#[test]
fn facade_path_request_requires_intrinsic_direction_and_accepts_both_directed_loop_steps() {
    let s = session();
    s.execute("INSERT (a:A)~[:E]~(b:B), (c:C)-[:D]->(c) FINISH")
        .unwrap();
    let graph = GraphId::new(s.context().dependencies().current_graph().get());
    for (edge, start, end, direction, valid) in [
        (1, 1, 2, EdgeDirection::Undirected, true),
        (1, 2, 1, EdgeDirection::Undirected, true),
        (1, 1, 2, EdgeDirection::Outgoing, false),
        (1, 2, 1, EdgeDirection::Incoming, false),
        (2, 3, 3, EdgeDirection::Outgoing, true),
        (2, 3, 3, EdgeDirection::Incoming, true),
        (2, 3, 3, EdgeDirection::Undirected, false),
    ] {
        let path = Path {
            graph,
            start: NodeId::new(start),
            segments: [PathSegment {
                edge: EdgeId::new(edge),
                node: NodeId::new(end),
                direction,
            }]
            .into_iter()
            .collect(),
        };
        let mut params = RequestParams::new();
        params
            .insert(
                "p",
                GeneralParameter::new(GqlType::Path, Value::Path(Box::new(path))).unwrap(),
            )
            .unwrap();
        let outcome = s.execute_request(Request::with_params("RETURN $p", params));
        if valid {
            assert!(
                matches!(outcome, RequestOutcome::Succeeded { .. }),
                "{outcome:?}"
            );
        } else {
            assert_eq!(
                outcome.error().unwrap().gqlstatus().unwrap().as_str(),
                "42002"
            );
        }
    }
}
