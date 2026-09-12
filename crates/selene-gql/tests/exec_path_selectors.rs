//! Executor path-selector tests.

mod exec_common;

use exec_common::{db_string, node_ids_for, planned, props};
use selene_core::{CancellationToken, GraphId, LabelSet, Value};
use selene_gql::{
    Binding, BindingTable, BindingTableSchema, EmptyProcedureRegistry, ExecutorError, TxContext,
    execute_pattern, execute_pipeline,
};
use selene_graph::SharedGraph;

struct PathSelectorFixture {
    graph: SharedGraph,
}

impl PathSelectorFixture {
    fn build() -> Self {
        let node = db_string("N");
        let edge = db_string("K");
        let name = db_string("name");
        let graph = SharedGraph::new(GraphId::new(6332));
        {
            let mut txn = graph.begin_write();
            let mut mutator = txn.mutator();
            let a = mutator
                .create_node(
                    LabelSet::single(node.clone()),
                    props([(name.clone(), Value::String(db_string("A")))]),
                )
                .expect("A inserts");
            let b1 = mutator
                .create_node(
                    LabelSet::single(node.clone()),
                    props([(name.clone(), Value::String(db_string("B1")))]),
                )
                .expect("B1 inserts");
            let b2 = mutator
                .create_node(
                    LabelSet::single(node.clone()),
                    props([(name.clone(), Value::String(db_string("B2")))]),
                )
                .expect("B2 inserts");
            let c = mutator
                .create_node(
                    LabelSet::single(node.clone()),
                    props([(name.clone(), Value::String(db_string("C")))]),
                )
                .expect("C inserts");
            let d = mutator
                .create_node(
                    LabelSet::single(node),
                    props([(name, Value::String(db_string("D")))]),
                )
                .expect("D inserts");

            mutator
                .create_edge(edge.clone(), a, c, props([]))
                .expect("edge 1");
            mutator
                .create_edge(edge.clone(), a, c, props([]))
                .expect("edge 2");
            mutator
                .create_edge(edge.clone(), a, b1, props([]))
                .expect("edge 3");
            mutator
                .create_edge(edge.clone(), b1, c, props([]))
                .expect("edge 4");
            mutator
                .create_edge(edge.clone(), a, b2, props([]))
                .expect("edge 5");
            mutator
                .create_edge(edge.clone(), b2, c, props([]))
                .expect("edge 6");
            mutator
                .create_edge(edge.clone(), b1, d, props([]))
                .expect("edge 7");
            mutator.create_edge(edge, d, c, props([])).expect("edge 8");
            txn.commit().expect("fixture commits");
        }
        Self { graph }
    }

    fn execute(&self, source: &str) -> Result<BindingTable, ExecutorError> {
        let plan = planned(source);
        let mut ctx = TxContext::read_only(
            self.graph.read(),
            &plan.impl_defined_caps,
            &EmptyProcedureRegistry,
            self.graph.index_providers(),
        )
        .with_plan_metadata(&plan.expr_ids, &plan.subqueries);
        let input = if let Some(pattern) = &plan.pattern_plan {
            execute_pattern(pattern, &ctx)?
        } else {
            BindingTable::new(
                BindingTableSchema {
                    columns: Vec::new(),
                },
                vec![Binding::empty()],
            )
        };
        execute_pipeline(&plan.pipeline, input, &mut ctx)
    }

    fn row_count(&self, source: &str) -> usize {
        self.execute(source).expect("query executes").row_count()
    }
}

fn edge_list_lengths(table: &BindingTable, name: &str) -> Vec<usize> {
    exec_common::column_values(table, name)
        .into_iter()
        .map(|value| match value {
            Value::List(items) => items.len(),
            other => panic!("expected edge list, got {other:?}"),
        })
        .collect()
}

#[test]
fn all_selector_preserves_rows_across_path_shapes() {
    let fixture = PathSelectorFixture::build();

    assert_eq!(
        fixture.row_count("MATCH ALL (a:N {name: 'A'})-[:K]->(c:N {name: 'C'}) RETURN c"),
        2
    );
    assert_eq!(
        fixture.row_count("MATCH ALL (a:N {name: 'A'})-[:K*1..3]->(c:N {name: 'C'}) RETURN c"),
        5
    );
    assert_eq!(
        fixture.row_count("MATCH ALL (a:N {name: 'A'})-[:K]->(m)-[:K]->(c:N {name: 'C'}) RETURN m"),
        2
    );
    assert_eq!(
        fixture.row_count(
            "MATCH ALL (a:N {name: 'A'})-[:K]->(m)-[r:K*1..2]->(c:N {name: 'C'}) RETURN r"
        ),
        3
    );
}

