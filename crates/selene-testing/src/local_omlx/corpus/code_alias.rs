//! Target-aware local embedding corpus for code and symbol alias retrieval rows.

use super::{CorpusInput, Topic};

pub(super) fn inputs() -> Vec<CorpusInput> {
    let mut inputs = Vec::new();
    for (topic, docs) in [
        (
            Topic::Gql,
            &[
                (
                    "Graph query anchors collect roots before procedure calls.",
                    None,
                ),
                (
                    "The native procedure catalog registers GQL CALL surfaces.",
                    None,
                ),
                (
                    "Source-string plan cache reuses analyzed GQL statements.",
                    Some("gql-plan-cache"),
                ),
                (
                    "ProcedureRegistry lookup returns metadata for native built-ins.",
                    Some("gql-procedure-registry"),
                ),
                ("Pattern matching binds node rows before aggregation.", None),
                (
                    "Graph type checks validate label and property shapes.",
                    None,
                ),
                (
                    "Planner pipelines lower RETURN items into projections.",
                    None,
                ),
                (
                    "A stale query-plan note is superseded by cache generation checks.",
                    None,
                ),
                (
                    "The parser accepts strict ISO GQL and rejects grammar aliases.",
                    None,
                ),
            ][..],
        ),
        (
            Topic::Vector,
            &[
                (
                    "Embedding roots seed graph expansion before exact rerank.",
                    None,
                ),
                ("Vector procedures expose cosine candidate scoring.", None),
                (
                    "score_vector_nodes_batch_checked reranks explicit NodeId lists.",
                    Some("vector-batch-score"),
                ),
                (
                    "approximate_vector_search_expanded_candidates_checked expands ANN roots.",
                    Some("vector-ann-expanded"),
                ),
                (
                    "HNSW search returns approximate nearest-neighbor roots.",
                    None,
                ),
                (
                    "IVF partitions dense vectors by coarse centroid assignment.",
                    None,
                ),
                (
                    "VectorCandidateSet algebra intersects graph-authored candidates.",
                    None,
                ),
                (
                    "A stale vector fact is contradicted by negative evidence.",
                    None,
                ),
                (
                    "Exact cosine top-k scores every component of each candidate vector.",
                    None,
                ),
            ][..],
        ),
        (
            Topic::AgentMemory,
            &[
                ("Session roots connect active tasks to memory facts.", None),
                (
                    "Support edges link recalled facts to provenance nodes.",
                    None,
                ),
                (
                    "omlx_current_support_facts excludes outgoing negative evidence.",
                    Some("memory-current-state"),
                ),
                (
                    "Provenance-required state keeps facts with grounded evidence links.",
                    Some("memory-provenance-state"),
                ),
                (
                    "Superseded memories stay linked but should not be retrieved.",
                    None,
                ),
                (
                    "Dependency hints keep task-local memories stable across calls.",
                    None,
                ),
                (
                    "A durable memory graph records why a preference was remembered.",
                    None,
                ),
                (
                    "Contradictory memory notes are unresolved until review.",
                    None,
                ),
                (
                    "Active facts should survive snapshot and WAL recovery.",
                    None,
                ),
            ][..],
        ),
        (
            Topic::Code,
            &[
                (
                    "Benchmark harnesses build graph fixtures before timing rows.",
                    None,
                ),
                (
                    "Rust tests assert deterministic GQL procedure outputs.",
                    None,
                ),
                (
                    "CandidateSet yields stable NodeId values directly without storage row mapping.",
                    Some("code-rowid-map"),
                ),
                (
                    "Criterion benchmark IDs include precision suffixes for comparisons.",
                    Some("code-bench-label"),
                ),
                (
                    "CallPlanCache stores reusable source-string execution plans.",
                    None,
                ),
                (
                    "The embedding client posts JSON without adding async dependencies.",
                    None,
                ),
                (
                    "File-size guards keep tracked Rust modules below the cap.",
                    None,
                ),
                (
                    "A stale helper that mutates callers should be replaced by a wrapper.",
                    None,
                ),
                (
                    "Cargo clippy lints benchmark targets as well as library code.",
                    None,
                ),
            ][..],
        ),
    ] {
        inputs.extend(
            docs.iter()
                .map(|(text, target_key)| CorpusInput::document(topic, *text, *target_key)),
        );
    }
    inputs.extend([
        query(
            Topic::Gql,
            "Which reusable query compiler avoids reparsing a statement string?",
            "gql-plan-cache",
        ),
        query(
            Topic::Gql,
            "Where does built-in procedure metadata get resolved?",
            "gql-procedure-registry",
        ),
        query(
            Topic::Vector,
            "Which vector routine reranks many caller-provided graph ids?",
            "vector-batch-score",
        ),
        query(
            Topic::Vector,
            "Which ANN path grows nearest-neighbor roots through support topology?",
            "vector-ann-expanded",
        ),
        query(
            Topic::AgentMemory,
            "Which maintained set removes contradicted memories before recall?",
            "memory-current-state",
        ),
        query(
            Topic::AgentMemory,
            "Which state keeps only memories with grounding evidence?",
            "memory-provenance-state",
        ),
        query(
            Topic::Code,
            "Which abstraction yields stable NodeId values without storage row mapping?",
            "code-rowid-map",
        ),
        query(
            Topic::Code,
            "Where do benchmark names carry quality measurements?",
            "code-bench-label",
        ),
    ]);
    inputs
}

