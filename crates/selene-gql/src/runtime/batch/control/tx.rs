//! Transaction services for physical controls; never owns a second writer.

use crate::{
    SourceSpan, TxOp,
    runtime::{ExecutorError, Session, StatementOutput},
};

/// Execute one top-level transaction-control operation.
pub(crate) fn execute(
    op: &TxOp,
    session: &mut Session<'_>,
) -> Result<StatementOutput, ExecutorError> {
    match op {
        TxOp::Start { span } => start(session, *span),
        TxOp::Commit { span } => commit(session, *span),
        TxOp::Rollback { span } => rollback(session, *span),
    }
}

fn start(session: &mut Session<'_>, span: SourceSpan) -> Result<StatementOutput, ExecutorError> {
    session
        .start_transaction()
        .map(|()| StatementOutput::Empty)
        .map_err(|err| rewrite_span(err, span))
}

fn commit(session: &mut Session<'_>, span: SourceSpan) -> Result<StatementOutput, ExecutorError> {
    session
        .commit_transaction()
        .map(|outcome| StatementOutput::Written(outcome.into_write_outcome()))
        .map_err(|err| rewrite_span(err, span))
}

fn rollback(session: &mut Session<'_>, span: SourceSpan) -> Result<StatementOutput, ExecutorError> {
    session
        .rollback_transaction()
        .map(|_outcome| StatementOutput::Empty)
        .map_err(|err| rewrite_span(err, span))
}

fn rewrite_span(err: ExecutorError, span: SourceSpan) -> ExecutorError {
    use ExecutorError::{
        GraphMutation, InFailedTransaction, NoActiveTransaction, TransactionAlreadyActive,
    };
    match err {
        TransactionAlreadyActive { .. } => TransactionAlreadyActive { span },
        NoActiveTransaction { .. } => NoActiveTransaction { span },
        InFailedTransaction { .. } => InFailedTransaction { span },
        GraphMutation { source, .. } => GraphMutation { source, span },
        other => other,
    }
}