#[test]
fn any_selector_keeps_one_row_per_endpoint_pair_across_path_shapes() {
    let fixture = PathSelectorFixture::build();

    assert_eq!(
        fixture.row_count("MATCH ANY (a:N {name: 'A'})-[:K]->(c:N {name: 'C'}) RETURN c"),
        1
    );
    assert_eq!(
        fixture.row_count("MATCH ANY (a:N {name: 'A'})-[:K*1..3]->(c:N {name: 'C'}) RETURN c"),
        1
    );
    assert_eq!(
        fixture.row_count("MATCH ANY (a:N {name: 'A'})-[:K]->(m)-[:K]->(c:N {name: 'C'}) RETURN m"),
        1
    );
    assert_eq!(
        fixture.row_count(
            "MATCH ANY (a:N {name: 'A'})-[:K]->(m)-[r:K*1..2]->(c:N {name: 'C'}) RETURN r"
        ),
        1
    );
}

#[test]
fn any_count_keeps_up_to_n_rows_per_endpoint_partition() {
    let fixture = PathSelectorFixture::build();

    // Endpoint partitions for A-[K*1..2]->t are:
    // (A,C): 4 bindings, (A,B1): 1, (A,B2): 1, (A,D): 1.
    // ANY 2 keeps at most two implementation-chosen bindings per partition.
    assert_eq!(
        fixture.row_count("MATCH ANY 2 (a:N {name: 'A'})-[r:K*1..2]->(t:N) RETURN r"),
        5
    );
    assert_eq!(
        fixture.row_count("MATCH ANY 99 (a:N {name: 'A'})-[r:K*1..2]->(t:N) RETURN r"),
        fixture.row_count("MATCH ALL (a:N {name: 'A'})-[r:K*1..2]->(t:N) RETURN r")
    );
}

#[test]
fn all_shortest_keeps_all_minimum_hop_rows_across_path_shapes() {
    let fixture = PathSelectorFixture::build();

    assert_eq!(
        fixture.row_count("MATCH ALL SHORTEST (a:N {name: 'A'})-[:K]->(c:N {name: 'C'}) RETURN c"),
        2
    );
    let quantified = fixture
        .execute("MATCH ALL SHORTEST (a:N {name: 'A'})-[r:K*1..3]->(c:N {name: 'C'}) RETURN r")
        .expect("query executes");
    assert_eq!(edge_list_lengths(&quantified, "r"), vec![1, 1]);
    assert_eq!(
        fixture.row_count(
            "MATCH ALL SHORTEST (a:N {name: 'A'})-[:K]->(m)-[:K]->(c:N {name: 'C'}) RETURN m"
        ),
        2
    );
    let mixed = fixture
        .execute(
            "MATCH ALL SHORTEST (a:N {name: 'A'})-[:K]->(m)-[r:K*1..2]->(c:N {name: 'C'}) RETURN r",
        )
        .expect("query executes");
    assert_eq!(edge_list_lengths(&mixed, "r"), vec![1, 1]);
}

#[test]
fn any_shortest_keeps_one_minimum_hop_row_across_path_shapes() {
    let fixture = PathSelectorFixture::build();

    assert_eq!(
        fixture.row_count("MATCH ANY SHORTEST (a:N {name: 'A'})-[:K]->(c:N {name: 'C'}) RETURN c"),
        1
    );
    assert_eq!(
        fixture.row_count(
            "MATCH ANY SHORTEST (a:N {name: 'A'})-[:K*1..3]->(c:N {name: 'C'}) RETURN c"
        ),
        1
    );
    assert_eq!(
        fixture.row_count(
            "MATCH ANY SHORTEST (a:N {name: 'A'})-[:K]->(m)-[:K]->(c:N {name: 'C'}) RETURN m"
        ),
        1
    );
    assert_eq!(
        fixture.row_count(
            "MATCH ANY SHORTEST (a:N {name: 'A'})-[:K]->(m)-[r:K*1..2]->(c:N {name: 'C'}) RETURN r"
        ),
        1
    );
}

