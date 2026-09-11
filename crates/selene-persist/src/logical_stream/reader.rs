//! Bounded selected-stream reader; epoch lease spans selection and consumption.

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
    incomplete: bool,
    _epoch: crate::PersistenceReadGuard,
}

impl LogicalReader {
    /// Select exact control metadata independently of the frames being validated.
    /// The read lease remains held until this reader is dropped. Do not re-enter
    /// same-directory mutation while consuming it.
    pub fn open(
        dir: &StoreDirectory,
        expected: &CompatibilityIdentity,
        limit: usize,
    ) -> Result<Self, StreamError> {
        if limit > logical_frame::MAX_PAYLOAD {
            return Err(logical_frame::FrameError::Limit.into());
        }
        let epoch = crate::PersistenceReadGuard::acquire_in(dir)?;
        let (file, context) = crate::control::logical::select(&epoch, expected)?;
        let length = file.metadata()?.len();
        Ok(Self {
            file: file.take(length),
            context,
            limit,
            ended: false,
            incomplete: false,
            _epoch: epoch,
        })
    }

    /// Read the next verified body. Complete corruption always fails closed.
    /// An incomplete final unsealed suffix returns `None` and is not repaired.
    /// After any error, the reader is terminal and must not be used for salvage.
    pub fn next_body(&mut self) -> Result<Option<Vec<u8>>, StreamError> {
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
                Decoded::Complete { body, digest, .. } => {
                    let body = body.into_owned();
                    self.context.sequence = self
                        .context
                        .sequence
                        .checked_add(1)
                        .ok_or(StreamError::Protocol("sequence exhausted"))?;
                    self.context.previous = digest;
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
