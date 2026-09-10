use std::{
    collections::HashMap,
    hash::{Hash, Hasher},
};

use crate::{
    BindingTableType, ExistsBody, GqlType, IsCheckKind, LabelExpr, Literal, MatchClause,
    NormalForm, PatternElement, RecordType, TruthValue, ValueExpr,
};

/// Structural fingerprint of one `ValueExpr` node.
///
/// Folds the node's discriminant + own scalar fields + each child node's
/// fingerprint. `memo` (created fresh per top-level source-key lookup)
/// caches each node's result by pointer identity so a shared/repeated subtree
/// within this one walk is hashed once; folding the cached child fingerprints
/// keeps the walk linear in the expression size rather than O(depth × n). The
/// folding is identical with or without a populated memo, so the key is stable
/// across the `insert` and `get` paths.
pub(super) fn node_fingerprint(expr: &ValueExpr, memo: &mut HashMap<usize, u64>) -> u64 {
    let key = std::ptr::from_ref(expr) as usize;
    if let Some(cached) = memo.get(&key) {
        return *cached;
    }
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    hash_value_expr(expr, &mut hasher, memo);
    let fingerprint = hasher.finish();
    memo.insert(key, fingerprint);
    fingerprint
}

/// Hash one child `ValueExpr`'s fingerprint into `state` (folds the child's
/// cached/computed `u64` fingerprint rather than its raw bytes, so the memo can
/// short-circuit subtrees while keeping the byte stream stable across paths).
fn hash_child<H: Hasher>(child: &ValueExpr, state: &mut H, memo: &mut HashMap<usize, u64>) {
    node_fingerprint(child, memo).hash(state);
}

fn hash_value_expr<H: Hasher>(expr: &ValueExpr, state: &mut H, memo: &mut HashMap<usize, u64>) {
    match expr {
        ValueExpr::Literal(literal) => {
            0u8.hash(state);
            hash_literal(literal, state);
        }
        ValueExpr::Variable { name, span } => {
            1u8.hash(state);
            name.hash(state);
            span.hash(state);
        }
        ValueExpr::Parameter {
            name,
            declared_type,
            span,
        } => {
            2u8.hash(state);
            name.hash(state);
            match declared_type {
                Some(ty) => {
                    1u8.hash(state);
                    hash_gql_type(ty, state);
                }
                None => 0u8.hash(state),
            }
            span.hash(state);
        }
        ValueExpr::PropertyAccess { target, key, span } => {
            3u8.hash(state);
            hash_child(target, state, memo);
            key.hash(state);
            span.hash(state);
        }
        ValueExpr::ListLiteral { items, span } => {
            5u8.hash(state);
            hash_exprs(items, state, memo);
            span.hash(state);
        }
        ValueExpr::RecordLiteral { fields, span } => {
            6u8.hash(state);
            fields.len().hash(state);
            for (key, value) in fields {
                key.hash(state);
                hash_child(value, state, memo);
            }
            span.hash(state);
        }
        ValueExpr::PathConstructor { elements, span } => {
            25u8.hash(state);
            hash_exprs(elements, state, memo);
            span.hash(state);
        }
        ValueExpr::BinaryOp { op, lhs, rhs, span } => {
            7u8.hash(state);
            op.hash(state);
            hash_child(lhs, state, memo);
            hash_child(rhs, state, memo);
            span.hash(state);
        }
        ValueExpr::UnaryOp { op, operand, span } => {
            8u8.hash(state);
            op.hash(state);
            hash_child(operand, state, memo);
            span.hash(state);
        }
        ValueExpr::FunctionCall {
            name,
            args,
            star,
            distinct,
            span,
        } => {
            9u8.hash(state);
            name.hash(state);
            hash_exprs(args, state, memo);
            star.hash(state);
            distinct.hash(state);
            span.hash(state);
        }
        ValueExpr::DurationBetween {
            start,
            end,
            qualifier,
            span,
        } => {
            24u8.hash(state);
            hash_child(start, state, memo);
            hash_child(end, state, memo);
            qualifier.hash(state);
            span.hash(state);
        }
        ValueExpr::Normalize { source, form, span } => {
            22u8.hash(state);
            hash_child(source, state, memo);
            form.hash(state);
            span.hash(state);
        }
        ValueExpr::Trim {
            spec,
            character,
            source,
            span,
        } => {
            23u8.hash(state);
            spec.hash(state);
            character.is_some().hash(state);
            if let Some(character) = character {
                hash_child(character, state, memo);
            }
            hash_child(source, state, memo);
            span.hash(state);
        }
        ValueExpr::IsCheck {
            operand,
            kind,
            negated,
            span,
        } => {
            10u8.hash(state);
            hash_child(operand, state, memo);
            hash_is_check(kind, state, memo);
            negated.hash(state);
            span.hash(state);
        }
        ValueExpr::InList {
            operand,
            list,
            negated,
            span,
        } => {
            11u8.hash(state);
            hash_child(operand, state, memo);
            hash_exprs(list, state, memo);
            negated.hash(state);
            span.hash(state);
        }
        ValueExpr::InListExpression {
            operand,
            list,
            negated,
            span,
        } => {
            12u8.hash(state);
            hash_child(operand, state, memo);
            hash_child(list, state, memo);
            negated.hash(state);
            span.hash(state);
        }
        ValueExpr::AllDifferent { items, span } => {
            14u8.hash(state);
            hash_exprs(items, state, memo);
            span.hash(state);
        }
        ValueExpr::Same { items, span } => {
            15u8.hash(state);
            hash_exprs(items, state, memo);
            span.hash(state);
        }
        ValueExpr::PropertyExists {
            target, key, span, ..
        } => {
            16u8.hash(state);
            hash_child(target, state, memo);
            key.hash(state);
            span.hash(state);
        }
        ValueExpr::Case {
            branches,
            else_branch,
            span,
        } => {
            17u8.hash(state);
            branches.len().hash(state);
            for (condition, value) in branches {
                hash_child(condition, state, memo);
                hash_child(value, state, memo);
            }
            if let Some(value) = else_branch {
                true.hash(state);
                hash_child(value, state, memo);
            } else {
                false.hash(state);
            }
            span.hash(state);
        }
        ValueExpr::Exists {
            body,
            negated,
            span,
        } => {
            18u8.hash(state);
            match body {
                ExistsBody::Match(pattern) => {
                    0u8.hash(state);
                    hash_match_clause(pattern, state, memo);
                }
                ExistsBody::Query(pipeline) => {
                    1u8.hash(state);
                    pipeline.span.hash(state);
                    pipeline.statements.len().hash(state);
                }
            }
            negated.hash(state);
            span.hash(state);
        }
        ValueExpr::ValueSubquery { body, span } => {
            20u8.hash(state);
            body.span.hash(state);
            body.statements.len().hash(state);
            span.hash(state);
        }
        ValueExpr::Cast {
            value,
            target_type,
            span,
        } => {
            // Hash discriminant + recursive value hash + target_type so that
            // CAST(x AS Integer) and CAST(x AS Float) get different dedup
            // keys. GqlType derives Hash per BRIEF-135a BF1 fold.
            21u8.hash(state);
            hash_child(value, state, memo);
            target_type.hash(state);
            span.hash(state);
        }
    }
}

