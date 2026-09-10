//! Bounded query-only-family rejection for legacy schema descriptor adapters.

use crate::{
    CoreResult, MAX_STRUCTURAL_TYPE_DEPTH, PredefinedValueType as P, RecordFieldStructure as R,
    RecordFieldStructureType as F, StoredValueError, ValueType,
};

fn query_only() -> crate::CoreError {
    StoredValueError::QueryOnly {
        family: "property descriptor",
    }
    .into()
}

impl ValueType {
    pub(crate) fn validate_stored_descriptor(&self) -> CoreResult<()> {
        let mut pending = vec![(self, 1)];
        while let Some((ty, depth)) = pending.pop() {
            if depth > MAX_STRUCTURAL_TYPE_DEPTH {
                return Err(StoredValueError::DepthLimit.into());
            }
            if matches!(
                ty.predefined,
                Some(
                    P::NodeRef | P::EdgeRef | P::GraphRef | P::TableRef | P::Path | P::Extended(_)
                )
            ) {
                return Err(query_only());
            }
            if let Some(element) = &ty.list_of {
                pending.push((element, depth + 1));
            }
            if let Some(members) = &ty.union {
                pending.extend(members.iter().map(|ty| (ty, depth + 1)));
            }
        }
        Ok(())
    }
}

impl R {
    pub(crate) fn validate_stored_descriptor(&self) -> CoreResult<()> {
        let mut pending = Vec::new();
        if let Self::Closed(fields) = self {
            pending.extend(fields.iter().map(|field| (&field.field_type, 1)));
        }
        while let Some((ty, depth)) = pending.pop() {
            if depth > MAX_STRUCTURAL_TYPE_DEPTH {
                return Err(StoredValueError::DepthLimit.into());
            }
            match ty {
                F::Scalar(scalar) if !scalar.structural_type().is_storable_descriptor() => {
                    return Err(query_only());
                }
                F::List(element) | F::NotNull(element) => pending.push((element, depth + 1)),
                F::Record(record) => {
                    if let R::Closed(fields) = record.as_ref() {
                        pending.extend(fields.iter().map(|field| (&field.field_type, depth + 1)));
                    }
                }
                _ => {}
            }
        }
        Ok(())
    }
}
