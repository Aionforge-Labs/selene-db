use super::*;

#[test]
fn artifact_context_is_bounded_and_routine_diagnostics_do_not_expose_source_paths() {
    let cause = std::io::Error::new(
        std::io::ErrorKind::PermissionDenied,
        "/private/unrelated/secret-payload",
    );
    let error = StorageError::stream(
        StoragePhase::Select,
        StreamError::Artifact {
            name: "/unrelated/\nMANIFEST.control".into(),
            offset: Some(12),
            expected_sequence: Some(7),
            source: Box::new(StreamError::Io(cause)),
        },
    );
    assert_eq!(error.kind, StorageErrorKind::Io);
    assert_eq!(error.artifact.as_deref(), Some("\\nMANIFEST.control"));
    assert_eq!((error.offset, error.expected_sequence), (Some(12), Some(7)));
    let text = format!("{error} {error:?}");
    assert!(!text.contains("secret-payload") && !text.contains("/unrelated"));
    let stream = error
        .source()
        .unwrap()
        .downcast_ref::<StreamError>()
        .unwrap();
    let StreamError::Io(io) = stream else {
        panic!("original I/O chain")
    };
    assert_eq!(io.kind(), std::io::ErrorKind::PermissionDenied);
    let long =
        StorageError::invalid(StoragePhase::Replay, "test").at(&"\n".repeat(4096), None, None);
    assert!(long.artifact.unwrap().len() <= 256);
}

#[test]
fn typed_sequence_context_survives_the_public_boundary_without_text_parsing() {
    let error = StorageError::stream(
        StoragePhase::Replay,
        selene_persist::logical_frame::FrameError::Sequence {
            expected: 7,
            observed: 6,
        }
        .into(),
    );
    assert_eq!(error.kind, StorageErrorKind::SequenceOverlap);
    assert_eq!(
        (error.expected_sequence, error.observed_sequence),
        (Some(7), Some(6))
    );
}
