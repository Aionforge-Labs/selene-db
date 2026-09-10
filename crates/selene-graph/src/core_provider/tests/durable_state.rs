//! `DurableState` HLC + audit + no-WAL coverage (GRAPH-16..19).
//!
//! The `DurableState` HLC counter (`next_hlc`) is the one lock-free
//! shared-mutable counter in the graph region, and `append_audit_event` plus the
//! `durable: None` (embeddable, no-persistence) arms of `write_commit`/`flush`
//! had no direct test. These pin:
//!
//! - GRAPH-16: N-thread `next_timestamp` contention yields DISTINCT, CONTIGUOUS
//!   HLC seconds (no lost/duplicated/skipped tick under concurrency).
//! - GRAPH-17: HLC continuity across a WAL reopen — after recovery the next
//!   timestamp advances strictly past the recovered WAL sequence.
//! - GRAPH-18: `append_audit_event` — no-log → false, success → true, and the
//!   kind/payload/timestamp round-trip through the attached `AuditLog`.
//! - GRAPH-19: the `durable: None` arms return `Ok(0)` / `Ok(None)`.

use std::collections::BTreeSet;
use std::sync::Barrier;
use std::thread;
use std::time::{SystemTime, UNIX_EPOCH};

use selene_persist::{AUDIT_KIND_RESERVED_0, AuditLog};

use super::*;

/// Current wall-clock nanoseconds since the Unix epoch — the test-side mirror of
/// the engine's internal stamp, used to bracket the recorded timestamp.
fn now_unix_nanos() -> u64 {
    u64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos(),
    )
    .unwrap()
}

/// Build a fresh live `DurableState` over a temp WAL.
fn durable_state(name: &str) -> DurableState {
    let path = temp_wal_path(name);
    let writer = WalWriter::open(&path, WalConfig::default()).unwrap();
    DurableState::new(writer)
}

/// Wrap a `DurableState` into a live CORE provider so the `DurableProvider`
/// trait methods (`next_timestamp` / `write_commit` / `flush`) drive it.
fn live_provider_with(durable: DurableState) -> Arc<CoreProvider> {
    let snapshot = Arc::new(ArcSwap::from_pointee(SeleneGraph::new(GraphId::new(1))));
    CoreProvider::new_for_live_with_wal(snapshot, Some(durable))
}

// ───────────────────────── GRAPH-16: HLC contention ─────────────────────────

#[test]
fn next_timestamp_yields_distinct_contiguous_seconds_under_contention() {
    // The HLC counter is `next_hlc.fetch_add(1)`; under N threads each pulling M
    // timestamps the returned `seconds` must be exactly the contiguous set
    // 1..=N*M with no duplicate (a non-atomic counter would drop or repeat ticks)
    // and no gap (a stride/skip bug would leave holes).
    const THREADS: u64 = 8;
    const PER_THREAD: u64 = 500;
    let provider = live_provider_with(durable_state("hlc-contention"));
    let barrier = Arc::new(Barrier::new(THREADS as usize));

    let mut all: Vec<u64> = thread::scope(|scope| {
        let mut handles = Vec::new();
        for _ in 0..THREADS {
            let provider = Arc::clone(&provider);
            let barrier = Arc::clone(&barrier);
            handles.push(scope.spawn(move || {
                barrier.wait();
                (0..PER_THREAD)
                    .map(|_| DurableProvider::next_timestamp(provider.as_ref()).seconds)
                    .collect::<Vec<u64>>()
            }));
        }
        handles
            .into_iter()
            .flat_map(|h| h.join().expect("hlc thread ok"))
            .collect()
    });

    let total = THREADS * PER_THREAD;
    assert_eq!(all.len() as u64, total);
    // Distinct: dedup by collecting into a set of the same length.
    let distinct: BTreeSet<u64> = all.iter().copied().collect();
    assert_eq!(
        distinct.len() as u64,
        total,
        "every HLC second is distinct (no lost/duplicated tick under contention)",
    );
    // Contiguous 1..=total (fetch_add starts at last_sequence == 0, first tick 1).
    all.sort_unstable();
    assert_eq!(*all.first().unwrap(), 1, "first HLC second is 1");
    assert_eq!(*all.last().unwrap(), total, "last HLC second is N*M");
    assert!(
        all.windows(2).all(|pair| pair[1] == pair[0] + 1),
        "HLC seconds are contiguous with no gap",
    );
}

// ───────────────────────── GRAPH-17: HLC reopen continuity ──────────────────

