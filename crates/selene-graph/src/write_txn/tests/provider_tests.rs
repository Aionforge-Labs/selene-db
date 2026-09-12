use super::*;

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

struct ReentrantProvider {
    tag: ProviderTag,
    shared: Mutex<Option<Arc<SharedGraph>>>,
    chained_count: Arc<Mutex<usize>>,
}

impl ReentrantProvider {
    fn new(tag: ProviderTag) -> Self {
        Self {
            tag,
            shared: Mutex::new(None),
            chained_count: Arc::new(Mutex::new(0)),
        }
    }

    fn install_shared(&self, shared: Arc<SharedGraph>) {
        *self.shared.lock() = Some(shared);
    }
}

impl IndexProvider for ReentrantProvider {
    fn provider_tag(&self) -> ProviderTag {
        self.tag
    }

    fn on_change(&self, _change: &Change) -> Result<(), ProviderError> {
        // begin_write() panics on this thread because the FanoutGuard is
        // active. The panic unwinds out of on_change before reaching the
        // chained_count increment.
        let shared = self.shared.lock().take();
        if let Some(shared) = shared {
            let txn = shared.begin_write();
            let _ = txn.commit();
            *self.chained_count.lock() += 1;
        }
        Ok(())
    }
}

#[test]
fn begin_write_inside_provider_callback_panics_and_is_caught() {
    let provider = Arc::new(ReentrantProvider::new(ProviderTag(*b"REEN")));
    let chained_count = Arc::clone(&provider.chained_count);
    let shared = Arc::new(
        SharedGraph::builder(GraphId::new(1))
            .with_provider(Arc::clone(&provider) as Arc<dyn IndexProvider>)
            .build()
            .unwrap(),
    );
    provider.install_shared(Arc::clone(&shared));

    let mut txn = shared.begin_write();
    {
        let mut mutator = txn.mutator();
        mutator
            .create_node(LabelSet::new(), PropertyMap::new())
            .expect("create_node ok");
    }
    // Outer commit completes despite the provider's misuse - the panic
    // raised by begin_write inside on_change is caught by
    // notify_providers' catch_unwind boundary.
    let outcome = txn.commit().unwrap();
    assert_eq!(outcome.changes.len(), 1);
    // The increment after begin_write/commit was never reached.
    assert_eq!(
        *chained_count.lock(),
        0,
        "provider's chained mutation must not have completed"
    );
    // After the outer commit returns, the graph's fanout flag is clear
    // and a fresh begin_write succeeds normally.
    let txn = shared.begin_write();
    txn.rollback();
}

/// A provider whose `on_change` panics. Used to verify the engine catches
/// the unwind and continues serving subsequent providers.
struct PanickingProvider {
    tag: ProviderTag,
}

impl IndexProvider for PanickingProvider {
    fn provider_tag(&self) -> ProviderTag {
        self.tag
    }

    fn on_change(&self, _change: &Change) -> Result<(), ProviderError> {
        panic!("synthetic provider panic");
    }
}

#[test]
fn provider_panic_does_not_crash_commit_or_block_other_providers() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let shared = SharedGraph::builder(GraphId::new(1))
        .with_provider(Arc::new(PanickingProvider {
            tag: ProviderTag(*b"PANC"),
        }))
        .with_provider(Arc::new(RecordingProvider::new(
            ProviderTag(*b"AFTR"),
            Arc::clone(&seen),
        )))
        .build()
        .unwrap();
    let mut txn = shared.begin_write();
    let id = {
        let mut mutator = txn.mutator();
        mutator
            .create_node(LabelSet::new(), PropertyMap::new())
            .expect("create_node ok")
    };
    let outcome = txn.commit().unwrap();
    assert!(shared.read().is_node_alive(id));
    assert_eq!(outcome.changes.len(), 1);
    // The provider AFTER the panicking one still received the change.
    assert_eq!(seen.lock().len(), 1);
}

/// A provider whose `on_change` blocks for a brief window so a second
/// writer thread can observe that it queues normally on the write lock
/// without panicking - the explicit non-regression for the round-4 P1.
struct SlowProvider {
    tag: ProviderTag,
    hold: std::time::Duration,
}

impl IndexProvider for SlowProvider {
    fn provider_tag(&self) -> ProviderTag {
        self.tag
    }

    fn on_change(&self, _change: &Change) -> Result<(), ProviderError> {
        std::thread::sleep(self.hold);
        Ok(())
    }
}

