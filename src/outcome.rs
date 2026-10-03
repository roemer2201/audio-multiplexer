//! Worker outcomes survive thread completion and explicit shutdown.

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::Result;

#[derive(Default)]
pub struct WorkerFailure(Mutex<Option<String>>);

impl WorkerFailure {
    pub fn record(&self, message: String) {
        let mut slot = self.0.lock().unwrap_or_else(|e| e.into_inner());
        if slot.is_none() {
            *slot = Some(message);
        }
    }

    pub fn message(&self) -> Option<String> {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// Catch panics at the worker boundary so status cannot remain Running
    /// after the thread has died. Retain the complete anyhow error chain.
    pub fn run(&self, task: impl FnOnce() -> Result<()>) {
        match catch_unwind(AssertUnwindSafe(task)) {
            Ok(Ok(())) => {}
            Ok(Err(error)) => self.record(format!("{error:#}")),
            Err(payload) => {
                let reason = payload
                    .downcast_ref::<String>()
                    .map(String::as_str)
                    .or_else(|| payload.downcast_ref::<&str>().copied())
                    .unwrap_or("unknown panic payload");
                self.record(format!("worker panicked: {reason}"));
            }
        }
    }

    pub fn run_source(&self, stop: &AtomicBool, task: impl FnOnce() -> Result<()>) {
        self.run(task);
        // Publish the outcome before a frontend observes that the source died.
        stop.store(true, Ordering::Release);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::{Context, bail};
    use std::sync::Arc;

    #[test]
    fn source_errors_survive_completion_and_shutdown() {
        for stage in ["opening capture", "starting capture", "draining capture"] {
            let failure = Arc::new(WorkerFailure::default());
            let stop = Arc::new(AtomicBool::new(false));
            let worker_failure = Arc::clone(&failure);
            let worker_stop = Arc::clone(&stop);
            std::thread::spawn(move || {
                worker_failure.run_source(&worker_stop, || {
                    (|| bail!("injected device failure"))().context(stage)
                });
            })
            .join()
            .unwrap();
            assert!(stop.load(Ordering::Acquire));
            stop.store(true, Ordering::Release);
            assert_eq!(
                failure.message().unwrap(),
                format!("{stage}: injected device failure")
            );
        }
    }

    #[test]
    fn normal_stop_is_success() {
        let stop = AtomicBool::new(true);
        let failure = WorkerFailure::default();
        failure.run_source(&stop, || Ok(()));
        assert!(failure.message().is_none());
    }

    #[test]
    fn source_panic_is_recorded_before_stop() {
        let stop = AtomicBool::new(false);
        let failure = WorkerFailure::default();
        failure.run_source(&stop, || panic!("injected panic"));
        assert!(stop.load(Ordering::Acquire));
        assert_eq!(
            failure.message().unwrap(),
            "worker panicked: injected panic"
        );
    }
}