#[test]
fn shortest_one_path_equals_any_shortest_and_one_group_equals_all_shortest() {
    // ISO §16.6 SR2c: SHORTEST 1 (PATH) == ANY SHORTEST; SHORTEST 1 GROUP ==
    // ALL SHORTEST. Pin the equivalences on the same multi-length endpoint pair.
    // Compare the full bound `r` edge-list column (binding identity), not merely
    // the hop-length multiset, so a future divergence between the SHORTEST-1 and
    // ANY/ALL-SHORTEST code paths (same edges? same order?) would be caught.
    let fixture = PathSelectorFixture::build();

    let any_shortest = fixture
        .execute("MATCH ANY SHORTEST (a:N {name: 'A'})-[r:K*1..3]->(c:N {name: 'C'}) RETURN r")
        .expect("ANY SHORTEST executes");
    let shortest_1 = fixture
        .execute("MATCH SHORTEST 1 (a:N {name: 'A'})-[r:K*1..3]->(c:N {name: 'C'}) RETURN r")
        .expect("SHORTEST 1 executes");
    assert_eq!(
        exec_common::column_values(&shortest_1, "r"),
        exec_common::column_values(&any_shortest, "r"),
        "SHORTEST 1 must equal ANY SHORTEST at full binding identity"
    );

    let all_shortest = fixture
        .execute("MATCH ALL SHORTEST (a:N {name: 'A'})-[r:K*1..3]->(c:N {name: 'C'}) RETURN r")
        .expect("ALL SHORTEST executes");
    let shortest_1_group = fixture
        .execute("MATCH SHORTEST 1 GROUP (a:N {name: 'A'})-[r:K*1..3]->(c:N {name: 'C'}) RETURN r")
        .expect("SHORTEST 1 GROUP executes");
    assert_eq!(
        exec_common::column_values(&shortest_1_group, "r"),
        exec_common::column_values(&all_shortest, "r"),
        "SHORTEST 1 GROUP must equal ALL SHORTEST at full binding identity"
    );
}

#[test]
fn counted_shortest_paths_and_groups_partition_by_hop_length() {
    // Endpoint pair (A, C) under [:K*1..3] (TRAIL default) has hop lengths
    // {1, 1, 2, 2, 3}: two direct A->C edges (len 1), A->B1->C and A->B2->C
    // (len 2), and A->B1->D->C (len 3).
    let fixture = PathSelectorFixture::build();
    let lengths = |source: &str| {
        let mut got = edge_list_lengths(&fixture.execute(source).expect("query executes"), "r");
        got.sort_unstable();
        got
    };

    // Baseline: ALL keeps every binding of every length.
    assert_eq!(
        lengths("MATCH ALL (a:N {name: 'A'})-[r:K*1..3]->(c:N {name: 'C'}) RETURN r"),
        vec![1, 1, 2, 2, 3]
    );

    // §22.4 GR13b-i: SHORTEST 2 PATHS keeps min(2, |PART|) bindings in
    // increasing-hop order = the two len-1 bindings.
    assert_eq!(
        lengths("MATCH SHORTEST 2 (a:N {name: 'A'})-[r:K*1..3]->(c:N {name: 'C'}) RETURN r"),
        vec![1, 1]
    );
    // SHORTEST 3 PATHS = the two len-1 bindings + ONE len-2 binding (first-N).
    assert_eq!(
        lengths("MATCH SHORTEST 3 (a:N {name: 'A'})-[r:K*1..3]->(c:N {name: 'C'}) RETURN r"),
        vec![1, 1, 2]
    );

    // §22.4 GR13b-ii: SHORTEST 2 GROUPS keeps all bindings in the 2 smallest
    // distinct lengths = both len-1 + both len-2 = 4 bindings.
    assert_eq!(
        lengths("MATCH SHORTEST 2 GROUPS (a:N {name: 'A'})-[r:K*1..3]->(c:N {name: 'C'}) RETURN r"),
        vec![1, 1, 2, 2]
    );
    // SHORTEST 1 GROUP = only the smallest distinct length (len 1).
    assert_eq!(
        lengths("MATCH SHORTEST 1 GROUP (a:N {name: 'A'})-[r:K*1..3]->(c:N {name: 'C'}) RETURN r"),
        vec![1, 1]
    );
}

