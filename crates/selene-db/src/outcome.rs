//! Stable row-bearing, write, and omitted execution outcomes.

use crate::{DiagnosticBundle, GqlStatus, GqlStatusObject, Type, Value};

#[cfg(test)]
mod path_tests;

/// One analyzer-inferred result field, with explicit inference uncertainty.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum DeclaredType {
    /// The analyzer resolved one concrete GQL type.
    Resolved(Type),
    /// Static inference deliberately retained a dynamic type cell.
    Dynamic,
}

/// One field in a regular result's declared descriptor.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ResultField {
    name: Option<String>,
    declared_type: DeclaredType,
}

impl ResultField {
    /// Borrow the declared field name, when the projection supplied one.
    #[must_use]
    pub fn name(&self) -> Option<&str> {
        self.name.as_deref()
    }

    /// Borrow the analyzer-inferred type.
    #[must_use]
    pub const fn declared_type(&self) -> &DeclaredType {
        &self.declared_type
    }
}

/// Analyzer-declared regular-result descriptor required by profile choice IA001.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ResultDescriptor {
    fields: Vec<ResultField>,
    ordering: Vec<crate::ResultOrderKey>,
    preferred_columns: Vec<usize>,
}

impl ResultDescriptor {
    /// Borrow result fields in column order.
    #[must_use]
    pub fn fields(&self) -> &[ResultField] {
        &self.fields
    }

    /// Declared global result ordering, including resolved null placement.
    #[must_use]
    pub fn ordering(&self) -> &[crate::ResultOrderKey] {
        &self.ordering
    }

    /// Preferred output column order, including for a zero-row result.
    #[must_use]
    pub fn preferred_columns(&self) -> &[usize] {
        &self.preferred_columns
    }

    fn from_engine(descriptor: &selene_gql::BindingTableDescriptor) -> crate::Result<Self> {
        Ok(Self {
            ordering: descriptor.ordering().to_vec(),
            preferred_columns: descriptor.preferred_columns().to_vec(),
            fields: descriptor
                .fields()
                .iter()
                .map(|field| {
                    Ok(ResultField {
                        name: field.name().map(str::to_owned),
                        declared_type: match field.declared_type() {
                            selene_gql::AnalyzedType::Resolved(ty) => DeclaredType::Resolved(
                                selene_gql::normalize_value_type(ty).map_err(|_| {
                                    crate::Error::catalog_invariant(
                                        "unsupported result type descriptor",
                                    )
                                })?,
                            ),
                            selene_gql::AnalyzedType::Dynamic => DeclaredType::Dynamic,
                        },
                    })
                })
                .collect::<crate::Result<_>>()?,
        })
    }
}

/// One immutable regular-result row.
#[derive(Clone, Debug, PartialEq)]
pub struct ResultRow {
    values: Vec<Value>,
}

impl ResultRow {
    /// Borrow values in descriptor-field order.
    #[must_use]
    pub fn values(&self) -> &[Value] {
        &self.values
    }
}

/// One regular result with immutable rows and an analyzer-declared descriptor.
#[derive(Clone, Debug, PartialEq)]
pub struct RegularResult {
    descriptor: ResultDescriptor,
    rows: Vec<ResultRow>,
}

impl RegularResult {
    /// Borrow the IA001 declared descriptor.
    #[must_use]
    pub const fn descriptor(&self) -> &ResultDescriptor {
        &self.descriptor
    }

    /// Borrow all result rows in executor order.
    #[must_use]
    pub fn rows(&self) -> &[ResultRow] {
        &self.rows
    }

    /// Return the number of rows.
    #[must_use]
    pub fn row_count(&self) -> usize {
        self.rows.len()
    }

    fn from_engine(
        table: &selene_gql::BindingTable,
        declared: &selene_gql::BindingTableDescriptor,
        graph: crate::GraphRef,
    ) -> crate::Result<Self> {
        Ok(Self {
            descriptor: ResultDescriptor::from_engine(declared)?,
            rows: table
                .rows()
                .iter()
                .map(|row| {
                    Ok(ResultRow {
                        values: row
                            .values()
                            .iter()
                            .map(|value| Value::from_lower(value, graph))
                            .collect::<crate::Result<_>>()?,
                    })
                })
                .collect::<crate::Result<_>>()?,
        })
    }
}

/// Structured facade result for one successful GQL request.
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub enum ExecutionOutcome {
    /// A successful regular result with rows and a declared descriptor.
    Rows {
        /// Immutable row values and declared result descriptor.
        result: RegularResult,
        /// Completion, no-data, and warning statuses.
        diagnostics: DiagnosticBundle,
    },
    /// A successful outcome whose regular result is omitted.
    OmittedResult {
        /// Completion and warning statuses. No empty descriptor is fabricated.
        diagnostics: DiagnosticBundle,
    },
    /// A successful graph write with optional returned rows.
    Written {
        /// Committed change summary.
        summary: WriteSummary,
        /// Returned regular result for a write with `RETURN`.
        result: Option<RegularResult>,
        /// Completion, no-data, and warning statuses.
        diagnostics: DiagnosticBundle,
    },
}

