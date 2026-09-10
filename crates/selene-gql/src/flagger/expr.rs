//! Expression Flagger walk.

use selene_core::DbString;
use selene_profile::FeatureId;

use crate::{
    ExistsBody, NonEmpty, ValueExpr,
    ast::{
        expr::{
            BinaryOp, CharacterStringLiteralKind, DecimalLiteralKind, FloatLiteralKind,
            IntegerLiteralKind, IsCheckKind, Literal,
        },
        types::{GqlType, RecordType},
    },
};

use super::{FeatureUse, query, record_feature};

pub(crate) fn value(value: &ValueExpr, uses: &mut Vec<FeatureUse>) {
    // Variants whose feature surface is not a function of their child
    // `ValueExpr`s — leaves, subqueries (bodies are `MatchClause` /
    // `QueryPipeline`, not `ValueExpr` children), and `IS` predicates (whose
    // `kind`-borne feature must be recorded *between* the operand and the
    // `IS [SOURCE|DESTINATION] OF` value, an ordering `for_each_child` cannot
    // express) — keep explicit handling. The early returns mean these arms own
    // their full traversal and must not fall through to the generic child walk.
    match value {
        ValueExpr::Literal(literal_value) => {
            literal(literal_value, uses);
            return;
        }
        ValueExpr::Variable { .. } => return,
        ValueExpr::Parameter {
            declared_type,
            span,
            ..
        } => {
            record_feature(uses, FeatureId::GE04, *span);
            record_feature(uses, FeatureId::GE05, *span);
            if declared_type.is_some() {
                record_feature(uses, FeatureId::IM_TYPED_PARAMS, *span);
            }
            if let Some(ty) = declared_type {
                gql_type(ty, *span, uses);
            }
            return;
        }
        ValueExpr::IsCheck {
            operand,
            kind,
            span,
            ..
        } => {
            // Preserve the original ordering: operand subtree, then the
            // `kind`-borne feature, then the `IS [SOURCE|DESTINATION] OF` value
            // subtree (`is_check` records the feature before recursing).
            self::value(operand, uses);
            is_check(kind, *span, uses);
            return;
        }
        ValueExpr::Exists { body, .. } => {
            match body {
                ExistsBody::Match(pattern) => query::match_clause(pattern, uses),
                ExistsBody::Query(pipeline) => query::query_pipeline(pipeline, uses),
            }
            return;
        }
        ValueExpr::ValueSubquery { body, span } => {
            record_feature(uses, FeatureId::GQ18, *span);
            query::query_pipeline(body, uses);
            return;
        }
        // Variants whose own feature surface is recorded before their direct
        // `ValueExpr` children. Feature emission stays here; child recursion is
        // delegated to `for_each_child` below so the per-variant traversal
        // shape lives in exactly one place (`ast/walk.rs`).
        ValueExpr::ListLiteral { span, .. } => record_feature(uses, FeatureId::GV50, *span),
        ValueExpr::RecordLiteral { span, .. } => record_feature(uses, FeatureId::GV45, *span),
        ValueExpr::PathConstructor { span, .. } => {
            record_feature(uses, FeatureId::GE06, *span);
            record_feature(uses, FeatureId::GV55, *span);
        }
        ValueExpr::BinaryOp { op, span, .. } => {
            if matches!(
                op,
                BinaryOp::Add
                    | BinaryOp::Sub
                    | BinaryOp::Mul
                    | BinaryOp::Div
                    | BinaryOp::Mod
                    | BinaryOp::Power
            ) {
                record_feature(uses, FeatureId::GA01, *span);
            }
            if *op == BinaryOp::Xor {
                record_feature(uses, FeatureId::GE07, *span);
            }
        }
        ValueExpr::FunctionCall {
            name, args, span, ..
        } => {
            if let Some(feature_id) = scalar_function_feature(name) {
                record_feature(uses, feature_id, *span);
            }
            if is_list_value_function(name, args.len()) {
                record_feature(uses, FeatureId::GV50, *span);
            }
            if is_byte_string_trim_function(name, args) {
                record_feature(uses, FeatureId::GF07, *span);
            }
            if let Some(feature_id) = aggregate_function_feature(name) {
                record_feature(uses, feature_id, *span);
            }
        }
        ValueExpr::DurationBetween { span, .. } => record_feature(uses, FeatureId::GV41, *span),
        ValueExpr::Trim {
            character,
            source,
            span,
            ..
        } => {
            record_feature(uses, FeatureId::GF06, *span);
            if is_byte_string_expr(source) || character.as_deref().is_some_and(is_byte_string_expr)
            {
                record_feature(uses, FeatureId::GF07, *span);
            }
        }
        ValueExpr::AllDifferent { span, .. } => record_feature(uses, FeatureId::G113, *span),
        ValueExpr::Same { span, .. } => record_feature(uses, FeatureId::G114, *span),
        ValueExpr::PropertyExists {
            key_source_kind,
            span,
            ..
        } => {
            record_feature(uses, FeatureId::G115, *span);
            character_string_literal(*key_source_kind, *span, uses);
        }
        ValueExpr::Cast {
            target_type, span, ..
        } => {
            // Per ISO/IEC 39075:2024 §20.8 <cast specification>: CAST is the
            // optional feature GA05 "Cast specification" (Annex D Table D.1 row
            // 53), NOT GE08 ("Reference parameters", §17.7). ISO Annex A item 52
            // requires GA05 for any `<cast specification>`. The Cast node records
            // GA05 first, then delegates child recursion to `for_each_child` (via
            // `value_children`), then walks the target type — the type tail must
            // run *after* child recursion to preserve source order.
            record_feature(uses, FeatureId::GA05, *span);
            self::value_children(value, uses);
            gql_type(target_type, *span, uses);
            return;
        }
        // Pure-recursion variants own no feature; they delegate entirely to the
        // generic child walk.
        ValueExpr::PropertyAccess { .. }
        | ValueExpr::UnaryOp { .. }
        | ValueExpr::Normalize { .. }
        | ValueExpr::InList { .. }
        | ValueExpr::InListExpression { .. }
        | ValueExpr::Case { .. } => {}
    }
    self::value_children(value, uses);
}

