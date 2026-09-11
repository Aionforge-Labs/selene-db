//! Shared selected-snapshot/full-prefix/suffix consumption, without writer authority.

use super::*;
use crate::{
    control::{CURRENT_FILE_NAME, CompatibilityIdentity},
    logical_snapshot::SnapshotContext,
};

/// Pinned read-only physical recovery. The caller must also validate semantics and
/// eagerly reconstruct runtime state; successful framing alone is not readiness.
pub struct RecoveryReader {
    pub(super) reader: LogicalReader,
    boundary_seen: bool,
    complete: bool,
    failed: std::cell::Cell<bool>,
    snapshot_read: std::cell::Cell<bool>,
    prefix_records: u64,
    suffix_records: u64,
}

impl RecoveryReader {
    /// Validated exact selector whose independently opened manifest remains leased.
    pub fn selector(&self) -> &crate::control::CurrentSelector {
        &self.reader.selected.selector
    }
    /// Validated selected snapshot basename.
    pub fn snapshot_name(&self) -> &str {
        &self
            .reader
            .selected
            .checkpoint
            .as_ref()
            .expect("snapshot selection")
            .name
    }
    /// Selected full snapshot extent.
    pub fn snapshot_bytes(&self) -> u64 {
        self.reader
            .selected
            .checkpoint
            .as_ref()
            .expect("snapshot selection")
            .bytes
    }
    /// Validated selected WAL basename, never a diagnostic host path.
    pub fn wal_name(&self) -> String {
        self.reader.selected.log_name()
    }
    /// WAL extent captured under the shared selection epoch. Later appends are excluded.
    pub fn captured_wal_bytes(&self) -> u64 {
        self.reader.captured_bytes
    }
    /// Start offset of the last complete body returned for semantic validation.
    pub fn body_offset(&self) -> u64 {
        self.reader.last_record_offset
    }
    /// Open existing format-2 snapshot control only. Never creates a missing store,
    /// repairs data, adopts orphans, or invokes legacy persistence.
    pub fn open(
        dir: &StoreDirectory,
        expected: &CompatibilityIdentity,
        limit: usize,
    ) -> Result<Self, StreamError> {
        Self::check_initialized(dir)?;
        Self::from_reader(LogicalReader::open(dir, expected, limit)?)
    }

    pub(super) fn check_initialized(dir: &StoreDirectory) -> Result<(), StreamError> {
        if !dir
            .contains(CURRENT_FILE_NAME)
            .map_err(|e| StreamError::from(e).at(CURRENT_FILE_NAME, None, None))?
        {
            for name in dir.entries()? {
                if name != crate::STORE_LOCK_FILE_NAME && name != crate::MANIFEST_LOCK_FILE_NAME {
                    return Err(StreamError::from(crate::PersistError::Control(
                        crate::ControlError::MixedArtifacts(std::path::PathBuf::from(&name)),
                    ))
                    .at(name.to_string_lossy(), None, None));
                }
            }
            return Err(StreamError::from(crate::PersistError::Control(
                crate::ControlError::NotInitialized,
            ))
            .at(CURRENT_FILE_NAME, None, None));
        }
        for name in [crate::STORE_LOCK_FILE_NAME, crate::MANIFEST_LOCK_FILE_NAME] {
            if !dir
                .contains(name)
                .map_err(|e| StreamError::from(e).at(name, None, None))?
            {
                return Err(StreamError::from(crate::PersistError::Control(
                    crate::ControlError::NotInitialized,
                ))
                .at(name, None, None));
            }
        }
        Ok(())
    }

    fn from_reader(reader: LogicalReader) -> Result<Self, StreamError> {
        let snapshot = reader.selected.checkpoint.as_ref().ok_or_else(|| {
            StreamError::MissingSnapshot.at(reader.selected.manifest_name(), None, None)
        })?;
        let boundary_seen =
            snapshot.boundary == reader.position || reader.selected.rotation.is_some();
        Ok(Self {
            reader,
            boundary_seen,
            complete: false,
            failed: std::cell::Cell::new(false),
            snapshot_read: std::cell::Cell::new(false),
            prefix_records: 0,
            suffix_records: 0,
        })
    }

