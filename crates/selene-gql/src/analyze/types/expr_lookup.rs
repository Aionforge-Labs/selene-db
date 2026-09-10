//! Temporary deterministic source-expression bridge to semantic identities.

use super::{ExprId, fingerprint::node_fingerprint};
use crate::{GqlType, SourceSpan, ValueExpr};
use std::{
    collections::{BTreeMap, HashMap},
    fmt,
};

/// Lookup from source expression shape to a semantic expression identity.
///
/// Spans distinguish original occurrences; structural fingerprints also
/// distinguish synthesized defaults sharing a call origin. Each fingerprint
/// walk owns a fresh pointer memo whose borrowed nodes stay alive throughout
/// that walk. No pointer key escapes the walk. F03-PR04 deletes this source
/// shape bridge when plan expressions carry semantic IDs directly.
#[derive(Clone, Default)]
pub struct ExprIdLookup {
    ids: HashMap<ExprKey, ExprId>,
    parameter_types: BTreeMap<selene_core::DbString, GqlType>,
}

impl ExprIdLookup {
    pub(crate) fn set_parameter_types(&mut self, parameters: &[crate::ParameterUse]) {
        self.parameter_types = parameters
            .iter()
            .filter_map(|parameter| {
                parameter
                    .declared_type
                    .clone()
                    .map(|ty| (parameter.name.clone(), ty))
            })
            .collect();
    }

    /// Effective statement-wide declaration, separate from source spelling.
    #[must_use]
    pub fn parameter_type(&self, name: &selene_core::DbString) -> Option<&GqlType> {
        self.parameter_types.get(name)
    }

    pub(crate) fn insert(&mut self, expr: &ValueExpr, id: ExprId) {
        self.ids.entry(ExprKey::for_expr(expr)).or_insert(id);
    }

    /// Return the semantic identity allocated for this source expression.
    #[must_use]
    pub fn get(&self, expr: &ValueExpr) -> Option<ExprId> {
        self.ids.get(&ExprKey::for_expr(expr)).copied()
    }

    /// Number of source-expression identities.
    #[must_use]
    pub fn len(&self) -> usize {
        self.ids.len()
    }

    /// Whether no source-expression identities were allocated.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.ids.is_empty()
    }
}

impl fmt::Debug for ExprIdLookup {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut entries = self.ids.iter().collect::<Vec<_>>();
        entries.sort_by_key(|(key, _)| (key.span.byte_offset, key.span.byte_len, key.fingerprint));
        formatter
            .debug_struct("ExprIdLookup")
            .field("ids", &entries)
            .field("parameter_types", &self.parameter_types)
            .finish()
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
struct ExprKey {
    span: SourceSpan,
    fingerprint: u64,
}

impl ExprKey {
    fn for_expr(expr: &ValueExpr) -> Self {
        Self {
            span: expr.span(),
            fingerprint: node_fingerprint(expr, &mut HashMap::new()),
        }
    }
}