fn hash_exprs<H: Hasher>(values: &[ValueExpr], state: &mut H, memo: &mut HashMap<usize, u64>) {
    values.len().hash(state);
    for value in values {
        hash_child(value, state, memo);
    }
}

fn hash_literal<H: Hasher>(literal: &Literal, state: &mut H) {
    match literal {
        Literal::Bool(value, span) => {
            0u8.hash(state);
            value.hash(state);
            span.hash(state);
        }
        Literal::Integer(value, span) => {
            1u8.hash(state);
            value.hash(state);
            span.hash(state);
        }
        Literal::RadixInteger(value, span, kind) => {
            15u8.hash(state);
            value.hash(state);
            span.hash(state);
            kind.hash(state);
        }
        Literal::Decimal(value, span, kind) => {
            18u8.hash(state);
            value.hash(state);
            span.hash(state);
            kind.hash(state);
        }
        Literal::Float(value, span, kind) => {
            2u8.hash(state);
            value.to_bits().hash(state);
            span.hash(state);
            kind.hash(state);
        }
        Literal::String(value, span, _) => {
            3u8.hash(state);
            value.hash(state);
            span.hash(state);
        }
        Literal::Bytes(value, span) => {
            12u8.hash(state);
            value.hash(state);
            span.hash(state);
        }
        Literal::Null(span) => {
            4u8.hash(state);
            span.hash(state);
        }
        Literal::Uuid(value, span, _) => {
            5u8.hash(state);
            value.hash(state);
            span.hash(state);
        }
        Literal::ZonedDateTime(value, span, _) => {
            6u8.hash(state);
            value.hash(state);
            span.hash(state);
        }
        Literal::LocalDateTime(value, span, _) => {
            7u8.hash(state);
            value.hash(state);
            span.hash(state);
        }
        Literal::Date(value, span, _) => {
            8u8.hash(state);
            value.hash(state);
            span.hash(state);
        }
        Literal::ZonedTime(value, span, _) => {
            9u8.hash(state);
            value.hash(state);
            span.hash(state);
        }
        Literal::LocalTime(value, span, _) => {
            10u8.hash(state);
            value.hash(state);
            span.hash(state);
        }
        Literal::Duration(value, span, _) => {
            11u8.hash(state);
            (
                value.get_years(),
                value.get_months(),
                value.get_weeks(),
                value.get_days(),
                value.get_hours(),
                value.get_minutes(),
                value.get_seconds(),
                value.get_milliseconds(),
                value.get_microseconds(),
                value.get_nanoseconds(),
            )
                .hash(state);
            span.hash(state);
        }
    }
}

