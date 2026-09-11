//! Writer admission around the shared read-only recovery pipeline.

use super::*;
use crate::control::CompatibilityIdentity;

/// Existing writer proof and the same pinned recovery reader used by verification.
pub struct ReopeningWal {
    recovery: RecoveryReader,
    authority: StoreWriter,
}

impl ReopeningWal {
    /// Acquire existing exclusive writer ownership, then select an artifact view.
    /// Never creates missing coordination state or repairs authoritative bytes.
    pub fn open(
        dir: &StoreDirectory,
        expected: &CompatibilityIdentity,
        limit: usize,
    ) -> Result<Self, StreamError> {
        // Validate the read prerequisites before opening any file writable.
        RecoveryReader::check_initialized(dir)?;
        let authority = StoreWriter::acquire_existing(dir)
            .map_err(|e| StreamError::from(e).at(crate::STORE_LOCK_FILE_NAME, None, None))?;
        let recovery = RecoveryReader::open(dir, expected, limit)?;
        Ok(Self {
            recovery,
            authority,
        })
    }

    /// Shared physical read/validation pipeline, without write methods.
    pub fn recovery(&mut self) -> &mut RecoveryReader {
        &mut self.recovery
    }

    /// Selected snapshot boundary and publication ordinal.
    pub fn snapshot_context(&self) -> crate::logical_snapshot::SnapshotContext {
        self.recovery.snapshot_context()
    }
    /// Load the exact selected snapshot; errors terminate recovery.
    pub fn snapshot_body(&self) -> Result<Vec<u8>, StreamError> {
        self.recovery.snapshot_body()
    }
    /// Verify prefix and return complete suffix transactions.
    pub fn next_body(&mut self) -> Result<Option<Vec<u8>>, StreamError> {
        self.recovery.next_body()
    }
    /// Physically verified prefix record count.
    pub fn prefix_records(&self) -> u64 {
        self.recovery.prefix_records()
    }
    /// Complete suffix record count returned for semantic replay.
    pub fn suffix_records(&self) -> u64 {
        self.recovery.suffix_records()
    }
    /// Independently verified complete cursor.
    pub fn position(&self) -> Position {
        self.recovery.position()
    }

    /// Synchronize and establish append ownership only after complete consumption
    /// and the caller's semantic/runtime validation. Never truncates a tail.
    pub fn finish(self) -> Result<LogicalWal, StreamError> {
        self.recovery.finish()?;
        let position = self.recovery.position();
        let reader = self.recovery.reader;
        let name = reader.selected.log_name();
        let file = (|| -> Result<File, StreamError> {
            let dir = self.authority.directory();
            let mut file = dir.open_write(std::path::Path::new(&name))?;
            if file.metadata()?.len() != position.offset {
                return Err(StreamError::Protocol("WAL changed during reopen"));
            }
            dir.check_fault("reopen.seek")?;
            file.seek(SeekFrom::Start(position.offset))?;
            dir.check_fault("reopen.sync")?;
            file.sync_all()?;
            Ok(file)
        })()
        .map_err(|e| e.at(name, Some(position.offset), Some(position.sequence)))?;
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
            selected: reader.selected,
            #[cfg(any(test, feature = "test-harness"))]
            fault: None,
        })
    }
}
