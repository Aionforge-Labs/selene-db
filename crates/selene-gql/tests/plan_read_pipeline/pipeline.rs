use super::*;

#[test]
fn preserves_set_op_in_composite_plan() {
    let plan = plan_one("RETURN 1 AS n UNION ALL RETURN 2 AS n");
    let Some(PipelineOp::Union { op, .. }) = plan.pipeline.last() else {
        panic!("expected union op");
    };
    assert_eq!(*op, SetOp::UnionAll);
}

#[test]
fn composite_subplans_keep_pattern_bindings_local() {
    // Arms must be column name-equal (ISO §14.2 SR v), so both project to `x`;
    // the internal pattern bindings (`n` vs `m`) stay arm-local regardless.
    let plan = plan_one("MATCH (n) RETURN n AS x UNION MATCH (m) RETURN m AS x");
    let first_pattern = plan.pattern_plan.as_ref().expect("first pattern");
    assert_eq!(first_pattern.bindings.len(), 1);
    assert_eq!(first_pattern.bindings[0].name.as_str(), "n");
    let Some(PipelineOp::Union { rhs, .. }) = plan.pipeline.last() else {
        panic!("expected union op");
    };
    let rhs_pattern = rhs.pattern_plan.as_ref().expect("rhs pattern");
    assert_eq!(rhs_pattern.bindings.len(), 1);
    assert_eq!(rhs_pattern.bindings[0].name.as_str(), "m");
}

#[test]
fn chained_query_uses_chain_pipeline_op() {
    let plan = plan_one("RETURN 1 AS n NEXT RETURN 2 AS n");
    assert!(matches!(plan.pipeline.last(), Some(PipelineOp::Chain(_))));
}

#[test]
fn correlated_chained_query_uses_correlated_chain_pipeline_op() {
    let plan = plan_one("FOR a IN [1, 2] RETURN a NEXT RETURN a + 10 AS b");
    assert!(matches!(
        plan.pipeline.last(),
        Some(PipelineOp::CorrelatedChain(_))
    ));
}

#[test]
fn limit_parameter_survives_to_plan() {
    let plan = plan_one("RETURN 1 AS n LIMIT $rows");
    let Some(PipelineOp::Limit { count, .. }) = plan.pipeline.last() else {
        panic!("expected limit");
    };
    assert!(matches!(
        count,
        LimitAmount::Parameter { name, .. } if name.as_str() == "rows"
    ));
}

#[test]
fn unaliased_derived_projection_column_has_no_name() {
    let plan = plan_one("MATCH (n) RETURN n.name");
    assert_eq!(plan.output_schema.columns.len(), 1);
    assert_eq!(plan.output_schema.columns[0].name, None);
}

#[test]
fn return_star_keeps_visible_binding_columns() {
    let plan = plan_one("MATCH (n) RETURN *");
    assert!(plan.output_schema.columns.iter().any(|column| {
        column
            .name
            .as_ref()
            .is_some_and(|name| name.as_str() == "n")
    }));
}

#[test]
fn order_key_carries_binding_refs() {
    let plan = plan_one("MATCH (n) RETURN n ORDER BY n.name");
    let Some(PipelineOp::OrderBy(keys)) = plan.pipeline.last() else {
        panic!("expected order-by");
    };
    assert_eq!(keys[0].binding_refs.len(), 1);
}

#[test]
fn non_leading_match_lowers_to_pipeline_match() {
    let plan = plan_one("MATCH (a) WITH a AS x MATCH (b) RETURN x, b");
    assert!(
        plan.pipeline
            .iter()
            .any(|op| matches!(op, PipelineOp::Match(_)))
    );
}

#[test]
fn non_leading_optional_match_lowers_to_pipeline_optional_match() {
    let plan = plan_one("MATCH (a) WITH a AS x OPTIONAL MATCH (b) RETURN x, b");
    assert!(
        plan.pipeline
            .iter()
            .any(|op| matches!(op, PipelineOp::OptionalMatch(_)))
    );
}

#[test]
fn lifted_quantifiers_transport_conditional_singleton_and_open_group() {
    let questioned = plan_one("MATCH (a)-[:K?]->(b) RETURN b");
    let selene_gql::PathSemanticElement::Edge(edge) =
        &path_program(&questioned).automata[0].semantic.elements[1]
    else {
        panic!("edge")
    };
    assert!(edge.exposure.is_conditional_singleton());

    let unbounded = plan_one("MATCH TRAIL (a)-[:K+]->(b) RETURN b");
    let automaton = &path_program(&unbounded).automata[0];
    assert_eq!(automaton.mode.mode, selene_gql::PathMode::Trail);
    let selene_gql::PathSemanticElement::Edge(edge) = &automaton.semantic.elements[1] else {
        panic!("edge")
    };
    assert!(edge.exposure.is_group());
    assert_eq!(
        edge.quantifier,
        selene_gql::EdgeQuantifierKind::Unbounded { min: 1 }
    );
}

