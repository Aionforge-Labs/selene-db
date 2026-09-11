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
        let s = self
            .selected
            .checkpoint
            .as_ref()
            .ok_or(StreamError::Protocol("missing initial database snapshot"))?;
        let mut file = self.directory.open_read(&s.name)?;
        if file.metadata()?.len() != s.bytes {
            return Err(StreamError::Protocol("snapshot descriptor length"));
        }
        let mut header = [0; logical_snapshot::HEADER_LEN];
        file.read_exact(&mut header)?;
        let context = self.snapshot_context().expect("snapshot selection");
        let length = logical_snapshot::required_length(&header, context, self.limit())
            .map_err(|e| StreamError::Preparation(Box::new(e)))?;
        if length as u64 != s.bytes {
            return Err(StreamError::Protocol("snapshot declared length"));
        }
        let mut bytes = Vec::new();
        bytes
            .try_reserve_exact(length)
            .map_err(|_| logical_frame::FrameError::Limit)?;
        bytes.extend_from_slice(&header);
        file.take((length - header.len()) as u64)
            .read_to_end(&mut bytes)?;
        let body_length = logical_snapshot::decode(&bytes, context, &s.digest, self.limit())
            .map_err(|e| StreamError::Preparation(Box::new(e)))?
            .len();
        bytes.copy_within(
            logical_snapshot::HEADER_LEN..logical_snapshot::HEADER_LEN + body_length,
            0,
        );
        bytes.truncate(body_length);
        Ok(bytes)
    }
}