/// Recurse into every direct child [`ValueExpr`] of `value`, in source order,
/// delegating the per-variant child-enumeration shape to
/// [`ValueExpr::for_each_child`].
fn value_children(value: &ValueExpr, uses: &mut Vec<FeatureUse>) {
    value.for_each_child(&mut |child| self::value(child, uses));
}

fn is_byte_string_expr(value: &ValueExpr) -> bool {
    match value {
        ValueExpr::Literal(Literal::Bytes(_, _)) => true,
        ValueExpr::Cast { target_type, .. } => {
            matches!(
                target_type.as_ref().strip_not_null(),
                GqlType::Bytes | GqlType::ByteString(_)
            )
        }
        ValueExpr::Parameter {
            declared_type: Some(ty),
            ..
        } => matches!(ty.strip_not_null(), GqlType::Bytes | GqlType::ByteString(_)),
        _ => false,
    }
}

fn scalar_function_feature(name: &NonEmpty<DbString>) -> Option<FeatureId> {
    if name.len() != 1 {
        return None;
    }
    match name.first().as_str().to_ascii_lowercase().as_str() {
        "element_id" => Some(FeatureId::G100),
        "abs" | "ceil" | "ceiling" | "floor" | "mod" | "sqrt" => Some(FeatureId::GF01),
        "acos" | "asin" | "atan" | "cos" | "cosh" | "cot" | "degrees" | "radians" | "sin"
        | "sinh" | "tan" | "tanh" => Some(FeatureId::GF02),
        "exp" | "ln" | "log" | "log10" | "power" => Some(FeatureId::GF03),
        "path_length" | "elements" => Some(FeatureId::GF04),
        "btrim" | "ltrim" | "rtrim" => Some(FeatureId::GF05),
        "cardinality" => Some(FeatureId::GF12),
        "size" => Some(FeatureId::GF13),
        "duration" | "duration_between" => Some(FeatureId::GV41),
        "current_date" | "date" | "datetime" | "local_datetime" | "time" | "local_time" => {
            Some(FeatureId::GV39)
        }
        "current_time" | "current_timestamp" | "zoned_datetime" | "zoned_time" => {
            Some(FeatureId::GV40)
        }
        "uuid" | "uuid_v4" | "uuid_v7" => Some(FeatureId::IM_UUID),
        "json"
        | "json_parse"
        | "json_stringify"
        | "json_type"
        | "json_array"
        | "json_object"
        | "json_array_length"
        | "json_object_keys"
        | "json_contains"
        | "json_merge_patch"
        | "json_patch"
        | "json_get"
        | "json_get_text"
        | "json_get_scalar"
        | "json_get_path"
        | "json_get_path_text"
        | "json_get_path_scalar"
        | "json_has_path" => Some(FeatureId::IM_JSON),
        _ => None,
    }
}