fn hash_is_check<H: Hasher>(kind: &IsCheckKind, state: &mut H, memo: &mut HashMap<usize, u64>) {
    match kind {
        IsCheckKind::Null => 0u8.hash(state),
        IsCheckKind::Directed => 1u8.hash(state),
        IsCheckKind::Labeled(label) => {
            2u8.hash(state);
            hash_label_expr(label, state);
        }
        IsCheckKind::TruthValue(value) => {
            3u8.hash(state);
            hash_truth_value(*value, state);
        }
        IsCheckKind::Typed(ty) => {
            4u8.hash(state);
            hash_gql_type(ty, state);
        }
        IsCheckKind::Normalized(form) => {
            5u8.hash(state);
            hash_normal_form(*form, state);
        }
        IsCheckKind::SourceOf(value) => {
            6u8.hash(state);
            hash_child(value, state, memo);
        }
        IsCheckKind::DestinationOf(value) => {
            7u8.hash(state);
            hash_child(value, state, memo);
        }
    }
}

fn hash_label_expr<H: Hasher>(expr: &LabelExpr, state: &mut H) {
    match expr {
        LabelExpr::Single(label) => {
            0u8.hash(state);
            label.hash(state);
        }
        LabelExpr::Conjunction(parts) => {
            1u8.hash(state);
            hash_label_exprs(parts, state);
        }
        LabelExpr::Disjunction(parts) => {
            2u8.hash(state);
            hash_label_exprs(parts, state);
        }
        LabelExpr::Negation(inner) => {
            3u8.hash(state);
            hash_label_expr(inner, state);
        }
        LabelExpr::Wildcard => {
            4u8.hash(state);
        }
    }
}

fn hash_label_exprs<H: Hasher>(values: &[LabelExpr], state: &mut H) {
    values.len().hash(state);
    for value in values {
        hash_label_expr(value, state);
    }
}

fn hash_match_clause<H: Hasher>(
    clause: &MatchClause,
    state: &mut H,
    memo: &mut HashMap<usize, u64>,
) {
    clause.optional.hash(state);
    clause.selector.hash(state);
    clause.match_mode.hash(state);
    clause.path_mode.hash(state);
    clause.patterns.len().hash(state);
    for pattern in &clause.patterns {
        pattern.path_binding.hash(state);
        pattern.elements.len().hash(state);
        for element in &pattern.elements {
            match element {
                PatternElement::Node(node) => {
                    0u8.hash(state);
                    node.binding.hash(state);
                    if let Some(expr) = &node.label_expr {
                        hash_label_expr(expr, state);
                    }
                    node.properties.len().hash(state);
                    for (key, value) in &node.properties {
                        key.hash(state);
                        hash_child(value, state, memo);
                    }
                    if let Some(expr) = &node.inline_where {
                        hash_child(expr, state, memo);
                    }
                    node.span.hash(state);
                }
                PatternElement::Edge(edge) => {
                    1u8.hash(state);
                    edge.binding.hash(state);
                    edge.direction.hash(state);
                    if let Some(expr) = &edge.label_expr {
                        hash_label_expr(expr, state);
                    }
                    edge.properties.len().hash(state);
                    for (key, value) in &edge.properties {
                        key.hash(state);
                        hash_child(value, state, memo);
                    }
                    edge.quantifier.hash(state);
                    if let Some(expr) = &edge.inline_where {
                        hash_child(expr, state, memo);
                    }
                    edge.span.hash(state);
                }
            }
        }
        pattern.span.hash(state);
    }
    if let Some(expr) = &clause.where_clause {
        hash_child(expr, state, memo);
    }
    clause.span.hash(state);
}

