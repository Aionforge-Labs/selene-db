//! Closed-graph static schema validation tests.

use selene_core::{DbString, LabelSet, PropertyValueType};
use selene_gql::{
    AnalysisError, AnalyzedStatement, EdgeDirection, EdgePattern, EmptyProcedureRegistry,
    GraphPattern, InsertStatement, LabelExpr, Literal, MutationPipeline, MutationStatement,
    NodePattern, NonEmpty, PatternElement, SourceSpan, Statement, ValueExpr, analyze,
    ast::CharacterStringLiteralKind, parse,
};
use selene_graph::{
    EdgeEndpointDef, EdgeTypeDef, GraphTypeDef, NodeTypeDef, PropertyTypeDef, RecordFieldType,
    RecordFieldTypeDef, RecordFieldTypes, ValidationMode,
};
use selene_testing::{
    mentions_one_of_graph_type, person_company_graph_type, person_only_graph_type,
};

fn db_string(value: &str) -> DbString {
    selene_core::db_string(value).expect("test strings fit DB string cap")
}

fn labels(values: &[&str]) -> LabelSet {
    values.iter().map(|value| db_string(value)).collect()
}

fn property(name: &str, value_type: PropertyValueType, required: bool) -> PropertyTypeDef {
    PropertyTypeDef {
        name: db_string(name),
        value_type,
        list_element_type: None,
        required,
        default: None,
        immutable: false,
        unique: false,
        decimal_type: None,
        character_string_type: None,
        byte_string_type: None,
        record_field_types: None,
    }
}

/// Closed graph type with `:Host` carrying a closed typed RECORD property
/// `config :: RECORD{host :: STRING, port :: INT}`.
fn host_record_graph_type() -> GraphTypeDef {
    let config = PropertyTypeDef {
        name: db_string("config"),
        value_type: PropertyValueType::RecordTyped,
        list_element_type: None,
        required: false,
        default: None,
        immutable: false,
        unique: false,
        decimal_type: None,
        character_string_type: None,
        byte_string_type: None,
        record_field_types: Some(RecordFieldTypes(vec![
            RecordFieldTypeDef {
                name: db_string("host"),
                field_type: RecordFieldType::Scalar(PropertyValueType::String),
                required: true,
            },
            RecordFieldTypeDef {
                name: db_string("port"),
                field_type: RecordFieldType::Scalar(PropertyValueType::Int),
                required: true,
            },
        ])),
    };
    GraphTypeDef {
        name: db_string("fixture.host_record"),
        node_types: vec![NodeTypeDef {
            name: db_string("Host"),
            key_labels: LabelSet::single(db_string("Host")),
            properties: vec![config],
            validation_mode: ValidationMode::Strict,
        }],
        edge_types: Vec::new(),
    }
    .validate()
    .expect("host record fixture graph type is valid")
}

fn json_graph_type() -> GraphTypeDef {
    GraphTypeDef {
        name: db_string("fixture.json"),
        node_types: vec![NodeTypeDef {
            name: db_string("Thing"),
            key_labels: LabelSet::single(db_string("Thing")),
            properties: vec![property("payload", PropertyValueType::Json, true)],
            validation_mode: ValidationMode::Strict,
        }],
        edge_types: Vec::new(),
    }
    .validate()
    .expect("JSON fixture graph type is valid")
}

fn analyze_source(
    source: &str,
    schema: Option<&GraphTypeDef>,
) -> Result<AnalyzedStatement, AnalysisError> {
    let statement = parse(source).expect("test input parses");
    analyze(statement, &EmptyProcedureRegistry, schema)
}

fn analyze_with_schema(
    source: &str,
    graph_type: &GraphTypeDef,
) -> Result<AnalyzedStatement, AnalysisError> {
    analyze_source(source, Some(graph_type))
}

fn schema_error(source: &str, graph_type: &GraphTypeDef) -> AnalysisError {
    analyze_with_schema(source, graph_type).expect_err("schema validation rejects input")
}

fn label_expr(label: &str) -> Option<LabelExpr> {
    Some(LabelExpr::Single(db_string(label)))
}

fn string_expr(value: &str, span: SourceSpan) -> ValueExpr {
    ValueExpr::Literal(Literal::String(
        db_string(value),
        span,
        CharacterStringLiteralKind::Escaped,
    ))
}

