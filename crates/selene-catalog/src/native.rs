//! Native registration contracts without callable code or derived provider state.

use serde::{Deserialize, Serialize};

use crate::{CatalogError, CatalogResult, DeclarationMetadata, DeclarationState};

/// Structural signature types used by the current closed native inventory.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub enum NativeType {
    /// Dynamic value.
    Any,
    /// Dynamic property value.
    AnyProperty,
    /// Boolean.
    Boolean,
    /// Signed integer family.
    Integer,
    /// Signed 64-bit integer.
    Int64,
    /// Unsigned 64-bit integer.
    Uint64,
    /// Floating point family.
    Float,
    /// 64-bit floating point.
    Float64,
    /// String.
    String,
    /// Native vector.
    Vector,
    /// Native JSON.
    Json,
    /// Stable node reference (no physical row).
    NodeRef,
    /// Stable edge reference.
    EdgeRef,
    /// Stable graph reference.
    GraphRef,
    /// Open record value.
    OpenRecord,
    /// Homogeneous list, with at most 64 wrappers at the decoding boundary.
    List(Box<NativeType>),
}

/// Executable logical defaults, never closures or source expressions.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum NativeDefault {
    /// Null.
    Null,
    /// Boolean.
    Boolean(bool),
    /// Signed integer.
    Integer(i64),
    /// String.
    String(String),
}

/// Signature field shared by native parameter and output descriptors.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct NativeField {
    /// Exact analyzed field name.
    pub name: String,
    /// Structural type.
    pub ty: NativeType,
    /// Whether null is accepted/produced.
    pub nullable: bool,
    /// Human-readable documentation.
    pub description: String,
}

/// One positional parameter.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct NativeParameter {
    /// Parameter field.
    pub field: NativeField,
    /// Optional executable default; optional arguments must form a suffix.
    pub default: Option<NativeDefault>,
    /// Documentation-only default spelling.
    pub default_doc: Option<String>,
}

/// Native execution effect/tier pair; invalid combinations are unrepresentable.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum NativeEffect {
    /// Read-only graph tier (ephemeral caches are not catalog writes).
    GraphRead,
    /// Transactional schema-write mutation tier.
    SchemaWrite,
    /// Maintenance tier, still rejected by the selected facade request path.
    MaintenanceWrite,
}

/// One known-code procedure signature. A matching symbol is not activation proof.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct NativeProcedure {
    /// Stable symbolic code binding, independent of opaque runtime handles.
    pub binding: Vec<String>,
    /// Human-readable summary.
    pub description: String,
    /// Version in which the callable surface was introduced.
    pub since_version: String,
    /// Parameters in declaration order.
    pub parameters: Vec<NativeParameter>,
    /// Result fields in declaration order.
    pub outputs: Vec<NativeField>,
    /// Execution effect and tier.
    pub effect: NativeEffect,
}

/// Declarative graph-derived candidate set configuration for first-party attachment.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct NativeCandidateState {
    /// Optional required node label.
    pub required_label: Option<String>,
    /// Required outgoing labels.
    pub require_outgoing: Vec<String>,
    /// Required incoming labels.
    pub require_incoming: Vec<String>,
    /// Disqualifying outgoing labels.
    pub exclude_outgoing: Vec<String>,
    /// Disqualifying incoming labels.
    pub exclude_incoming: Vec<String>,
}

/// Declarative projection configuration, not a cached graph projection.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct NativeProjection {
    /// Node labels, empty means all.
    pub node_labels: Vec<String>,
    /// Edge labels, empty means all.
    pub edge_labels: Vec<String>,
    /// Numeric edge weight key; missing/non-numeric values use the current 1.0 default.
    pub weight_property: Option<String>,
}

/// Storage-neutral binding configuration.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum NativeBinding {
    /// Static known-code procedure.
    Procedure(NativeProcedure),
    /// Graph-owned, rebuildable maintained candidate state.
    CandidateState(NativeCandidateState),
    /// Inactive projection declaration; F04-PR06 owns catalog-backed activation.
    Projection(NativeProjection),
}