#[test]
fn concurrent_writer_does_not_panic_during_other_commits_fanout() {
    // Regression test for the round-4 P1: the previous design had
    // begin_write panic on every concurrent writer while another
    // commit's fanout was running. Concurrent writers must queue on
    // the write lock and proceed normally instead.
    let shared = Arc::new(
        SharedGraph::builder(GraphId::new(1))
            .with_provider(Arc::new(SlowProvider {
                tag: ProviderTag(*b"SLOW"),
                hold: std::time::Duration::from_millis(40),
            }))
            .build()
            .unwrap(),
    );

    let writer_a = {
        let shared = Arc::clone(&shared);
        thread::spawn(move || {
            let mut txn = shared.begin_write();
            {
                let mut mutator = txn.mutator();
                mutator
                    .create_node(LabelSet::new(), PropertyMap::new())
                    .expect("create_node ok");
            }
            txn.commit().unwrap();
        })
    };

    // Give writer A a head start so it owns the lock and is inside the
    // SlowProvider's on_change.
    thread::sleep(std::time::Duration::from_millis(10));

    // Writer B begins a write while A's fanout is running. It must
    // queue on the write lock, NOT panic.
    let writer_b = {
        let shared = Arc::clone(&shared);
        thread::spawn(move || {
            let mut txn = shared.begin_write();
            {
                let mut mutator = txn.mutator();
                mutator
                    .create_node(LabelSet::new(), PropertyMap::new())
                    .expect("create_node ok");
            }
            txn.commit().unwrap();
        })
    };

    writer_a.join().expect("writer A finished without panic");
    writer_b.join().expect("writer B finished without panic");
    assert_eq!(shared.read().node_count(), 2);
}

/// A provider whose `provider_tag()` panics only after the build-time
/// uniqueness check has already run successfully. Used to verify the
/// engine short-circuits `on_change` for that provider rather than
/// calling it after a fanout-time tag panic.
struct ConditionallyTagPanickingProvider {
    tag: ProviderTag,
    panic_during_fanout: Arc<std::sync::atomic::AtomicBool>,
    on_change_called: Arc<Mutex<bool>>,
}

impl IndexProvider for ConditionallyTagPanickingProvider {
    fn provider_tag(&self) -> ProviderTag {
        if self.panic_during_fanout.load(Ordering::Acquire) {
            panic!("synthetic provider_tag() panic during fanout");
        }
        self.tag
    }

    fn on_change(&self, _change: &Change) -> Result<(), ProviderError> {
        *self.on_change_called.lock() = true;
        Ok(())
    }
}

#[test]
fn provider_tag_panic_short_circuits_on_change_for_that_provider() {
    let panic_flag = Arc::new(AtomicBool::new(false));
    let on_change_called = Arc::new(Mutex::new(false));
    let other_seen = Arc::new(Mutex::new(Vec::new()));
    let shared = SharedGraph::builder(GraphId::new(1))
        .with_provider(Arc::new(ConditionallyTagPanickingProvider {
            tag: ProviderTag(*b"TPNC"),
            panic_during_fanout: Arc::clone(&panic_flag),
            on_change_called: Arc::clone(&on_change_called),
        }))
        .with_provider(Arc::new(RecordingProvider::new(
            ProviderTag(*b"OTHR"),
            Arc::clone(&other_seen),
        )))
        .build()
        .unwrap();

    // Arm the panic only after the build-time uniqueness check has run.
    panic_flag.store(true, Ordering::Release);

    let mut txn = shared.begin_write();
    {
        let mut mutator = txn.mutator();
        mutator
            .create_node(LabelSet::new(), PropertyMap::new())
            .expect("create_node ok");
    }
    txn.commit().unwrap();
    assert!(
        !*on_change_called.lock(),
        "on_change must not run after provider_tag() panicked",
    );
    // The non-panicking provider after the panicking one still ran.
    assert_eq!(other_seen.lock().len(), 1);
}

struct WatermarkCandidateProvider {
    fail_fanout: bool,
    batch: bool,
    watermark: AtomicU64,
    changes_seen: AtomicU64,
    commit_applied_calls: AtomicU64,
}

impl WatermarkCandidateProvider {
    fn new(fail_fanout: bool, batch: bool) -> Self {
        Self {
            fail_fanout,
            batch,
            watermark: AtomicU64::new(0),
            changes_seen: AtomicU64::new(0),
            commit_applied_calls: AtomicU64::new(0),
        }
    }

    fn watermark(&self) -> u64 {
        self.watermark.load(Ordering::Acquire)
    }

    fn changes_seen(&self) -> u64 {
        self.changes_seen.load(Ordering::Acquire)
    }

    fn commit_applied_calls(&self) -> u64 {
        self.commit_applied_calls.load(Ordering::Acquire)
    }
}

impl IndexProvider for WatermarkCandidateProvider {
    fn provider_tag(&self) -> ProviderTag {
        ProviderTag(crate::CANDIDATE_STATE_PROVIDER_TAG)
    }

    fn on_change(&self, _change: &Change) -> Result<(), ProviderError> {
        self.changes_seen.fetch_add(1, Ordering::AcqRel);
        if self.fail_fanout {
            Err(ProviderError::Inconsistent {
                reason: "synthetic candidate-state change failure".to_owned(),
            })
        } else {
            Ok(())
        }
    }

    fn handles_change_batches(&self) -> bool {
        self.batch
    }