#[test]
fn restrictive_path_mode_single_node_lowers_to_filter() {
    let plan = plan_one("MATCH SIMPLE (n) RETURN n");
    let pattern = plan.pattern_plan.as_ref().expect("pattern plan");

    assert!(matches!(pattern.join_tree, JoinTree::Paths(_)));
}

#[test]
fn different_edges_match_mode_is_clause_scoped_in_the_path_program() {
    let plan = plan_one("MATCH DIFFERENT EDGES (a)-[:K]->(b) RETURN a");
    assert_eq!(
        path_program(&plan).automata[0].match_mode.mode,
        Some(selene_gql::MatchMode::DifferentEdges)
    );
}

#[test]
fn repeatable_elements_and_default_install_no_match_mode_filter() {
    // ISO 39075:2024 §16.4 GR8(b): REPEATABLE ELEMENTS is BINDINGS = INNER, so
    // it installs no wrapper. selene's ID086 default is REPEATABLE ELEMENTS, so
    // a no-prefix MATCH installs none either. Both lower to a bare Expand.
    for source in [
        "MATCH REPEATABLE ELEMENTS (a)-[:K]->(b) RETURN a",
        "MATCH (a)-[:K]->(b) RETURN a",
    ] {
        let plan = plan_one(source);
        let pattern = plan.pattern_plan.as_ref().expect("pattern plan");
        assert!(matches!(pattern.join_tree, JoinTree::Expand { .. }));
    }
}

#[test]
fn quantifier_max_exceeding_cap_emits_program_limit() {
    let err = plan_err("MATCH (a)-[:K*1..101]->(b) RETURN a");
    assert!(matches!(
        err,
        PlannerError::ProgramLimitExceeded {
            limit_name: "max_quantifier",
            limit: 100,
            actual: 101,
            ..
        }
    ));
    assert_eq!(err.gqlstatus().as_str(), "5GQL1");
}

#[test]
fn gp03_subquery_label_does_not_leak_to_outer_binding() {
    // GP03 regression: a body pattern that reuses an imported binding with a
    // label must NOT refine the OUTER declaration's labels. `binding_defs`
    // (match_clause.rs) copies `decl.label_expr()` into the outer pattern plan,
    // so a leak would conjunct `:Sensor` onto the outer `a` and corrupt its scan
    // — invisible at execution (the same label also empties the inner semi-join)
    // but a real plan corruption. Imports are read-only, so outer `a` keeps
    // exactly the single `:Person` label, never a Conjunction.
    let plan = plan_one(
        "MATCH (a:Person) CALL (a) { MATCH (a:Sensor) RETURN 1 AS n LIMIT 1 } YIELD n RETURN n",
    );
    let pattern = plan.pattern_plan.as_ref().expect("outer pattern plan");
    let a = pattern
        .bindings
        .iter()
        .find(|binding| binding.name.as_str() == "a")
        .expect("outer binding a");
    assert!(
        matches!(&a.label_predicate, Some(LabelExpr::Single(label)) if label.as_str() == "Person"),
        "outer `a` label must stay the single :Person (no subquery leak), got {:?}",
        a.label_predicate
    );
}

#[test]
fn return_star_after_with_uses_with_projection() {
    // Why: prior planner walked `analyzed.scopes.declarations()` for RETURN *,
    // leaking bindings discarded by a WITH boundary into the output schema.
    let plan = plan_one("MATCH (n) WITH n AS x RETURN *");
    let names: Vec<_> = plan
        .output_schema
        .columns
        .iter()
        .filter_map(|column| column.name.as_ref().map(|name| name.as_str().to_string()))
        .collect();
    assert!(
        names.iter().any(|name| name == "x"),
        "x must remain visible after WITH: {names:?}"
    );
    assert!(
        !names.iter().any(|name| name == "n"),
        "n must not leak past WITH boundary: {names:?}"
    );
}

#[test]
fn return_star_does_not_emit_empty_project() {
    // Why: RETURN * carries empty `clause.items`, so the prior unconditional
    // `Project(project_items(...))` push produced an empty projection that an
    // executor would interpret as "drop all columns".
    let plan = plan_one("MATCH (n) RETURN *");
    let empty_projects = plan
        .pipeline
        .iter()
        .filter(|op| matches!(op, PipelineOp::Project(items) if items.is_empty()))
        .count();
    assert_eq!(empty_projects, 0, "got pipeline {:?}", variant_names(&plan));
    assert!(
        plan.output_schema.columns.iter().any(|column| column
            .name
            .as_ref()
            .is_some_and(|name| name.as_str() == "n")),
        "RETURN * output_schema must still expose visible bindings"
    );
}

