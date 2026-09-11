//! Snapshot reads protected by the selected manifest's owned artifact lease.
use super::*;
use crate::logical_snapshot::{self, SnapshotContext};
use std::io::Read;

impl LogicalReader {
    /// Selected image boundary, or None for a lower-level WAL without an image.
    pub fn snapshot_context(&self) -> Option<SnapshotContext> {
        self.selected.checkpoint.as_ref().map(|s| SnapshotContext {
            boundary: s.boundary,
            publication: s.publication,
        })
    }

    /// Load the selected snapshot while its manifest lease keeps the name and
    /// dependencies retained. Check expected identity, bounded length and full hash.
    pub fn snapshot_body(&self) -> Result<Vec<u8>, StreamError> {
        if self.failed.get() {
            return Err(StreamError::Terminated);
        }
        let result = self.read_snapshot();
        if result.is_err() {
            self.failed.set(true);
        }
        result.map_err(|e| match &self.selected.checkpoint {
            Some(s) => e.at(&s.name, Some(0), Some(s.boundary.sequence)),
            None => e,
        })
    }

    fn read_snapshot(&self) -> Result<Vec<u8>, StreamError> {
        let s = self
            .selected
            .checkpoint
            .as_ref()
            .ok_or(StreamError::MissingSnapshot)?;
        let mut file = self.directory.open_read(&s.name)?;
        let actual = file.metadata()?.len();
        if actual < s.bytes {
            return Err(logical_frame::FrameError::CorruptIncomplete.into());
        }
        if actual > s.bytes {
            return Err(logical_frame::FrameError::Invalid("snapshot descriptor length").into());
        }
        let mut header = [0; logical_snapshot::HEADER_LEN];
        file.read_exact(&mut header)?;
        let context = self.snapshot_context().expect("snapshot selection");
        let length = logical_snapshot::required_length(&header, context, self.limit())?;
        if length as u64 != s.bytes {
            return Err(logical_frame::FrameError::Invalid("snapshot declared length").into());
        }
        let mut bytes = Vec::new();
        bytes
            .try_reserve_exact(length)
            .map_err(|_| logical_frame::FrameError::Limit)?;
        bytes.extend_from_slice(&header);
        file.take((length - header.len()) as u64)
            .read_to_end(&mut bytes)?;
        let body_length = logical_snapshot::decode(&bytes, context, &s.digest, self.limit())?.len();
        bytes.copy_within(
            logical_snapshot::HEADER_LEN..logical_snapshot::HEADER_LEN + body_length,
            0,
        );
        bytes.truncate(body_length);
        Ok(bytes)
    }
}