#[test]
fn counted_shortest_count_at_or_above_partition_size_keeps_all() {
    // §22.4 GR13b: when N >= |partition| (paths) or N >= #distinct-lengths
    // (groups), the whole partition is retained.
    let fixture = PathSelectorFixture::build();
    let mut all_lengths = edge_list_lengths(
        &fixture
            .execute("MATCH ALL (a:N {name: 'A'})-[r:K*1..3]->(c:N {name: 'C'}) RETURN r")
            .expect("query executes"),
        "r",
    );
    all_lengths.sort_unstable();

    for source in [
        // 99 >= 5 bindings: keep all.
        "MATCH SHORTEST 99 (a:N {name: 'A'})-[r:K*1..3]->(c:N {name: 'C'}) RETURN r",
        // 99 >= 3 distinct lengths: keep all.
        "MATCH SHORTEST 99 GROUPS (a:N {name: 'A'})-[r:K*1..3]->(c:N {name: 'C'}) RETURN r",
    ] {
        let mut got = edge_list_lengths(&fixture.execute(source).expect("query executes"), "r");
        got.sort_unstable();
        assert_eq!(got, all_lengths, "{source} must keep the whole partition");
    }
}

#[test]
fn counted_shortest_cutoff_applies_per_endpoint_partition() {
    // §22.4 GR12-13: the N cutoff is per (source, final) partition. Build two
    // distinct endpoint pairs and assert each is counted independently.
    let node = db_string("N");
    // The midpoint carries a distinct label so `(t:N)` excludes it: the only
    // final-node targets are X and Y, giving exactly two endpoint partitions.
    let mid_label = db_string("M");
    let edge = db_string("K");
    let name = db_string("name");
    let graph = SharedGraph::new(GraphId::new(6335));
    {
        let mut txn = graph.begin_write();
        let mut mutator = txn.mutator();
        let src = mutator
            .create_node(
                LabelSet::single(node.clone()),
                props([(name.clone(), Value::String(db_string("S")))]),
            )
            .expect("S inserts");
        // Partition (S, X): one len-1 + one len-2 binding.
        let mid_x = mutator
            .create_node(
                LabelSet::single(mid_label),
                props([(name.clone(), Value::String(db_string("MX")))]),
            )
            .expect("MX inserts");
        let x = mutator
            .create_node(
                LabelSet::single(node.clone()),
                props([(name.clone(), Value::String(db_string("X")))]),
            )
            .expect("X inserts");
        // Partition (S, Y): two len-1 bindings.
        let y = mutator
            .create_node(
                LabelSet::single(node),
                props([(name, Value::String(db_string("Y")))]),
            )
            .expect("Y inserts");

        mutator
            .create_edge(edge.clone(), src, x, props([]))
            .expect("S->X");
        mutator
            .create_edge(edge.clone(), src, mid_x, props([]))
            .expect("S->MX");
        mutator
            .create_edge(edge.clone(), mid_x, x, props([]))
            .expect("MX->X");
        mutator
            .create_edge(edge.clone(), src, y, props([]))
            .expect("S->Y a");
        mutator
            .create_edge(edge, src, y, props([]))
            .expect("S->Y b");
        txn.commit().expect("fixture commits");
    }
    let fixture = PathSelectorFixture { graph };

    // SHORTEST 1 PATH: each partition keeps exactly one binding -> 2 rows total.
    assert_eq!(
        fixture.row_count("MATCH SHORTEST 1 (s:N {name: 'S'})-[r:K*1..2]->(t:N) RETURN r"),
        2
    );
    // SHORTEST 2 PATHS: (S,X) keeps 2 (its len-1 + len-2), (S,Y) keeps 2 -> 4.
    assert_eq!(
        fixture.row_count("MATCH SHORTEST 2 (s:N {name: 'S'})-[r:K*1..2]->(t:N) RETURN r"),
        4
    );
    // SHORTEST 1 GROUP: (S,X) keeps only its len-1 binding (1), (S,Y) keeps both
    // len-1 bindings (2) -> 3 rows. Distinguishes group from path counting.
    assert_eq!(
        fixture.row_count("MATCH SHORTEST 1 GROUP (s:N {name: 'S'})-[r:K*1..2]->(t:N) RETURN r"),
        3
    );
}