#[test]
fn chained_plan_output_schema_uses_last_block() {
    // Why: NEXT establishes a fresh binding scope, so the outer plan's
    // output_schema must reflect the final block's projection — not the
    // first block's, which was the prior behavior.
    let plan = plan_one("RETURN 1 AS a NEXT RETURN 2 AS b");
    let names: Vec<_> = plan
        .output_schema
        .columns
        .iter()
        .filter_map(|column| column.name.as_ref().map(|name| name.as_str().to_string()))
        .collect();
    assert_eq!(names, vec!["b".to_string()]);
}

#[test]
fn limit_then_offset_fuses_into_single_op() {
    // Why: parsers can emit either source order. `LIMIT n OFFSET m` previously
    // lowered to two sequential Limit ops, which means "take n then skip m" —
    // the inverse of GQL semantics.
    let plan = plan_one("RETURN 1 AS n LIMIT 10 OFFSET 5");
    let limits: Vec<_> = plan
        .pipeline
        .iter()
        .filter_map(|op| {
            if let PipelineOp::Limit { offset, count } = op {
                Some((offset.clone(), count.clone()))
            } else {
                None
            }
        })
        .collect();
    assert_eq!(limits.len(), 1, "got pipeline {:?}", variant_names(&plan));
    assert_eq!(limits[0].0, LimitAmount::Literal(5));
    assert_eq!(limits[0].1, LimitAmount::Literal(10));
}

#[test]
fn plan_records_next_pipeline_op_id_high_water_mark() {
    let plan = plan_one("RETURN 1 AS n LIMIT 10");

    assert_eq!(plan.next_pipeline_op_id.get(), plan.pipeline.len() as u32);
}

#[test]
fn expression_subqueries_populate_plan_registry() {
    let plan = plan_one("MATCH (a) RETURN EXISTS { MATCH (a)-[]->(b) } AS e");
    let Some(PipelineOp::Project(projects)) = plan.pipeline.first() else {
        panic!("expected project op");
    };

    let exists = plan
        .subqueries
        .get(projects[0].expr_id)
        .expect("exists subquery planned");

    assert!(matches!(
        exists.kind,
        SubqueryKind::Exists { negated: false }
    ));
    assert_eq!(exists.outer_binding_refs.len(), 1);
}

#[test]
fn exists_query_body_populates_plan_body() {
    let plan = plan_one("MATCH (a) RETURN EXISTS { MATCH (a)-[]->(b) RETURN b } AS e");
    let Some(PipelineOp::Project(projects)) = plan.pipeline.first() else {
        panic!("expected project op");
    };

    let exists = plan
        .subqueries
        .get(projects[0].expr_id)
        .expect("exists subquery planned");

    assert!(matches!(
        exists.kind,
        SubqueryKind::Exists { negated: false }
    ));
    assert!(matches!(exists.body, SubqueryBody::Plan(_)));
    assert_eq!(exists.outer_binding_refs.len(), 1);
}

#[test]
fn composite_plan_refreshes_next_pipeline_op_id_after_union_append() {
    let plan = plan_one("RETURN 1 AS n UNION ALL RETURN 2 AS n");

    assert_eq!(plan.next_pipeline_op_id.get(), plan.pipeline.len() as u32);
    let Some(PipelineOp::Union { rhs, .. }) = plan.pipeline.last() else {
        panic!("expected union op");
    };
    assert_eq!(rhs.next_pipeline_op_id.get(), rhs.pipeline.len() as u32);
}

#[test]
fn chained_plan_refreshes_next_pipeline_op_id_after_chain_append() {
    let plan = plan_one("RETURN 1 AS n NEXT RETURN 2 AS n");

    assert_eq!(plan.next_pipeline_op_id.get(), plan.pipeline.len() as u32);
    let Some(PipelineOp::Chain(rhs)) = plan.pipeline.last() else {
        panic!("expected chain op");
    };
    assert_eq!(rhs.next_pipeline_op_id.get(), rhs.pipeline.len() as u32);
}

#[test]
fn anonymous_intermediate_node_does_not_leak_to_next_edge() {
    // Why: prior `leftmost_binding` fell back from `right_binding` to
    // `left_binding`, so `(a)-[]->()-[]->(c)` reported the second edge's left
    // endpoint as `Some(a)` instead of `None`, contradicting the parsed
    // topology.
    let plan = plan_one("MATCH (a)-[:K]->()-[:K]->(c) RETURN a, c");
    let pattern = plan.pattern_plan.as_ref().expect("pattern plan");
    let JoinTree::Expand {
        edge: outer_edge, ..
    } = &pattern.join_tree
    else {
        panic!("expected outer expand, got {:?}", pattern.join_tree);
    };
    assert!(outer_edge.right_binding.is_some());
    assert_eq!(
        outer_edge.left_binding, None,
        "left_binding of the second edge must be None for anonymous middle node"
    );
}
