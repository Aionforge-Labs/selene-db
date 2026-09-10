//! Per-capability deterministic fault and race seams. Never process-global.

use super::StoreDirectory;
use crate::PersistResult;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

#[derive(Default)]
pub(super) struct TestHooks {
    fault: Mutex<Option<&'static str>>,
    before_open: Mutex<Option<Box<dyn FnOnce() + Send>>>,
    control_payload_opens: AtomicUsize,
}

impl std::fmt::Debug for TestHooks {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("TestHooks")
    }
}

impl StoreDirectory {
    pub(crate) fn record_control_payload_open(&self) {
        self.hooks
            .control_payload_opens
            .fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn reset_control_payload_opens(&self) {
        self.hooks.control_payload_opens.store(0, Ordering::Relaxed);
    }

    pub(crate) fn control_payload_opens(&self) -> usize {
        self.hooks.control_payload_opens.load(Ordering::Relaxed)
    }

    pub(crate) fn fail_at(&self, point: &'static str) {
        *self.hooks.fault.lock().unwrap() = Some(point);
    }

    pub(super) fn run_fault(&self, point: &'static str) -> PersistResult<()> {
        let mut fault = self.hooks.fault.lock().unwrap();
        if *fault == Some(point) {
            *fault = None;
            return Err(std::io::Error::other(format!("injected {point}")).into());
        }
        Ok(())
    }

    pub(crate) fn before_open(&self, hook: impl FnOnce() + Send + 'static) {
        *self.hooks.before_open.lock().unwrap() = Some(Box::new(hook));
    }

    pub(super) fn run_open_hook(&self) {
        let hook = self.hooks.before_open.lock().unwrap().take();
        if let Some(hook) = hook {
            hook();
        }
    }
}
