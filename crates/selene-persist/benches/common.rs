#![allow(missing_docs)]
#![allow(dead_code)]

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use criterion::Criterion;
use selene_core::{
    Change, HlcTimestamp, JsonValue, LabelSet, NodeId, Origin, PropertyMap, Value, VectorValue,
    db_string,
};
use selene_persist::{
    DEFAULT_WAL_FILE_NAME, SectionCompression, SnapshotBuilder, SnapshotConfig, SyncPolicy,
    WalCompression, WalConfig, WalWriter, snapshot_path,
};
use selene_testing::BenchProfile;

static NEXT_TEMP_ID: AtomicU64 = AtomicU64::new(1);

pub(crate) struct TempDir {
    path: PathBuf,
}

impl TempDir {
    pub(crate) fn new(name: &str) -> Self {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock is after epoch")
            .as_nanos();
        let id = NEXT_TEMP_ID.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "selene-bench-{name}-{}-{nanos}-{id}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir(&path).expect("temp dir is created");
        Self { path }
    }

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

pub(crate) fn criterion_config() -> Criterion {
    let profile = BenchProfile::from_env();
    Criterion::default()
        .sample_size(profile.sample_size())
        .warm_up_time(Duration::from_millis(100))
        .measurement_time(Duration::from_millis(match profile {
            BenchProfile::Quick => 500,
            BenchProfile::Full | BenchProfile::Stress => 1_500,
            _ => 500,
        }))
}

pub(crate) fn scales() -> &'static [usize] {
    BenchProfile::from_env().scales()
}

pub(crate) fn changes(count: usize) -> Vec<Change> {
    let label = db_string("bench.persist.node").expect("fixture string fits");
    let key = db_string("bench.persist.value").expect("fixture string fits");
    (0..count)
        .map(|idx| Change::NodeCreated {
            id: NodeId::new(idx as u64 + 1),
            labels: LabelSet::single(label.clone()),
            properties: PropertyMap::from_pairs([(key.clone(), Value::Int(idx as i64))])
                .expect("fixture properties fit"),
        })
        .collect()
}

#[derive(Clone, Copy)]
pub(crate) enum PayloadShape {
    ScalarInt,
    JsonMetadata,
    Vector128,
    Vector768,
}

impl PayloadShape {
    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::ScalarInt => "scalar_i64",
            Self::JsonMetadata => "json_metadata",
            Self::Vector128 => "vector128",
            Self::Vector768 => "vector768",
        }
    }

    fn value(self, idx: usize) -> Value {
        match self {
            Self::ScalarInt => Value::Int(idx as i64),
            Self::JsonMetadata => json_metadata_value(idx),
            Self::Vector128 => vector_value(idx, 128),
            Self::Vector768 => vector_value(idx, 768),
        }
    }
}

pub(crate) fn payload_shapes() -> &'static [PayloadShape] {
    &[
        PayloadShape::ScalarInt,
        PayloadShape::JsonMetadata,
        PayloadShape::Vector128,
        PayloadShape::Vector768,
    ]
}

pub(crate) fn changes_with_payload(count: usize, shape: PayloadShape) -> Vec<Change> {
    let label = db_string("bench.persist.node").expect("fixture string fits");
    let key = db_string("bench.persist.payload").expect("fixture string fits");
    (0..count)
        .map(|idx| Change::NodeCreated {
            id: NodeId::new(idx as u64 + 1),
            labels: LabelSet::single(label.clone()),
            properties: PropertyMap::from_pairs([(key.clone(), shape.value(idx))])
                .expect("fixture properties fit"),
        })
        .collect()
}

pub(crate) fn write_wal(dir: &Path, entries: usize, batch_size: usize, snapshot_seq: u64) {
    let path = dir.join(DEFAULT_WAL_FILE_NAME);
    let mut writer = WalWriter::open(
        &path,
        WalConfig {
            sync_policy: SyncPolicy::EveryN(1_000),
            snapshot_seq,
        },
    )
    .expect("wal opens");
    let payload = changes(batch_size);
    for idx in 0..entries {
        writer
            .append(
                HlcTimestamp::new(idx as u64 + 1, 0),
                Origin::Local,
                None,
                &payload,
            )
            .expect("wal append succeeds");
    }
    writer.flush().expect("wal flush succeeds");
}

