//! Typed parameter runtime validation regressions.

use selene_core::{DbString, GraphId, NodeId, Record, Value, VectorValue};
use selene_gql::{
    EmptyProcedureRegistry, ExecutorError, GqlType, PipelineStatement, QueryPipeline, RecordType,
    ReturnClause, ReturnItem, Session, SourceSpan, Statement, ValueExpr, analyze,
    execute_statement, plan,
};
use selene_graph::SharedGraph;
use smallvec::smallvec;

fn db_string(value: &str) -> DbString {
    selene_core::db_string(value).expect("test string fits DB string cap")
}

fn typed_parameter_statement(name: DbString, declared_type: GqlType) -> Statement {
    let span = SourceSpan::new(0, 4);
    Statement::Query(QueryPipeline {
        working_scopes: Vec::new(),
        select_origin: None,
        statements: vec![PipelineStatement::Return(ReturnClause {
            distinct: false,
            star: false,
            items: vec![ReturnItem {
                expr: ValueExpr::Parameter {
                    name: name.clone(),
                    declared_type: Some(declared_type),
                    span,
                },
                alias: Some(name),
                span,
            }],
            group_by: None,
            having: None,
            span,
        })],
        span,
    })
}

#[test]
fn typed_parameter_runtime_rejects_mismatched_list_elements() {
    let graph = SharedGraph::new(GraphId::new(4122));
    let mut session = Session::new(&graph);
    session.bind_parameter(
        db_string("vals"),
        Value::List(vec![Value::Int(1), Value::String(db_string("bad"))]),
    );

    let err = session
        .execute_source("RETURN $vals :: LIST<INT> AS vals", &EmptyProcedureRegistry)
        .expect_err("typed list parameter rejects mismatched element");
    assert!(matches!(
        err,
        ExecutorError::InvalidParameterType {
            name,
            ref expected,
            actual: "LIST",
            ..
        } if name.as_str() == "vals" && expected == "LIST<INTEGER>"
    ));
}

#[test]
fn typed_parameter_runtime_accepts_vector() {
    let name = db_string("vec");
    let statement = typed_parameter_statement(name.clone(), GqlType::Vector);
    let analyzed = analyze(statement, &EmptyProcedureRegistry, None).expect("statement analyzes");
    let plan = plan(&analyzed, &EmptyProcedureRegistry).expect("statement plans");
    let graph = SharedGraph::new(GraphId::new(4125));
    let mut session = Session::new(&graph);

    session.bind_parameter(
        name,
        Value::Vector(VectorValue::new(vec![1.0, 2.0]).expect("test vector is valid")),
    );
    execute_statement(&plan, &mut session, &EmptyProcedureRegistry)
        .expect("vector parameter accepts matching value");
}

#[test]
fn typed_parameter_runtime_rejects_non_vector() {
    let name = db_string("vec");
    let statement = typed_parameter_statement(name.clone(), GqlType::Vector);
    let analyzed = analyze(statement, &EmptyProcedureRegistry, None).expect("statement analyzes");
    let plan = plan(&analyzed, &EmptyProcedureRegistry).expect("statement plans");
    let graph = SharedGraph::new(GraphId::new(4126));
    let mut session = Session::new(&graph);

    session.bind_parameter(name.clone(), Value::List(vec![Value::Float(1.0)]));
    let err = execute_statement(&plan, &mut session, &EmptyProcedureRegistry)
        .expect_err("vector parameter rejects mismatched value");
    assert!(matches!(
        err,
        ExecutorError::InvalidParameterType {
            name: err_name,
            ref expected,
            actual: "LIST",
            ..
        } if err_name == name && expected == "VECTOR"
    ));
}

#[test]
fn typed_parameter_runtime_accepts_qualified_duration_family() {
    let graph = SharedGraph::new(GraphId::new(4128));
    let mut session = Session::new(&graph);
    session.bind_parameter(
        db_string("span"),
        Value::Duration(Box::new("P1Y2M".parse().unwrap())),
    );

    session
        .execute_source(
            "RETURN $span :: DURATION (YEAR TO MONTH) AS span",
            &EmptyProcedureRegistry,
        )
        .expect("qualified duration parameter accepts matching family");
}

#[test]
fn typed_parameter_runtime_rejects_wrong_qualified_duration_family() {
    let graph = SharedGraph::new(GraphId::new(4129));
    let mut session = Session::new(&graph);
    session.bind_parameter(
        db_string("span"),
        Value::Duration(Box::new("PT1H".parse().unwrap())),
    );

    let err = session
        .execute_source(
            "RETURN $span :: DURATION (YEAR TO MONTH) AS span",
            &EmptyProcedureRegistry,
        )
        .expect_err("qualified duration parameter rejects wrong family");
    assert!(matches!(
        err,
        ExecutorError::InvalidParameterType {
            name,
            ref expected,
            actual: "DURATION",
            ..
        } if name.as_str() == "span" && expected == "DURATION (YEAR TO MONTH)"
    ));
}

