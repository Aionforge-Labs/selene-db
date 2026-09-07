//! Reusable corpora for opt-in embedding benchmark rows.

use selene_core::DbString;

mod code_alias;
mod project_code;
mod project_migration;
mod source_chunks;
mod source_code;
mod source_files;
mod workspace_source;

#[cfg(test)]
mod tests;

/// Corpus size and ambiguity profile for local embedding benchmarks.
#[derive(Clone, Copy)]
pub enum CorpusProfile {
    /// Four documents per topic plus one query per topic.
    Tiny,
    /// Agent-memory-oriented corpus with two queries per topic.
    AgentMemory,
    /// Corpus with overlapping terminology across topics to stress vector-only retrieval.
    AmbiguousMemory,
    /// Combined ambiguous + agent-memory corpus for larger local rows.
    ScaledAmbiguousMemory,
    /// Code and symbol alias corpus with per-query target facts.
    CodeAliasMemory,
    /// Wider code and symbol alias corpus with more target-hit queries.
    CodeAliasWideMemory,
    /// Target-aware corpus shaped like current selene-db source files.
    ProjectCodeMemory,
    /// Harder source-shaped alias corpus with lexical decoys.
    ProjectCodeAliasMemory,
    /// Source excerpt corpus with target-aware real code snippets.
    ProjectSourceCodeMemory,
    /// Source chunk corpus with target-aware implementation snippets.
    ProjectSourceChunkMemory,
    /// File-level corpus with selected real selene-db source files.
    ProjectSourceFileMemory,
    /// Live workspace-source corpus extracted from current selene-db files.
    ProjectWorkspaceSourceMemory,
    /// Legacy-alias corpus mapping archived/prototype terms to current engine surfaces.
    ProjectMigrationMemory,
}

impl CorpusProfile {
    /// Resolve a profile from `env_name`, defaulting to [`Self::Tiny`].
    pub fn from_env(env_name: &str) -> Self {
        std::env::var(env_name)
            .ok()
            .as_deref()
            .map_or(Self::Tiny, Self::from_value)
    }

    /// Resolve a profile from an environment value.
    pub fn from_value(value: &str) -> Self {
        match value {
            "" | "tiny" => Self::Tiny,
            "agent_memory" | "memory" => Self::AgentMemory,
            "ambiguous_memory" | "ambiguous" => Self::AmbiguousMemory,
            "scaled_ambiguous_memory" | "scaled_ambiguous" => Self::ScaledAmbiguousMemory,
            "code_alias_memory" | "code_alias" => Self::CodeAliasMemory,
            "code_alias_wide_memory" | "code_alias_wide" => Self::CodeAliasWideMemory,
            "project_code_memory" | "project_code" | "selene_project_code" => {
                Self::ProjectCodeMemory
            }
            "project_code_alias_memory" | "project_code_alias" | "selene_project_code_alias" => {
                Self::ProjectCodeAliasMemory
            }
            "project_source_code_memory" | "project_source_code" | "selene_source_code" => {
                Self::ProjectSourceCodeMemory
            }
            "project_source_chunk_memory" | "project_source_chunk" | "selene_source_chunk" => {
                Self::ProjectSourceChunkMemory
            }
            "project_source_file_memory" | "project_source_file" | "selene_source_file" => {
                Self::ProjectSourceFileMemory
            }
            "project_workspace_source_memory"
            | "project_workspace_source"
            | "selene_workspace_source" => Self::ProjectWorkspaceSourceMemory,
            "project_migration_memory" | "project_migration" | "selene_project_migration" => {
                Self::ProjectMigrationMemory
            }
            other => panic!("unsupported embedding corpus value: {other}"),
        }
    }

    /// Materialize the profile as ordered document and query inputs.
    pub fn inputs(self) -> Vec<CorpusInput> {
        match self {
            Self::Tiny => tiny_inputs(),
            Self::AgentMemory => agent_memory_inputs(),
            Self::AmbiguousMemory => ambiguous_memory_inputs(),
            Self::ScaledAmbiguousMemory => scaled_ambiguous_memory_inputs(),
            Self::CodeAliasMemory => code_alias::inputs(),
            Self::CodeAliasWideMemory => code_alias::wide_inputs(),
            Self::ProjectCodeMemory => project_code::inputs(),
            Self::ProjectCodeAliasMemory => project_code::alias_inputs(),
            Self::ProjectSourceCodeMemory => source_code::inputs(),
            Self::ProjectSourceChunkMemory => source_chunks::inputs(),
            Self::ProjectSourceFileMemory => source_files::inputs(),
            Self::ProjectWorkspaceSourceMemory => workspace_source::inputs(),
            Self::ProjectMigrationMemory => project_migration::inputs(),
        }
    }
}