pub(crate) fn write_wal_with_payload(
    dir: &Path,
    entries: usize,
    batch_size: usize,
    shape: PayloadShape,
    snapshot_seq: u64,
    sync_policy: SyncPolicy,
) {
    let path = dir.join(DEFAULT_WAL_FILE_NAME);
    let mut writer = WalWriter::open(
        &path,
        WalConfig {
            sync_policy,
            snapshot_seq,
        },
    )
    .expect("wal opens");
    let payload = changes_with_payload(batch_size, shape);
    for idx in 0..entries {
        writer
            .append(
                HlcTimestamp::new(idx as u64 + 1, 0),
                Origin::Local,
                None,
                &payload,
            )
            .expect("wal append succeeds");
    }
    writer.flush().expect("wal flush succeeds");
}

pub(crate) fn write_wal_with_payload_compression(
    dir: &Path,
    entries: usize,
    batch_size: usize,
    shape: PayloadShape,
    snapshot_seq: u64,
    sync_policy: SyncPolicy,
    compression: WalCompression,
) {
    let path = dir.join(DEFAULT_WAL_FILE_NAME);
    let mut writer = WalWriter::open_with_compression(
        &path,
        WalConfig {
            sync_policy,
            snapshot_seq,
        },
        compression,
    )
    .expect("wal opens");
    let payload = changes_with_payload(batch_size, shape);
    for idx in 0..entries {
        writer
            .append(
                HlcTimestamp::new(idx as u64 + 1, 0),
                Origin::Local,
                None,
                &payload,
            )
            .expect("wal append succeeds");
    }
    writer.flush().expect("wal flush succeeds");
}

pub(crate) fn wal_file_len_with_payload_compression(
    entries: usize,
    batch_size: usize,
    shape: PayloadShape,
    compression: WalCompression,
) -> u64 {
    let dir = TempDir::new("wal-compression-policy-size");
    write_wal_with_payload_compression(
        dir.path(),
        entries,
        batch_size,
        shape,
        0,
        SyncPolicy::OnFlushOnly,
        compression,
    );
    fs::metadata(dir.path().join(DEFAULT_WAL_FILE_NAME))
        .expect("wal file metadata reads")
        .len()
}

pub(crate) fn write_snapshot(dir: &Path, sequence: u64, bytes: usize) -> PathBuf {
    write_snapshot_with_compression(dir, sequence, bytes, SectionCompression::DEFAULT)
}

pub(crate) fn write_snapshot_with_compression(
    dir: &Path,
    sequence: u64,
    bytes: usize,
    compression: SectionCompression,
) -> PathBuf {
    let mut builder = SnapshotBuilder::new(SnapshotConfig {
        dir: dir.to_path_buf(),
        sequence,
        compression,
        fsync: false,
    })
    .expect("snapshot directory opens");
    let section_tags = [*b"DAT0", *b"DAT1", *b"DAT2", *b"DAT3", *b"DAT4"];
    let base_len = bytes / section_tags.len();
    let mut remaining = bytes % section_tags.len();
    for (idx, sub) in section_tags.into_iter().enumerate() {
        let extra = usize::from(remaining > 0);
        remaining = remaining.saturating_sub(extra);
        builder
            .add_section(
                *b"CORE",
                sub,
                vec![0x5a_u8.wrapping_add(idx as u8); base_len + extra],
            )
            .expect("section add succeeds");
    }
    builder.finalize().expect("snapshot write succeeds");
    snapshot_path(dir, sequence)
}

fn json_metadata_value(idx: usize) -> Value {
    let value = serde_json::json!({
        "kind": "episodic",
        "topic": format!("topic-{}", idx % 32),
        "session": format!("session-{}", idx / 64),
        "importance": (idx % 100) as u64,
        "source": {
            "tool": "omlx",
            "model": "Qwen3-Embedding-0.6B-4bit-DWQ"
        },
        "tags": [
            format!("tag-{}", idx % 7),
            format!("bucket-{}", idx % 13)
        ],
        "metrics": {
            "confidence": (idx % 1000) as f64 / 1000.0,
            "retrievals": idx % 17
        }
    });
    Value::Json(JsonValue::new(value).expect("fixture JSON is valid"))
}

fn vector_value(idx: usize, dim: usize) -> Value {
    let components: Vec<f32> = (0..dim)
        .map(|component| {
            let mixed = idx
                .wrapping_mul(31)
                .wrapping_add(component.wrapping_mul(17))
                % 2048;
            mixed as f32 / 2048.0
        })
        .collect();
    Value::Vector(VectorValue::new(components).expect("fixture vector is valid"))
}
