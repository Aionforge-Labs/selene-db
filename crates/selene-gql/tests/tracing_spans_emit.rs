//! Tracing span emission smoke tests.

use std::{
    collections::{HashMap, HashSet},
    sync::{Arc, Mutex},
};

use selene_core::GraphId;
use selene_gql::{
    BuiltinProcedureRegistry, OptimizeContext, Session, analyze, optimize, parse, plan,
};
use selene_graph::SharedGraph;
use tracing::{Id, Subscriber};
use tracing_subscriber::{
    Layer, Registry,
    layer::{Context, SubscriberExt},
};

const EXPECTED_SPANS: &[&str] = &[
    "selene.gql.parse",
    "selene.gql.analyze",
    "selene.gql.plan",
    "selene.gql.optimize",
    "selene.gql.execute_statement",
    "selene.graph.begin_write",
    "selene.graph.commit",
    "selene.graph.notify_providers",
    "selene.procedure.dispatch",
];

#[test]
fn tracing_spans_emit_for_write_and_call() {
    let observed = ObservedSpans::default();
    let subscriber = Registry::default().with(CollectingLayer {
        observed: observed.clone(),
    });
    let registry = BuiltinProcedureRegistry::new();
    let graph = SharedGraph::new(GraphId::new(121_001));

    // v1.2 (BRIEF 1): the commit publish tail — including the
    // `selene.graph.notify_providers` span — now runs on the per-graph
    // committer thread, not the calling thread. A thread-local subscriber would
    // not observe the committer thread's spans, so this test installs a
    // process-global subscriber (this is the only subscriber installed in this
    // test binary, so the set-once contract holds). The `selene.graph.commit`
    // span still emits on the calling thread (it wraps seal + submit on the
    // session thread). `commit()` blocks until the committer acks, so by the
    // time `execute_source` returns the committer's spans have closed.
    tracing::subscriber::set_global_default(subscriber)
        .expect("global subscriber set once in this test binary");
    {
        let mut session = Session::new(&graph);
        session
            .execute_source("INSERT (:TraceProbe)", &registry)
            .expect("write executes");
        session
            .execute_source(
                "CALL selene.health() YIELD graph_id, node_count, edge_count, schema_bound",
                &registry,
            )
            .expect("call executes");

        let stmt = parse("RETURN 1").expect("statement parses");
        let analyzed = analyze(stmt, &registry, None).expect("statement analyzes");
        let planned = plan(&analyzed, &registry).expect("statement plans");
        let _optimized = optimize(planned, &OptimizeContext::default());
    }

    // Drop the graph so its committer thread is joined; its spans have already
    // closed (commit() blocks until the committer acks) before we read the set.
    drop(graph);

    let closed = observed.closed();
    for expected in EXPECTED_SPANS {
        assert!(
            closed.contains(*expected),
            "span {expected} should be emitted and closed"
        );
    }
}

#[derive(Clone, Default)]
struct ObservedSpans {
    inner: Arc<Mutex<ObservedState>>,
}

impl ObservedSpans {
    fn closed(&self) -> HashSet<String> {
        self.inner
            .lock()
            .expect("span collector lock")
            .closed
            .clone()
    }
}

#[derive(Default)]
struct ObservedState {
    active: HashMap<Id, &'static str>,
    closed: HashSet<String>,
}

struct CollectingLayer {
    observed: ObservedSpans,
}

impl<S> Layer<S> for CollectingLayer
where
    S: Subscriber,
{
    fn on_new_span(&self, attrs: &tracing::span::Attributes<'_>, id: &Id, _ctx: Context<'_, S>) {
        self.observed
            .inner
            .lock()
            .expect("span collector lock")
            .active
            .insert(id.clone(), attrs.metadata().name());
    }

    fn on_close(&self, id: Id, _ctx: Context<'_, S>) {
        let mut observed = self.observed.inner.lock().expect("span collector lock");
        if let Some(name) = observed.active.remove(&id) {
            observed.closed.insert(name.to_owned());
        }
    }
}
