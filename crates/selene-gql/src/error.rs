//! Parser error types and GQLSTATUS mappings.

use std::fmt;

use selene_profile::FeatureId;

use crate::ast::span::SourceSpan;

/// Five-character ISO GQLSTATUS code.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct GqlStatus([u8; 5]);

impl GqlStatus {
    /// Maps to GQLSTATUS 00000: successful completion with a regular result.
    pub const SUCCESSFUL_COMPLETION: Self = Self(*b"00000");
    /// Maps to GQLSTATUS 00001 per ISO/IEC 39075:2024 section 23.1 Table 8:
    /// the completion condition of a successful outcome with an omitted
    /// result (section 4.9.3), which every catalog-modifying statement
    /// produces (section 12.1 GR2).
    pub const SUCCESSFUL_COMPLETION_OMITTED_RESULT: Self = Self(*b"00001");
    /// Maps to GQLSTATUS 01G03 per ISO/IEC 39075:2024 section 23.1 Table 8:
    /// the `DROP GRAPH IF EXISTS` completion condition when the graph is
    /// absent (section 12.5 GR1).
    pub const GRAPH_DOES_NOT_EXIST: Self = Self(*b"01G03");
    /// Maps to GQLSTATUS 02000: successful completion with no data.
    pub const NO_DATA: Self = Self(*b"02000");
    /// Maps to GQLSTATUS 42000 per ISO/IEC 39075:2024 section 23.1 Table 8:
    /// the class-level "syntax error or access rule violation" condition.
    /// ISO leaves access rules to the implementation (IE005).
    pub const SYNTAX_ERROR_OR_ACCESS_RULE_VIOLATION: Self = Self(*b"42000");
    /// Maps to GQLSTATUS 42001 per ISO/IEC 39075:2024 section 23.1 Table 8.
    pub const SYNTAX_ERROR: Self = Self(*b"42001");
    /// Maps to GQLSTATUS 42N01, a selene-db implementation-defined subclass
    /// under standard class 42 per ISO/IEC 39075:2024 section 23.1.
    pub const FEATURE_NOT_SUPPORTED: Self = Self(*b"42N01");
    /// Maps to GQLSTATUS 5GQL1, a selene-db implementation-defined class per
    /// ISO/IEC 39075:2024 section 23.1.
    pub const PROGRAM_LIMIT_EXCEEDED: Self = Self(*b"5GQL1");
    /// Maps to GQLSTATUS 5GQL2, a selene-db implementation-defined class per
    /// ISO/IEC 39075:2024 section 23.1.
    pub const OPERATION_CANCELLED: Self = Self(*b"5GQL2");
    /// Maps to GQLSTATUS 5GQL3, a selene-db implementation-defined class per
    /// ISO/IEC 39075:2024 section 23.1.
    pub const DEADLINE_EXCEEDED: Self = Self(*b"5GQL3");
    /// Maps to GQLSTATUS 42N03, a selene-db implementation-defined subclass
    /// under standard class 42 per ISO/IEC 39075:2024 section 23.1.
    pub const UNDEFINED_REFERENCE: Self = Self(*b"42N03");
    /// Maps to GQLSTATUS 42002 per ISO/IEC 39075:2024 section 23.1 Table 8.
    pub const INVALID_REFERENCE: Self = Self(*b"42002");
    /// Maps to GQLSTATUS 42N10, a selene-db implementation-defined subclass
    /// under standard class 42 per ISO/IEC 39075:2024 section 23.1.
    pub const DUPLICATE_OBJECT: Self = Self(*b"42N10");
    /// Maps to GQLSTATUS 42012 per ISO/IEC 39075:2024 section 23.1 Table 8
    /// ("number of node type key labels below supported minimum"). Raised when
    /// the cardinality of an explicit `<node type key label set>` effective key
    /// label set is below the implementation-defined (IL003) node-type key
    /// label set minimum cardinality (§18.2 SR10).
    pub const NODE_TYPE_KEY_LABELS_BELOW_MINIMUM: Self = Self(*b"42012");
    /// Maps to GQLSTATUS 42013 per ISO/IEC 39075:2024 section 23.1 Table 8
    /// ("number of node type key labels exceeds supported maximum"). Raised when
    /// the cardinality of an explicit `<node type key label set>` effective key
    /// label set exceeds the implementation-defined (IL003) node-type key label
    /// set maximum cardinality (§18.2 SR11).
    pub const NODE_TYPE_KEY_LABELS_EXCEED_MAXIMUM: Self = Self(*b"42013");
    /// Maps to GQLSTATUS 42014 per ISO/IEC 39075:2024 section 23.1 Table 8
    /// ("number of edge type key labels below supported minimum"). Raised when
    /// the cardinality of an explicit `<edge type key label set>` effective key
    /// label set is below the implementation-defined (IL003) edge-type key label
    /// set minimum cardinality (§18.3 SR11).
    pub const EDGE_TYPE_KEY_LABELS_BELOW_MINIMUM: Self = Self(*b"42014");
    /// Maps to GQLSTATUS 42015 per ISO/IEC 39075:2024 section 23.1 Table 8
    /// ("number of edge type key labels exceeds supported maximum"). Raised when
    /// the cardinality of an explicit `<edge type key label set>` effective key
    /// label set exceeds the implementation-defined (IL003) edge-type key label
    /// set maximum cardinality (§18.3 SR12).
    pub const EDGE_TYPE_KEY_LABELS_EXCEED_MAXIMUM: Self = Self(*b"42015");
    /// Maps to GQLSTATUS 22G03 per ISO/IEC 39075:2024 section 23.1 Table 8.
    pub const DATATYPE_MISMATCH: Self = Self(*b"22G03");
    /// Maps to GQLSTATUS 22000 per ISO/IEC 39075:2024 section 23.1 Table 8.
    pub const DATA_EXCEPTION: Self = Self(*b"22000");
    /// Maps to GQLSTATUS 22003 per ISO/IEC 39075:2024 section 23.1 Table 8.
    pub const NUMERIC_VALUE_OUT_OF_RANGE: Self = Self(*b"22003");
    /// Maps to GQLSTATUS 22004 per ISO/IEC 39075:2024 section 23.1 Table 8.
    pub const NULL_VALUE_NOT_ALLOWED: Self = Self(*b"22004");
    /// Maps to GQLSTATUS 22007 per ISO/IEC 39075:2024 section 23.1 Table 8.
    pub const INVALID_DATETIME_FORMAT: Self = Self(*b"22007");
    /// Maps to GQLSTATUS 22011 per ISO/IEC 39075:2024 section 23.1 Table 8.
    pub const SUBSTRING_ERROR: Self = Self(*b"22011");
    /// Maps to GQLSTATUS 22012 per ISO/IEC 39075:2024 section 23.1 Table 8.
    pub const DIVISION_BY_ZERO: Self = Self(*b"22012");
    /// Maps to GQLSTATUS 22018 per ISO/IEC 39075:2024 section 23.1 Table 8.
    pub const INVALID_CHARACTER_VALUE_FOR_CAST: Self = Self(*b"22018");
    /// Maps to GQLSTATUS 22001 per ISO/IEC 39075:2024 section 23.1 Table 8.
    pub const STRING_DATA_RIGHT_TRUNCATION: Self = Self(*b"22001");
    /// Maps to GQLSTATUS 22009 per ISO/IEC 39075:2024 section 23.1 Table 8
    /// (data exception — invalid time zone displacement value).
    pub const INVALID_TIME_ZONE: Self = Self(*b"22009");
    /// Maps to GQLSTATUS 2201E per ISO/IEC 39075:2024 section 23.1 Table 8.
    pub const INVALID_ARGUMENT_FOR_NATURAL_LOGARITHM: Self = Self(*b"2201E");
    /// Maps to GQLSTATUS 2201F per ISO/IEC 39075:2024 section 23.1 Table 8.
    pub const INVALID_ARGUMENT_FOR_POWER_FUNCTION: Self = Self(*b"2201F");
    /// Maps to GQLSTATUS 22027 per ISO/IEC 39075:2024 section 23.1 Table 8.
    pub const TRIM_ERROR: Self = Self(*b"22027");
    /// Maps to GQLSTATUS 22G02 per ISO/IEC 39075:2024 section 23.1 Table 8.
    pub const NEGATIVE_LIMIT_VALUE: Self = Self(*b"22G02");
    /// Maps to GQLSTATUS 22G04 per ISO/IEC 39075:2024 section 23.1 Table 8.
    pub const VALUES_NOT_COMPARABLE: Self = Self(*b"22G04");
    /// Maps to GQLSTATUS 22G05 per ISO/IEC 39075:2024 section 23.1 Table 8.
    pub const INVALID_DATETIME_FUNCTION_FIELD_NAME: Self = Self(*b"22G05");
    /// Maps to GQLSTATUS 22G06 per ISO/IEC 39075:2024 section 23.1 Table 8.
    pub const INVALID_DATETIME_FUNCTION_VALUE: Self = Self(*b"22G06");
    /// Maps to GQLSTATUS 22G07 per ISO/IEC 39075:2024 section 23.1 Table 8.
    pub const INVALID_DURATION_FUNCTION_FIELD_NAME: Self = Self(*b"22G07");
    /// Maps to GQLSTATUS 22G0B per ISO/IEC 39075:2024 section 23.1 Table 8.
    pub const LIST_DATA_RIGHT_TRUNCATION: Self = Self(*b"22G0B");
    /// Maps to GQLSTATUS 22G0C per ISO/IEC 39075:2024 section 23.1 Table 8.
    pub const LIST_ELEMENT_ERROR: Self = Self(*b"22G0C");
    /// Maps to GQLSTATUS 22G0F per ISO/IEC 39075:2024 section 23.1 Table 8
    /// (data exception — invalid number of paths or groups). Raised when a
    /// counted shortest path/group count (§16.6, §22.4 GR7) is not a positive
    /// integer.
    pub const INVALID_NUMBER_OF_PATHS_OR_GROUPS: Self = Self(*b"22G0F");
    /// Maps to GQLSTATUS 22G0H per ISO/IEC 39075:2024 section 23.1 Table 8.
    pub const INVALID_DURATION_FORMAT: Self = Self(*b"22G0H");
    /// Maps to GQLSTATUS 22G10 per ISO/IEC 39075:2024 section 23.1 Table 8.
    pub const PATH_DATA_RIGHT_TRUNCATION: Self = Self(*b"22G10");
    /// Access to the referent of a deleted reference (ISO section 4.4.4).
    pub const INVALID_REFERENCE_VALUE: Self = Self(*b"22G11");
    /// Maps to GQLSTATUS 22G14 per ISO/IEC 39075:2024 section 23.1 Table 8.
    pub const INCOMPATIBLE_TEMPORAL_INSTANT_UNIT_GROUPS: Self = Self(*b"22G14");
    /// Maps to GQLSTATUS 22G0M per ISO/IEC 39075:2024 section 23.1 Table 8.
    pub const MULTIPLE_ASSIGNMENTS_TO_GRAPH_ELEMENT_PROPERTY: Self = Self(*b"22G0M");
    /// Maps to GQLSTATUS 22G0N per ISO/IEC 39075:2024 section 23.1 Table 8.
    pub const NODE_LABELS_BELOW_SUPPORTED_MINIMUM: Self = Self(*b"22G0N");
    /// Maps to GQLSTATUS 22G0P per ISO/IEC 39075:2024 section 23.1 Table 8.
    pub const NODE_LABELS_EXCEED_SUPPORTED_MAXIMUM: Self = Self(*b"22G0P");
    /// Maps to GQLSTATUS 22G0Q per ISO/IEC 39075:2024 section 23.1 Table 8.
    pub const EDGE_LABELS_BELOW_SUPPORTED_MINIMUM: Self = Self(*b"22G0Q");
    /// Maps to GQLSTATUS 22G0R per ISO/IEC 39075:2024 section 23.1 Table 8.
    pub const EDGE_LABELS_EXCEED_SUPPORTED_MAXIMUM: Self = Self(*b"22G0R");
    /// Maps to GQLSTATUS 22G0S per ISO/IEC 39075:2024 section 23.1 Table 8.
    pub const NODE_PROPERTIES_EXCEED_SUPPORTED_MAXIMUM: Self = Self(*b"22G0S");
    /// Maps to GQLSTATUS 22G0T per ISO/IEC 39075:2024 section 23.1 Table 8.
    pub const EDGE_PROPERTIES_EXCEED_SUPPORTED_MAXIMUM: Self = Self(*b"22G0T");
    /// Maps to GQLSTATUS 22G0U per ISO/IEC 39075:2024 section 23.1 Table 8.
    pub const RECORD_FIELDS_DO_NOT_MATCH: Self = Self(*b"22G0U");
    /// Maps to GQLSTATUS 22G0X per ISO/IEC 39075:2024 section 23.1 Table 8.
    pub const RECORD_DATA_FIELD_UNASSIGNABLE: Self = Self(*b"22G0X");
    /// Maps to GQLSTATUS 22G0Z per ISO/IEC 39075:2024 section 23.1 Table 8.
    pub const MALFORMED_PATH: Self = Self(*b"22G0Z");
    /// Maps to GQLSTATUS 25000 per ISO/IEC 39075:2024 section 23.1 Table 8.
    pub const INVALID_TRANSACTION_STATE: Self = Self(*b"25000");
    /// Maps to GQLSTATUS 25G01 per ISO/IEC 39075:2024 section 23.1 Table 8.
    pub const ACTIVE_TRANSACTION: Self = Self(*b"25G01");
    /// Maps to GQLSTATUS 25G02 per ISO/IEC 39075:2024 section 23.1 Table 8.
    pub const INVALID_TRANSACTION_STATE_MIXING: Self = Self(*b"25G02");
    /// Maps to GQLSTATUS 25G03 per ISO/IEC 39075:2024 section 23.1 Table 8.
    pub const READ_ONLY_TRANSACTION_VIOLATION: Self = Self(*b"25G03");
    /// ISO §9.1 GR1d: accessing multiple graphs is not supported (GT03).
    pub const MULTIPLE_GRAPHS_NOT_SUPPORTED: Self = Self(*b"25G04");
    /// Maps to GQLSTATUS 25N02, a selene-db implementation-defined subclass
    /// under standard class 25 per ISO/IEC 39075:2024 section 23.1.
    pub const IN_FAILED_TRANSACTION: Self = Self(*b"25N02");
    /// Maps to GQLSTATUS 2D000 per ISO/IEC 39075:2024 section 23.1 Table 8.
    pub const INVALID_TRANSACTION_TERMINATION: Self = Self(*b"2D000");
    /// Maps to GQLSTATUS 2DN01, a selene-db implementation-defined subclass
    /// under standard class 2D (invalid transaction/session termination) per
    /// ISO/IEC 39075:2024 section 23.1. Raised when a GQL-request is issued
    /// against a session already closed by `SESSION CLOSE` (section 7.3).
    pub const SESSION_CLOSED: Self = Self(*b"2DN01");
    /// Maps to GQLSTATUS 01G11 per ISO/IEC 39075:2024 section 23.1 Table 8.
    pub const NULL_VALUE_ELIMINATED_IN_SET_FUNCTION: Self = Self(*b"01G11");
    /// Maps to GQLSTATUS 01N01, a selene-db implementation-defined subclass
    /// under standard warning class 01 per ISO/IEC 39075:2024 section 23.1.
    pub const VALIDATION_MODE_RELAXED_WRITE: Self = Self(*b"01N01");
    /// Maps to GQLSTATUS 42N04, a selene-db implementation-defined subclass
    /// under standard class 42 per ISO/IEC 39075:2024 section 23.1.
    pub const UNKNOWN_PROCEDURE: Self = Self(*b"42N04");
    /// Maps to GQLSTATUS 22G03 per ISO/IEC 39075:2024 section 23.1 Table 8.
    pub const INVALID_PROCEDURE_ARGUMENT: Self = Self(*b"22G03");
    /// Maps to GQLSTATUS 42N28, a selene-db implementation-defined subclass
    /// under standard class 42 per ISO/IEC 39075:2024 section 23.1.
    pub const CAPABILITY_VIOLATION: Self = Self(*b"42N28");
    /// Maps to GQLSTATUS 5GQL0, a selene-db implementation-defined class per
    /// ISO/IEC 39075:2024 section 23.1. Specific executor diagnostics carry
    /// detail tags under this single public class.
    pub const IMPLEMENTATION_DEFINED_ERROR: Self = Self(*b"5GQL0");
    /// Maps to GQLSTATUS G1000 per ISO/IEC 39075:2024 section 23.1 Table 8:
    /// the class-level "dependent object error". selene-db reports it when a
    /// RESTRICT drop finds live contents or dependents (a nonempty graph or
    /// schema, or a referenced graph type). It is deliberately not `G1001`,
    /// whose subclass names "edges still exist".
    pub const DEPENDENT_OBJECT_ERROR: Self = Self(*b"G1000");
    /// Maps to GQLSTATUS G1001 per ISO/IEC 39075:2024 section 23.1 Table 8.
    pub const DEPENDENT_OBJECT_STILL_EXISTS: Self = Self(*b"G1001");
    /// Maps to GQLSTATUS G2000 per ISO/IEC 39075:2024 section 23.1 Table 8.
    pub const GRAPH_TYPE_VIOLATION: Self = Self(*b"G2000");