/// Topic assigned to a corpus document or query.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum Topic {
    /// ISO GQL and graph-query language behavior.
    Gql,
    /// Vector storage, search, indexing, and rerank behavior.
    Vector,
    /// Agentic memory retrieval, provenance, and currentness.
    AgentMemory,
    /// Rust implementation and benchmark-code behavior.
    Code,
}

/// One text item sent to the local embedding endpoint.
#[derive(Clone)]
pub struct CorpusInput {
    /// Semantic topic used by graph labels and precision checks.
    pub topic: Topic,
    /// Whether this item is a searchable document (`true`) or query (`false`).
    pub is_document: bool,
    /// Text submitted to the embedding endpoint.
    pub text: String,
    /// Optional document key or query target key for target-hit benchmark rows.
    pub target_key: Option<&'static str>,
}

impl CorpusInput {
    /// Build a searchable document input.
    pub fn document(
        topic: Topic,
        text: impl Into<String>,
        target_key: Option<&'static str>,
    ) -> Self {
        Self {
            topic,
            is_document: true,
            text: text.into(),
            target_key,
        }
    }

    /// Build a query input.
    pub fn query(topic: Topic, text: impl Into<String>, target_key: Option<&'static str>) -> Self {
        Self {
            topic,
            is_document: false,
            text: text.into(),
            target_key,
        }
    }

    /// Borrow the text submitted to the embedding endpoint.
    pub fn text(&self) -> &str {
        &self.text
    }
}

/// Repeat document inputs while preserving a single copy of each query.
///
/// Duplicate documents keep their topic but clear target keys, so target-hit
/// rows keep a unique original target document. A short marker is appended to
/// duplicate text so embedding providers do not see exact byte-identical rows.
pub fn scale_document_inputs(
    inputs: Vec<CorpusInput>,
    document_repetitions: usize,
) -> Vec<CorpusInput> {
    assert!(
        document_repetitions > 0,
        "embedding corpus repeat must be greater than zero"
    );
    if document_repetitions == 1 {
        return inputs;
    }

    let documents = inputs
        .iter()
        .filter(|input| input.is_document)
        .cloned()
        .collect::<Vec<_>>();
    let queries = inputs
        .into_iter()
        .filter(|input| !input.is_document)
        .collect::<Vec<_>>();
    let mut scaled = Vec::with_capacity(documents.len() * document_repetitions + queries.len());
    scaled.extend(documents.iter().cloned());
    for repetition in 2..=document_repetitions {
        for input in &documents {
            let mut duplicate = input.clone();
            duplicate.target_key = None;
            duplicate.text.push_str(&format!(
                "\n\n[embedding corpus duplicate {repetition}/{document_repetitions}]"
            ));
            scaled.push(duplicate);
        }
    }
    scaled.extend(queries);
    scaled
}

/// Return the graph label used for `topic` in benchmark fixtures.
pub fn topic_label(topic: Topic) -> DbString {
    match topic {
        Topic::Gql => db_string("OmlxTopicGql"),
        Topic::Vector => db_string("OmlxTopicVector"),
        Topic::AgentMemory => db_string("OmlxTopicAgentMemory"),
        Topic::Code => db_string("OmlxTopicCode"),
    }
}

fn db_string(value: &str) -> DbString {
    selene_core::db_string(value).expect("local embedding fixture strings fit DB string cap")
}

