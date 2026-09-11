//! Independent ACK/canceled/unknown identity model over the actual public durable facade.
use super::*;
use crate::{ExecutionOutcome, Value};
use selene_persist::logical_stream::Fault;

#[test]
fn public_recovery_ack_set_model_includes_whole_committed_and_excludes_proven_canceled() {
    for scenario in 0..6 {
        let dir = tempfile::tempdir().unwrap();
        let db = Database::create(dir.path()).unwrap();
        let graph = ObjectPath::regular("selene", "model", "data").unwrap();
        db.catalog()
            .create_schema(
                &SchemaPath::regular("selene", "model").unwrap(),
                CreatePolicy::Strict,
            )
            .unwrap();
        db.catalog()
            .create_graph(&graph, None, CreatePolicy::Strict)
            .unwrap();
        let s = db.session(&graph).unwrap();
        // Independent acknowledged identities and property sets, not replay counts.
        s.execute(
            "INSERT (:Txn {id: 1, part: 0, value: 100}), (:Txn {id: 1, part: 1, value: 101})",
        )
        .unwrap();
        db.checkpoint().unwrap();
        s.start_transaction(TransactionAccessMode::ReadWrite)
            .unwrap();
        s.execute("INSERT (:Txn {id: 99, part: 0, value: 9900})")
            .unwrap();
        s.execute("INSERT (:Txn {id: 99, part: 1, value: 9901})")
            .unwrap();
        s.rollback_transaction().unwrap();
        s.start_transaction(TransactionAccessMode::ReadWrite)
            .unwrap();
        s.execute("INSERT (:Txn {id: 2, part: 0, value: 200})")
            .unwrap();
        s.execute("INSERT (:Txn {id: 2, part: 1, value: 201})")
            .unwrap();
        if scenario < 3 {
            *db.inner.failure.lock() = Some(match scenario {
                0 => FailurePoint::BeforeAuthorityPrepare,
                1 => FailurePoint::BeforePublication,
                _ => FailurePoint::AfterPublicationAcknowledgement,
            });
        } else {
            db.inner
                .transactions
                .durable
                .lock()
                .as_mut()
                .unwrap()
                .wal
                .inject_fault(match scenario {
                    3 => Fault::PartialAppend,
                    4 => Fault::CleanupSync,
                    _ => Fault::Truncate,
                });
        }
        let error = s.commit_transaction().unwrap_err();
        let committed = matches!(scenario, 1 | 2);
        let uncertain = scenario >= 4;
        assert_eq!(
            error.durable_commit_outcome().unwrap().state,
            if committed {
                DurableCommitState::CommittedUnacknowledged
            } else if uncertain {
                DurableCommitState::Uncertain
            } else {
                DurableCommitState::Canceled
            }
        );
        drop(s);
        drop(db);
        if scenario == 5 {
            // Uncertain partial append is not a supported whole-state recovery.
            // It stays damaged for inspection, never relabeled canceled/salvaged.
            assert_eq!(
                Database::verify(dir.path()).unwrap_err().kind,
                crate::StorageErrorKind::IncompleteTail
            );
            assert_eq!(
                Database::open(dir.path()).err().unwrap().kind,
                crate::StorageErrorKind::IncompleteTail
            );
            continue;
        }
        let report = Database::verify(dir.path()).unwrap();
        let db = Database::open(dir.path()).unwrap();
        assert_eq!(
            report.recovery.position,
            db.recovery_info().unwrap().position
        );
        let ExecutionOutcome::Rows { result, .. } = db
            .session(&graph)
            .unwrap()
            .execute("MATCH (n:Txn) RETURN n.id, n.part, n.value ORDER BY n.id, n.part")
            .unwrap()
        else {
            panic!("rows")
        };
        let mut expected = vec![
            vec![Value::Int(1), Value::Int(0), Value::Int(100)],
            vec![Value::Int(1), Value::Int(1), Value::Int(101)],
        ];
        if committed {
            expected.extend([
                vec![Value::Int(2), Value::Int(0), Value::Int(200)],
                vec![Value::Int(2), Value::Int(1), Value::Int(201)],
            ]);
        }
        assert_eq!(
            result
                .rows()
                .iter()
                .map(|r| r.values().to_vec())
                .collect::<Vec<_>>(),
            expected
        );
        assert_eq!(report.nodes, expected.len());
        db.checkpoint().unwrap();
        db.checkpoint().unwrap();
        assert!(db.prune().unwrap().cleanup_error.is_none());
    }
}