#[test]
fn pure_node_selector_is_zero_hop_noop() {
    let fixture = PathSelectorFixture::build();
    for selector in ["ALL", "ANY", "ALL SHORTEST", "ANY SHORTEST"] {
        let source = format!("MATCH {selector} (a:N {{name: 'A'}}) RETURN a");
        assert_eq!(fixture.row_count(&source), 1);
    }
}

#[test]
fn anonymous_quantified_edge_under_selector_uses_hidden_hop_list() {
    let fixture = PathSelectorFixture::build();

    let table = fixture
        .execute("MATCH ANY SHORTEST (a:N {name: 'A'})-[:K*1..3]->(c:N {name: 'C'}) RETURN c")
        .expect("query executes");

    assert_eq!(node_ids_for(&table, "c"), vec![Some(4)]);
}

#[test]
fn path_selector_composes_under_optional_match() {
    let fixture = PathSelectorFixture::build();

    let table = fixture
        .execute(
            "MATCH (a:N {name: 'A'}) OPTIONAL MATCH ANY SHORTEST (a)-[:K*1..2]->(z:N {name: 'Z'}) RETURN a, z",
        )
        .expect("query executes");

    assert_eq!(node_ids_for(&table, "a"), vec![Some(1)]);
    assert_eq!(node_ids_for(&table, "z"), vec![None]);
}

#[test]
fn path_selector_keeps_repeat_inline_group_predicate_shape() {
    let fixture = PathSelectorFixture::build();

    let table = fixture
        .execute(
            "MATCH ALL SHORTEST (a:N {name: 'A'})-[r:K*1..3 WHERE size(r) = 1]->(c:N {name: 'C'}) RETURN r",
        )
        .expect("query executes");

    assert_eq!(edge_list_lengths(&table, "r"), vec![1, 1]);
}

#[test]
fn shortest_selectors_preserve_default_walk_without_inventing_trail() {
    let node = db_string("N");
    let edge = db_string("K");
    let name = db_string("name");
    let graph = SharedGraph::new(GraphId::new(6334));
    {
        let mut txn = graph.begin_write();
        let mut mutator = txn.mutator();
        let a = mutator
            .create_node(
                LabelSet::single(node),
                props([(name, Value::String(db_string("A")))]),
            )
            .expect("A inserts");
        mutator
            .create_edge(edge, a, a, props([]))
            .expect("loop edge");
        txn.commit().expect("fixture commits");
    }
    let fixture = PathSelectorFixture { graph };

    assert_eq!(
        fixture.row_count(
            "MATCH ALL SHORTEST (a:N {name: 'A'})-[:K*2..2]->(b:N {name: 'A'}) RETURN b"
        ),
        1
    );
    assert_eq!(
        fixture.row_count(
            "MATCH ANY SHORTEST (a:N {name: 'A'})-[:K]->(:N {name: 'A'})-[:K]->(b:N {name: 'A'}) RETURN b"
        ),
        1
    );
    assert_eq!(
        fixture.row_count(
            "MATCH ALL SHORTEST TRAIL (a:N {name: 'A'})-[:K*2..2]->(b:N {name: 'A'}) RETURN b"
        ),
        0
    );
}

#[test]
fn path_selector_checks_cancellation_while_filtering_rows() {
    let root = db_string("Root");
    let graph = SharedGraph::new(GraphId::new(6333));
    {
        let mut txn = graph.begin_write();
        let mut mutator = txn.mutator();
        for _ in 0..1100 {
            mutator
                .create_node(LabelSet::single(root.clone()), props([]))
                .expect("root inserts");
        }
        txn.commit().expect("fixture commits");
    }

    let plan = planned("MATCH ANY (n:Root) RETURN n");
    let pattern = plan.pattern_plan.as_ref().expect("pattern plan");
    let token = CancellationToken::new();
    token.cancel();
    let ctx = TxContext::read_only(
        graph.read(),
        &plan.impl_defined_caps,
        &EmptyProcedureRegistry,
        graph.index_providers(),
    )
    .with_plan_metadata(&plan.expr_ids, &plan.subqueries)
    .with_resource_limits(Some(&token), None, None, None);

    let err = execute_pattern(pattern, &ctx).expect_err("path selector observes token");

    assert!(matches!(err, ExecutorError::Cancelled { .. }));
    assert_eq!(err.gqlstatus().as_str(), "5GQL2");
}
