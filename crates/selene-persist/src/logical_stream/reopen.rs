//! Non-destructive writer reopen: retain LOCK and a selected artifact lease through use.

use super::*;
use crate::{
    control::{CURRENT_FILE_NAME, CompatibilityIdentity},
    logical_snapshot::SnapshotContext,
};

/// Isolated recovery owner. No database is usable until semantic validation and
/// eager runtime rebuilding finish and the caller consumes [`Self::finish`].
pub struct ReopeningWal {
    reader: LogicalReader,
    authority: StoreWriter,
    boundary_seen: bool,
    complete: bool,
    failed: bool,
    prefix_records: u64,
    suffix_records: u64,
}

impl ReopeningWal {
    /// Open existing format-2 snapshot control only. Never creates a missing store,
    /// repairs data, adopts orphans, or invokes legacy persistence.
    pub fn open(
        dir: &StoreDirectory,
        expected: &CompatibilityIdentity,
        limit: usize,
    ) -> Result<Self, StreamError> {
        if !dir.contains(CURRENT_FILE_NAME)? {
            for name in dir.entries()? {
                if name != crate::STORE_LOCK_FILE_NAME && name != crate::MANIFEST_LOCK_FILE_NAME {
                    return Err(
                        crate::PersistError::Control(crate::ControlError::MixedArtifacts(
                            name.into(),
                        ))
                        .into(),
                    );
                }
            }
            return Err(StreamError::Protocol("store is not initialized"));
        }
        if !dir.contains(crate::STORE_LOCK_FILE_NAME)?
            || !dir.contains(crate::MANIFEST_LOCK_FILE_NAME)?
        {
            return Err(StreamError::Protocol("store is not initialized"));
        }
        let authority = StoreWriter::acquire_existing(dir)?;
        let reader = LogicalReader::open(dir, expected, limit)?;
        let snapshot = reader
            .selected
            .checkpoint
            .as_ref()
            .ok_or(StreamError::Protocol("missing initial database snapshot"))?;
        let boundary_seen =
            snapshot.boundary == reader.position || reader.selected.rotation.is_some();
        Ok(Self {
            reader,
            authority,
            boundary_seen,
            complete: false,
            failed: false,
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
        self.reader.snapshot_body()
    }

    /// Verify the selected segment from its declared base, returning suffix bodies
    /// for semantic replay. PR05 selections still verify their complete prefix;
    /// rotating selections start at the snapshot-covered global sequence instead.
    pub fn next_body(&mut self) -> Result<Option<Vec<u8>>, StreamError> {
        if self.failed {
            return Err(StreamError::Protocol("reopen reader terminated by failure"));
        }
        let result = self.next_checked();
        if result.is_err() {
            self.failed = true;
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
                        return Err(StreamError::Protocol("checkpoint WAL boundary mismatch"));
                    }
                    self.boundary_seen = true;
                }
            } else {
                if !self.boundary_seen {
                    return Err(StreamError::Protocol("checkpoint boundary absent from WAL"));
                }
                self.suffix_records += 1;
                return Ok(Some(body));
            }
        }
        if self.reader.incomplete_tail() {
            return Err(StreamError::Protocol(
                "incomplete authoritative WAL tail; no repair",
            ));
        }
        if !self.boundary_seen {
            return Err(StreamError::Protocol("checkpoint boundary absent from WAL"));
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

    /// Establish synchronized append state after the caller has rebuilt and
    /// validated its entire runtime. Complete recovered records are retained even
    /// when their prior live acknowledgment cannot be proved. No truncation occurs.
    pub fn finish(self) -> Result<LogicalWal, StreamError> {
        if self.failed || !self.complete {
            return Err(StreamError::Protocol("reopen consumption incomplete"));
        }
        let position = self.reader.position;
        let mut file = self
            .authority
            .directory()
            .open_write(std::path::Path::new(&self.reader.selected.log_name()))?;
        if file.metadata()?.len() != position.offset {
            return Err(StreamError::Protocol("WAL changed during reopen"));
        }
        file.seek(SeekFrom::Start(position.offset))?;
        file.sync_all()?;
        Ok(LogicalWal {
            authority: self.authority,
            file,
            progress: Progress {
                written: position,
                synchronized: position,
                published: Some(position),
                acknowledged: None,
            },
            fenced: false,
            selected: self.reader.selected,
            #[cfg(any(test, feature = "test-harness"))]
            fault: None,
        })
    }
}