fn is_list_value_function(name: &NonEmpty<DbString>, arity: usize) -> bool {
    if name.len() != 1 {
        return false;
    }
    let function_name = name.first().as_str();
    (arity == 2 && function_name.eq_ignore_ascii_case("trim"))
        || (arity == 1 && function_name.eq_ignore_ascii_case("elements"))
}

fn is_byte_string_trim_function(name: &NonEmpty<DbString>, args: &[ValueExpr]) -> bool {
    name.len() == 1
        && name.first().as_str().eq_ignore_ascii_case("trim")
        && matches!(args, [arg] if is_byte_string_expr(arg))
}

fn aggregate_function_feature(name: &NonEmpty<DbString>) -> Option<FeatureId> {
    if name.len() != 1 {
        return None;
    }
    match name.first().as_str().to_ascii_lowercase().as_str() {
        "stddev_pop" | "stddev_samp" | "collect_list" => Some(FeatureId::GF10),
        "percentile_cont" | "percentile_disc" => Some(FeatureId::GF11),
        _ => None,
    }
}

fn literal(value: &Literal, uses: &mut Vec<FeatureUse>) {
    match value {
        Literal::RadixInteger(_, span, kind) => {
            let feature_id = match kind {
                IntegerLiteralKind::Hexadecimal => FeatureId::GL01,
                IntegerLiteralKind::Octal => FeatureId::GL02,
                IntegerLiteralKind::Binary => FeatureId::GL03,
            };
            record_feature(uses, feature_id, *span);
        }
        Literal::Float(_, span, kind) => {
            record_feature(uses, FeatureId::GA01, *span);
            float_literal(*kind, *span, uses);
        }
        Literal::Decimal(_, span, kind) => {
            record_feature(uses, FeatureId::GV17, *span);
            decimal_literal(*kind, *span, uses);
        }
        Literal::String(_, span, kind) => character_string_literal(*kind, *span, uses),
        Literal::Uuid(_, span, kind) => {
            record_feature(uses, FeatureId::IM_UUID, *span);
            character_string_literal(*kind, *span, uses);
        }
        Literal::Duration(_, span, kind) => {
            record_feature(uses, FeatureId::GV41, *span);
            character_string_literal(*kind, *span, uses);
        }
        Literal::ZonedDateTime(_, span, kind)
        | Literal::LocalDateTime(_, span, kind)
        | Literal::Date(_, span, kind)
        | Literal::ZonedTime(_, span, kind)
        | Literal::LocalTime(_, span, kind) => {
            character_string_literal(*kind, *span, uses);
        }
        Literal::Bytes(_, _) | Literal::Bool(_, _) | Literal::Integer(_, _) | Literal::Null(_) => {}
    }
}

pub(super) fn character_string_literal(
    kind: CharacterStringLiteralKind,
    span: crate::SourceSpan,
    uses: &mut Vec<FeatureUse>,
) {
    if kind == CharacterStringLiteralKind::NoEscape {
        record_feature(uses, FeatureId::GL11, span);
    }
}

