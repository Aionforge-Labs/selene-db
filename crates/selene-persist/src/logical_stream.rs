//! One format-2 WAL authority with immutable checkpoint selection and non-destructive reopen.
//! No legacy writer, provider voters, rotation or prune.
//!
//! Groups are caller-bounded synchronous units, not a background queue. File and
//! directory sync requests rely on the supported native filesystem/device contract;
//! process-crash evidence is not a power-loss guarantee.

use crate::{
    StoreDirectory, StoreWriter,
    control::EmptyStoreControl,
    logical_frame::{self, Compression, Context},
    manifest_lock::ManifestEpochGuard,
};
use std::{
    fs::File,
    io::{Seek, SeekFrom, Write},
    panic::{AssertUnwindSafe, catch_unwind},
};

mod checkpoint;
mod outcome;
mod reader;
mod reopen;
pub use checkpoint::CheckpointInfo;
pub use outcome::*;
pub use reader::LogicalReader;
pub use reopen::ReopeningWal;

/// Maximum controlled synchronous group size; there is no unbounded admission queue.
pub const MAX_GROUP: usize = 32;

/// Validate bounded logical-manifest bytes without I/O or granting trusted context.
/// Successful decoding is not selection: CURRENT, compatibility and the retained
/// directory epoch must still be validated by [`LogicalReader::open`].
pub fn validate_manifest(bytes: &[u8]) -> crate::PersistResult<()> {
    crate::control::logical::validate(bytes)
}

/// Fully encoded bounded group, tied to its exact synchronized base and segment.
pub struct PreparedGroup {
    base: Position,
    frames: Vec<(Vec<u8>, Position)>,
}

/// Borrowed notification capability for the sole outer publication authority.
pub struct Publication<'a> {
    progress: &'a mut Progress,
    candidate: Position,
}
impl Publication<'_> {
    /// Call immediately after the single infallible outer state store, before observers.
    pub fn mark_published(&mut self) {
        self.progress.published = Some(self.candidate);
    }
}

/// Retained synchronous single-segment format-2 stream owner.
pub struct LogicalWal {
    authority: StoreWriter,
    file: File,
    progress: Progress,
    fenced: bool,
    selected: crate::control::logical::Selected,
    #[cfg(any(test, feature = "test-harness"))]
    fault: Option<Fault>,
}

impl LogicalWal {
    /// Consume empty control and select a real unsealed WAL segment. Does not
    /// construct a facade, reopen a database, or select empty control as data.
    pub fn create(control: EmptyStoreControl) -> Result<Self, StreamError> {
        let (authority, file, selected) = crate::control::logical::create(control)?;
        let context = selected.context;
        let initial = Position {
            store: context.store,
            epoch: context.epoch,
            segment: context.segment,
            sequence: 0,
            offset: 0,
            digest: context.previous,
        };
        Ok(Self {
            authority,
            file,
            progress: Progress {
                written: initial,
                synchronized: initial,
                published: None,
                acknowledged: None,
            },
            fenced: false,
            selected,
            #[cfg(any(test, feature = "test-harness"))]
            fault: None,
        })
    }

    /// Independently established commit boundaries.
    pub fn progress(&self) -> Progress {
        self.progress
    }
    /// Whether any later append is prohibited on this owner.
    pub fn is_fenced(&self) -> bool {
        self.fenced
    }

    /// Inject one deterministic native-file failure scenario for phase tests.
    #[cfg(feature = "test-harness")]
    #[doc(hidden)]
    pub fn inject_fault(&mut self, fault: Fault) {
        self.fault = Some(fault);
    }

    /// Encode every member before touching the authoritative file. The combined
    /// encoded group is bounded by the same payload ceiling plus fixed overhead.
    pub fn prepare(
        &self,
        bodies: &[&[u8]],
        compression: Compression,
        limit: usize,
    ) -> Result<PreparedGroup, StreamError> {
        if self.fenced {
            return Err(StreamError::Protocol(
                "writer fenced; reconcile before reopen",
            ));
        }
        if bodies.is_empty() || bodies.len() > MAX_GROUP {
            return Err(StreamError::Protocol("group size"));
        }
        let base = self.progress.synchronized;
        let mut next = base;
        let mut frames = Vec::with_capacity(bodies.len());
        let mut total = 0usize;
        for body in bodies {
            let sequence = next
                .sequence
                .checked_add(1)
                .ok_or(StreamError::Protocol("sequence exhausted"))?;
            let bytes = logical_frame::encode(
                body,
                Context {
                    store: next.store,
                    epoch: next.epoch,
                    segment: next.segment,
                    sequence,
                    previous: next.digest,
                },
                compression,
                limit,
            )?;
            total = total
                .checked_add(bytes.len())
                .ok_or(logical_frame::FrameError::Limit)?;
            if total > logical_frame::MAX_PAYLOAD + MAX_GROUP * logical_frame::FRAME_OVERHEAD {
                return Err(logical_frame::FrameError::Limit.into());
            }
            next.sequence = sequence;
            next.offset = next
                .offset
                .checked_add(bytes.len() as u64)
                .ok_or(logical_frame::FrameError::Limit)?;
            next.digest.copy_from_slice(&bytes[bytes.len() - 32..]);
            frames.push((bytes, next));
        }
        Ok(PreparedGroup { base, frames })
    }

