//! Metadata serde remains supported; runtime-default carriers use format 2.
use super::{dbs, property_def, rt, rt_property};
use crate::*;

#[test]
fn property_default_and_flags_format2_round_trip() {
    let mut property = property_def("unique");
    property.unique = true;
    property.immutable = true;
    property.default = Some(Value::String(dbs("default")));
    rt_property(&property);
}
#[test]
fn record_fields_none_open_closed_format2_round_trip() {
    for structure in [
        None,
        Some(RecordFieldStructure::Open),
        Some(RecordFieldStructure::Closed(vec![
            RecordFieldStructureDef {
                name: dbs("field"),
                field_type: RecordFieldStructureType::Scalar(PropertyValueType::Int),
                required: true,
            },
        ])),
    ] {
        let mut property = property_def("record");
        property.record_fields = structure.map(Box::new);
        rt_property(&property);
    }
}
#[test]
fn nested_record_field_structure_postcard_and_format2_round_trip() {
    let inner = RecordFieldStructure::Closed(vec![RecordFieldStructureDef {
        name: dbs("inner"),
        field_type: RecordFieldStructureType::Scalar(PropertyValueType::Bool),
        required: false,
    }]);
    let structure = RecordFieldStructure::Closed(vec![
        RecordFieldStructureDef {
            name: dbs("list"),
            field_type: RecordFieldStructureType::List(Box::new(
                RecordFieldStructureType::NotNull(Box::new(RecordFieldStructureType::Record(
                    Box::new(inner.clone()),
                ))),
            )),
            required: true,
        },
        RecordFieldStructureDef {
            name: dbs("open"),
            field_type: RecordFieldStructureType::Record(Box::new(RecordFieldStructure::Open)),
            required: false,
        },
        RecordFieldStructureDef {
            name: dbs("closed"),
            field_type: RecordFieldStructureType::Record(Box::new(inner)),
            required: true,
        },
    ]);
    rt(&structure);
    let mut property = property_def("nested");
    property.record_fields = Some(Box::new(structure));
    rt_property(&property);
}
#[test]
fn scalar_metadata_postcard_and_format2_defaults_round_trip() {
    let decimal = DecimalType::new(5, 2).unwrap();
    let string = CharacterStringType::new(2, 4).unwrap();
    let bytes = ByteStringType::new(2, 4).unwrap();
    for (kind, field, value) in [
        (
            PredefinedValueType::Decimal,
            RecordFieldStructureType::Decimal(decimal),
            Value::Decimal("123.45".parse().unwrap()),
        ),
        (
            PredefinedValueType::String,
            RecordFieldStructureType::CharacterString(string),
            Value::String(dbs("core")),
        ),
        (
            PredefinedValueType::Bytes,
            RecordFieldStructureType::ByteString(bytes),
            Value::Bytes(vec![0xca, 0xfe].into()),
        ),
    ] {
        let mut ty = ValueType::predefined(kind);
        match kind {
            PredefinedValueType::Decimal => ty.decimal_type = Some(decimal),
            PredefinedValueType::String => ty.character_string_type = Some(string),
            PredefinedValueType::Bytes => ty.byte_string_type = Some(bytes),
            _ => unreachable!(),
        }
        rt(&ty);
        rt(&field);
        let mut property = property_def("typed");
        property.value_type = ty;
        property.default = Some(value);
        rt_property(&property);
    }
}