fn decimal_literal(kind: DecimalLiteralKind, span: crate::SourceSpan, uses: &mut Vec<FeatureUse>) {
    let feature_id = match kind {
        DecimalLiteralKind::CommonWithoutSuffix => FeatureId::GL04,
        DecimalLiteralKind::CommonOrIntegerWithSuffix => FeatureId::GL05,
        DecimalLiteralKind::ScientificWithSuffix => FeatureId::GL06,
    };
    record_feature(uses, feature_id, span);
}

fn float_literal(kind: FloatLiteralKind, span: crate::SourceSpan, uses: &mut Vec<FeatureUse>) {
    match kind {
        FloatLiteralKind::ScientificWithoutSuffix => {}
        FloatLiteralKind::CommonOrIntegerWithFloatSuffix => {
            record_feature(uses, FeatureId::GL07, span);
        }
        FloatLiteralKind::CommonOrIntegerWithDoubleSuffix => {
            record_feature(uses, FeatureId::GL07, span);
            record_feature(uses, FeatureId::GL10, span);
        }
        FloatLiteralKind::ScientificWithFloatSuffix => {
            record_feature(uses, FeatureId::GL08, span);
            record_feature(uses, FeatureId::GL09, span);
        }
        FloatLiteralKind::ScientificWithDoubleSuffix => {
            record_feature(uses, FeatureId::GL08, span);
            record_feature(uses, FeatureId::GL10, span);
        }
    }
}

fn is_check(kind: &IsCheckKind, span: crate::SourceSpan, uses: &mut Vec<FeatureUse>) {
    match kind {
        IsCheckKind::Null | IsCheckKind::TruthValue(_) => {}
        IsCheckKind::Typed(ty) => {
            // ISO/IEC 39075:2024 §19.6 `<value type predicate>` is optional feature
            // GA06. The construct stamp is emitted before the target type features,
            // mirroring the `CAST` GA05 ordering above.
            record_feature(uses, FeatureId::GA06, span);
            gql_type(ty, span, uses);
        }
        IsCheckKind::Normalized(_) => {}
        IsCheckKind::Directed => record_feature(uses, FeatureId::G110, span),
        IsCheckKind::Labeled(_) => {
            record_feature(uses, FeatureId::G111, span);
        }
        IsCheckKind::SourceOf(value) => {
            record_feature(uses, FeatureId::G112, span);
            self::value(value, uses);
        }
        IsCheckKind::DestinationOf(value) => {
            record_feature(uses, FeatureId::G112, span);
            self::value(value, uses);
        }
    }
}

