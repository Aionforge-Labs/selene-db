use super::*;
use selene_core::logical::{Budget, Limits};

fn read(bytes: &[u8]) -> CodecResult<ScalarIndexExpression> {
    let mut budget = Budget::new(Limits::default())?;
    let mut decoder = Decoder::new(bytes, &mut budget)?;
    let expression = decode(&mut decoder)?;
    decoder.finish()?;
    Ok(expression)
}

#[test]
fn explicit_expression_wire_tags_and_every_truncated_prefix() {
    // Authored from the specified tag sequence, not from encode(expression).
    let mut wire = Encoder::new(Limits::default()).unwrap();
    wire.u32(1).unwrap();
    wire.text("body").unwrap();
    wire.count(1).unwrap();
    wire.u8(3).unwrap(); // scalar JSON path, not text coercion
    wire.count(2).unwrap();
    wire.u8(1).unwrap();
    wire.text("a.b").unwrap();
    wire.u8(2).unwrap();
    wire.u64(u64::MAX).unwrap(); // signed -1
    let bytes = wire.finish();
    let expected = ScalarIndexExpression {
        semantics: 1,
        property: "body".into(),
        operations: vec![Op::JsonScalarPath(vec![
            Selector::Key("a.b".into()),
            Selector::Index(-1),
        ])],
    };
    assert_eq!(read(&bytes).unwrap(), expected);
    let mut encoded = Encoder::new(Limits::default()).unwrap();
    encode(&mut encoded, &expected).unwrap();
    assert_eq!(encoded.finish(), bytes);
    for length in 0..bytes.len() {
        assert!(read(&bytes[..length]).is_err());
    }
    let mut trailing = bytes;
    trailing.push(0);
    assert!(read(&trailing).is_err());
}

#[test]
fn unknown_semantics_operations_and_excessive_counts_fail_closed() {
    for (semantics, count, tag) in [(999, 1, 1), (1, 17, 1), (1, 1, 255)] {
        let mut wire = Encoder::new(Limits::default()).unwrap();
        wire.u32(semantics).unwrap();
        wire.text("body").unwrap();
        wire.count(count).unwrap();
        wire.u8(tag).unwrap();
        assert!(read(&wire.finish()).is_err());
    }
}
