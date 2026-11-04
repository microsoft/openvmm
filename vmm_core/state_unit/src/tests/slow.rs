// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Tests for logging the units that a slow state change waits for.

use super::TestUnit;
use crate::SLOW_STATE_CHANGE_INTERVAL;
use crate::StateUnit;
use crate::StateUnits;
use crate::run_unit;
use inspect::InspectMut;
use pal_async::DefaultDriver;
use pal_async::async_test;
use parking_lot::Mutex;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use test_with_tracing::test;
use vmcore::save_restore::RestoreError;
use vmcore::save_restore::SaveError;
use vmcore::save_restore::SavedStateBlob;

/// A unit whose stop takes three logging intervals.
struct SlowStopUnit {
    driver: DefaultDriver,
    stopped: Arc<AtomicBool>,
}

impl StateUnit for SlowStopUnit {
    async fn start(&mut self) {}

    async fn stop(&mut self) {
        pal_async::timer::PolledTimer::new(&self.driver)
            .sleep(3 * SLOW_STATE_CHANGE_INTERVAL)
            .await;
        self.stopped.store(true, Ordering::Relaxed);
    }

    async fn reset(&mut self) -> anyhow::Result<()> {
        Ok(())
    }

    async fn save(&mut self) -> Result<Option<SavedStateBlob>, SaveError> {
        Ok(None)
    }

    async fn restore(&mut self, _state: SavedStateBlob) -> Result<(), RestoreError> {
        Err(RestoreError::SavedStateNotSupported)
    }
}

impl InspectMut for SlowStopUnit {
    fn inspect_mut(&mut self, req: inspect::Request<'_>) {
        req.respond();
    }
}

/// The text that a subscriber logs.
#[derive(Clone, Default)]
pub(super) struct Log(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for Log {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl Log {
    pub(super) fn subscriber(&self) -> impl tracing::Subscriber + Send + Sync + use<> {
        let log = self.clone();
        tracing_subscriber::fmt()
            .with_ansi(false)
            .with_writer(move || log.clone())
            .finish()
    }

    /// Returns the logged lines that report an unanswered request.
    pub(super) fn unanswered(&self) -> Vec<String> {
        String::from_utf8_lossy(&self.0.lock())
            .lines()
            .filter(|line| line.contains("has not answered its state change request"))
            .map(str::to_owned)
            .collect()
    }
}

#[async_test]
async fn a_slow_stop_is_awaited_and_names_its_unit(driver: DefaultDriver) {
    let log = Log::default();
    let _subscriber = tracing::subscriber::set_default(log.subscriber());
    let mut units = StateUnits::new();
    let _fast = units
        .add("fast")
        .spawn(&driver, |recv| run_unit(TestUnit::default(), recv))
        .unwrap();
    let stopped = Arc::new(AtomicBool::new(false));
    let _slow = units
        .add("slow")
        .spawn(&driver, {
            let driver = driver.clone();
            let stopped = stopped.clone();
            |recv| run_unit(SlowStopUnit { driver, stopped }, recv)
        })
        .unwrap();
    units.start().await.unwrap();
    assert!(log.unanswered().is_empty());

    units.stop().await;

    assert!(stopped.load(Ordering::Relaxed));
    assert!(!units.is_running());
    let unanswered = log.unanswered();
    assert!(!unanswered.is_empty(), "the slow stop was not logged");
    for line in unanswered {
        assert!(line.contains(r#"unit="slow""#), "{line}");
        assert!(!line.contains("fast"), "{line}");
    }
}