    /// Return this status as its 5-character string form.
    #[must_use]
    pub fn as_str(&self) -> &str {
        std::str::from_utf8(&self.0).unwrap_or("5GQL0")
    }

    /// Return this status's two-character class code.
    #[must_use]
    pub const fn class(&self) -> [u8; 2] {
        [self.0[0], self.0[1]]
    }

    /// Build a status from a 5-character code already emitted by a lower crate.
    #[must_use]
    pub fn from_code(code: &str) -> Option<Self> {
        let bytes: [u8; 5] = code.as_bytes().try_into().ok()?;
        bytes.iter().all(u8::is_ascii).then_some(Self(bytes))
    }
}

impl fmt::Display for GqlStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// GQL parser and flagger error.
#[derive(Debug, thiserror::Error, miette::Diagnostic)]
#[non_exhaustive]
pub enum ParserError {
    /// Source text did not parse as supported GQL syntax.
    #[error("{message}")]
    #[diagnostic(code(SLENE_GQL_42001))]
    SyntaxError {
        /// ISO GQLSTATUS code.
        status: GqlStatus,
        /// Human-readable diagnostic message.
        message: String,
        /// Source span for the parse failure.
        #[label("here")]
        span: SourceSpan,
        /// Optional repair hint.
        #[help]
        hint: Option<String>,
    },