    fn on_changes(&self, changes: &[Change]) -> Result<(), ProviderError> {
        self.changes_seen
            .fetch_add(changes.len() as u64, Ordering::AcqRel);
        if self.fail_fanout {
            Err(ProviderError::Inconsistent {
                reason: "synthetic candidate-state batch failure".to_owned(),
            })
        } else {
            Ok(())
        }
    }

    fn on_commit_applied(&self, generation: u64) -> Result<(), ProviderError> {
        self.commit_applied_calls.fetch_add(1, Ordering::AcqRel);
        self.watermark.store(generation, Ordering::Release);
        Ok(())
    }

    fn vector_candidate_state_infos(
        &self,
        generation: u64,
    ) -> Result<Vec<crate::VectorCandidateStateInfo>, ProviderError> {
        let watermark = self.watermark();
        if watermark != generation {
            return Err(ProviderError::Inconsistent {
                reason: format!(
                    "candidate-state generation {watermark} does not match graph generation {generation}"
                ),
            });
        }
        Ok(vec![crate::VectorCandidateStateInfo {
            name: db_string("watermark"),
            generation,
            candidate_count: 0,
            required_label: None,
            require_outgoing: Vec::new(),
            require_incoming: Vec::new(),
            exclude_outgoing: Vec::new(),
            exclude_incoming: Vec::new(),
        }])
    }
}

fn commit_one_node_with_provider(provider: Arc<WatermarkCandidateProvider>) -> SharedGraph {
    let shared = SharedGraph::builder(GraphId::new(2))
        .with_provider(provider as Arc<dyn IndexProvider>)
        .build()
        .unwrap();
    let mut txn = shared.begin_write();
    txn.mutator()
        .create_node(LabelSet::new(), PropertyMap::new())
        .expect("create_node ok");
    let outcome = txn.commit().expect("provider fanout does not fail commit");
    assert_eq!(outcome.generation, 1);
    assert_eq!(shared.read().meta.generation, 1);
    shared
}

#[test]
fn provider_generation_watermark_advances_after_successful_fanout() {
    let provider = Arc::new(WatermarkCandidateProvider::new(false, false));
    let shared = commit_one_node_with_provider(Arc::clone(&provider));

    assert_eq!(provider.changes_seen(), 1);
    assert_eq!(provider.commit_applied_calls(), 1);
    assert_eq!(provider.watermark(), 1);
    let infos = shared
        .vector_candidate_state_infos()
        .expect("successful provider advertises current generation");
    assert_eq!(infos.len(), 1);
    assert_eq!(infos[0].generation, 1);
}

#[test]
fn provider_generation_watermark_stays_stale_after_change_error() {
    let provider = Arc::new(WatermarkCandidateProvider::new(true, false));
    let shared = commit_one_node_with_provider(Arc::clone(&provider));

    assert_eq!(provider.changes_seen(), 1);
    assert_eq!(provider.commit_applied_calls(), 0);
    assert_eq!(provider.watermark(), 0);
    let err = shared
        .vector_candidate_state_infos()
        .expect_err("stale candidate provider must not advertise current state");
    assert!(matches!(err, ProviderError::Inconsistent { reason }
            if reason.contains("candidate-state generation 0 does not match graph generation 1")));
}

#[test]
fn provider_generation_watermark_stays_stale_after_batch_error() {
    let provider = Arc::new(WatermarkCandidateProvider::new(true, true));
    let shared = commit_one_node_with_provider(Arc::clone(&provider));

    assert_eq!(provider.changes_seen(), 1);
    assert_eq!(provider.commit_applied_calls(), 0);
    assert_eq!(provider.watermark(), 0);
    let err = shared
        .vector_candidate_state_infos()
        .expect_err("stale batch provider must not advertise current state");
    assert!(matches!(err, ProviderError::Inconsistent { reason }
            if reason.contains("candidate-state generation 0 does not match graph generation 1")));
}

#[test]
#[cfg(not(miri))]
fn concurrent_writers_notify_provider_for_every_change() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let shared = Arc::new(
        SharedGraph::builder(GraphId::new(1))
            .with_provider(Arc::new(RecordingProvider::new(
                ProviderTag(*b"CNCR"),
                Arc::clone(&seen),
            )))
            .build()
            .unwrap(),
    );
    let nodes_per_thread = 64;
    thread::scope(|scope| {
        for _ in 0..4 {
            let shared = Arc::clone(&shared);
            scope.spawn(move || {
                let mut txn = shared.begin_write();
                {
                    let mut mutator = txn.mutator();
                    for _ in 0..nodes_per_thread {
                        mutator
                            .create_node(LabelSet::new(), PropertyMap::new())
                            .expect("create_node ok");
                    }
                }
                txn.commit().unwrap();
            });
        }
    });
    assert_eq!(shared.read().node_count(), 4 * nodes_per_thread);
    assert_eq!(seen.lock().len(), 4 * nodes_per_thread);
}
