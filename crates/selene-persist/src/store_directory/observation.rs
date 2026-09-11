//! Native per-capability test observation; no process-global hooks or store files.
use super::*;
use std::sync::{
    Mutex,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};

type Hook = (&'static str, Box<dyn FnOnce() + Send>);

#[derive(Default)]
pub(super) struct Observation {
    forbid: AtomicBool,
    writes: AtomicUsize,
    phase: Mutex<Option<Hook>>,
}
impl std::fmt::Debug for Observation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Observation")
    }
}

impl StoreDirectory {
    /// Test-only reject and count subsequent writable opens and directory syncs.
    #[doc(hidden)]
    pub fn test_forbid_writes(&self) {
        self.observation.writes.store(0, Ordering::Relaxed);
        self.observation.forbid.store(true, Ordering::Relaxed);
    }
    /// Test-only number of writable opens or directory sync attempts on this capability.
    #[doc(hidden)]
    pub fn test_write_attempts(&self) -> usize {
        self.observation.writes.load(Ordering::Relaxed)
    }
    /// Test-only one-shot pause at a real native persistence phase.
    #[doc(hidden)]
    pub fn test_at_phase(&self, point: &'static str, hook: impl FnOnce() + Send + 'static) {
        *self.observation.phase.lock().unwrap() = Some((point, Box::new(hook)));
    }
    pub(super) fn observe_mutation(&self) -> PersistResult<()> {
        self.observation.writes.fetch_add(1, Ordering::Relaxed);
        if self.observation.forbid.load(Ordering::Relaxed) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "test forbids mutation",
            )
            .into());
        }
        Ok(())
    }
    pub(super) fn observe_phase(&self, point: &'static str) {
        let hook = {
            let mut slot = self.observation.phase.lock().unwrap();
            if slot.as_ref().is_some_and(|(p, _)| *p == point) {
                slot.take()
            } else {
                None
            }
        };
        if let Some((_, hook)) = hook {
            hook();
        }
    }
}