impl ExecutionOutcome {
    /// Canonical successful omitted outcome used by catalog modifications.
    pub const SUCCESSFUL_OMITTED: Self = Self::OmittedResult {
        diagnostics: DiagnosticBundle::new(
            GqlStatusObject::static_message(
                GqlStatus::SUCCESSFUL_COMPLETION_OMITTED_RESULT,
                "successful completion with omitted result",
            ),
            Vec::new(),
        ),
    };

    /// Canonical warning outcome for `DROP GRAPH IF EXISTS` on an absent graph.
    pub const GRAPH_NOT_FOUND_OMITTED: Self = Self::OmittedResult {
        diagnostics: DiagnosticBundle::new(
            GqlStatusObject::static_message(
                GqlStatus::GRAPH_DOES_NOT_EXIST,
                "DROP GRAPH IF EXISTS found no graph",
            ),
            Vec::new(),
        ),
    };

    pub(crate) fn from_engine(
        output: selene_gql::ExecutionOutcome,
        graph: crate::GraphRef,
    ) -> crate::Result<Self> {
        match output {
            selene_gql::ExecutionOutcome::RegularResult {
                table,
                declared,
                diagnostics,
            } => Ok(Self::Rows {
                result: RegularResult::from_engine(&table, &declared, graph)?,
                diagnostics: DiagnosticBundle::from_engine(&diagnostics),
            }),
            selene_gql::ExecutionOutcome::OmittedResult { diagnostics } => {
                Ok(Self::OmittedResult {
                    diagnostics: DiagnosticBundle::from_engine(&diagnostics),
                })
            }
            selene_gql::ExecutionOutcome::Written {
                write,
                declared,
                diagnostics,
            } => {
                let result = match (write.rows.as_ref(), declared.as_ref()) {
                    (Some(table), Some(declared)) => {
                        Some(RegularResult::from_engine(table, declared, graph)?)
                    }
                    (None, None) => None,
                    _ => return Err(crate::Error::unsupported_engine_outcome()),
                };
                Ok(Self::Written {
                    summary: WriteSummary::new(
                        write.changes.len(),
                        result.as_ref().map(RegularResult::row_count),
                    ),
                    result,
                    diagnostics: DiagnosticBundle::from_engine(&diagnostics),
                })
            }
            selene_gql::ExecutionOutcome::Failed { .. } => {
                Err(crate::Error::unsupported_engine_outcome())
            }
            _ => Err(crate::Error::unsupported_engine_outcome()),
        }
    }

    /// Borrow this outcome's complete diagnostic bundle.
    #[must_use]
    pub const fn diagnostics(&self) -> &DiagnosticBundle {
        match self {
            Self::Rows { diagnostics, .. }
            | Self::OmittedResult { diagnostics }
            | Self::Written { diagnostics, .. } => diagnostics,
        }
    }

    /// Return the regular-result row count, including write-return rows.
    #[must_use]
    pub fn row_count(&self) -> Option<usize> {
        match self {
            Self::Rows { result, .. } => Some(result.row_count()),
            Self::Written { result, .. } => result.as_ref().map(RegularResult::row_count),
            Self::OmittedResult { .. } => None,
        }
    }

    /// Return the committed write summary, when this is a write outcome.
    #[must_use]
    pub const fn write_summary(&self) -> Option<WriteSummary> {
        match self {
            Self::Written { summary, .. } => Some(*summary),
            Self::Rows { .. } | Self::OmittedResult { .. } => None,
        }
    }
}

/// Summary of a staged explicit write or committed implicit write.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct WriteSummary {
    change_count: usize,
    returned_row_count: Option<usize>,
}

impl WriteSummary {
    /// Construct a write summary from change and returned-row counts.
    #[must_use]
    pub const fn new(change_count: usize, returned_row_count: Option<usize>) -> Self {
        Self {
            change_count,
            returned_row_count,
        }
    }

    /// Return the number of committed graph changes.
    #[must_use]
    pub const fn change_count(self) -> usize {
        self.change_count
    }

    /// Return the row count for a write with `RETURN`, when present.
    #[must_use]
    pub const fn returned_row_count(self) -> Option<usize> {
        self.returned_row_count
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lower_omitted_output_does_not_fabricate_a_descriptor() {
        let lower = selene_gql::ExecutionOutcome::successful_omitted();
        let mapped = ExecutionOutcome::from_engine(
            lower,
            crate::GraphRef::new(crate::DatabaseId::from_raw(1), crate::GraphId(1)),
        )
        .unwrap();
        assert!(matches!(mapped, ExecutionOutcome::OmittedResult { .. }));
        assert_eq!(
            mapped.diagnostics().primary().status(),
            GqlStatus::SUCCESSFUL_COMPLETION_OMITTED_RESULT
        );
    }
}
