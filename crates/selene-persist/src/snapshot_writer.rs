//! Atomic snapshot envelope writer.

use std::collections::HashSet;
use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::time::Instant;

use rayon::prelude::*;
use selene_core::metrics;

use crate::compression::compress_zstd;
use crate::section::{
    MAX_SECTION_COUNT, SECTION_TABLE_ROW_LEN, SectionEntry, body_hash, section_table_bytes,
    validate_section_payload_len,
};
use crate::snapshot_file_header::{
    FLAG_SECTION_COMPRESSED, SNAPSHOT_FILE_HEADER_LEN, SnapshotFileHeader,
};
use crate::snapshot_path::{snapshot_path, snapshot_tmp_path};
use crate::{PersistError, PersistResult};

// Local snapshot-write benches put the serial/parallel compression crossover
// between the 640 KiB and 3.2 MiB synthetic snapshot rows.
const PARALLEL_SNAPSHOT_COMPRESSION_MIN_BYTES: usize = 1024 * 1024;

/// Snapshot section compression mode.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SectionCompression {
    /// Store section payloads uncompressed.
    None,
    /// Compress each section independently using zstd.
    PerSection {
        /// zstd compression level.
        level: i32,
    },
}

impl SectionCompression {
    /// Default v1.0 compression mode.
    pub const DEFAULT: Self = Self::PerSection { level: 1 };
}

impl Default for SectionCompression {
    fn default() -> Self {
        Self::DEFAULT
    }
}

/// Snapshot write configuration.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SnapshotConfig {
    /// Directory where `snapshot.{sequence}.snap` is written.
    pub dir: PathBuf,
    /// Snapshot sequence.
    pub sequence: u64,
    /// Section compression mode.
    pub compression: SectionCompression,
    /// Whether to fsync the snapshot file before rename.
    pub fsync: bool,
}

impl Default for SnapshotConfig {
    fn default() -> Self {
        Self {
            dir: PathBuf::new(),
            sequence: 0,
            compression: SectionCompression::DEFAULT,
            fsync: true,
        }
    }
}

/// Builder for one snapshot envelope.
#[derive(Debug)]
pub struct SnapshotBuilder {
    directory: crate::StoreDirectory,
    config: SnapshotConfig,
    sections: Vec<RawSection>,
    seen: HashSet<([u8; 4], [u8; 4])>,
}

#[derive(Debug)]
struct RawSection {
    provider: [u8; 4],
    sub: [u8; 4],
    payload: Vec<u8>,
}

struct PreparedSection {
    entry: SectionEntry,
    payload: Vec<u8>,
}

/// Metadata returned after a snapshot is durably finalized.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SnapshotFinalizeOutcome {
    /// Snapshot sequence written into the finalized snapshot filename.
    pub snapshot_seq: u64,
    /// Low 128 bits of the finalized snapshot body hash.
    pub body_hash: [u8; 16],
    /// Number of section-table rows in the snapshot.
    pub section_count: u32,
}

impl SnapshotBuilder {
    /// Anchor the configured directory and construct an empty snapshot builder.
    ///
    /// # Errors
    /// Returns directory-open or unsupported-platform errors.
    pub fn new(config: SnapshotConfig) -> PersistResult<Self> {
        let dir = crate::StoreDirectory::open(&config.dir)?;
        Ok(Self::new_in(&dir, config))
    }

    /// Construct a snapshot builder using retained authority; the config path is diagnostic.
    #[must_use]
    pub fn new_in(dir: &crate::StoreDirectory, mut config: SnapshotConfig) -> Self {
        config.dir = dir.locator().to_path_buf();
        Self {
            directory: dir.clone(),
            config,
            sections: Vec::new(),
            seen: HashSet::new(),
        }
    }

    /// Return the snapshot sequence this builder will finalize.
    #[must_use]
    pub const fn sequence(&self) -> u64 {
        self.config.sequence
    }

    /// Return the directory where this builder will publish its snapshot.
    #[must_use]
    pub fn dir(&self) -> &Path {
        &self.config.dir
    }

    /// Retained publication directory. [`Self::dir`] is only a locator.
    #[must_use]
    pub fn directory(&self) -> &crate::StoreDirectory {
        &self.directory
    }

    /// Add one opaque section payload.
    ///
    /// # Errors
    ///
    /// Returns cap errors for too many sections or oversized payloads, and
    /// [`PersistError::DuplicateSection`] if the tag pair was already added.
    pub fn add_section(
        &mut self,
        provider: [u8; 4],
        sub: [u8; 4],
        payload: Vec<u8>,
    ) -> PersistResult<()> {
        if self.sections.len() == MAX_SECTION_COUNT {
            return Err(PersistError::TooManySections {
                count: self.sections.len() + 1,
                max: MAX_SECTION_COUNT,
            });
        }
        validate_section_payload_len(payload.len())?;
        if !self.seen.insert((provider, sub)) {
            return Err(PersistError::DuplicateSection { provider, sub });
        }
        self.sections.push(RawSection {
            provider,
            sub,
            payload,
        });
        Ok(())
    }