fn hash_gql_type<H: Hasher>(ty: &GqlType, state: &mut H) {
    match ty {
        GqlType::Any => 50u8.hash(state),
        GqlType::AnyProperty => 51u8.hash(state),
        GqlType::ClosedDynamicUnion(components) => {
            54u8.hash(state);
            components.len().hash(state);
            for component in components {
                hash_gql_type(component, state);
            }
        }
        GqlType::String => 0u8.hash(state),
        GqlType::CharacterString(character) => {
            48u8.hash(state);
            character.hash(state);
        }
        GqlType::Boolean => 1u8.hash(state),
        GqlType::Integer => 2u8.hash(state),
        GqlType::Float => 3u8.hash(state),
        GqlType::Int8 => 4u8.hash(state),
        GqlType::Int16 => 5u8.hash(state),
        GqlType::Int32 => 6u8.hash(state),
        GqlType::Int64 => 7u8.hash(state),
        GqlType::Int128 => 8u8.hash(state),
        GqlType::Uint8 => 9u8.hash(state),
        GqlType::Uint16 => 10u8.hash(state),
        GqlType::Uint32 => 11u8.hash(state),
        GqlType::Uint64 => 12u8.hash(state),
        GqlType::Uint128 => 13u8.hash(state),
        GqlType::USmallInt => 43u8.hash(state),
        GqlType::Uint => 44u8.hash(state),
        GqlType::UBigInt => 45u8.hash(state),
        GqlType::SmallInt => 14u8.hash(state),
        GqlType::BigInt => 15u8.hash(state),
        GqlType::Decimal => 16u8.hash(state),
        GqlType::DecimalExact(decimal) => {
            47u8.hash(state);
            decimal.hash(state);
        }
        GqlType::Float32 => 17u8.hash(state),
        GqlType::Float64 => 18u8.hash(state),
        GqlType::Real => 40u8.hash(state),
        GqlType::Double => 41u8.hash(state),
        GqlType::Bytes => 19u8.hash(state),
        GqlType::ByteString(bytes) => {
            42u8.hash(state);
            bytes.hash(state);
        }
        GqlType::ZonedDateTime => 22u8.hash(state),
        GqlType::LocalDateTime => 23u8.hash(state),
        GqlType::Date => 24u8.hash(state),
        GqlType::ZonedTime => 25u8.hash(state),
        GqlType::LocalTime => 26u8.hash(state),
        GqlType::Duration => 27u8.hash(state),
        GqlType::DurationYearToMonth => 35u8.hash(state),
        GqlType::DurationDayToSecond => 36u8.hash(state),
        GqlType::Record(record) => {
            28u8.hash(state);
            hash_record_type(record, state);
        }
        GqlType::List(inner) => {
            29u8.hash(state);
            hash_gql_type(inner, state);
        }
        GqlType::BoundedList {
            element_type,
            max_len,
        } => {
            49u8.hash(state);
            max_len.hash(state);
            hash_gql_type(element_type, state);
        }
        GqlType::NotNull(inner) => {
            46u8.hash(state);
            hash_gql_type(inner, state);
        }
        GqlType::Path => 30u8.hash(state),
        GqlType::GraphRef => 31u8.hash(state),
        GqlType::NodeRef => 32u8.hash(state),
        GqlType::EdgeRef => 33u8.hash(state),
        GqlType::TableRef(table) => {
            34u8.hash(state);
            hash_binding_table_type(table, state);
        }
        GqlType::Null => 52u8.hash(state),
        GqlType::Nothing => 53u8.hash(state),
        GqlType::Uuid => 37u8.hash(state),
        GqlType::Vector => 38u8.hash(state),
        GqlType::Json => 39u8.hash(state),
    }
}

fn hash_binding_table_type<H: Hasher>(table: &BindingTableType, state: &mut H) {
    match table {
        BindingTableType::Any => 0u8.hash(state),
        BindingTableType::Closed(fields) => {
            1u8.hash(state);
            fields.len().hash(state);
            for (name, ty) in fields {
                name.hash(state);
                hash_gql_type(ty, state);
            }
        }
    }
}

fn hash_record_type<H: Hasher>(record: &RecordType, state: &mut H) {
    match record {
        RecordType::Open => 0u8.hash(state),
        RecordType::Closed(fields) => {
            1u8.hash(state);
            fields.len().hash(state);
            for (name, ty) in fields {
                name.hash(state);
                hash_gql_type(ty, state);
            }
        }
    }
}

fn hash_truth_value<H: Hasher>(value: TruthValue, state: &mut H) {
    match value {
        TruthValue::True => 0u8.hash(state),
        TruthValue::False => 1u8.hash(state),
        TruthValue::Unknown => 2u8.hash(state),
    }
}

fn hash_normal_form<H: Hasher>(form: NormalForm, state: &mut H) {
    match form {
        NormalForm::Nfc => 0u8.hash(state),
        NormalForm::Nfd => 1u8.hash(state),
        NormalForm::Nfkc => 2u8.hash(state),
        NormalForm::Nfkd => 3u8.hash(state),
    }
}