fn tiny_inputs() -> Vec<CorpusInput> {
    let mut inputs = Vec::new();
    for (topic, texts) in [
        (
            Topic::Gql,
            &[
                "ISO GQL MATCH over graph patterns returns rows from a property graph.",
                "A GQL CALL procedure can expose implementation-defined graph algorithms.",
                "Graph type constraints validate labels and property value types.",
                "Serializable graph transactions preserve committed mutation ordering.",
            ][..],
        ),
        (
            Topic::Vector,
            &[
                "A dense embedding vector is stored as a first-class graph property value.",
                "HNSW approximate nearest-neighbor search trades recall for latency.",
                "IVF partitions vectors through coarse centroids before reranking candidates.",
                "Candidate-set scoring exact-reranks graph-derived vector candidates.",
            ][..],
        ),
        (
            Topic::AgentMemory,
            &[
                "Agent memory retrieval should prefer current facts over superseded evidence.",
                "Contradiction edges mark unresolved memory facts that should be filtered.",
                "Session and scope nodes narrow memory retrieval to a task-local subgraph.",
                "Recency windows and dependency edges produce active memory candidate sets.",
            ][..],
        ),
        (
            Topic::Code,
            &[
                "Rust benchmarks should use Criterion groups with stable benchmark IDs.",
                "A VectorCandidateSet stores sorted and deduplicated NodeId values.",
                "The score_vector_candidate_sets_batch_checked API scores many queries.",
                "A safe local HTTP client can post JSON without adding a runtime dependency.",
            ][..],
        ),
    ] {
        inputs.extend(
            texts
                .iter()
                .map(|text| CorpusInput::document(topic, *text, None)),
        );
    }
    inputs.extend([
        CorpusInput::query(
            Topic::Gql,
            "How does GQL execute graph pattern matching and procedure calls?",
            None,
        ),
        CorpusInput::query(
            Topic::Vector,
            "Which vector index should rerank embedding candidates in memory?",
            None,
        ),
        CorpusInput::query(
            Topic::AgentMemory,
            "Find current task memory while ignoring contradicted facts.",
            None,
        ),
        CorpusInput::query(
            Topic::Code,
            "Where is the Rust batch vector candidate scoring API implemented?",
            None,
        ),
    ]);
    inputs
}

fn agent_memory_inputs() -> Vec<CorpusInput> {
    let mut inputs = Vec::new();
    for (topic, texts) in [
        (
            Topic::Gql,
            &[
                "MATCH finds active memory nodes linked to the current agent session.",
                "CALL selene.vector_score_nodes reranks graph-derived memory candidates.",
                "A graph type can require memory facts to carry confidence and scope.",
                "Serializable GQL writes keep memory replacement edges ordered.",
                "Path patterns can traverse from a task node to supporting evidence.",
                "Closed graph constraints reject malformed agent memory records.",
                "Projection catalogs let algorithms run over scoped memory subgraphs.",
                "Procedure calls expose graph algorithms without extending GQL grammar.",
            ][..],
        ),
        (
            Topic::Vector,
            &[
                "A first-class embedding vector stores semantic memory content.",
                "HNSW returns approximate candidates before exact vector reranking.",
                "IVF partitions memory vectors into coarse semantic regions.",
                "Candidate-set scoring compares query embeddings to selected nodes.",
                "Vector dimensions differ across local embedding models.",
                "Cosine distance is the default semantic retrieval metric here.",
                "ANN recall can fail when graph hints already identify precise facts.",
                "Batch vector scoring amortizes query setup over multiple memories.",
            ][..],
        ),
        (
            Topic::AgentMemory,
            &[
                "The current preference says use rustls and avoid native TLS.",
                "A superseded preference should remain linked but not retrieved as current.",
                "Contradictory evidence marks a memory fact unresolved until reviewed.",
                "A session scope narrows retrieval to facts relevant to the task.",
                "Dependency edges identify facts required by the active planning step.",
                "Recent observations can be maintained as graph-authored active hints.",
                "A durable memory graph should preserve why a fact was remembered.",
                "Agent memory retrieval balances semantic similarity with graph currentness.",
            ][..],
        ),
        (
            Topic::Code,
            &[
                "The benchmark runner serializes Criterion invocations by bench name.",
                "VectorCandidateSet intersection uses sorted NodeId arrays.",
                "Candidate sets yield stable node ids without row index mapping.",
                "The local oMLX HTTP client posts JSON without adding async dependencies.",
                "Criterion benchmark IDs include quality suffixes for comparison.",
                "Graph fixtures build HNSW indexes before committing the write transaction.",
                "Rust clippy runs all targets so benchmark code must lint cleanly.",
                "The file-size script enforces a 700 line cap for tracked Rust files.",
            ][..],
        ),
    ] {
        inputs.extend(
            texts
                .iter()
                .map(|text| CorpusInput::document(topic, *text, None)),
        );
    }
    inputs.extend([
        CorpusInput::query(
            Topic::Gql,
            "How can GQL retrieve active agent memories through graph procedures?",
            None,
        ),
        CorpusInput::query(
            Topic::Gql,
            "Which graph patterns connect a task to supporting memory evidence?",
            None,
        ),
        CorpusInput::query(
            Topic::Vector,
            "How should vector candidates be reranked after graph filtering?",
            None,
        ),
        CorpusInput::query(
            Topic::Vector,
            "When does ANN help compared with exact scoring over graph candidates?",
            None,
        ),
        CorpusInput::query(
            Topic::AgentMemory,
            "Find current preferences and ignore superseded or contradictory facts.",
            None,
        ),
        CorpusInput::query(
            Topic::AgentMemory,
            "Retrieve session-scoped agent memory with provenance and recency hints.",
            None,
        ),
        CorpusInput::query(
            Topic::Code,
            "Where does the Rust benchmark derive graph candidate sets?",
            None,
        ),
        CorpusInput::query(
            Topic::Code,
            "Which code path yields stable node ids from candidate sets?",
            None,
        ),
    ]);
    inputs
}

