//! Detached provider preparation must not escape a canceled outer publication.
use crate::*;

#[test]
fn canceled_candidate_attachment_and_replacement_preserve_prior_runtime() {
    let db = Database::builder().build();
    let schema = SchemaPath::regular("selene", "providers").unwrap();
    let path = ObjectPath::regular("selene", "providers", "memory").unwrap();
    db.catalog()
        .create_schema(&schema, CreatePolicy::Strict)
        .unwrap();
    db.catalog()
        .create_graph(&path, None, CreatePolicy::Strict)
        .unwrap();
    let name = PathSegment::delimited("current").unwrap();
    let define = |label: &str| {
        DeclarationDefinition::Native(NativeDeclaration {
            metadata: DeclarationMetadata::new(DeclarationState::Ready),
            binding: NativeBinding::CandidateState(NativeCandidateState {
                required_label: Some(label.into()),
                require_outgoing: vec![],
                require_incoming: vec![],
                exclude_outgoing: vec![],
                exclude_incoming: vec![],
            }),
        })
    };
    let s = db.session(&path).unwrap();
    s.execute("INSERT (:Memory {body: 'memory'})").unwrap();
    s.execute("CALL selene.create_text_index('Memory', 'body')")
        .unwrap();
    let source = "CALL selene.text_score_candidate_state('Memory', 'body', 'memory', 'current', 10) YIELD node_id, score";
    for replacing in [false, true] {
        let before = db.catalog().snapshot();
        *db.catalog().inner.failure.lock() = Some(crate::catalog::FailurePoint::BeforePublication);
        let error = db
            .catalog()
            .declare(&path, &name, define("Absent"), CreatePolicy::OrReplace)
            .unwrap_err();
        assert_eq!(error.kind(), ErrorKind::MutationCanceled);
        assert!(before.shares_state_with(&db.catalog().snapshot()));
        if replacing {
            let ExecutionOutcome::Rows { result, .. } = s.execute(source).unwrap() else {
                panic!("rows")
            };
            assert_eq!(result.rows().len(), 1);
        } else {
            assert_eq!(
                s.execute(source).unwrap_err().gqlstatus().unwrap().as_str(),
                "22G03"
            );
            db.catalog()
                .declare(&path, &name, define("Memory"), CreatePolicy::Strict)
                .unwrap();
        }
    }
}