    /// Selected checkpoint boundary and publication ordinal, never inferred from image bytes.
    pub fn snapshot_context(&self) -> SnapshotContext {
        let s = self
            .reader
            .selected
            .checkpoint
            .as_ref()
            .expect("full snapshot selection");
        SnapshotContext {
            boundary: s.boundary,
            publication: s.publication,
        }
    }

    /// Load exact selected image under the retained artifact lease. Fixed-header checks
    /// and the aggregate encoded-byte ceiling precede the full allocation.
    pub fn snapshot_body(&self) -> Result<Vec<u8>, StreamError> {
        if self.failed.get() {
            return Err(StreamError::Terminated);
        }
        let result = self.reader.snapshot_body();
        if result.is_err() {
            self.failed.set(true);
        } else {
            self.snapshot_read.set(true);
        }
        result
    }

    /// Verify the selected segment from its declared base, returning suffix bodies
    /// for semantic replay. PR05 selections still verify their complete prefix;
    /// rotating selections start at the snapshot-covered global sequence instead.
    pub fn next_body(&mut self) -> Result<Option<Vec<u8>>, StreamError> {
        if self.failed.get() {
            return Err(StreamError::Terminated);
        }
        let result = self.next_checked();
        if result.is_err() {
            self.failed.set(true);
        }
        result
    }

    fn next_checked(&mut self) -> Result<Option<Vec<u8>>, StreamError> {
        if self.complete {
            return Ok(None);
        }
        let boundary = self.snapshot_context().boundary;
        while let Some(body) = self.reader.next_body()? {
            let position = self.reader.position;
            if position.sequence <= boundary.sequence {
                self.prefix_records += 1;
                if position.sequence == boundary.sequence {
                    if position != boundary {
                        return Err(
                            StreamError::from(logical_frame::FrameError::SnapshotBoundary).at(
                                self.wal_name(),
                                Some(position.offset),
                                Some(boundary.sequence),
                            ),
                        );
                    }
                    self.boundary_seen = true;
                }
            } else {
                if !self.boundary_seen {
                    return Err(
                        StreamError::from(logical_frame::FrameError::SnapshotBoundary).at(
                            self.wal_name(),
                            Some(position.offset),
                            Some(boundary.sequence),
                        ),
                    );
                }
                self.suffix_records += 1;
                return Ok(Some(body));
            }
        }
        if self.reader.incomplete_tail() {
            // A selected PR05 snapshot proves its covered prefix must be complete.
            // EOF before that boundary is required/interior damage, not an unsealed
            // suffix whose prior acknowledgment remains unknown.
            let error = if self.boundary_seen {
                StreamError::IncompleteTail
            } else {
                logical_frame::FrameError::CorruptIncomplete.into()
            };
            return Err(error.at(
                self.wal_name(),
                Some(self.position().offset),
                self.position().sequence.checked_add(1),
            ));
        }
        if !self.boundary_seen {
            let position = self.position();
            // Clean EOF can still be missing required prefix bytes: truncation
            // at zero or an earlier complete-record end leaves no partial frame.
            // A present expected sequence is checked against the full boundary
            // above and remains lineage failure, even if its declared offset is wrong.
            let error =
                if position.sequence < boundary.sequence && position.offset < boundary.offset {
                    logical_frame::FrameError::CorruptIncomplete
                } else {
                    logical_frame::FrameError::SnapshotBoundary
                };
            return Err(StreamError::from(error).at(
                self.wal_name(),
                Some(position.offset),
                position.sequence.checked_add(1),
            ));
        }
        self.complete = true;
        Ok(None)
    }

    /// Verified prefix records, counted separately from semantically replayed suffix records.
    pub fn prefix_records(&self) -> u64 {
        self.prefix_records
    }
    /// Complete suffix transactions returned to the semantic replay owner.
    pub fn suffix_records(&self) -> u64 {
        self.suffix_records
    }
    /// Current independently verified full-record cursor.
    pub fn position(&self) -> Position {
        self.reader.position
    }

    /// Require successful snapshot and complete segment consumption. This proves
    /// physical consumption only; the owning facade must also validate all semantics.
    pub fn finish(&self) -> Result<(), StreamError> {
        if self.failed.get() || !self.complete || !self.snapshot_read.get() {
            return Err(StreamError::Protocol("reopen consumption incomplete"));
        }
        Ok(())
    }
}