    /// Atomically write the snapshot and return the finalized metadata.
    ///
    /// The writer creates a unique `snapshot.{sequence}.snap.tmp.{pid}.{n}`,
    /// writes and optionally fsyncs it, then **hard-links** it to
    /// `snapshot.{sequence}.snap` and removes the tmp. Unique attempts keep a
    /// stale temporary left by a crashed writer from stranding later snapshots.
    /// `hard_link` fails atomically with `AlreadyExists` when
    /// the final path already has a snapshot for this sequence; this is the
    /// race-safe alternative to `rename` (which silently overwrites on POSIX)
    /// without requiring `unsafe` for `renameat2(RENAME_NOREPLACE)`. On a
    /// collision the tmp is removed best-effort. When `config.fsync` is set the
    /// parent directory is fsynced after the `hard_link` so the new directory
    /// entry is durable — closing the crash window for a downstream embedder that
    /// publishes a snapshot via `finalize` directly. (The rotation path adds its
    /// own fsync-independent re-open/`sync_all` barrier on top of this.)
    ///
    /// # Errors
    ///
    /// Returns writer contention, cap, compression, hash/header construction, or I/O errors,
    /// including `Io(AlreadyExists)` when the final snapshot path is taken.
    /// MANIFEST rotation uses a crate-private companion that accepts only an
    /// exact byte-for-byte match at that path; standalone callers retain this
    /// fail-on-collision contract.
    pub fn finalize(self) -> PersistResult<SnapshotFinalizeOutcome> {
        let authority = crate::StoreWriter::acquire(&self.directory)?;
        self.finalize_with_authority(&authority)
    }

    /// Publish with an existing writer lease for this builder's retained directory.
    ///
    /// Directory identity is checked before acquiring the epoch or creating files.
    /// The lease is shared explicitly; `LOCK` is never reacquired here.
    ///
    /// # Errors
    /// Returns directory mismatch, epoch-lock, or the same errors as [`Self::finalize`].
    pub fn finalize_with_authority(
        self,
        authority: &crate::StoreWriter,
    ) -> PersistResult<SnapshotFinalizeOutcome> {
        self.validate_directory(authority.directory())?;
        let guard = crate::manifest_lock::ManifestEpochGuard::acquire(authority)?;
        self.finalize_inner(&guard, false, false)
    }

    /// Finalize for MANIFEST rotation, accepting only a byte-identical final
    /// snapshot left by an earlier attempt. When `require_existing` is true,
    /// absence is an identity failure and this method never publishes the temp.
    pub(crate) fn finalize_for_rotation(
        self,
        guard: &crate::manifest_lock::ManifestEpochGuard,
        require_existing: bool,
    ) -> PersistResult<SnapshotFinalizeOutcome> {
        self.validate_directory(guard.directory())?;
        self.finalize_inner(guard, true, require_existing)
    }

    fn validate_directory(&self, publish_dir: &crate::StoreDirectory) -> PersistResult<()> {
        // Compare opened physical directory identities, never re-resolve the
        // builder's original locator. Both handles survive parent replacement.
        if !self.directory.same_directory(publish_dir)? {
            return Err(PersistError::WalRotationDirectoryMismatch {
                snapshot_dir: self.config.dir.clone(),
                wal_dir: publish_dir.locator().to_path_buf(),
            });
        }
        Ok(())
    }

    #[tracing::instrument(
        name = "selene.persist.snapshot.finalize",
        skip(self, guard),
        fields(snapshot_seq = self.config.sequence, section_count = self.sections.len())
    )]
    fn finalize_inner(
        self,
        guard: &crate::manifest_lock::ManifestEpochGuard,
        accept_identical_existing: bool,
        require_existing: bool,
    ) -> PersistResult<SnapshotFinalizeOutcome> {
        let started = Instant::now();
        let dir = guard.directory();
        dir.require_legacy()?;
        let final_path = snapshot_path(Path::new(""), self.config.sequence);
        let prepared = prepare_sections(self.sections, self.config.compression)?;
        let entries: Vec<_> = prepared.iter().map(|section| section.entry).collect();
        let table = section_table_bytes(&entries)?;
        let hash = body_hash(
            &table,
            prepared.iter().map(|section| section.payload.as_slice()),
        );
        let flags = match self.config.compression {
            SectionCompression::None => 0,
            SectionCompression::PerSection { .. } => FLAG_SECTION_COMPRESSED,
        };
        let header = SnapshotFileHeader::new(flags, entries.len(), hash)?;
        let (tmp_path, file) = create_snapshot_tmp(dir, self.config.sequence)?;
        let result = (|| -> PersistResult<SnapshotFinalizeOutcome> {
            let mut writer = BufWriter::new(file);
            header.write_to(&mut writer)?;
            writer.write_all(&table)?;
            for section in prepared {
                writer.write_all(&section.payload)?;
            }
            writer.flush()?;
            if self.config.fsync {
                writer.get_ref().sync_data()?;
            }
            drop(writer);
            let published = if require_existing {
                crate::artifact_identity::require_identical_regular_files(
                    dir,
                    &tmp_path,
                    &final_path,
                )?;
                false
            } else {
                match dir.hard_link(&tmp_path, &final_path) {
                    Ok(()) => true,
                    Err(PersistError::Io(error))
                        if accept_identical_existing
                            && error.kind() == std::io::ErrorKind::AlreadyExists =>
                    {
                        crate::artifact_identity::require_identical_regular_files(
                            dir,
                            &tmp_path,
                            &final_path,
                        )?;
                        false
                    }
                    Err(error) => return Err(error),
                }
            };
            let _ = dir.remove(&tmp_path);
            // Make the new directory entry durable AFTER the publish (the
            // file's own sync_data above precedes it). Gated on config.fsync
            // so the no-fsync benchmark/offline path stays barrier-free.
            if self.config.fsync && published {
                dir.sync()?;
            }
            if published {
                metrics::counter_inc(metrics::SNAPSHOTS_TOTAL);
                metrics::histogram_record(
                    metrics::SNAPSHOT_DURATION_SECONDS,
                    started.elapsed().as_secs_f64(),
                );
            }
            Ok(SnapshotFinalizeOutcome {
                snapshot_seq: self.config.sequence,
                body_hash: hash,
                section_count: entries.len() as u32,
            })
        })();
        if result.is_err() {
            let _ = dir.remove(&tmp_path);
        }
        result
    }
}

