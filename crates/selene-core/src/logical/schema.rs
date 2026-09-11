//! Logical graph-type definitions with named endpoints, never physical positions.

use super::{CodecError as E, CodecResult, Decoder, Encoder};
use crate::{
    DbString, EdgeEndpointDef, EdgeTypeDef, LabelSet, NodeTypeDef, NodeTypeRef, PropertyDef,
    RecordFieldStructure as R, RecordFieldStructureDef, RecordFieldStructureType as T,
    ValidationMode, ValueType, ValueTypeCardinality, db_string,
};

/// Complete logical graph schema. Order is declaration order, not a storage row.
#[derive(Clone, Debug, PartialEq)]
pub struct GraphDefinition {
    /// Semantic type name (the catalog identity is carried separately).
    pub name: DbString,
    /// Named node definitions, including defaults and restrictions.
    pub nodes: Vec<(DbString, NodeTypeDef)>,
    /// Named edge definitions; endpoints reference node type names.
    pub edges: Vec<(DbString, EdgeTypeDef)>,
}

impl Encoder {
    /// Encode complete named graph-type metadata, excluding legacy record intern IDs.
    pub fn graph_definition(&mut self, def: &GraphDefinition) -> CodecResult<()> {
        self.budget.metadata(1)?;
        self.text(def.name.as_str())?;
        self.count_for::<(DbString, NodeTypeDef)>(def.nodes.len())?;
        for (name, node) in &def.nodes {
            self.budget.metadata(1)?;
            if node.key.is_some() {
                return Err(E::Invalid("legacy node key"));
            }
            self.text(name.as_str())?;
            self.labels(&node.labels)?;
            self.properties_definition(&node.properties)?;
            self.validation_mode(node.validation_mode)?;
        }
        self.count_for::<(DbString, EdgeTypeDef)>(def.edges.len())?;
        for (name, edge) in &def.edges {
            self.budget.metadata(1)?;
            self.text(name.as_str())?;
            self.text(edge.label.as_str())?;
            self.endpoint(&edge.source_node_type)?;
            self.endpoint(&edge.target_node_type)?;
            self.properties_definition(&edge.properties)?;
            self.validation_mode(edge.validation_mode)?;
        }
        Ok(())
    }
    /// Encode labels in canonical strictly increasing exact-name order.
    pub fn labels(&mut self, labels: &LabelSet) -> CodecResult<()> {
        self.count(labels.len())?;
        for label in labels.iter() {
            self.text(label.as_str())?;
        }
        Ok(())
    }
    fn validation_mode(&mut self, mode: ValidationMode) -> CodecResult<()> {
        self.u8(match mode {
            ValidationMode::Strict => 0,
            ValidationMode::Warn => 1,
        })
    }
    fn endpoint(&mut self, endpoint: &EdgeEndpointDef) -> CodecResult<()> {
        match endpoint {
            EdgeEndpointDef::Any => self.u8(0),
            EdgeEndpointDef::NodeType(name) => {
                self.u8(1)?;
                self.text(name.0.as_str())
            }
            EdgeEndpointDef::OneOf(names) => {
                self.u8(2)?;
                self.count(names.len())?;
                let mut names: Vec<_> = names.iter().map(|name| &name.0).collect();
                names.sort();
                if names.len() < 2 || names.windows(2).any(|p| p[0] == p[1]) {
                    return Err(E::Semantic);
                }
                for name in names {
                    self.text(name.as_str())?;
                }
                Ok(())
            }
        }
    }
    fn properties_definition(&mut self, properties: &[PropertyDef]) -> CodecResult<()> {
        self.count_for::<PropertyDef>(properties.len())?;
        for property in properties {
            self.budget.metadata(1)?;
            self.text(property.name.as_str())?;
            self.value_type(&property.value_type, 1)?;
            self.boolean(property.nullable)?;
            self.boolean(property.default.is_some())?;
            if let Some(value) = &property.default {
                self.value(value, 1)?;
            }
            self.boolean(property.immutable)?;
            self.boolean(property.unique)?;
            self.boolean(property.record_fields.is_some())?;
            if let Some(fields) = &property.record_fields {
                self.record_structure(fields, 1)?;
            }
        }
        Ok(())
    }
    fn value_type(&mut self, ty: &ValueType, depth: usize) -> CodecResult<()> {
        self.budget.depth(depth)?;
        metadata::<ValueType>(&mut self.budget)?;
        if ty.union.is_some() || ty.record.is_some() {
            return Err(E::Invalid("legacy type reference/union"));
        }
        self.boolean(ty.predefined.is_some())?;
        if let Some(predefined) = ty.predefined {
            self.predefined(predefined)?;
        }
        self.boolean(ty.decimal_type.is_some())?;
        if let Some(bounds) = ty.decimal_type {
            self.decimal_bounds(bounds)?;
        }
        self.boolean(ty.character_string_type.is_some())?;
        if let Some(bounds) = ty.character_string_type {
            self.u64(bounds.min_len)?;
            self.u64(bounds.max_len)?;
        }
        self.boolean(ty.byte_string_type.is_some())?;
        if let Some(bounds) = ty.byte_string_type {
            self.u64(bounds.min_len)?;
            self.u64(bounds.max_len)?;
        }
        self.boolean(ty.list_of.is_some())?;
        if let Some(inner) = &ty.list_of {
            self.value_type(inner, depth + 1)?;
        }
        self.boolean(ty.not_null)?;
        self.u8(match ty.cardinality {
            ValueTypeCardinality::ExactlyOne => 0,
            ValueTypeCardinality::ZeroOrOne => 1,
        })
    }
    fn decimal_bounds(&mut self, bounds: crate::DecimalType) -> CodecResult<()> {
        self.u32(u32::from(bounds.precision))?;
        self.u32(u32::from(bounds.scale))
    }
    fn record_structure(&mut self, record: &R, depth: usize) -> CodecResult<()> {
        self.budget.depth(depth)?;
        metadata::<R>(&mut self.budget)?;
        match record {
            R::Open => self.u8(0),
            R::Closed(fields) => {
                self.u8(1)?;
                self.count(fields.len())?;
                for field in fields {
                    self.text(field.name.as_str())?;
                    self.field_type(&field.field_type, depth + 1)?;
                    self.boolean(field.required)?;
                }
                Ok(())
            }
        }
    }
    fn field_type(&mut self, ty: &T, depth: usize) -> CodecResult<()> {
        self.budget.depth(depth)?;
        metadata::<T>(&mut self.budget)?;
        match ty {
            T::Scalar(v) => {
                if !v.structural_type().is_storable_descriptor() {
                    return Err(E::Semantic);
                }
                self.u8(0)?;
                self.property_kind(*v)
            }
            T::CharacterString(v) => {
                self.u8(1)?;
                self.u64(v.min_len)?;
                self.u64(v.max_len)
            }
            T::Decimal(v) => {
                self.u8(2)?;
                self.decimal_bounds(*v)
            }
            T::ByteString(v) => {
                self.u8(3)?;
                self.u64(v.min_len)?;
                self.u64(v.max_len)
            }
            T::List(v) => {
                self.u8(4)?;
                self.field_type(v, depth + 1)
            }
            T::Record(v) => {
                self.u8(5)?;
                self.record_structure(v, depth + 1)
            }
            T::NotNull(v) => {
                self.u8(6)?;
                self.field_type(v, depth + 1)
            }
        }
    }
}

