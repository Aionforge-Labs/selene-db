//! Single-shot physical controls over borrowed session authority.
//!
//! Control operations have no row input and never acquire a writer themselves.
//! The selected facade receives the same prepared command and remains the only
//! publication authority; bare engine sessions use their existing services.
//! In particular there is no cancellation check after COMMIT: a durable or
//! indeterminate outcome must not be rewritten into a cancellation diagnosis.

use crate::{
    ExecutionPlan, PipelineOp, ProcedureRegistry, SessionOp, TxOp,
    runtime::{
        ExecutorError, PreparedSessionControl, PreparedTransactionControl, Session, StatementOutput,
    },
};

mod session;
mod tx;

/// A physical control lowered from the effect-verified executable plan.
pub(crate) enum PhysicalControl<'p> {
    Transaction(&'p TxOp),
    Session(&'p SessionOp),
}

impl<'p> PhysicalControl<'p> {
    pub(crate) fn lower(plan: &'p ExecutionPlan) -> Result<Self, ExecutorError> {
        match plan.pipeline.as_slice() {
            [PipelineOp::Tx(op)] => Ok(Self::Transaction(op)),
            [PipelineOp::Session(op)] => Ok(Self::Session(op)),
            _ => Err(ExecutorError::ImplementationDefined {
                detail: "control plan must contain exactly one control operation",
            }),
        }
    }

    pub(crate) fn transaction(&self) -> Result<PreparedTransactionControl, ExecutorError> {
        match self {
            Self::Transaction(TxOp::Start { .. }) => Ok(PreparedTransactionControl::Start),
            Self::Transaction(TxOp::Commit { .. }) => Ok(PreparedTransactionControl::Commit),
            Self::Transaction(TxOp::Rollback { .. }) => Ok(PreparedTransactionControl::Rollback),
            Self::Session(_) => Err(ExecutorError::ImplementationDefined {
                detail: "session control used as a transaction control",
            }),
        }
    }

    pub(crate) fn prepare_session(
        self,
        session: &Session<'_>,
        registry: &dyn ProcedureRegistry,
        skip_if_exists: bool,
    ) -> Result<PreparedSessionControl, ExecutorError> {
        match self {
            Self::Session(op) => session::prepare(op, session, registry, skip_if_exists),
            Self::Transaction(_) => Err(ExecutorError::ImplementationDefined {
                detail: "transaction control used as a session control",
            }),
        }
    }

    pub(crate) fn execute(
        self,
        session: &mut Session<'_>,
        registry: &dyn ProcedureRegistry,
    ) -> Result<StatementOutput, ExecutorError> {
        match self {
            Self::Transaction(op) => tx::execute(op, session),
            Self::Session(op) => session::execute(op, session, registry),
        }
    }
}
