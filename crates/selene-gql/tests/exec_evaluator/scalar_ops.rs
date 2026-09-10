use super::*;

#[test]
fn null_equality_propagates_unknown() {
    let expr = ValueExpr::BinaryOp {
        op: BinaryOp::Eq,
        lhs: Box::new(null_lit()),
        rhs: Box::new(null_lit()),
        span: span(),
    };

    assert_eq!(eval(&expr), Value::Null);
}

#[test]
fn numeric_equal_top_level_float_nan_returns_null() {
    let expr = ValueExpr::BinaryOp {
        op: BinaryOp::Eq,
        lhs: Box::new(float_lit(f64::NAN)),
        rhs: Box::new(float_lit(f64::NAN)),
        span: span(),
    };

    assert_eq!(eval(&expr), Value::Null);
}

#[test]
fn three_valued_and_or_not_truth_table() {
    let cases = [
        (
            BinaryOp::And,
            bool_lit(false),
            null_lit(),
            Value::Bool(false),
        ),
        (BinaryOp::And, bool_lit(true), null_lit(), Value::Null),
        (BinaryOp::Or, bool_lit(true), null_lit(), Value::Bool(true)),
        (BinaryOp::Or, bool_lit(false), null_lit(), Value::Null),
    ];

    for (op, lhs, rhs, expected) in cases {
        let expr = ValueExpr::BinaryOp {
            op,
            lhs: Box::new(lhs),
            rhs: Box::new(rhs),
            span: span(),
        };
        assert_eq!(eval(&expr), expected);
    }

    let not_null = ValueExpr::UnaryOp {
        op: UnaryOp::Not,
        operand: Box::new(null_lit()),
        span: span(),
    };
    assert_eq!(eval(&not_null), Value::Null);
}

#[test]
fn arithmetic_overflow_is_data_exception() {
    let expr = ValueExpr::BinaryOp {
        op: BinaryOp::Add,
        lhs: Box::new(lit(Literal::Integer(i64::MAX, span()))),
        rhs: Box::new(lit(Literal::Integer(1, span()))),
        span: span(),
    };
    let caps = ImplDefinedCaps::default();
    let ctx = empty_graph_context(&caps);
    let err = selene_gql::runtime::evaluate_for_test(
        &expr,
        &Binding::empty(),
        &BindingTableSchema { columns: vec![] },
        &ctx,
    )
    .expect_err("overflow errors");

    assert!(matches!(err, ExecutorError::DataException { .. }));
    assert_eq!(err.gqlstatus().as_str(), "22003");
}

#[test]
fn eval_arithmetic_uint_uint_no_overflow_returns_uint() {
    assert_eq!(
        eval_binary_values(BinaryOp::Add, Value::Uint(10), Value::Uint(20))
            .expect("uint arithmetic succeeds"),
        Value::Uint(30)
    );
}

#[test]
fn eval_arithmetic_uint_uint_overflow_widens_to_int128() {
    assert_eq!(
        eval_binary_values(BinaryOp::Add, Value::Uint(u64::MAX), Value::Uint(1))
            .expect("uint overflow widens"),
        Value::Int128(i128::from(u64::MAX) + 1)
    );
}

#[test]
fn eval_arithmetic_int_uint_returns_int128() {
    assert_eq!(
        eval_binary_values(BinaryOp::Add, Value::Int(-1), Value::Uint(2))
            .expect("mixed arithmetic widens"),
        Value::Int128(1)
    );
}

#[test]
fn eval_arithmetic_uint_int_returns_int128() {
    assert_eq!(
        eval_binary_values(BinaryOp::Add, Value::Uint(2), Value::Int(-1))
            .expect("mixed arithmetic widens"),
        Value::Int128(1)
    );
}

#[test]
fn eval_arithmetic_int128_int_chains_exactly() {
    let widened = eval_binary_values(BinaryOp::Add, Value::Uint(u64::MAX), Value::Uint(1))
        .expect("uint overflow widens");

    assert_eq!(
        eval_binary_values(BinaryOp::Add, widened, Value::Int(1))
            .expect("chained Int128 arithmetic succeeds"),
        Value::Int128(i128::from(u64::MAX) + 2)
    );
}

#[test]
fn eval_arithmetic_int128_int128_succeeds() {
    let half = i128::MAX / 2;

    assert_eq!(
        eval_binary_values(BinaryOp::Add, Value::Int128(half), Value::Int128(half))
            .expect("Int128 arithmetic succeeds"),
        Value::Int128(half + half)
    );
}

#[test]
fn eval_arithmetic_int_int128_returns_int128() {
    assert_eq!(
        eval_binary_values(BinaryOp::Add, Value::Int(5), Value::Int128(7))
            .expect("Int + Int128 arithmetic succeeds"),
        Value::Int128(12)
    );
}

#[test]
fn eval_arithmetic_int128_overflow_returns_data_exception() {
    let err = eval_binary_values(BinaryOp::Mul, Value::Uint(u64::MAX), Value::Uint(u64::MAX))
        .expect_err("i128 widening can still overflow");

    assert!(matches!(err, ExecutorError::DataException { .. }));
    assert_eq!(err.gqlstatus().as_str(), "22003");
}

#[test]
fn large_integer_equality_is_exact() {
    let expr = ValueExpr::BinaryOp {
        op: BinaryOp::Eq,
        lhs: Box::new(int_lit(9_007_199_254_740_992)),
        rhs: Box::new(int_lit(9_007_199_254_740_993)),
        span: span(),
    };

    assert_eq!(eval(&expr), Value::Bool(false));
}

#[test]
fn large_integer_in_list_is_exact() {
    let expr = ValueExpr::InList {
        operand: Box::new(int_lit(9_007_199_254_740_993)),
        list: vec![int_lit(9_007_199_254_740_992)],
        negated: false,
        span: span(),
    };

    assert_eq!(eval(&expr), Value::Bool(false));
}

#[test]
fn integer_float_ordering_compares_exact_values_without_a_lossy_cast() {
    let expr = ValueExpr::BinaryOp {
        op: BinaryOp::Gt,
        lhs: Box::new(int_lit(9_007_199_254_740_993)),
        rhs: Box::new(float_lit(9_007_199_254_740_992.0)),
        span: span(),
    };
    // ISO §19.3 compares numeric values. The exact successor of 2^53 is
    // greater than the exactly representable binary value 2^53, even though
    // converting that successor to f64 would erase the difference.
    assert_eq!(eval_result(&expr).unwrap(), Value::Bool(true));
}

#[test]
fn unknown_scalar_function_returns_22g03() {
    let expr = ValueExpr::FunctionCall {
        name: NonEmpty::try_from_vec(vec![db_string("unsupported").unwrap()]).expect("non-empty"),
        args: Vec::new(),
        star: false,
        distinct: false,
        span: span(),
    };
    let caps = ImplDefinedCaps::default();
    let ctx = empty_graph_context(&caps);
    let err = selene_gql::runtime::evaluate_for_test(
        &expr,
        &Binding::empty(),
        &BindingTableSchema { columns: vec![] },
        &ctx,
    )
    .expect_err("unknown function errors");

    assert!(matches!(err, ExecutorError::UnknownFunction { .. }));
    assert_eq!(err.gqlstatus().as_str(), "22G03");
}