impl Decoder<'_, '_> {
    /// Decode a complete named graph definition. Owning graph validation follows.
    pub fn graph_definition(&mut self) -> CodecResult<GraphDefinition> {
        self.budget.metadata(1)?;
        let name = self.name()?;
        let count = self.count_for::<(DbString, NodeTypeDef)>()?;
        let mut nodes = Vec::with_capacity(count);
        for _ in 0..count {
            self.budget.metadata(1)?;
            let name = self.name()?;
            let labels = self.labels()?;
            let properties = self.properties_definition()?.into_iter().collect();
            let validation_mode = self.validation_mode()?;
            nodes.push((
                name,
                NodeTypeDef {
                    labels,
                    properties,
                    key: None,
                    validation_mode,
                },
            ));
        }
        let count = self.count_for::<(DbString, EdgeTypeDef)>()?;
        let mut edges = Vec::with_capacity(count);
        for _ in 0..count {
            self.budget.metadata(1)?;
            let name = self.name()?;
            let label = self.name()?;
            let source_node_type = self.endpoint()?;
            let target_node_type = self.endpoint()?;
            let properties = self.properties_definition()?.into_iter().collect();
            let validation_mode = self.validation_mode()?;
            edges.push((
                name,
                EdgeTypeDef {
                    label,
                    source_node_type,
                    target_node_type,
                    properties,
                    validation_mode,
                },
            ));
        }
        Ok(GraphDefinition { name, nodes, edges })
    }
    /// Read an exact database string with the normal database string limit.
    pub fn name(&mut self) -> CodecResult<DbString> {
        db_string(self.text()?).map_err(|_| E::Semantic)
    }
    /// Read a canonical label set; do not normalize malicious duplicate/order input.
    pub fn labels(&mut self) -> CodecResult<LabelSet> {
        let count = self.count()?;
        let mut labels = LabelSet::new();
        let mut previous = None;
        for _ in 0..count {
            let name = self.name()?;
            if previous.as_ref().is_some_and(|p| p >= &name) {
                return Err(E::Invalid("label order"));
            }
            previous = Some(name.clone());
            labels.insert(name);
        }
        Ok(labels)
    }
    fn validation_mode(&mut self) -> CodecResult<ValidationMode> {
        match self.u8()? {
            0 => Ok(ValidationMode::Strict),
            1 => Ok(ValidationMode::Warn),
            _ => Err(E::Invalid("validation mode")),
        }
    }
    fn endpoint(&mut self) -> CodecResult<EdgeEndpointDef> {
        match self.u8()? {
            0 => Ok(EdgeEndpointDef::Any),
            1 => Ok(EdgeEndpointDef::NodeType(NodeTypeRef(self.name()?))),
            2 => {
                let count = self.count()?;
                if count < 2 {
                    return Err(E::Invalid("endpoint arity"));
                }
                let mut refs: smallvec::SmallVec<[NodeTypeRef; 4]> =
                    smallvec::SmallVec::with_capacity(count);
                for _ in 0..count {
                    let name = self.name()?;
                    if refs.last().is_some_and(|p| p.0 >= name) {
                        return Err(E::Invalid("endpoint order"));
                    }
                    refs.push(NodeTypeRef(name));
                }
                Ok(EdgeEndpointDef::OneOf(refs))
            }
            _ => Err(E::Invalid("endpoint tag")),
        }
    }
    fn properties_definition(&mut self) -> CodecResult<Vec<PropertyDef>> {
        let count = self.count_for::<PropertyDef>()?;
        let mut properties = Vec::with_capacity(count);
        for _ in 0..count {
            self.budget.metadata(1)?;
            properties.push(PropertyDef {
                name: self.name()?,
                value_type: self.value_type(1)?,
                nullable: self.boolean()?,
                default: if self.boolean()? {
                    Some(self.value(1)?)
                } else {
                    None
                },
                immutable: self.boolean()?,
                unique: self.boolean()?,
                record_fields: if self.boolean()? {
                    Some(Box::new(self.record_structure(1)?))
                } else {
                    None
                },
            });
        }
        Ok(properties)
    }
    fn value_type(&mut self, depth: usize) -> CodecResult<ValueType> {
        self.budget.depth(depth)?;
        metadata::<ValueType>(self.budget)?;
        Ok(ValueType {
            predefined: if self.boolean()? {
                Some(self.predefined()?)
            } else {
                None
            },
            decimal_type: if self.boolean()? {
                Some(self.decimal_bounds()?)
            } else {
                None
            },
            character_string_type: if self.boolean()? {
                Some(crate::CharacterStringType::new(self.u64()?, self.u64()?).ok_or(E::Semantic)?)
            } else {
                None
            },
            byte_string_type: if self.boolean()? {
                Some(crate::ByteStringType::new(self.u64()?, self.u64()?).ok_or(E::Semantic)?)
            } else {
                None
            },
            list_of: if self.boolean()? {
                Some(Box::new(self.value_type(depth + 1)?))
            } else {
                None
            },
            not_null: self.boolean()?,
            cardinality: match self.u8()? {
                0 => ValueTypeCardinality::ExactlyOne,
                1 => ValueTypeCardinality::ZeroOrOne,
                _ => return Err(E::Invalid("cardinality")),
            },
            record: None,
            union: None,
        })
    }
    fn decimal_bounds(&mut self) -> CodecResult<crate::DecimalType> {
        let precision = u16::try_from(self.u32()?).map_err(|_| E::Semantic)?;
        let scale = u16::try_from(self.u32()?).map_err(|_| E::Semantic)?;
        crate::DecimalType::new(precision, scale).ok_or(E::Semantic)
    }
    fn record_structure(&mut self, depth: usize) -> CodecResult<R> {
        self.budget.depth(depth)?;
        metadata::<R>(self.budget)?;
        match self.u8()? {
            0 => Ok(R::Open),
            1 => {
                let count = self.count()?;
                let mut fields = Vec::with_capacity(count);
                for _ in 0..count {
                    fields.push(RecordFieldStructureDef {
                        name: self.name()?,
                        field_type: self.field_type(depth + 1)?,
                        required: self.boolean()?,
                    });
                }
                Ok(R::Closed(fields))
            }
            _ => Err(E::Invalid("record structure tag")),
        }
    }
    fn field_type(&mut self, depth: usize) -> CodecResult<T> {
        self.budget.depth(depth)?;
        metadata::<T>(self.budget)?;
        Ok(match self.u8()? {
            0 => {
                let kind = self.property_kind()?;
                if !kind.structural_type().is_storable_descriptor() {
                    return Err(E::Semantic);
                }
                T::Scalar(kind)
            }
            1 => T::CharacterString(
                crate::CharacterStringType::new(self.u64()?, self.u64()?).ok_or(E::Semantic)?,
            ),
            2 => T::Decimal(self.decimal_bounds()?),
            3 => T::ByteString(
                crate::ByteStringType::new(self.u64()?, self.u64()?).ok_or(E::Semantic)?,
            ),
            4 => T::List(Box::new(self.field_type(depth + 1)?)),
            5 => T::Record(Box::new(self.record_structure(depth + 1)?)),
            6 => T::NotNull(Box::new(self.field_type(depth + 1)?)),
            _ => return Err(E::Invalid("record field type tag")),
        })
    }
}

fn metadata<U>(budget: &mut super::Budget) -> CodecResult<()> {
    budget.metadata(1)?;
    budget.charge(1, std::mem::size_of::<U>().saturating_mul(4).max(128))
}