    /// Parsed syntax requires a feature outside the current support set.
    #[error("feature not supported: {} ({display_name})", feature_id.as_str())]
    #[diagnostic(code(SLENE_GQL_42N01))]
    UnsupportedFeature {
        /// ISO feature identifier.
        feature_id: FeatureId,
        /// Human-readable feature name.
        display_name: &'static str,
        /// Source span requiring the feature.
        #[label("requires feature")]
        span: SourceSpan,
        /// Static hint for enabling or avoiding the feature.
        #[help]
        hint: &'static str,
    },

    /// Query nesting exceeded the parser's recursion cap.
    #[error("parser nesting limit exceeded ({limit})")]
    #[diagnostic(
        code(SLENE_GQL_5GQL1),
        help("queries are bounded to 64 nested grouping, list, and record delimiters")
    )]
    NestingLimitExceeded {
        /// Maximum admitted syntactic nesting depth.
        limit: u32,
        /// Source span that crossed the limit.
        #[label("exceeds parser nesting limit")]
        span: SourceSpan,
    },

    /// Query exceeded a parser complexity cap that bounds pest's recursive
    /// descent before it begins.
    ///
    /// Maps to GQLSTATUS 5GQL1 (PROGRAM_LIMIT_EXCEEDED), a selene-db
    /// implementation-defined class per ISO/IEC 39075:2024 section 23.1 (see
    /// [`GqlStatus::PROGRAM_LIMIT_EXCEEDED`]).
    ///
    /// Distinct from [`Self::NestingLimitExceeded`], which bounds *all-bracket*
    /// net delimiter nesting depth at a looser cap. This variant covers the
    /// parser's complexity guards, each of which would otherwise drive
    /// pathological pest behavior or overflow the native stack on a small
    /// hostile input:
    ///
    /// - **`[`-depth** (pre-pest byte scan). pest is not packrat-memoized, so a
    ///   run of *unclosed* `[` nests the three ambiguous `[`-prefixed grammar
    ///   rules and recomputes their failed branches at every level, driving
    ///   super-linear backtracking. Balanced, promptly closed `[` (an edge
    ///   pattern, a flat list) never accrue depth, so legitimate wide paths and
    ///   lists are unaffected.
    /// - **Zero-delimiter recursion depth** (pre-pest byte scan). A long run of
    ///   leading unary signs (`unary`), `NOT` keywords (`not_expr`), or nested
    ///   `CASE … END` expressions (`case_expr`) recurses pest's descent one stack
    ///   frame per level with no `(`/`[`/`{` delimiter to bound it, overflowing
    ///   the native stack (a non-unwindable crash) on a small input. Signs and
    ///   `NOT` are bounded as runs; `CASE` is bounded by a *monotone*
    ///   over-approximation — a real opener (counted only outside identifier
    ///   positions: property names, map keys, aliases, `YIELD` columns, params)
    ///   adds 1 plus its wrapping run and never decrements, because an identifier
    ///   `END` cannot be soundly distinguished from the keyword closer. The
    ///   conformant cost is that a statement whose combined `CASE`-count and
    ///   nesting pressure exceeds the ceiling (256) is rejected.
    /// - **Expression nesting depth** (post-build, iterative scan). A flat
    ///   left-associative operator fold (`a OR a OR …`) or postfix chain
    ///   (`a.b.c.…`) parses and builds iteratively but yields a depth-N
    ///   `Box<ValueExpr>` tree whose *recursive* consumers (the Flagger,
    ///   `Drop`, the analyzer) overflow the native stack at ~130k deep. The
    ///   parser rejects any expression deeper than the shared recursion ceiling
    ///   (256) before the Flagger walks it.
    ///
    /// The pre-pest guards reject before recursive descent begins; the
    /// expression-depth guard runs after AST construction and before the
    /// Flagger. All are deterministic and cheap.
    #[error("parser complexity limit exceeded (limit {limit})")]
    #[diagnostic(
        code(SLENE_GQL_5GQL1),
        help(
            "queries are bounded in `[` (list, index, comprehension) nesting depth, in \
             zero-delimiter recursion depth (consecutive unary signs or `NOT` keywords, or nested \
             `CASE … END` expressions), and in expression nesting depth (deep operator folds or \
             access chains); deep nesting of any of these drives pathological parser recursion or \
             overflows the native stack"
        )
    )]
    ComplexityLimitExceeded {
        /// Maximum admitted depth for the complexity dimension that was exceeded.
        limit: u32,
        /// Source span of the token that crossed the limit.
        #[label("exceeds parser complexity limit")]
        span: SourceSpan,
    },

    /// Source parsed at the grammar level, but no AST builder is implemented yet.
    ///
    /// Distinct from [`Self::SyntaxError`] (parse failed) and
    /// [`Self::UnsupportedFeature`] (specific capability rejected by the
    /// generated Flagger disposition).
    /// This variant covers grammar surfaces selene-db will support but whose
    /// builders land in a later brief.
    #[error("not implemented: {message}")]
    #[diagnostic(code(SLENE_GQL_42N01))]
    NotImplemented {
        /// Human-readable description of the missing capability.
        message: String,
        /// Source span requiring the missing capability.
        #[label("not implemented yet")]
        span: SourceSpan,
        /// Pointer to the brief or milestone that lands the capability.
        #[help]
        hint: Option<String>,
    },
}