fn create_snapshot_tmp(
    dir: &crate::StoreDirectory,
    sequence: u64,
) -> PersistResult<(PathBuf, File)> {
    let base = snapshot_tmp_path(Path::new(""), sequence);
    for attempt in 0..128_u8 {
        let mut name = base
            .file_name()
            .expect("snapshot temporary path always has a file name")
            .to_os_string();
        name.push(format!(".{}.{attempt}", std::process::id()));
        let path = base.with_file_name(name);
        match dir.create_new(&path) {
            Ok(file) => return Ok((path, file)),
            Err(PersistError::Io(error)) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                continue;
            }
            Err(error) => return Err(error),
        }
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::AlreadyExists,
        "snapshot temporary path attempts exhausted",
    )
    .into())
}

fn prepare_sections(
    sections: Vec<RawSection>,
    compression: SectionCompression,
) -> PersistResult<Vec<PreparedSection>> {
    let count = sections.len();
    let use_parallel_compression = matches!(compression, SectionCompression::PerSection { .. })
        && should_prepare_compressed_sections_parallel(&sections);
    let mut prepared = match compression {
        SectionCompression::None => sections
            .into_iter()
            .map(prepare_uncompressed_section)
            .collect::<PersistResult<Vec<_>>>()?,
        SectionCompression::PerSection { level } if use_parallel_compression => sections
            .into_par_iter()
            .map(|section| prepare_compressed_section(section, level))
            .collect::<PersistResult<Vec<_>>>()?,
        SectionCompression::PerSection { level } => sections
            .into_iter()
            .map(|section| prepare_compressed_section(section, level))
            .collect::<PersistResult<Vec<_>>>()?,
    };
    assign_section_offsets(&mut prepared, count);
    Ok(prepared)
}

fn should_prepare_compressed_sections_parallel(sections: &[RawSection]) -> bool {
    if sections.len() < 2 {
        return false;
    }
    let total_bytes = sections.iter().fold(0_usize, |sum, section| {
        sum.saturating_add(section.payload.len())
    });
    total_bytes >= PARALLEL_SNAPSHOT_COMPRESSION_MIN_BYTES
}

fn prepare_uncompressed_section(section: RawSection) -> PersistResult<PreparedSection> {
    let RawSection {
        provider,
        sub,
        payload,
    } = section;
    validate_section_payload_len(payload.len())?;
    Ok(PreparedSection {
        entry: SectionEntry {
            provider,
            sub,
            payload_offset: 0,
            payload_len: payload.len() as u64,
        },
        payload,
    })
}

fn prepare_compressed_section(section: RawSection, level: i32) -> PersistResult<PreparedSection> {
    let RawSection {
        provider,
        sub,
        payload,
    } = section;
    let payload = compress_zstd(&payload, level)?;
    validate_section_payload_len(payload.len())?;
    Ok(PreparedSection {
        entry: SectionEntry {
            provider,
            sub,
            payload_offset: 0,
            payload_len: payload.len() as u64,
        },
        payload,
    })
}

fn assign_section_offsets(prepared: &mut [PreparedSection], section_count: usize) {
    let mut payload_offset =
        SNAPSHOT_FILE_HEADER_LEN as u64 + (section_count * SECTION_TABLE_ROW_LEN) as u64;
    for section in prepared {
        section.entry.payload_offset = payload_offset;
        payload_offset = payload_offset.saturating_add(section.entry.payload_len);
    }
}

#[cfg(test)]
#[path = "snapshot_writer/tests.rs"]
mod tests;