fn ambiguous_memory_inputs() -> Vec<CorpusInput> {
    let mut inputs = Vec::new();
    for (topic, texts) in [
        (
            Topic::Gql,
            &[
                "A query anchor matches active facts before vector scoring begins.",
                "The graph pattern filters current memory candidates for a request.",
                "A GQL procedure scores candidate nodes after a MATCH-derived scope.",
                "Closed graph types validate the same memory record shape every commit.",
                "Path traversal finds the supporting evidence behind a recalled fact.",
                "A graph query can exclude stale facts without changing vector distance.",
                "Serializable mutation order decides which replacement edge is current.",
                "Projection names keep reusable candidate subgraphs stable across calls.",
            ][..],
        ),
        (
            Topic::Vector,
            &[
                "A query embedding scores active facts after graph filtering.",
                "The vector index returns semantic candidates for a request.",
                "An ANN procedure scores candidate nodes after an embedding-derived scope.",
                "Vector dimensions validate the same memory record shape every insert.",
                "Nearest-neighbor search finds the supporting evidence behind a recalled fact.",
                "A vector query can retrieve stale facts unless graph currentness filters them.",
                "Approximate ranking order decides which replacement fact appears nearest.",
                "Centroid partitions keep reusable candidate regions stable across searches.",
            ][..],
        ),
        (
            Topic::AgentMemory,
            &[
                "A session anchor scores active facts after graph and vector filtering.",
                "The memory graph returns current candidates for a request.",
                "An agent recall procedure scores candidate nodes after a session-derived scope.",
                "Memory schemas validate the same preference record shape every commit.",
                "Provenance traversal finds the supporting evidence behind a recalled fact.",
                "A memory query can exclude stale facts through graph currentness.",
                "Supersession edge order decides which replacement fact is current.",
                "Dependency hints keep reusable candidate memories stable across tasks.",
            ][..],
        ),
        (
            Topic::Code,
            &[
                "A benchmark anchor scores active fixtures after graph filtering.",
                "The Rust fixture returns current candidates for a request.",
                "A batch scoring function scores candidate nodes after a test-derived scope.",
                "Constructor assertions validate the same vector record shape every run.",
                "Fixture traversal finds the supporting edge behind a measured fact.",
                "A benchmark query can exclude stale fixtures without changing vector distance.",
                "Commit order decides which replacement edge appears in the snapshot.",
                "Stable benchmark IDs keep reusable candidate rows comparable across runs.",
            ][..],
        ),
    ] {
        inputs.extend(
            texts
                .iter()
                .map(|text| CorpusInput::document(topic, *text, None)),
        );
    }
    inputs.extend([
        CorpusInput::query(
            Topic::Gql,
            "Which graph query filters current candidate facts before scoring?",
            None,
        ),
        CorpusInput::query(
            Topic::Gql,
            "How does GQL traversal find supporting evidence for a recalled fact?",
            None,
        ),
        CorpusInput::query(
            Topic::Vector,
            "Which embedding search returns semantic candidates before reranking?",
            None,
        ),
        CorpusInput::query(
            Topic::Vector,
            "How can vector ranking retrieve stale facts without graph filtering?",
            None,
        ),
        CorpusInput::query(
            Topic::AgentMemory,
            "Which memory graph candidates are current for this session request?",
            None,
        ),
        CorpusInput::query(
            Topic::AgentMemory,
            "How do dependency hints keep recalled agent memories stable?",
            None,
        ),
        CorpusInput::query(
            Topic::Code,
            "Which Rust fixture function scores candidate nodes in a batch?",
            None,
        ),
        CorpusInput::query(
            Topic::Code,
            "How do stable benchmark IDs keep candidate rows comparable?",
            None,
        ),
    ]);
    inputs
}

fn scaled_ambiguous_memory_inputs() -> Vec<CorpusInput> {
    let mut inputs = ambiguous_memory_inputs();
    inputs.extend(agent_memory_inputs());
    inputs
}
