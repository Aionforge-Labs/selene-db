//! BRIEF-27 DDL planner lowering tests.

use selene_gql::{
    AnalyzedType, CatalogOp, DdlStatement, EmptyProcedureRegistry, GqlType, PipelineOp,
    PlannedTypePropertyConstraint, analyze, parse, plan,
};

fn plan_one(source: &str) -> selene_gql::ExecutionPlan {
    let statement = parse(source).expect("test input parses");
    let analyzed = analyze(statement, &EmptyProcedureRegistry, None).expect("test input analyzes");
    plan(&analyzed, &EmptyProcedureRegistry).expect("test input plans")
}

fn catalog_op(plan: &selene_gql::ExecutionPlan) -> &CatalogOp {
    let [PipelineOp::Catalog(op)] = plan.pipeline.as_slice() else {
        panic!("expected single catalog op, got {:?}", plan.pipeline);
    };
    op
}

#[test]
fn create_node_type_preserves_properties_default_and_validation() {
    let plan = plan_one(
        "CREATE NODE TYPE IF NOT EXISTS :Person EXTENDS :Entity \
         (name :: STRING NOT NULL, age :: INT DEFAULT 0) STRICT",
    );
    let CatalogOp::CreateNodeType {
        if_not_exists,
        extends,
        properties,
        validation_mode,
        ..
    } = catalog_op(&plan)
    else {
        panic!("expected create node type");
    };
    assert!(*if_not_exists);
    assert_eq!(extends.clone().expect("extends").as_str(), "Entity");
    assert_eq!(*validation_mode, Some(selene_gql::ValidationMode::Strict));
    assert_eq!(properties.len(), 2);
    assert!(matches!(
        properties[1].constraints.as_slice(),
        [PlannedTypePropertyConstraint::Default(project, _)]
            if project.ty == AnalyzedType::Resolved(GqlType::Integer)
    ));
}

#[test]
fn create_edge_type_preserves_endpoints() {
    let plan = plan_one(
        "CREATE EDGE TYPE :KNOWS EXTENDS :RELATIONSHIP (FROM :Person TO :Person, since :: DATE)",
    );
    let CatalogOp::CreateEdgeType {
        label,
        extends,
        endpoints,
        properties,
        ..
    } = catalog_op(&plan)
    else {
        panic!("expected create edge type");
    };
    assert_eq!(label.as_str(), "KNOWS");
    assert_eq!(extends.clone().expect("parent").as_str(), "RELATIONSHIP");
    let endpoints = endpoints.as_ref().expect("endpoint spec");
    assert_eq!(endpoints.from_labels[0].as_str(), "Person");
    assert_eq!(endpoints.to_labels[0].as_str(), "Person");
    assert_eq!(properties[0].gql_type, GqlType::Date);
}

#[test]
fn alter_node_type_lowers_property_defaults() {
    let plan = plan_one("ALTER NODE TYPE :Person (active BOOLEAN DEFAULT true)");
    let CatalogOp::AlterNodeType {
        label, properties, ..
    } = catalog_op(&plan)
    else {
        panic!("expected alter node type");
    };
    assert_eq!(label.as_str(), "Person");
    assert_eq!(properties.len(), 1);
    assert_eq!(properties[0].gql_type, GqlType::Boolean);
    assert!(matches!(
        properties[0].constraints.as_slice(),
        [PlannedTypePropertyConstraint::Default(project, _)]
            if project.ty == AnalyzedType::Resolved(GqlType::Boolean)
    ));
}

#[test]
fn drop_type_and_show_type_plans() {
    let plan = plan_one("DROP EDGE TYPE IF EXISTS :KNOWS");
    assert!(matches!(
        catalog_op(&plan),
        CatalogOp::DropEdgeType {
            if_exists: true,
            ..
        }
    ));

    let plan = plan_one("SHOW NODE TYPES");
    assert!(matches!(catalog_op(&plan), CatalogOp::ShowNodeTypes(_)));
    let columns = &plan.output_schema.columns;
    assert_eq!(columns[0].name.clone().expect("label").as_str(), "label");
    assert_eq!(columns[0].ty, AnalyzedType::Resolved(GqlType::String));
    assert_eq!(
        columns[1].name.clone().expect("definition").as_str(),
        "definition"
    );
    assert_eq!(columns[1].ty, AnalyzedType::DYNAMIC);
}