pub(crate) fn gql_type(ty: &GqlType, span: crate::SourceSpan, uses: &mut Vec<FeatureUse>) {
    match ty {
        GqlType::NotNull(inner) => {
            record_feature(uses, FeatureId::GV90, span);
            gql_type(inner, span, uses);
        }
        GqlType::Uuid => record_feature(uses, FeatureId::IM_UUID, span),
        GqlType::Json => record_feature(uses, FeatureId::IM_JSON, span),
        GqlType::Vector => record_feature(uses, FeatureId::IM_VECTOR, span),
        GqlType::Any => record_feature(uses, FeatureId::GV66, span),
        GqlType::AnyProperty => record_feature(uses, FeatureId::GV68, span),
        GqlType::ClosedDynamicUnion(components) => {
            record_feature(uses, FeatureId::GV67, span);
            for component in components {
                gql_type(component, span, uses);
            }
        }
        GqlType::String | GqlType::Boolean | GqlType::Integer | GqlType::Float => {}
        GqlType::CharacterString(character_type) => match character_type.form {
            crate::ast::CharacterStringTypeForm::StringMax
            | crate::ast::CharacterStringTypeForm::VarcharMax => {
                record_feature(uses, FeatureId::GV31, span);
            }
            crate::ast::CharacterStringTypeForm::StringMinMax => {
                record_feature(uses, FeatureId::GV30, span);
                record_feature(uses, FeatureId::GV31, span);
            }
            crate::ast::CharacterStringTypeForm::CharFixed => {
                record_feature(uses, FeatureId::GV32, span);
            }
        },
        GqlType::Uint8 => {
            record_feature(uses, FeatureId::GV01, span);
            record_feature(uses, FeatureId::GV09, span);
        }
        GqlType::Int8 => {
            record_feature(uses, FeatureId::GV02, span);
            record_feature(uses, FeatureId::GV09, span);
        }
        GqlType::Uint16 => {
            record_feature(uses, FeatureId::GV03, span);
            record_feature(uses, FeatureId::GV09, span);
        }
        GqlType::Int16 => {
            record_feature(uses, FeatureId::GV04, span);
            record_feature(uses, FeatureId::GV09, span);
        }
        GqlType::SmallInt => record_feature(uses, FeatureId::GV18, span),
        GqlType::Uint32 => {
            record_feature(uses, FeatureId::GV06, span);
            record_feature(uses, FeatureId::GV09, span);
        }
        GqlType::Int32 => {
            record_feature(uses, FeatureId::GV07, span);
            record_feature(uses, FeatureId::GV09, span);
        }
        GqlType::Uint64 => {
            record_feature(uses, FeatureId::GV11, span);
            record_feature(uses, FeatureId::GV09, span);
        }
        GqlType::USmallInt => record_feature(uses, FeatureId::GV05, span),
        GqlType::Uint => record_feature(uses, FeatureId::GV08, span),
        GqlType::UBigInt => record_feature(uses, FeatureId::GV10, span),
        GqlType::BigInt => {
            record_feature(uses, FeatureId::GV19, span);
        }
        GqlType::Int64 => {
            record_feature(uses, FeatureId::GV12, span);
            record_feature(uses, FeatureId::GV09, span);
        }
        GqlType::Uint128 => {
            record_feature(uses, FeatureId::GV13, span);
            record_feature(uses, FeatureId::GV09, span);
        }
        GqlType::Int128 => {
            record_feature(uses, FeatureId::GV14, span);
            record_feature(uses, FeatureId::GV09, span);
        }
        GqlType::Decimal | GqlType::DecimalExact(_) => record_feature(uses, FeatureId::GV17, span),
        GqlType::Float32 => {
            record_feature(uses, FeatureId::GV21, span);
            record_feature(uses, FeatureId::GV22, span);
        }
        GqlType::Float64 => {
            record_feature(uses, FeatureId::GV22, span);
            record_feature(uses, FeatureId::GV24, span);
        }
        GqlType::Real => {
            record_feature(uses, FeatureId::GV21, span);
            record_feature(uses, FeatureId::GV23, span);
        }
        GqlType::Double => {
            record_feature(uses, FeatureId::GV23, span);
            record_feature(uses, FeatureId::GV24, span);
        }
        GqlType::Bytes => {
            record_feature(uses, FeatureId::GV35, span);
        }
        GqlType::ByteString(byte_type) => {
            record_feature(uses, FeatureId::GV35, span);
            match byte_type.form {
                crate::ast::ByteStringTypeForm::BytesMax
                | crate::ast::ByteStringTypeForm::VarbinaryMax => {
                    record_feature(uses, FeatureId::GV37, span);
                }
                crate::ast::ByteStringTypeForm::BytesMinMax => {
                    record_feature(uses, FeatureId::GV36, span);
                    record_feature(uses, FeatureId::GV37, span);
                }
                crate::ast::ByteStringTypeForm::BinaryFixed => {
                    record_feature(uses, FeatureId::GV38, span);
                }
            }
        }
        GqlType::Date | GqlType::LocalDateTime | GqlType::LocalTime => {
            record_feature(uses, FeatureId::GV39, span);
        }
        GqlType::ZonedDateTime | GqlType::ZonedTime => {
            record_feature(uses, FeatureId::GV40, span);
        }
        GqlType::Duration | GqlType::DurationYearToMonth | GqlType::DurationDayToSecond => {
            record_feature(uses, FeatureId::GV41, span)
        }
        GqlType::Record(record) => {
            record_feature(uses, FeatureId::GV45, span);
            match record {
                RecordType::Open => record_feature(uses, FeatureId::GV47, span),
                RecordType::Closed(fields) => {
                    record_feature(uses, FeatureId::GV46, span);
                    for (_, ty) in fields {
                        if matches!(ty, GqlType::Record(_)) {
                            record_feature(uses, FeatureId::GV48, span);
                        }
                        gql_type(ty, span, uses);
                    }
                }
            }
        }
        GqlType::List(inner)
        | GqlType::BoundedList {
            element_type: inner,
            ..
        } => {
            record_feature(uses, FeatureId::GV50, span);
            gql_type(inner, span, uses);
        }
        GqlType::Path => record_feature(uses, FeatureId::GV55, span),
        GqlType::GraphRef => record_feature(uses, FeatureId::GV60, span),
        GqlType::TableRef(table) => {
            record_feature(uses, FeatureId::GV61, span);
            if let crate::BindingTableType::Closed(fields) = table {
                for (_, ty) in fields {
                    gql_type(ty, span, uses);
                }
            }
        }
        GqlType::Null => {
            record_feature(uses, FeatureId::GV70, span);
            record_feature(uses, FeatureId::GV71, span);
        }
        GqlType::Nothing => {
            record_feature(uses, FeatureId::GV70, span);
            record_feature(uses, FeatureId::GV72, span);
        }
        GqlType::NodeRef | GqlType::EdgeRef => {}
    }
}