    /// Synchronize a complete group, invoke the sole outer publication, then
    /// acknowledge. Cancellation is sampled before append and after sync; the
    /// latter never authorizes truncating successfully synchronized records.
    /// Any append/sync error fences this owner before rollback is attempted.
    /// Callback unwind is classified, never interpreted as rollback.
    pub fn commit(
        &mut self,
        group: PreparedGroup,
        canceled: impl Fn() -> bool,
        publish: impl FnOnce(&mut Publication<'_>) -> Result<(), StreamError>,
    ) -> Result<Position, Box<CommitFailure>> {
        let candidate = group.frames.last().expect("prepared group is nonempty").1;
        let mut phase = CommitPhase::Prepare;
        let mut synchronized = false;
        let mut epoch = None;
        let result = catch_unwind(AssertUnwindSafe(|| -> Result<(), StreamError> {
            if self.fenced || group.base != self.progress.synchronized {
                return Err(StreamError::Protocol("fenced or stale prepared group"));
            }
            if canceled() {
                return Err(StreamError::Protocol("canceled before append"));
            }
            epoch = Some(ManifestEpochGuard::acquire(&self.authority)?);
            self.fenced = true;
            phase = CommitPhase::Append;
            for (index, (bytes, position)) in group.frames.iter().enumerate() {
                #[cfg(not(any(test, feature = "test-harness")))]
                let _ = index;
                #[cfg(any(test, feature = "test-harness"))]
                if matches!(
                    self.fault,
                    Some(Fault::PartialAppend | Fault::Truncate | Fault::CleanupSync)
                ) && index == group.frames.len() - 1
                {
                    self.file.write_all(&bytes[..bytes.len() / 2])?;
                    return Err(std::io::Error::other("injected partial append").into());
                }
                self.file.write_all(bytes)?;
                self.progress.written = *position;
            }
            phase = CommitPhase::Synchronize;
            #[cfg(any(test, feature = "test-harness"))]
            if matches!(self.fault, Some(Fault::Synchronize)) {
                return Err(std::io::Error::other("injected synchronization failure").into());
            }
            self.file.sync_all()?;
            self.progress.synchronized = candidate;
            synchronized = true;
            phase = CommitPhase::Publish;
            if canceled() {
                return Err(StreamError::Protocol("canceled after synchronization"));
            }
            publish(&mut Publication {
                progress: &mut self.progress,
                candidate,
            })?;
            if self.progress.published != Some(candidate) {
                return Err(StreamError::Protocol("outer state not published"));
            }
            phase = CommitPhase::Acknowledge;
            if canceled() {
                return Err(StreamError::Protocol("canceled after publication"));
            }
            self.progress.acknowledged = Some(candidate);
            self.fenced = false;
            Ok(())
        }));
        let source = match result {
            Ok(Ok(())) => return Ok(candidate),
            Ok(Err(error)) => error,
            Err(_) => StreamError::Panicked,
        };
        if synchronized && self.progress.published == Some(candidate) {
            phase = CommitPhase::Acknowledge;
        }
        let (durability, cleanup) = if synchronized {
            (Durability::Committed, None)
        } else if phase == CommitPhase::Prepare {
            (Durability::Canceled, None)
        } else {
            // Fence is already set, and the exclusive epoch spans both the failed
            // operation and rollback. Never apply an offset from another segment.
            let cleanup = catch_unwind(AssertUnwindSafe(|| self.rollback(group.base)));
            match cleanup {
                Ok(Ok(())) => (Durability::Canceled, None),
                Ok(Err(error)) => (Durability::Uncertain, Some(error)),
                Err(_) => (Durability::Uncertain, Some(StreamError::Panicked)),
            }
        };
        Err(Box::new(CommitFailure {
            phase,
            durability,
            progress: self.progress,
            candidate: (phase != CommitPhase::Prepare).then_some(candidate),
            source,
            cleanup,
        }))
    }

    fn rollback(&mut self, boundary: Position) -> Result<(), StreamError> {
        if boundary != self.progress.synchronized {
            return Err(StreamError::Protocol(
                "rollback boundary is not the synchronized prefix",
            ));
        }
        #[cfg(any(test, feature = "test-harness"))]
        if matches!(self.fault, Some(Fault::Truncate)) {
            return Err(std::io::Error::other("injected truncation failure").into());
        }
        self.file.set_len(boundary.offset)?;
        // set_len does not move the cursor. Explicitly restore both length and position.
        self.file.seek(SeekFrom::Start(boundary.offset))?;
        #[cfg(any(test, feature = "test-harness"))]
        if matches!(self.fault, Some(Fault::CleanupSync)) {
            return Err(std::io::Error::other("injected cleanup synchronization failure").into());
        }
        self.file.sync_all()?;
        self.progress.written = boundary;
        Ok(())
    }
}

/// Explicit test-only scenarios; every scenario performs real file operations.
#[cfg(any(test, feature = "test-harness"))]
#[doc(hidden)]
#[derive(Clone, Copy)]
pub enum Fault {
    /// Write part of the last group member, then fail.
    PartialAppend,
    /// Fail a partial append and its cleanup truncation.
    Truncate,
    /// Fail a partial append and synchronization after truncation.
    CleanupSync,
    /// Fail synchronization after all candidate bytes were written.
    Synchronize,
}
#[cfg(test)]
mod tests;
