//! Structured execution outcomes and GQL status-object chains.

use selene_core::DbString;

use crate::{AnalyzedType, GqlStatus};

use super::{BindingTable, BindingTableSchema, StatementOutput, WriteOutcome};

/// One analyzer-declared binding-table field.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BindingTableField {
    name: Option<DbString>,
    declared_type: AnalyzedType,
}

impl BindingTableField {
    /// Borrow the declared field name, when the projection supplied one.
    #[must_use]
    pub fn name(&self) -> Option<&str> {
        self.name.as_ref().map(DbString::as_str)
    }

    /// Borrow the analyzer-inferred field type.
    #[must_use]
    pub const fn declared_type(&self) -> &AnalyzedType {
        &self.declared_type
    }
}

/// Analyzer-declared descriptor exposed separately from runtime row storage.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BindingTableDescriptor {
    fields: Vec<BindingTableField>,
    ordering: Vec<selene_core::ResultOrderKey>,
    preferred_columns: Vec<usize>,
}

impl BindingTableDescriptor {
    /// Copy a typed descriptor from the analyzer/planner schema.
    #[must_use]
    pub fn from_schema(schema: &BindingTableSchema) -> Self {
        Self {
            ordering: Vec::new(),
            preferred_columns: (0..schema.columns.len()).collect(),
            fields: schema
                .columns
                .iter()
                .map(|column| BindingTableField {
                    name: column.name.clone(),
                    declared_type: column.ty.clone(),
                })
                .collect(),
        }
    }

    /// Borrow declared fields in result-column order.
    #[must_use]
    pub fn fields(&self) -> &[BindingTableField] {
        &self.fields
    }

    fn from_table(table: &BindingTable) -> Self {
        let mut descriptor = Self::from_schema(table.schema());
        descriptor.ordering = table.ordering().to_vec();
        descriptor
    }

    /// Declared ordering, not an observation of the current rows.
    #[must_use]
    pub fn ordering(&self) -> &[selene_core::ResultOrderKey] {
        &self.ordering
    }

    /// Preferred presentation order as zero-based descriptor field coordinates.
    #[must_use]
    pub fn preferred_columns(&self) -> &[usize] {
        &self.preferred_columns
    }
}

/// One structured GQL status object, including every produced nested cause.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GqlStatusObject {
    status: GqlStatus,
    message: String,
    causes: Vec<GqlStatusObject>,
}

impl GqlStatusObject {
    /// Construct a status object without nested causes.
    #[must_use]
    pub fn new(status: GqlStatus, message: impl Into<String>) -> Self {
        Self {
            status,
            message: message.into(),
            causes: Vec::new(),
        }
    }

    /// Attach ordered nested causes without truncation.
    #[must_use]
    pub fn with_causes(mut self, causes: Vec<Self>) -> Self {
        self.causes = causes;
        self
    }

    /// Return this object's GQLSTATUS code.
    #[must_use]
    pub const fn status(&self) -> GqlStatus {
        self.status
    }

    /// Borrow the diagnostic message.
    #[must_use]
    pub fn message(&self) -> &str {
        &self.message
    }

    /// Borrow all nested causes in production order.
    #[must_use]
    pub fn causes(&self) -> &[Self] {
        &self.causes
    }
}

/// Primary and ordered additional status objects for one outcome.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DiagnosticBundle {
    primary: GqlStatusObject,
    additional: Vec<GqlStatusObject>,
}

impl DiagnosticBundle {
    /// Construct an explicit primary/additional bundle without truncation.
    #[must_use]
    pub const fn new(primary: GqlStatusObject, additional: Vec<GqlStatusObject>) -> Self {
        Self {
            primary,
            additional,
        }
    }

    /// Borrow the primary status object.
    #[must_use]
    pub const fn primary(&self) -> &GqlStatusObject {
        &self.primary
    }

    /// Borrow every additional status in deterministic production order.
    #[must_use]
    pub fn additional(&self) -> &[GqlStatusObject] {
        &self.additional
    }

    fn successful(base: GqlStatusObject, produced: Vec<GqlStatusObject>) -> Self {
        let mut candidates = Vec::with_capacity(produced.len() + 1);
        candidates.push(base);
        candidates.extend(produced);
        let primary_index = candidates
            .iter()
            .enumerate()
            .max_by_key(|(index, status)| (precedence(status.status()), std::cmp::Reverse(*index)))
            .map_or(0, |(index, _)| index);
        let primary = candidates.remove(primary_index);
        Self::new(primary, candidates)
    }
}

/// Structured result of runtime execution.
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub enum ExecutionOutcome {
    /// A successful regular result with rows and a declared descriptor.
    RegularResult {
        /// Runtime binding-table rows.
        table: BindingTable,
        /// Analyzer-declared descriptor, tracked separately from row storage.
        declared: BindingTableDescriptor,
        /// Completion, no-data, and warning statuses.
        diagnostics: DiagnosticBundle,
    },
    /// A successful outcome for which the regular result is omitted.
    OmittedResult {
        /// Completion and warning statuses; no descriptor is fabricated.
        diagnostics: DiagnosticBundle,
    },
    /// A successful write, optionally carrying a regular returned result.
    Written {
        /// Existing write metadata and optional returned rows.
        write: WriteOutcome,
        /// Declared descriptor for returned rows, when present.
        declared: Option<BindingTableDescriptor>,
        /// Completion, no-data, and warning statuses.
        diagnostics: DiagnosticBundle,
    },
    /// A failed outcome with a complete status-object chain.
    Failed {
        /// Failure statuses in deterministic precedence and production order.
        diagnostics: DiagnosticBundle,
    },
}

