//! Read-only recognition of retired artifact headers, never a legacy decoder.

use std::{io::Read, path::Path};

use crate::{PersistArtifact, PersistError, PersistResult, StoreDirectory};

pub(crate) fn reject(dir: &StoreDirectory) -> PersistResult<()> {
    for name in dir.entries()? {
        let Some(text) = name.to_str() else { continue };
        // Do not inspect unselected format-2 payloads or infer authority from names.
        let candidate = matches!(text, "wal.log" | "audit.log" | "MANIFEST")
            || numbered(text, "snapshot.", ".snap")
            || numbered(text, "wal.", ".archive");
        if !candidate {
            continue;
        }
        let result = (|| {
            let mut prefix = Vec::with_capacity(8);
            dir.open_read(Path::new(&name))?
                .take(8)
                .read_to_end(&mut prefix)?;
            let artifact = match prefix.get(..4) {
                Some(b"SLDB") => PersistArtifact::Wal,
                Some(b"SLSN") if !prefix.starts_with(b"SLSNP2") => PersistArtifact::Snapshot,
                Some(b"SLMF") => PersistArtifact::Manifest,
                Some(b"SLAU") => PersistArtifact::AuditLog,
                _ => return Ok(()),
            };
            // Missing version bytes do not make a recognized old store writable.
            let version = |offset| {
                prefix
                    .get(offset..offset + 2)
                    .map_or(0, |bytes| u16::from_le_bytes([bytes[0], bytes[1]]))
            };
            Err(PersistError::UnsupportedVersion {
                artifact,
                major: version(4),
                minor: if matches!(artifact, PersistArtifact::Wal | PersistArtifact::Snapshot) {
                    version(6)
                } else {
                    0
                },
            })
        })();
        result.map_err(|source| PersistError::Artifact {
            name: text.into(),
            source: Box::new(source),
        })?;
    }
    Ok(())
}

fn numbered(name: &str, prefix: &str, suffix: &str) -> bool {
    name.strip_prefix(prefix)
        .and_then(|name| name.strip_suffix(suffix))
        .is_some_and(|number| !number.is_empty() && number.bytes().all(|b| b.is_ascii_digit()))
}