/// Typed native declaration. Runtime handles and accelerator payloads are excluded.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct NativeDeclaration {
    /// Shared lifecycle/profile/dependencies.
    pub metadata: DeclarationMetadata,
    /// Declarative binding data.
    pub binding: NativeBinding,
}

impl NativeDeclaration {
    pub(crate) fn validate(&self) -> CatalogResult<()> {
        self.metadata.validate()?;
        let invalid = || CatalogError::InvalidDeclaration {
            reason: "invalid_native_signature",
        };
        match &self.binding {
            NativeBinding::Procedure(procedure) => {
                if procedure.binding.is_empty()
                    || procedure.binding.len() > 8
                    || procedure
                        .binding
                        .iter()
                        .any(|name| name.is_empty() || name.len() > 65_535)
                    || procedure.parameters.len() > 256
                    || procedure.outputs.len() > 256
                    || procedure.since_version.is_empty()
                {
                    return Err(invalid());
                }
                let mut optional = false;
                let mut names = std::collections::BTreeSet::new();
                for parameter in &procedure.parameters {
                    validate_field(&parameter.field)?;
                    if !names.insert(&parameter.field.name)
                        || (optional && parameter.default.is_none())
                    {
                        return Err(invalid());
                    }
                    optional |= parameter.default.is_some();
                    if let Some(default) = &parameter.default {
                        let valid = match default {
                            NativeDefault::Null => parameter.field.nullable,
                            NativeDefault::Boolean(_) => matches!(
                                parameter.field.ty,
                                NativeType::Boolean | NativeType::Any | NativeType::AnyProperty
                            ),
                            NativeDefault::Integer(_) => matches!(
                                parameter.field.ty,
                                NativeType::Integer
                                    | NativeType::Int64
                                    | NativeType::Float
                                    | NativeType::Float64
                                    | NativeType::Any
                                    | NativeType::AnyProperty
                            ),
                            NativeDefault::String(_) => matches!(
                                parameter.field.ty,
                                NativeType::String | NativeType::Any | NativeType::AnyProperty
                            ),
                        };
                        if !valid {
                            return Err(invalid());
                        }
                    }
                }
                names.clear();
                for field in &procedure.outputs {
                    validate_field(field)?;
                    if !names.insert(&field.name) {
                        return Err(invalid());
                    }
                }
            }
            NativeBinding::CandidateState(state)
                if self.metadata.state == DeclarationState::Ready =>
            {
                for label in state.required_label.iter().chain(
                    state
                        .require_outgoing
                        .iter()
                        .chain(&state.require_incoming)
                        .chain(&state.exclude_outgoing)
                        .chain(&state.exclude_incoming),
                ) {
                    if label.is_empty() || selene_core::db_string(label).is_err() {
                        return Err(CatalogError::InvalidDeclaration {
                            reason: "invalid_candidate_state_label",
                        });
                    }
                }
            }
            NativeBinding::CandidateState(_) => {}
            NativeBinding::Projection(_) => {
                if self.metadata.state == DeclarationState::Ready {
                    return Err(CatalogError::InvalidDeclaration {
                        reason: "unsupported_native_activation",
                    });
                }
            }
        }
        Ok(())
    }
}

fn validate_field(field: &NativeField) -> CatalogResult<()> {
    let mut depth = 0;
    let mut ty = &field.ty;
    while let NativeType::List(element) = ty {
        depth += 1;
        if depth > usize::from(crate::native_type::MAX_NATIVE_TYPE_DEPTH) {
            return Err(CatalogError::InvalidDeclaration {
                reason: "native_type_depth",
            });
        }
        ty = element;
    }
    if field.name.is_empty() || field.name.len() > 65_535 {
        return Err(CatalogError::InvalidDeclaration {
            reason: "native_field_name",
        });
    }
    Ok(())
}
