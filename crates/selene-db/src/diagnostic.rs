//! Stable facade GQL status objects and diagnostic bundles.

use std::borrow::Cow;

use crate::{Error, GqlStatus};

/// One public GQL status object with ordered nested causes.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GqlStatusObject {
    status: GqlStatus,
    message: Cow<'static, str>,
    causes: Vec<GqlStatusObject>,
}

impl GqlStatusObject {
    /// Construct a status object without nested causes.
    #[must_use]
    pub fn new(status: GqlStatus, message: impl Into<String>) -> Self {
        Self {
            status,
            message: Cow::Owned(message.into()),
            causes: Vec::new(),
        }
    }

    pub(crate) const fn static_message(status: GqlStatus, message: &'static str) -> Self {
        Self {
            status,
            message: Cow::Borrowed(message),
            causes: Vec::new(),
        }
    }

    /// Attach all nested causes in deterministic production order.
    #[must_use]
    pub fn with_causes(mut self, causes: Vec<Self>) -> Self {
        self.causes = causes;
        self
    }

    /// Return this object's GQLSTATUS code.
    #[must_use]
    pub const fn status(&self) -> GqlStatus {
        self.status
    }

    /// Borrow the human-readable diagnostic message.
    #[must_use]
    pub fn message(&self) -> &str {
        &self.message
    }

    /// Borrow every nested cause in production order.
    #[must_use]
    pub fn causes(&self) -> &[Self] {
        &self.causes
    }

    pub(crate) fn from_engine(status: &selene_gql::GqlStatusObject) -> Self {
        Self {
            status: GqlStatus::from_engine(status.status()),
            message: Cow::Owned(status.message().to_owned()),
            causes: status.causes().iter().map(Self::from_engine).collect(),
        }
    }
}

/// One primary status plus every ordered additional status produced by a request.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DiagnosticBundle {
    primary: GqlStatusObject,
    additional: Vec<GqlStatusObject>,
}

impl DiagnosticBundle {
    /// Construct an explicit complete diagnostic chain.
    #[must_use]
    pub const fn new(primary: GqlStatusObject, additional: Vec<GqlStatusObject>) -> Self {
        Self {
            primary,
            additional,
        }
    }

    /// Borrow the primary status object selected by outcome precedence.
    #[must_use]
    pub const fn primary(&self) -> &GqlStatusObject {
        &self.primary
    }

    /// Borrow all additional status objects in deterministic production order.
    #[must_use]
    pub fn additional(&self) -> &[GqlStatusObject] {
        &self.additional
    }

    pub(crate) fn from_engine(bundle: &selene_gql::DiagnosticBundle) -> Self {
        Self {
            primary: GqlStatusObject::from_engine(bundle.primary()),
            additional: bundle
                .additional()
                .iter()
                .map(GqlStatusObject::from_engine)
                .collect(),
        }
    }

    pub(crate) fn from_error_and_engine_statuses(
        error: &Error,
        additional: &[selene_gql::GqlStatusObject],
    ) -> Self {
        let status = error
            .gqlstatus()
            .unwrap_or(GqlStatus::IMPLEMENTATION_DEFINED_ERROR);
        let mut causes = Vec::new();
        let mut source = std::error::Error::source(error);
        while let Some(cause) = source {
            if let Some(cause) = cause.downcast_ref::<Error>()
                && let Some(status) = cause.gqlstatus()
            {
                causes.push(GqlStatusObject::new(status, cause.message()));
            }
            source = cause.source();
        }
        Self::new(
            GqlStatusObject::new(status, error.message()).with_causes(causes),
            additional
                .iter()
                .map(GqlStatusObject::from_engine)
                .collect(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn public_bundle_preserves_all_additional_and_nested_statuses() {
        let causes = vec![
            selene_gql::GqlStatusObject::new(selene_gql::GqlStatus::INVALID_REFERENCE, "first"),
            selene_gql::GqlStatusObject::new(selene_gql::GqlStatus::DATA_EXCEPTION, "second"),
        ];
        let primary = selene_gql::GqlStatusObject::new(
            selene_gql::GqlStatus::FEATURE_NOT_SUPPORTED,
            "primary",
        )
        .with_causes(causes.clone());
        let additional = vec![
            selene_gql::GqlStatusObject::new(selene_gql::GqlStatus::NO_DATA, "no data"),
            selene_gql::GqlStatusObject::new(
                selene_gql::GqlStatus::NULL_VALUE_ELIMINATED_IN_SET_FUNCTION,
                "warning",
            ),
        ];
        let lower = selene_gql::DiagnosticBundle::new(primary, additional);
        let bundle = DiagnosticBundle::from_engine(&lower);

        assert_eq!(bundle.primary().status(), GqlStatus::FEATURE_NOT_SUPPORTED);
        assert_eq!(bundle.primary().causes()[0].message(), "first");
        assert_eq!(bundle.primary().causes()[1].message(), "second");
        assert_eq!(bundle.additional()[0].status(), GqlStatus::NO_DATA);
        assert_eq!(
            bundle.additional()[1].status(),
            GqlStatus::NULL_VALUE_ELIMINATED_IN_SET_FUNCTION
        );
    }
}