#[cfg(test)]
mod tests {
    use selene_profile::FeatureId;

    use crate::ast::expr::{BinaryOp, IsCheckKind, Literal};
    use crate::ast::{CharacterStringLiteralKind, ValueExpr};
    use crate::ast::{expr::FloatLiteralKind, span::SourceSpan};

    use super::{FeatureUse, value};

    fn span(offset: u32) -> SourceSpan {
        SourceSpan::new(offset, 1)
    }

    fn int(value: i64, offset: u32) -> ValueExpr {
        ValueExpr::Literal(Literal::Integer(value, span(offset)))
    }

    fn float(value: f64, offset: u32) -> ValueExpr {
        ValueExpr::Literal(Literal::Float(
            value,
            span(offset),
            FloatLiteralKind::CommonOrIntegerWithDoubleSuffix,
        ))
    }

    fn duration(value: &str, offset: u32) -> ValueExpr {
        ValueExpr::Literal(Literal::Duration(
            Box::new(value.parse().expect("duration literal parses")),
            span(offset),
            CharacterStringLiteralKind::Escaped,
        ))
    }

    fn ids(expr: &ValueExpr) -> Vec<FeatureId> {
        let mut uses = Vec::new();
        value(expr, &mut uses);
        uses.into_iter()
            .map(|feature: FeatureUse| feature.feature_id)
            .collect()
    }

    #[test]
    fn nested_child_features_record_transitively() {
        // `[1 + 2]`: the parent `ListLiteral` owns `GV50`; the nested
        // `BinaryOp::Add` child owns `GA01`. Both must surface, proving the
        // `for_each_child` delegation recurses into children.
        let expr = ValueExpr::ListLiteral {
            items: vec![ValueExpr::BinaryOp {
                op: BinaryOp::Add,
                lhs: int(1, 5).into(),
                rhs: int(2, 7).into(),
                span: span(5),
            }],
            span: span(0),
        };
        let observed = ids(&expr);
        assert!(
            observed.contains(&FeatureId::GV50),
            "parent ListLiteral feature missing: {observed:?}"
        );
        assert!(
            observed.contains(&FeatureId::GA01),
            "nested BinaryOp arithmetic feature missing transitively: {observed:?}"
        );
    }