#[test]
fn recover_then_commit_advances_hlc_past_recovered_sequence() {
    // The doc on `DurableState` promises the HLC is seeded from
    // `WalWriter::last_sequence`, so after reopening a WAL the next timestamp
    // advances PAST the recovered sequence (no replay of stamps). Commit three
    // entries through one writer, drop it, reopen the same file in a fresh
    // DurableState, and assert the next timestamp is recovered_seq + 1.
    let path = temp_wal_path("hlc-reopen");
    {
        let provider = live_provider_with({
            let writer = WalWriter::open(&path, WalConfig::default()).unwrap();
            DurableState::new(writer)
        });
        for _ in 0..3 {
            let ts = DurableProvider::next_timestamp(provider.as_ref());
            DurableProvider::write_commit(provider.as_ref(), None, &[], ts).unwrap();
        }
        DurableProvider::flush(provider.as_ref()).unwrap();
        // The third commit took HLC second 3.
        assert_eq!(
            DurableProvider::next_timestamp(provider.as_ref()).seconds,
            4
        );
    }

    // Reopen the same WAL file: last_sequence is recovered from the on-disk tail
    // (3), so the freshly-seeded HLC's next tick is 4 — strictly past the
    // recovered sequence, never restarting at 1.
    let reopened = WalWriter::open(&path, WalConfig::default()).unwrap();
    let recovered_seq = reopened.last_sequence();
    assert_eq!(recovered_seq, 3, "WAL recovered three entries");
    let provider = live_provider_with(DurableState::new(reopened));
    let next = DurableProvider::next_timestamp(provider.as_ref());
    assert_eq!(
        next.seconds,
        recovered_seq + 1,
        "post-reopen HLC advances past the recovered WAL sequence",
    );
}

// ───────────────────────── GRAPH-18: append_audit_event ─────────────────────

#[test]
fn append_audit_event_without_log_returns_false() {
    // No audit log attached → the engine-event mirror is a no-op returning false
    // (the caller distinguishes "audit disabled" from "appended").
    let durable = durable_state("audit-none");
    assert!(
        !durable.append_audit_event(AUDIT_KIND_RESERVED_0, vec![1, 2, 3]),
        "append_audit_event must return false when no audit log is attached",
    );
}

#[test]
fn append_audit_event_with_log_round_trips_kind_payload_timestamp() {
    // With an audit log attached, append returns true and the record (kind +
    // payload + a non-zero wall-clock stamp) round-trips through AuditLog.
    let wal_path = temp_wal_path("audit-some");
    let dir = wal_path.parent().unwrap().to_path_buf();
    let audit_path = dir.join("audit.log");
    let before = now_unix_nanos();
    let writer = WalWriter::open(&wal_path, WalConfig::default()).unwrap();
    let audit = AuditLog::open_with_authority(writer.authority(), Path::new("audit.log")).unwrap();
    let durable = DurableState::new(writer).with_audit_log(audit).unwrap();

    let payload = vec![0xDE_u8, 0xAD, 0xBE, 0xEF];
    assert!(
        durable.append_audit_event(AUDIT_KIND_RESERVED_0, payload.clone()),
        "append_audit_event returns true on a successful append",
    );
    // Drop the DurableState's audit handle by reading the file independently.
    let records = AuditLog::read_all(&audit_path).unwrap();
    assert_eq!(records.len(), 1, "exactly one event persisted");
    assert_eq!(records[0].kind, AUDIT_KIND_RESERVED_0);
    assert_eq!(records[0].payload, payload);
    let after = now_unix_nanos();
    assert!(
        (before..=after).contains(&records[0].recorded_at_unix_nanos),
        "the engine stamps a current wall-clock time (got {}, window {before}..={after})",
        records[0].recorded_at_unix_nanos,
    );
}

// ───────────────────────── GRAPH-19: no-WAL (durable: None) ──────────────────

#[test]
fn write_commit_without_wal_returns_zero_sequence() {
    // Embeddable, no-persistence mode: `durable: None`. write_commit reports
    // sequence 0 (nothing appended) and flush reports None (nothing to flush) —
    // the in-memory commit still succeeds, durability is simply absent.
    let snapshot = Arc::new(ArcSwap::from_pointee(SeleneGraph::new(GraphId::new(1))));
    let provider = CoreProvider::new_for_live(snapshot); // no DurableState

    let ts = DurableProvider::next_timestamp(provider.as_ref());
    assert_eq!(ts, HlcTimestamp::zero(), "no-WAL HLC is the zero timestamp");
    let seq = DurableProvider::write_commit(
        provider.as_ref(),
        None,
        &[Change::NodeCreated {
            id: NodeId::new(1),
            labels: LabelSet::single(db_string("nowal.node").unwrap()),
            properties: PropertyMap::new(),
        }],
        ts,
    )
    .unwrap();
    assert_eq!(seq, 0, "no-WAL write_commit returns sequence 0");
    assert_eq!(
        DurableProvider::flush(provider.as_ref()).unwrap(),
        None,
        "no-WAL flush returns None",
    );
}
