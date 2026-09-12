//! Inline unit tests for catalog-pipeline DDL rendering.
//!
//! Split out of `mod.rs` via `#[path]` to keep the production module under the
//! 700-LOC cap (CLAUDE.md hard rule 5); `use super::*` resolves against the
//! `catalog` module where the `mod tests;` declaration lives.

use super::*;

fn db_string(value: &str) -> DbString {
    selene_core::db_string(value).expect("test label admits")
}

#[test]
fn render_partial_any_edge_endpoint_as_endpoint_less_ddl() {
    let person = db_string("Person");
    let graph_type = GraphTypeDef {
        name: db_string("catalog.partial.any.graph"),
        node_types: vec![NodeTypeDef {
            name: person.clone(),
            key_labels: LabelSet::single(person),
            properties: Vec::new(),
            validation_mode: GraphValidationMode::Strict,
        }],
        edge_types: Vec::new(),
    };
    let knows = db_string("KNOWS");

    for (source_node_type, target_node_type) in [
        (EdgeEndpointDef::Any, EdgeEndpointDef::NodeType(0)),
        (EdgeEndpointDef::NodeType(0), EdgeEndpointDef::Any),
    ] {
        let edge_type = EdgeTypeDef {
            name: knows.clone(),
            label: knows.clone(),
            source_node_type,
            target_node_type,
            properties: Vec::new(),
            validation_mode: GraphValidationMode::Strict,
        };
        let rendered =
            render_edge_type_def(&graph_type, &edge_type).expect("edge type DDL renders");
        assert_eq!(rendered, "CREATE EDGE TYPE :KNOWS ()");
        crate::parse(&rendered).expect("rendered edge type DDL parses");
    }
}
