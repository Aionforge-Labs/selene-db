//! Source declarations use the same normalized structural membership service
//! as parameters, schema admission, and result descriptors.

use crate::GqlType;
use selene_core::Value;

pub(crate) fn value_matches_gql_type(value: &Value, ty: &GqlType) -> bool {
    crate::normalize_value_type(ty).is_ok_and(|ty| ty.matches(value))
}

#[cfg(test)]
mod tests {
    use super::value_matches_gql_type;
    use crate::{GqlType, RecordType};
    use selene_core::{ExtensionTypeId, RecordTypeId, RecordTyped, Value};
    use std::sync::Arc;

    fn sample_recordtyped() -> Value {
        Value::RecordTyped(Box::new(RecordTyped {
            type_id: RecordTypeId::new(1),
            values: [Some(Value::Int(1))].into_iter().collect(),
        }))
    }

    #[test]
    fn closed_record_type_rejects_recordtyped_operand_fail_closed() {
        let field = selene_core::db_string("a").unwrap();
        let ty = GqlType::Record(RecordType::Closed(vec![(field, GqlType::Integer)]));
        assert!(!value_matches_gql_type(&sample_recordtyped(), &ty));
    }

    #[test]
    fn open_record_type_rejects_recordtyped_operand() {
        assert!(!value_matches_gql_type(
            &sample_recordtyped(),
            &GqlType::Record(RecordType::Open)
        ));
    }

    #[test]
    fn property_value_type_rejects_extension_owned_values() {
        let value = Value::Extended {
            type_id: ExtensionTypeId::FIRST_PARTY_MIN,
            payload: Arc::from([1_u8]),
        };
        assert!(!value_matches_gql_type(&value, &GqlType::Any));
        assert!(!value_matches_gql_type(&value, &GqlType::AnyProperty));
    }
}
