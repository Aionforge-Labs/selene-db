//! Explicit tags for bounded expression programs; existing index tags are unchanged.

use selene_core::{
    logical::{CodecError as E, CodecResult, Decoder, Encoder},
    scalar_index_expression::{
        ScalarIndexExpression, ScalarIndexOperation as Op, ScalarIndexSelector as Selector,
    },
};

#[cfg(test)]
#[path = "expression/tests.rs"]
mod tests;

pub(super) fn encode(e: &mut Encoder, expression: &ScalarIndexExpression) -> CodecResult<()> {
    if !expression.is_valid() {
        return Err(E::Semantic);
    }
    e.u32(expression.semantics)?;
    e.text(&expression.property)?;
    e.count(expression.operations.len())?;
    for operation in &expression.operations {
        let path = match operation {
            Op::Lower => {
                e.u8(1)?;
                continue;
            }
            Op::Upper => {
                e.u8(2)?;
                continue;
            }
            Op::JsonScalarPath(path) => {
                e.u8(3)?;
                path
            }
            Op::JsonTextPath(path) => {
                e.u8(4)?;
                path
            }
        };
        e.count(path.len())?;
        for selector in path {
            match selector {
                Selector::Key(key) => {
                    e.u8(1)?;
                    e.text(key)?;
                }
                Selector::Index(index) => {
                    e.u8(2)?;
                    e.u64(*index as u64)?;
                }
            }
        }
    }
    Ok(())
}

pub(super) fn decode(d: &mut Decoder<'_, '_>) -> CodecResult<ScalarIndexExpression> {
    let semantics = d.u32()?;
    let property = d.text()?.to_owned();
    let count = d.count()?;
    if count > 16 {
        return Err(E::Limit);
    }
    let mut operations = Vec::with_capacity(count);
    for _ in 0..count {
        let tag = d.u8()?;
        operations.push(match tag {
            1 => Op::Lower,
            2 => Op::Upper,
            3 | 4 => {
                let count = d.count()?;
                if !(1..=64).contains(&count) {
                    return Err(E::Limit);
                }
                let mut selectors = Vec::with_capacity(count);
                for _ in 0..count {
                    selectors.push(match d.u8()? {
                        1 => Selector::Key(d.text()?.to_owned()),
                        2 => Selector::Index(d.u64()? as i64),
                        _ => return Err(E::Invalid("expression selector")),
                    });
                }
                if tag == 3 {
                    Op::JsonScalarPath(selectors)
                } else {
                    Op::JsonTextPath(selectors)
                }
            }
            _ => return Err(E::Invalid("expression operation")),
        });
    }
    let expression = ScalarIndexExpression {
        semantics,
        property,
        operations,
    };
    if !expression.is_valid() {
        return Err(E::Semantic);
    }
    Ok(expression)
}
