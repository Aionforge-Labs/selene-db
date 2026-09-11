//! Per-capability deterministic fault and race seams. Never process-global.

use super::StoreDirectory;
use crate::PersistResult;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

type PhaseHook = (&'static str, Box<dyn FnOnce() + Send>);

#[derive(Default)]
pub(super) struct TestHooks {
    fault: Mutex<Option<(&'static str, std::io::ErrorKind)>>,
    before_open: Mutex<Option<Box<dyn FnOnce() + Send>>>,
    control_payload_opens: AtomicUsize,
    phase: Mutex<Option<PhaseHook>>,
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
        self.fail_with(point, std::io::ErrorKind::Other);
    }

    pub(crate) fn fail_with(&self, point: &'static str, kind: std::io::ErrorKind) {
        *self.hooks.fault.lock().unwrap() = Some((point, kind));
    }

    pub(super) fn run_fault(&self, point: &'static str) -> PersistResult<()> {
        let hook = {
            let mut phase = self.hooks.phase.lock().unwrap();
            if phase.as_ref().is_some_and(|(name, _)| *name == point) {
                phase.take().map(|(_, hook)| hook)
            } else {
                None
            }
        };
        if let Some(hook) = hook {
            hook();
        }
        let mut fault = self.hooks.fault.lock().unwrap();
        if fault.as_ref().is_some_and(|(name, _)| *name == point) {
            let (_, kind) = fault.take().unwrap();
            return Err(std::io::Error::new(kind, format!("injected {point}")).into());
        }
        Ok(())
    }

    pub(crate) fn at_phase(&self, point: &'static str, hook: impl FnOnce() + Send + 'static) {
        *self.hooks.phase.lock().unwrap() = Some((point, Box::new(hook)));
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