pub(super) fn wide_inputs() -> Vec<CorpusInput> {
    let mut inputs = inputs();
    for (topic, docs) in [
        (
            Topic::Gql,
            &[
                (
                    "OPTIONAL MATCH keeps graph rows even when an optional edge is absent.",
                    Some("gql-optional-match"),
                ),
                (
                    "GROUP BY with collect_list roots prepares a batched procedure call.",
                    Some("gql-group-collect"),
                ),
            ][..],
        ),
        (
            Topic::Vector,
            &[
                (
                    "Cosine distance compares embedding direction after normalization.",
                    Some("vector-cosine-metric"),
                ),
                (
                    "ef_search widens HNSW query exploration before exact rerank.",
                    Some("vector-ef-search"),
                ),
            ][..],
        ),
        (
            Topic::AgentMemory,
            &[
                (
                    "A replacement edge marks an older memory preference as superseded.",
                    Some("memory-replacement-edge"),
                ),
                (
                    "A provenance link records the evidence source for a remembered fact.",
                    Some("memory-provenance-link"),
                ),
            ][..],
        ),
        (
            Topic::Code,
            &[
                (
                    "apply_patch changes files through structured hunks instead of shell redirection.",
                    Some("code-apply-patch"),
                ),
                (
                    "scripts/run-benches.sh runs Criterion targets sequentially to reduce timing noise.",
                    Some("code-bench-runner"),
                ),
            ][..],
        ),
    ] {
        inputs.extend(
            docs.iter()
                .map(|(text, target_key)| CorpusInput::document(topic, *text, *target_key)),
        );
    }
    inputs.extend([
        query(
            Topic::Gql,
            "Which pattern keeps a row when an optional relationship is missing?",
            "gql-optional-match",
        ),
        query(
            Topic::Gql,
            "Which grouping step batches roots before a procedure call?",
            "gql-group-collect",
        ),
        query(
            Topic::Vector,
            "Which metric scores embedding direction instead of raw magnitude?",
            "vector-cosine-metric",
        ),
        query(
            Topic::Vector,
            "Which HNSW query setting increases exploration breadth?",
            "vector-ef-search",
        ),
        query(
            Topic::AgentMemory,
            "Which graph link says an older memory should not be recalled?",
            "memory-replacement-edge",
        ),
        query(
            Topic::AgentMemory,
            "Which link stores the evidence source for a memory fact?",
            "memory-provenance-link",
        ),
        query(
            Topic::Code,
            "Which edit path changes files without shell redirection?",
            "code-apply-patch",
        ),
        query(
            Topic::Code,
            "Which script prevents Criterion bench targets from running concurrently?",
            "code-bench-runner",
        ),
    ]);
    inputs
}

fn query(topic: Topic, text: &'static str, target_key: &'static str) -> CorpusInput {
    CorpusInput::query(topic, text, Some(target_key))
}