    #[test]
    fn binary_op_records_parent_before_children() {
        // Ordering is observable: `flag()` reports the *first* unsupported
        // feature. The parent's `GA01` must precede the child float literal's
        // `GA01`, matching the pre-refactor recursion order.
        let expr = ValueExpr::BinaryOp {
            op: BinaryOp::Mul,
            lhs: float(1.5, 3).into(),
            rhs: int(2, 9).into(),
            span: span(0),
        };
        let mut uses = Vec::new();
        value(&expr, &mut uses);
        // First recorded span is the parent BinaryOp span (offset 0), then the
        // child float literal span (offset 3) — both `GA01`.
        assert_eq!(uses[0].feature_id, FeatureId::GA01);
        assert_eq!(uses[0].span.byte_offset, 0);
        assert_eq!(uses[1].feature_id, FeatureId::GA01);
        assert_eq!(uses[1].span.byte_offset, 3);
    }

    #[test]
    fn duration_literal_records_duration_feature() {
        let observed = ids(&duration("PT1H", 4));
        assert_eq!(observed, vec![FeatureId::GV41]);
    }

    #[test]
    fn is_check_source_of_orders_operand_then_kind_then_value() {
        // `(p :: INT8) IS SOURCE OF (e :: INT8)`: the operand subtree's typed
        // parameter and declared-type features, then the `IS SOURCE OF` `G112`,
        // then the value subtree's typed parameter and declared-type features
        // must appear in that exact order — the ordering `for_each_child` alone
        // cannot express, hence the explicit `IsCheck` arm.
        let typed_param = |name: &str, offset: u32| ValueExpr::Parameter {
            name: selene_core::db_string(name).expect("string fits DB string cap"),
            declared_type: Some(crate::ast::types::GqlType::Int8),
            span: span(offset),
        };
        let expr = ValueExpr::IsCheck {
            operand: typed_param("p", 1).into(),
            kind: IsCheckKind::SourceOf(typed_param("e", 20).into()),
            negated: false,
            span: span(10),
        };
        let mut uses = Vec::new();
        value(&expr, &mut uses);
        let trace: Vec<(FeatureId, u32)> = uses
            .iter()
            .map(|feature| (feature.feature_id, feature.span.byte_offset))
            .collect();
        // operand: GE04/GE05/IM_TYPED_PARAMS/GV02/GV09 @1, then G112 @10,
        // then value: GE04/GE05/IM_TYPED_PARAMS/GV02/GV09 @20.
        assert_eq!(
            trace,
            vec![
                (FeatureId::GE04, 1),
                (FeatureId::GE05, 1),
                (FeatureId::IM_TYPED_PARAMS, 1),
                (FeatureId::GV02, 1),
                (FeatureId::GV09, 1),
                (FeatureId::G112, 10),
                (FeatureId::GE04, 20),
                (FeatureId::GE05, 20),
                (FeatureId::IM_TYPED_PARAMS, 20),
                (FeatureId::GV02, 20),
                (FeatureId::GV09, 20),
            ]
        );
    }

    #[test]
    fn cast_records_feature_then_value_then_target_type() {
        // `CAST(1.5 AS INT8)`: `GA05` (Cast specification, ISO Annex A item 52),
        // then the value subtree's `GA01` (float literal), then the target-type
        // walk's `GV02`/`GV09`. The Cast node's own feature is recorded first and
        // the type tail runs *after* the `for_each_child` value recursion.
        let expr = ValueExpr::Cast {
            value: float(1.5, 5).into(),
            target_type: crate::ast::types::GqlType::Int8.into(),
            span: span(0),
        };
        let observed = ids(&expr);
        assert_eq!(observed.first(), Some(&FeatureId::GA05));
        let ga05 = observed.iter().position(|id| *id == FeatureId::GA05);
        let ga01 = observed.iter().position(|id| *id == FeatureId::GA01);
        let gv02 = observed.iter().position(|id| *id == FeatureId::GV02);
        assert!(
            ga05 < ga01 && ga01 < gv02,
            "expected GA05 < value(GA01) < target-type(GV02): {observed:?}"
        );
    }
}