#[test]
fn typed_parameter_runtime_rejects_mismatched_closed_record_fields() {
    let span = SourceSpan::new(0, 4);
    let field = db_string("count");
    let name = db_string("rec");
    let statement = Statement::Query(QueryPipeline {
        working_scopes: Vec::new(),
        select_origin: None,
        statements: vec![PipelineStatement::Return(ReturnClause {
            distinct: false,
            star: false,
            items: vec![ReturnItem {
                expr: ValueExpr::Parameter {
                    name: name.clone(),
                    declared_type: Some(GqlType::Record(RecordType::Closed(vec![(
                        field.clone(),
                        GqlType::Integer,
                    )]))),
                    span,
                },
                alias: Some(name.clone()),
                span,
            }],
            group_by: None,
            having: None,
            span,
        })],
        span,
    });
    let analyzed = analyze(statement, &EmptyProcedureRegistry, None).expect("statement analyzes");
    let plan = plan(&analyzed, &EmptyProcedureRegistry).expect("statement plans");
    let graph = SharedGraph::new(GraphId::new(4123));
    let mut session = Session::new(&graph);

    session.bind_parameter(
        name.clone(),
        Value::Record(Box::new(Record::Open(smallvec![(
            field,
            Value::String(db_string("three")),
        )]))),
    );
    let err = execute_statement(&plan, &mut session, &EmptyProcedureRegistry)
        .expect_err("closed record parameter rejects mismatched field type");
    let ExecutorError::InvalidParameterType {
        name: err_name,
        expected,
        actual,
        ..
    } = err
    else {
        panic!("expected InvalidParameterType");
    };
    assert_eq!(err_name, name);
    assert_eq!(expected, "RECORD{\"count\" :: INTEGER}");
    assert_eq!(actual, "RECORD");
}

#[test]
fn typed_parameter_runtime_accepts_matching_closed_record_fields() {
    let span = SourceSpan::new(0, 4);
    let field = db_string("count");
    let name = db_string("rec");
    let statement = Statement::Query(QueryPipeline {
        working_scopes: Vec::new(),
        select_origin: None,
        statements: vec![PipelineStatement::Return(ReturnClause {
            distinct: false,
            star: false,
            items: vec![ReturnItem {
                expr: ValueExpr::Parameter {
                    name: name.clone(),
                    declared_type: Some(GqlType::Record(RecordType::Closed(vec![(
                        field.clone(),
                        GqlType::Integer,
                    )]))),
                    span,
                },
                alias: Some(name.clone()),
                span,
            }],
            group_by: None,
            having: None,
            span,
        })],
        span,
    });
    let analyzed = analyze(statement, &EmptyProcedureRegistry, None).expect("statement analyzes");
    let plan = plan(&analyzed, &EmptyProcedureRegistry).expect("statement plans");
    let graph = SharedGraph::new(GraphId::new(4124));
    let mut session = Session::new(&graph);

    session.bind_parameter(
        name,
        Value::Record(Box::new(Record::Open(smallvec![(field, Value::Int(3))]))),
    );
    execute_statement(&plan, &mut session, &EmptyProcedureRegistry)
        .expect("closed record parameter accepts matching field type");
}

#[test]
fn typed_parameter_runtime_renders_nested_closed_record_type() {
    let graph = SharedGraph::new(GraphId::new(4127));
    let mut session = Session::new(&graph);
    session.bind_parameter(
        db_string("records"),
        Value::List(vec![Value::Record(Box::new(Record::Open(smallvec![(
            db_string("count"),
            Value::String(db_string("three")),
        )])))]),
    );

    let err = session
        .execute_source(
            "RETURN $records :: LIST<RECORD{count :: INT}> AS records",
            &EmptyProcedureRegistry,
        )
        .expect_err("nested closed record parameter rejects mismatched field");
    let ExecutorError::InvalidParameterType {
        name,
        expected,
        actual,
        ..
    } = err
    else {
        panic!("expected InvalidParameterType");
    };
    assert_eq!(name.as_str(), "records");
    assert_eq!(expected, "LIST<RECORD{\"count\" :: INTEGER}>");
    assert_eq!(actual, "LIST");
}

#[test]
fn typed_parameter_runtime_renders_internal_reference_type() {
    let name = db_string("node");
    let statement = typed_parameter_statement(name.clone(), GqlType::NodeRef);
    let analyzed = analyze(statement, &EmptyProcedureRegistry, None).expect("statement analyzes");
    let plan = plan(&analyzed, &EmptyProcedureRegistry).expect("statement plans");
    let graph = SharedGraph::new(GraphId::new(4128));
    let mut session = Session::new(&graph);

    session.bind_parameter(name.clone(), Value::String(db_string("not-node")));
    let err = execute_statement(&plan, &mut session, &EmptyProcedureRegistry)
        .expect_err("reference parameter rejects mismatched value");
    let ExecutorError::InvalidParameterType {
        name: err_name,
        expected,
        actual,
        ..
    } = err
    else {
        panic!("expected InvalidParameterType");
    };
    assert_eq!(err_name, name);
    assert_eq!(expected, "NODE");
    assert_eq!(actual, "STRING");

    let mut matching = Session::new(&graph);
    matching.bind_parameter(db_string("node"), Value::NodeRef(NodeId::new(1)));
    execute_statement(&plan, &mut matching, &EmptyProcedureRegistry)
        .expect("reference parameter accepts matching value");
}
