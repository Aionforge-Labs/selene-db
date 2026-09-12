//! Errors for retained filesystem authority and format-2 control.

/// Persistence operation result.
pub type PersistResult<T> = Result<T, PersistError>;

/// Retired artifact recognized only by a bounded, read-only header probe.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum PersistArtifact {
    /// Old SLDB WAL or archive.
    Wal,
    /// Old SLSN snapshot (not SLSNP2).
    Snapshot,
    /// Old SLMF manifest.
    Manifest,
    /// Old SLAU audit log; audit is excluded from the preview.
    AuditLog,
}
impl std::fmt::Display for PersistArtifact {
    fn fmt(&self, out: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        out.write_str(match self {
            Self::Wal => "wal",
            Self::Snapshot => "snapshot",
            Self::Manifest => "MANIFEST",
            Self::AuditLog => "audit log",
        })
    }
}

/// Filesystem/control failure. Logical frame and stream errors retain their own types.
#[derive(Debug, thiserror::Error, miette::Diagnostic)]
#[non_exhaustive]
pub enum PersistError {
    /// Managed basename diagnostic, never filesystem authority.
    #[error("selected artifact {name:?}: {source}")]
    Artifact {
        /// Managed basename.
        name: String,
        /// Original typed cause.
        source: Box<PersistError>,
    },
    /// Retained-directory or platform contract failure.
    #[error(transparent)]
    Directory(#[from] crate::DirectoryError),
    /// Format-2 control failure.
    #[error(transparent)]
    Control(#[from] crate::ControlError),
    /// Native I/O failure.
    #[error("persistence io: {0}")]
    #[diagnostic(code(SLENE_P_001))]
    Io(#[from] std::io::Error),
    /// Recognized retired artifact, without decoding or validating its payload.
    #[error("{artifact} version unsupported: {major}.{minor}")]
    #[diagnostic(code(SLENE_P_008))]
    UnsupportedVersion {
        /// Recognized artifact family.
        artifact: PersistArtifact,
        /// Observed major version, zero when the prefix is incomplete.
        major: u16,
        /// Observed minor version, zero when absent.
        minor: u16,
    },
    /// Another handle or process owns the permanent writer lock.
    #[error("store writer lock is held by another process or handle")]
    #[diagnostic(code(SLENE_P_013))]
    WriterLockHeld,
}
impl PersistError {
    /// Five-character GQLSTATUS for the lower persistence diagnostic.
    #[must_use]
    pub const fn gqlstatus(&self) -> &'static str {
        match self {
            Self::Artifact { source, .. } => source.gqlstatus(),
            Self::UnsupportedVersion { .. } => "08000",
            Self::Directory(_) | Self::Control(_) | Self::Io(_) | Self::WriterLockHeld => "5GQL0",
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn gqlstatus_for_retained_errors_and_artifact_context() {
        for error in [
            PersistError::WriterLockHeld,
            std::io::Error::other("io").into(),
            crate::ControlError::Checksum.into(),
            crate::DirectoryError::NotDirectory.into(),
        ] {
            assert_eq!(error.gqlstatus(), "5GQL0");
        }
        let error = PersistError::Artifact {
            name: "wal.log".into(),
            source: Box::new(PersistError::UnsupportedVersion {
                artifact: PersistArtifact::Wal,
                major: 3,
                minor: 1,
            }),
        };
        assert_eq!(error.gqlstatus(), "08000");
    }
}