fn node(name: &str, label: &str, name_value: &str, span: SourceSpan) -> PatternElement {
    PatternElement::Node(NodePattern {
        binding: Some(db_string(name)),
        label_expr: label_expr(label),
        properties: vec![(db_string("name"), string_expr(name_value, span))],
        inline_where: None,
        span,
    })
}

fn edge(label: &str, direction: EdgeDirection, span: SourceSpan) -> PatternElement {
    PatternElement::Edge(EdgePattern {
        binding: None,
        direction,
        abbreviated: false,
        label_expr: label_expr(label),
        properties: Vec::new(),
        quantifier: None,
        inline_where: None,
        span,
    })
}

fn ambiguous_property_graph_type() -> GraphTypeDef {
    GraphTypeDef {
        name: db_string("fixture.ambiguous"),
        node_types: vec![
            NodeTypeDef {
                name: db_string("Person"),
                key_labels: LabelSet::single(db_string("Person")),
                properties: vec![
                    property("name", PropertyValueType::String, true),
                    property("flag", PropertyValueType::String, false),
                ],
                validation_mode: ValidationMode::Strict,
            },
            NodeTypeDef {
                name: db_string("ActivePerson"),
                key_labels: labels(&["Person", "Active"]),
                properties: vec![
                    property("name", PropertyValueType::String, true),
                    property("flag", PropertyValueType::Int, false),
                ],
                validation_mode: ValidationMode::Strict,
            },
        ],
        edge_types: Vec::new(),
    }
    .validate()
    .expect("fixture graph type is valid")
}

fn duplicate_edge_label_graph_type() -> GraphTypeDef {
    GraphTypeDef {
        name: db_string("fixture.duplicate_edge_label"),
        node_types: vec![
            NodeTypeDef {
                name: db_string("Person"),
                key_labels: LabelSet::single(db_string("Person")),
                properties: vec![property("name", PropertyValueType::String, true)],
                validation_mode: ValidationMode::Strict,
            },
            NodeTypeDef {
                name: db_string("Company"),
                key_labels: LabelSet::single(db_string("Company")),
                properties: vec![property("name", PropertyValueType::String, true)],
                validation_mode: ValidationMode::Strict,
            },
        ],
        edge_types: vec![
            EdgeTypeDef {
                name: db_string("WorksAt"),
                label: db_string("REL"),
                source_node_type: EdgeEndpointDef::NodeType(0),
                target_node_type: EdgeEndpointDef::NodeType(1),
                properties: vec![property("since", PropertyValueType::Int, false)],
                validation_mode: ValidationMode::Strict,
            },
            EdgeTypeDef {
                name: db_string("Knows"),
                label: db_string("REL"),
                source_node_type: EdgeEndpointDef::NodeType(0),
                target_node_type: EdgeEndpointDef::NodeType(0),
                properties: vec![property("strength", PropertyValueType::Int, false)],
                validation_mode: ValidationMode::Strict,
            },
        ],
    }
    .validate()
    .expect("fixture graph type is valid")
}

fn label_transition_graph_type() -> GraphTypeDef {
    GraphTypeDef {
        name: db_string("fixture.label_transition"),
        node_types: vec![
            NodeTypeDef {
                name: db_string("Person"),
                key_labels: LabelSet::single(db_string("Person")),
                properties: vec![property("name", PropertyValueType::String, true)],
                validation_mode: ValidationMode::Strict,
            },
            NodeTypeDef {
                name: db_string("ActivePerson"),
                key_labels: labels(&["Person", "Active"]),
                properties: vec![property("name", PropertyValueType::String, true)],
                validation_mode: ValidationMode::Strict,
            },
            NodeTypeDef {
                name: db_string("SeniorPerson"),
                key_labels: labels(&["Person", "Senior"]),
                properties: vec![property("name", PropertyValueType::String, true)],
                validation_mode: ValidationMode::Strict,
            },
            NodeTypeDef {
                name: db_string("VisitorPerson"),
                key_labels: labels(&["Person", "Visitor"]),
                properties: vec![property("name", PropertyValueType::String, true)],
                validation_mode: ValidationMode::Strict,
            },
        ],
        edge_types: Vec::new(),
    }
    .validate()
    .expect("fixture graph type is valid")
}

#[path = "analyze_schema/inserts.rs"]
mod inserts;
#[path = "analyze_schema/properties.rs"]
mod properties;
#[path = "analyze_schema/transitions.rs"]
mod transitions;