impl ParserError {
    /// Map this error to its 5-character ISO GQLSTATUS code.
    #[must_use]
    pub const fn gqlstatus(&self) -> GqlStatus {
        match self {
            Self::SyntaxError { status, .. } => *status,
            Self::UnsupportedFeature { .. } | Self::NotImplemented { .. } => {
                GqlStatus::FEATURE_NOT_SUPPORTED
            }
            Self::NestingLimitExceeded { .. } | Self::ComplexityLimitExceeded { .. } => {
                GqlStatus::PROGRAM_LIMIT_EXCEEDED
            }
        }
    }

    pub(crate) fn syntax(
        message: impl Into<String>,
        span: SourceSpan,
        hint: Option<String>,
    ) -> Self {
        Self::SyntaxError {
            status: GqlStatus::SYNTAX_ERROR,
            message: message.into(),
            span,
            hint,
        }
    }

    /// Build a [`Self::SyntaxError`] carrying an explicit GQLSTATUS code rather
    /// than the default `42001`. Used by builder-level static validations whose
    /// ISO diagnostic is a specific data-exception subclass (e.g. counted
    /// shortest path/group count `22G0F`, §22.4 GR7).
    pub(crate) fn syntax_with_status(
        status: GqlStatus,
        message: impl Into<String>,
        span: SourceSpan,
        hint: Option<String>,
    ) -> Self {
        Self::SyntaxError {
            status,
            message: message.into(),
            span,
            hint,
        }
    }

