//! Bounded selected-stream reader; an immutable manifest lock pins its dependencies.

use super::*;
use crate::control::CompatibilityIdentity;
use crate::logical_frame::{Boundary, Decoded, HEADER_LEN};
use std::io::Read;

/// Isolated format-2 body reader, not a query-ready database or writer reopen.
pub struct LogicalReader {
    file: std::io::Take<File>,
    context: Context,
    limit: usize,
    ended: bool,
    pub(super) failed: std::cell::Cell<bool>,
    incomplete: bool,
    pub(super) position: Position,
    pub(super) selected: crate::control::logical::Selected,
    pub(super) directory: StoreDirectory,
    // Independently opened description, locked once under the selection epoch.
    // Prune probes this immutable named inode before removing it or dependencies.
    _lease: File,
    pub(super) captured_bytes: u64,
    pub(super) last_record_offset: u64,
}

impl LogicalReader {
    /// Current verified cursor; it is not a historical acknowledgment proof.
    pub fn position(&self) -> Position {
        self.position
    }
    pub(super) fn limit(&self) -> usize {
        self.limit
    }
    /// Select exact control metadata independently of the frames being validated.
    /// The artifact lease remains held until this reader is dropped. Selection
    /// participates in the epoch, but consumption does not block publication.
    pub fn open(
        dir: &StoreDirectory,
        expected: &CompatibilityIdentity,
        limit: usize,
    ) -> Result<Self, StreamError> {
        crate::legacy_probe::reject(dir)?;
        if limit > logical_frame::MAX_PAYLOAD {
            return Err(logical_frame::FrameError::Limit.into());
        }
        let epoch = crate::PersistenceReadGuard::acquire_existing_in(dir)
            .map_err(|e| StreamError::from(e).at(crate::MANIFEST_LOCK_FILE_NAME, None, None))?;
        let (file, selected) = crate::control::logical::select(&epoch, expected)?;
        let reader = Self::from_selection(dir, file, selected, limit)?;
        drop(epoch);
        dir.check_fault("reader.captured")?;
        Ok(reader)
    }

    // Caller holds either shared selection epoch or exclusive maintenance epoch.
    pub(crate) fn from_selection(
        dir: &StoreDirectory,
        file: File,
        selected: crate::control::logical::Selected,
        limit: usize,
    ) -> Result<Self, StreamError> {
        let selection_error = |e| StreamError::from(e).at(selected.manifest_name(), None, None);
        let lease = dir
            .open_read(selected.manifest_name())
            .map_err(selection_error)?;
        dir.check_fault("reader.selected")
            .map_err(selection_error)?;
        lease.lock_shared().map_err(|e| selection_error(e.into()))?;
        let context = selected.context;
        let length = file
            .metadata()
            .map_err(|e| StreamError::Io(e).at(selected.log_name(), None, None))?
            .len();
        Ok(Self {
            file: file.take(length),
            context,
            limit,
            ended: false,
            failed: std::cell::Cell::new(false),
            incomplete: false,
            position: selected.base(),
            selected,
            directory: dir.clone(),
            _lease: lease,
            captured_bytes: length,
            last_record_offset: 0,
        })
    }

    /// Read the next verified body. Complete corruption always fails closed.
    /// An incomplete final unsealed suffix returns `None` and is not repaired.
    /// After any error, the reader is terminal and must not be used for salvage.
    pub fn next_body(&mut self) -> Result<Option<Vec<u8>>, StreamError> {
        if self.failed.get() {
            return Err(StreamError::Terminated);
        }
        let offset = self.position.offset;
        let expected = self.context.sequence;
        let result = self.read_next();
        if result.is_err() {
            self.failed.set(true);
        }
        result.map_err(|e| e.at(self.selected.log_name(), Some(offset), Some(expected)))
    }

    fn read_next(&mut self) -> Result<Option<Vec<u8>>, StreamError> {
        if self.ended {
            return Ok(None);
        }
        self.ended = true;
        let mut bytes = Vec::new();
        self.file
            .by_ref()
            .take(HEADER_LEN as u64)
            .read_to_end(&mut bytes)?;
        if bytes.is_empty() {
            return Ok(None);
        }
        loop {
            match logical_frame::decode(&bytes, self.context, Boundary::UnsealedEnd, self.limit)? {
                Decoded::Incomplete { needed } if needed > bytes.len() => {
                    let missing = needed - bytes.len();
                    bytes
                        .try_reserve_exact(missing)
                        .map_err(|_| logical_frame::FrameError::Limit)?;
                    let read = self
                        .file
                        .by_ref()
                        .take(missing as u64)
                        .read_to_end(&mut bytes)?;
                    if read < missing {
                        self.incomplete = true;
                        return Ok(None);
                    }
                }
                Decoded::Incomplete { .. } => {
                    return Err(StreamError::Protocol("decoder made no progress"));
                }
                Decoded::Complete {
                    body,
                    digest,
                    consumed,
                } => {
                    self.last_record_offset = self.position.offset;
                    let body = body.into_owned();
                    self.context.sequence = self
                        .context
                        .sequence
                        .checked_add(1)
                        .ok_or(StreamError::Protocol("sequence exhausted"))?;
                    self.context.previous = digest;
                    self.position.sequence = self.context.sequence - 1;
                    self.position.offset = self
                        .position
                        .offset
                        .checked_add(consumed as u64)
                        .ok_or(logical_frame::FrameError::Limit)?;
                    self.position.digest = digest;
                    self.ended = false;
                    return Ok(Some(body));
                }
            }
        }
    }

    /// Whether consumption ended at a specifically incomplete unsealed suffix.
    pub fn incomplete_tail(&self) -> bool {
        self.incomplete
    }
}
