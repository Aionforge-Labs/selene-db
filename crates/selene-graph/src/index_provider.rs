//! Engine-owned in-memory observers, not persistence adapters or loadable packs.

use crate::{CandidateSet, Node, SeleneGraph, VectorCandidateSet};
use selene_core::{Change, DbString};
use std::fmt;

/// Four-byte identity for a fixed engine observer registration.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct ProviderTag(
    /// Raw observer tag.
    pub [u8; 4],
);
impl fmt::Display for ProviderTag {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.0.iter().all(|byte| byte.is_ascii_graphic()) {
            for byte in self.0 {
                f.write_str(char::from(byte).encode_utf8(&mut [0; 4]))?;
            }
            Ok(())
        } else {
            write!(
                f,
                "0x{:02X}{:02X}{:02X}{:02X}",
                self.0[0], self.0[1], self.0[2], self.0[3]
            )
        }
    }
}

/// First-party derived-state observer. Calls for a graph are serialized.
///
/// Live fanout observes publication first and expands truncate/reset into the
/// staged per-row tombstones. Error/panic isolation is per change by default,
/// or per batch when opted in. A failed observer cannot undo a commit; generation
/// notification is withheld so stale state cannot claim current readiness.
///
/// Callbacks must not re-enter graph mutation/maintenance. Same-thread re-entry
/// panics before taking a lock and is caught by the observer boundary. Waiting on
/// cross-thread graph work from a callback is unsupported and can deadlock.
/// Provider-owned persisted state is not part of the durable facade preview.
pub trait IndexProvider: Send + Sync + 'static {
    /// Unique registration tag.
    fn provider_tag(&self) -> ProviderTag;
    /// Observe a published mutation; errors are isolated, not commit voters.
    fn on_change(&self, change: &Change) -> Result<(), ProviderError>;
    /// Opt into one callback per batch instead of per-change isolation.
    fn handles_change_batches(&self) -> bool {
        false
    }
    /// Observe one published batch in order.
    fn on_changes(&self, changes: &[Change]) -> Result<(), ProviderError> {
        for change in changes {
            self.on_change(change)?;
        }
        Ok(())
    }
    /// Explicitly rebuild derived state from a pinned graph when supported.
    fn rebuild_from_graph(&self, _graph: &SeleneGraph) -> Result<(), ProviderError> {
        Err(ProviderError::Inconsistent {
            reason: format!(
                "provider {} does not support graph rebuild",
                self.provider_tag()
            ),
        })
    }
    /// Mark a generation applied only after successful, non-panicking fanout.
    fn on_commit_applied(&self, _generation: u64) -> Result<(), ProviderError> {
        Ok(())
    }
    /// Bind maintained stable IDs to this graph/layout, rejecting stale state.
    fn node_candidate_set(
        &self,
        _name: &DbString,
        _graph: &SeleneGraph,
    ) -> Result<Option<CandidateSet<Node>>, ProviderError> {
        Ok(None)
    }
    /// Return maintained vector candidates only at the requested generation.
    fn vector_candidate_set(
        &self,
        _name: &DbString,
        _generation: u64,
    ) -> Result<Option<VectorCandidateSet>, ProviderError> {
        Ok(None)
    }
    /// Discover maintained vector state only at the requested generation.
    fn vector_candidate_state_infos(
        &self,
        _generation: u64,
    ) -> Result<Vec<VectorCandidateStateInfo>, ProviderError> {
        Ok(Vec::new())
    }
}

/// Metadata for one provider-owned vector candidate state.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VectorCandidateStateInfo {
    /// Stable set name.
    pub name: DbString,
    /// Applied graph generation.
    pub generation: u64,
    /// Current member count.
    pub candidate_count: usize,
    /// Required node label.
    pub required_label: Option<DbString>,
    /// Required outgoing edge labels.
    pub require_outgoing: Vec<DbString>,
    /// Required incoming edge labels.
    pub require_incoming: Vec<DbString>,
    /// Disqualifying outgoing labels.
    pub exclude_outgoing: Vec<DbString>,
    /// Disqualifying incoming labels.
    pub exclude_incoming: Vec<DbString>,
}

/// Observer-local failure, isolated from already-published graph state.
#[derive(Debug, thiserror::Error, miette::Diagnostic)]
#[non_exhaustive]
pub enum ProviderError {
    /// Invalid observer input.
    #[error("invalid provider payload: {reason}")]
    #[diagnostic(code(SLENE_G_010))]
    InvalidPayload {
        /// Failure detail.
        reason: String,
    },
    /// Inconsistent registration or derived state.
    #[error("provider state inconsistency: {reason}")]
    #[diagnostic(code(SLENE_G_014))]
    Inconsistent {
        /// Failure detail.
        reason: String,
    },
}

#[cfg(test)]
mod tests {
    use super::*;
    use parking_lot::Mutex;
    struct RecordingProvider(Mutex<Vec<Change>>);
    impl IndexProvider for RecordingProvider {
        fn provider_tag(&self) -> ProviderTag {
            ProviderTag(*b"TEST")
        }
        fn on_change(&self, change: &Change) -> Result<(), ProviderError> {
            self.0.lock().push(change.clone());
            Ok(())
        }
    }
    #[test]
    fn provider_tag_equality_and_ordering() {
        let demo = ProviderTag(*b"DEMO");
        assert_eq!(demo, ProviderTag(*b"DEMO"));
        assert!(demo < ProviderTag(*b"META"));
        assert_eq!(demo.to_string(), "DEMO");
        assert_eq!(ProviderTag([0, 1, 2, 3]).to_string(), "0x00010203");
    }
    #[test]
    fn provider_error_gqlstatus_mappings() {
        for error in [
            ProviderError::InvalidPayload {
                reason: "bad".into(),
            },
            ProviderError::Inconsistent {
                reason: "duplicate".into(),
            },
        ] {
            assert_eq!(crate::GraphError::Provider(error).gqlstatus(), "5GQL0");
        }
    }
    #[test]
    fn dummy_provider_with_interior_mutability() {
        fn send_sync_static<T: Send + Sync + 'static>() {}
        send_sync_static::<RecordingProvider>();
        let provider = RecordingProvider(Mutex::new(Vec::new()));
        provider
            .on_change(&Change::NodeCreated {
                id: selene_core::NodeId::new(1),
                labels: selene_core::LabelSet::new(),
                properties: selene_core::PropertyMap::new(),
            })
            .unwrap();
        assert_eq!(provider.0.lock().len(), 1);
        assert_eq!(provider.provider_tag(), ProviderTag(*b"TEST"));
    }
}