    pub(crate) fn not_implemented(
        message: impl Into<String>,
        span: SourceSpan,
        hint: Option<&'static str>,
    ) -> Self {
        Self::NotImplemented {
            message: message.into(),
            span,
            hint: hint.map(str::to_owned),
        }
    }

    pub(crate) fn empty_program() -> Self {
        Self::syntax(
            "empty GQL program",
            SourceSpan::default(),
            Some("provide a RETURN statement".into()),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::GqlStatus;

    #[test]
    fn gqlstatus_codes_match_iso_table_8_remap() {
        let cases = [
            (
                GqlStatus::SUCCESSFUL_COMPLETION_OMITTED_RESULT,
                "00001",
                *b"00",
            ),
            (GqlStatus::GRAPH_DOES_NOT_EXIST, "01G03", *b"01"),
            (
                GqlStatus::SYNTAX_ERROR_OR_ACCESS_RULE_VIOLATION,
                "42000",
                *b"42",
            ),
            (GqlStatus::SYNTAX_ERROR, "42001", *b"42"),
            (GqlStatus::FEATURE_NOT_SUPPORTED, "42N01", *b"42"),
            (GqlStatus::PROGRAM_LIMIT_EXCEEDED, "5GQL1", *b"5G"),
            (GqlStatus::OPERATION_CANCELLED, "5GQL2", *b"5G"),
            (GqlStatus::DEADLINE_EXCEEDED, "5GQL3", *b"5G"),
            (GqlStatus::UNDEFINED_REFERENCE, "42N03", *b"42"),
            (GqlStatus::INVALID_REFERENCE, "42002", *b"42"),
            (GqlStatus::DUPLICATE_OBJECT, "42N10", *b"42"),
            (
                GqlStatus::NODE_TYPE_KEY_LABELS_BELOW_MINIMUM,
                "42012",
                *b"42",
            ),
            (
                GqlStatus::NODE_TYPE_KEY_LABELS_EXCEED_MAXIMUM,
                "42013",
                *b"42",
            ),
            (
                GqlStatus::EDGE_TYPE_KEY_LABELS_BELOW_MINIMUM,
                "42014",
                *b"42",
            ),
            (
                GqlStatus::EDGE_TYPE_KEY_LABELS_EXCEED_MAXIMUM,
                "42015",
                *b"42",
            ),
            (GqlStatus::DATATYPE_MISMATCH, "22G03", *b"22"),
            (GqlStatus::DATA_EXCEPTION, "22000", *b"22"),
            (GqlStatus::NUMERIC_VALUE_OUT_OF_RANGE, "22003", *b"22"),
            (GqlStatus::NULL_VALUE_NOT_ALLOWED, "22004", *b"22"),
            (GqlStatus::INVALID_DATETIME_FORMAT, "22007", *b"22"),
            (GqlStatus::SUBSTRING_ERROR, "22011", *b"22"),
            (GqlStatus::DIVISION_BY_ZERO, "22012", *b"22"),
            (GqlStatus::INVALID_CHARACTER_VALUE_FOR_CAST, "22018", *b"22"),
            (GqlStatus::STRING_DATA_RIGHT_TRUNCATION, "22001", *b"22"),
            (GqlStatus::INVALID_TIME_ZONE, "22009", *b"22"),
            (
                GqlStatus::INVALID_ARGUMENT_FOR_NATURAL_LOGARITHM,
                "2201E",
                *b"22",
            ),
            (
                GqlStatus::INVALID_ARGUMENT_FOR_POWER_FUNCTION,
                "2201F",
                *b"22",
            ),
            (GqlStatus::TRIM_ERROR, "22027", *b"22"),
            (GqlStatus::NEGATIVE_LIMIT_VALUE, "22G02", *b"22"),
            (GqlStatus::VALUES_NOT_COMPARABLE, "22G04", *b"22"),
            (
                GqlStatus::INVALID_DATETIME_FUNCTION_FIELD_NAME,
                "22G05",
                *b"22",
            ),
            (GqlStatus::INVALID_DATETIME_FUNCTION_VALUE, "22G06", *b"22"),
            (
                GqlStatus::INVALID_DURATION_FUNCTION_FIELD_NAME,
                "22G07",
                *b"22",
            ),
            (GqlStatus::LIST_DATA_RIGHT_TRUNCATION, "22G0B", *b"22"),
            (GqlStatus::LIST_ELEMENT_ERROR, "22G0C", *b"22"),
            (
                GqlStatus::INVALID_NUMBER_OF_PATHS_OR_GROUPS,
                "22G0F",
                *b"22",
            ),
            (GqlStatus::INVALID_DURATION_FORMAT, "22G0H", *b"22"),
            (GqlStatus::PATH_DATA_RIGHT_TRUNCATION, "22G10", *b"22"),
            (
                GqlStatus::INCOMPATIBLE_TEMPORAL_INSTANT_UNIT_GROUPS,
                "22G14",
                *b"22",
            ),
            (
                GqlStatus::MULTIPLE_ASSIGNMENTS_TO_GRAPH_ELEMENT_PROPERTY,
                "22G0M",
                *b"22",
            ),
            (
                GqlStatus::NODE_LABELS_BELOW_SUPPORTED_MINIMUM,
                "22G0N",
                *b"22",
            ),
            (
                GqlStatus::NODE_LABELS_EXCEED_SUPPORTED_MAXIMUM,
                "22G0P",
                *b"22",
            ),
            (
                GqlStatus::EDGE_LABELS_BELOW_SUPPORTED_MINIMUM,
                "22G0Q",
                *b"22",
            ),
            (
                GqlStatus::EDGE_LABELS_EXCEED_SUPPORTED_MAXIMUM,
                "22G0R",
                *b"22",
            ),
            (
                GqlStatus::NODE_PROPERTIES_EXCEED_SUPPORTED_MAXIMUM,
                "22G0S",
                *b"22",
            ),
            (
                GqlStatus::EDGE_PROPERTIES_EXCEED_SUPPORTED_MAXIMUM,
                "22G0T",
                *b"22",
            ),
            (GqlStatus::RECORD_FIELDS_DO_NOT_MATCH, "22G0U", *b"22"),
            (GqlStatus::RECORD_DATA_FIELD_UNASSIGNABLE, "22G0X", *b"22"),
            (GqlStatus::MALFORMED_PATH, "22G0Z", *b"22"),
            (GqlStatus::INVALID_TRANSACTION_STATE, "25000", *b"25"),
            (GqlStatus::ACTIVE_TRANSACTION, "25G01", *b"25"),
            (GqlStatus::INVALID_TRANSACTION_STATE_MIXING, "25G02", *b"25"),
            (GqlStatus::READ_ONLY_TRANSACTION_VIOLATION, "25G03", *b"25"),
            (GqlStatus::IN_FAILED_TRANSACTION, "25N02", *b"25"),
            (GqlStatus::INVALID_TRANSACTION_TERMINATION, "2D000", *b"2D"),
            (GqlStatus::SESSION_CLOSED, "2DN01", *b"2D"),
            (
                GqlStatus::NULL_VALUE_ELIMINATED_IN_SET_FUNCTION,
                "01G11",
                *b"01",
            ),
            (GqlStatus::VALIDATION_MODE_RELAXED_WRITE, "01N01", *b"01"),
            (GqlStatus::UNKNOWN_PROCEDURE, "42N04", *b"42"),
            (GqlStatus::INVALID_PROCEDURE_ARGUMENT, "22G03", *b"22"),
            (GqlStatus::CAPABILITY_VIOLATION, "42N28", *b"42"),
            (GqlStatus::IMPLEMENTATION_DEFINED_ERROR, "5GQL0", *b"5G"),
            (GqlStatus::DEPENDENT_OBJECT_ERROR, "G1000", *b"G1"),
            (GqlStatus::DEPENDENT_OBJECT_STILL_EXISTS, "G1001", *b"G1"),
            (GqlStatus::GRAPH_TYPE_VIOLATION, "G2000", *b"G2"),
        ];

        for (status, code, class) in cases {
            assert_eq!(status.as_str(), code);
            assert_eq!(status.class(), class);
            assert!(
                selene_core::gqlstatus_name(code).is_some(),
                "GQLSTATUS {code} is public in selene-gql but missing from selene-core names"
            );
        }
    }

    #[test]
    fn gqlstatus_from_code_accepts_five_ascii_bytes() {
        assert_eq!(
            GqlStatus::from_code("G2000"),
            Some(GqlStatus::GRAPH_TYPE_VIOLATION)
        );
        assert_eq!(GqlStatus::from_code("2200"), None);
    }
}