#[test]
fn create_index_plan_preserves_name_label_properties_and_if_not_exists() {
    let plan = plan_one("CREATE INDEX IF NOT EXISTS sensor_ts_idx ON :Sensor(ts, value)");
    let CatalogOp::CreateIndex {
        name,
        label,
        properties,
        if_not_exists,
        ..
    } = catalog_op(&plan)
    else {
        panic!("expected create index");
    };
    assert_eq!(name.as_str(), "sensor_ts_idx");
    assert_eq!(label.as_str(), "Sensor");
    assert_eq!(
        properties
            .iter()
            .map(|property| property.as_str())
            .collect::<Vec<_>>(),
        ["ts", "value"]
    );
    assert!(*if_not_exists);
}

#[test]
fn drop_index_plan_preserves_name_and_if_exists() {
    let plan = plan_one("DROP INDEX IF EXISTS sensor_ts_idx");
    let CatalogOp::DropIndex {
        name, if_exists, ..
    } = catalog_op(&plan)
    else {
        panic!("expected drop index");
    };
    assert_eq!(name.as_str(), "sensor_ts_idx");
    assert!(*if_exists);
}

#[test]
fn all_property_constraints_lower() {
    // The ISO/IEC 39075:2024 §18 property constraints the engine accepts:
    // DEFAULT, IMMUTABLE, UNIQUE, INDEXED. Explicit value-type nullability is
    // carried on the GqlType and lowered to the required-property bit at the
    // catalog boundary. Donor full-text/time-series constraints —
    // SEARCHABLE/DICTIONARY/FILL/INTERVAL/ENCODING — were removed from the
    // grammar; they are now clean 42001 syntax errors.
    let plan = plan_one(
        "CREATE NODE TYPE :Sensor \
         (v :: STRING NOT NULL DEFAULT 'x' IMMUTABLE UNIQUE INDEXED)",
    );
    let CatalogOp::CreateNodeType { properties, .. } = catalog_op(&plan) else {
        panic!("expected create node type");
    };
    assert!(matches!(
        &properties[0].gql_type,
        GqlType::NotNull(inner) if **inner == GqlType::String
    ));
    assert_eq!(properties[0].constraints.len(), 4);
}

#[test]
fn database_catalog_ddl_lowers_to_its_storage_neutral_command() {
    let reference = selene_gql::CatalogObjectReference {
        absolute: false,
        segments: vec![selene_gql::CatalogPathSegment {
            name: selene_core::db_string("g").expect("test string fits DB string cap"),
            form: selene_gql::IdentifierForm::Regular,
        }],
        span: selene_gql::SourceSpan::new(0, 1),
    };
    let analyzed = selene_gql::analyze(
        selene_gql::Statement::Ddl(DdlStatement::CreateGraph {
            reference: reference.clone(),
            or_replace: true,
            if_not_exists: true,
            graph_type: None,
            span: selene_gql::SourceSpan::new(0, 1),
        }),
        &EmptyProcedureRegistry,
        None,
    )
    .expect("DDL analyzes");
    let plan = plan(&analyzed, &EmptyProcedureRegistry).expect("plans");
    assert_eq!(
        catalog_op(&plan),
        &CatalogOp::DatabaseCatalog(selene_gql::DatabaseCatalogCommand::CreateGraph {
            reference,
            or_replace: true,
            if_not_exists: true,
            graph_type: None,
            span: selene_gql::SourceSpan::new(0, 1),
        })
    );
}

#[test]
fn sentinel_ddl_plan_shape_snapshot() {
    let plan = plan_one("SHOW EDGE TYPES");
    let summary = format!(
        "pipeline={}\ncolumns={}:{}",
        if matches!(catalog_op(&plan), CatalogOp::ShowEdgeTypes(_)) {
            "show-edge-types"
        } else {
            "other"
        },
        plan.output_schema.columns[0]
            .name
            .clone()
            .expect("label")
            .as_str(),
        plan.output_schema.columns[1]
            .name
            .clone()
            .expect("definition")
            .as_str(),
    );
    insta::assert_snapshot!(summary, @r###"
pipeline=show-edge-types
columns=label:definition
"###);
}