impl ExecutionOutcome {
    /// Root-context outcome before an operation supplies a result.
    #[must_use]
    pub fn successful_omitted() -> Self {
        Self::OmittedResult {
            diagnostics: DiagnosticBundle::new(
                GqlStatusObject::new(
                    GqlStatus::SUCCESSFUL_COMPLETION_OMITTED_RESULT,
                    "successful completion with omitted result",
                ),
                Vec::new(),
            ),
        }
    }

    /// Convert the compatibility statement output without dropping rows or types.
    #[must_use]
    pub fn from_statement(output: StatementOutput, statuses: Vec<GqlStatusObject>) -> Self {
        match output {
            StatementOutput::Rows(table) => {
                let base = regular_completion(table.row_count());
                let declared = BindingTableDescriptor::from_table(&table);
                Self::RegularResult {
                    table,
                    declared,
                    diagnostics: DiagnosticBundle::successful(base, statuses),
                }
            }
            StatementOutput::Written(write) => {
                let (base, declared) = write.rows.as_ref().map_or_else(
                    || {
                        (
                            GqlStatusObject::new(
                                GqlStatus::SUCCESSFUL_COMPLETION_OMITTED_RESULT,
                                "successful write with omitted result",
                            ),
                            None,
                        )
                    },
                    |table| {
                        (
                            regular_completion(table.row_count()),
                            Some(BindingTableDescriptor::from_table(table)),
                        )
                    },
                );
                Self::Written {
                    write,
                    declared,
                    diagnostics: DiagnosticBundle::successful(base, statuses),
                }
            }
            StatementOutput::Empty => Self::OmittedResult {
                diagnostics: DiagnosticBundle::successful(
                    GqlStatusObject::new(
                        GqlStatus::SUCCESSFUL_COMPLETION_OMITTED_RESULT,
                        "successful completion with omitted result",
                    ),
                    statuses,
                ),
            },
        }
    }

    /// Construct a failed outcome from primary and ordered additional statuses.
    #[must_use]
    pub const fn failed(diagnostics: DiagnosticBundle) -> Self {
        Self::Failed { diagnostics }
    }

    /// Borrow this outcome's complete diagnostic bundle.
    #[must_use]
    pub const fn diagnostics(&self) -> &DiagnosticBundle {
        match self {
            Self::RegularResult { diagnostics, .. }
            | Self::OmittedResult { diagnostics }
            | Self::Written { diagnostics, .. }
            | Self::Failed { diagnostics } => diagnostics,
        }
    }
}

fn regular_completion(row_count: usize) -> GqlStatusObject {
    if row_count == 0 {
        GqlStatusObject::new(GqlStatus::NO_DATA, "successful completion with no data")
    } else {
        GqlStatusObject::new(
            GqlStatus::SUCCESSFUL_COMPLETION,
            "successful completion with a regular result",
        )
    }
}

fn precedence(status: GqlStatus) -> u8 {
    match status.class() {
        [b'0', b'0'] => 0,
        [b'0', b'2'] => 1,
        [b'0', b'1'] => 2,
        _ => 3,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn warning_precedes_no_data_and_preserves_every_status_and_cause() {
        let nested = GqlStatusObject::new(GqlStatus::DATA_EXCEPTION, "nested");
        let warning =
            GqlStatusObject::new(GqlStatus::NULL_VALUE_ELIMINATED_IN_SET_FUNCTION, "warning")
                .with_causes(vec![nested.clone()]);
        let second = GqlStatusObject::new(GqlStatus::GRAPH_DOES_NOT_EXIST, "second warning");
        let bundle = DiagnosticBundle::successful(
            GqlStatusObject::new(GqlStatus::NO_DATA, "no data"),
            vec![warning.clone(), second.clone()],
        );

        assert_eq!(bundle.primary(), &warning);
        assert_eq!(bundle.primary().causes(), &[nested]);
        assert_eq!(
            bundle.additional(),
            &[GqlStatusObject::new(GqlStatus::NO_DATA, "no data"), second]
        );
    }

    #[test]
    fn failed_bundle_keeps_primary_additional_and_nested_order() {
        let nested = vec![
            GqlStatusObject::new(GqlStatus::INVALID_REFERENCE, "first cause"),
            GqlStatusObject::new(GqlStatus::DATA_EXCEPTION, "second cause"),
        ];
        let primary = GqlStatusObject::new(GqlStatus::IMPLEMENTATION_DEFINED_ERROR, "failure")
            .with_causes(nested.clone());
        let additional = vec![GqlStatusObject::new(
            GqlStatus::OPERATION_CANCELLED,
            "additional",
        )];
        let outcome =
            ExecutionOutcome::failed(DiagnosticBundle::new(primary.clone(), additional.clone()));

        assert_eq!(outcome.diagnostics().primary(), &primary);
        assert_eq!(outcome.diagnostics().primary().causes(), nested);
        assert_eq!(outcome.diagnostics().additional(), additional);
    }
}
