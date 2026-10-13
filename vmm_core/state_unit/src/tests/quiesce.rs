// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Tests for snapshot quiesce and for restart after a failed save.

use super::TestUnit;
use crate::State;
use crate::StateRequest;
use crate::StateUnit;
use crate::StateUnits;
use crate::run_unit;
use futures::StreamExt;
use inspect::InspectMut;
use pal_async::DefaultDriver;
use pal_async::async_test;
use std::sync::Arc;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::time::Duration;
use test_with_tracing::test;
use vmcore::save_restore::RestoreError;
use vmcore::save_restore::SaveError;
use vmcore::save_restore::SavedStateBlob;

struct SlowStopUnit {
    driver: DefaultDriver,
    delay: Duration,
}

impl StateUnit for SlowStopUnit {
    async fn start(&mut self) -> anyhow::Result<()> {
        Ok(())
    }

    async fn stop(&mut self) {
        pal_async::timer::PolledTimer::new(&self.driver)
            .sleep(self.delay)
            .await;
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

struct SlowRollbackStartUnit {
    driver: DefaultDriver,
    starts: u32,
}

impl StateUnit for SlowRollbackStartUnit {
    async fn start(&mut self) -> anyhow::Result<()> {
        if self.starts != 0 {
            pal_async::timer::PolledTimer::new(&self.driver)
                .sleep(Duration::from_secs(1))
                .await;
        }
        self.starts += 1;
        Ok(())
    }

    async fn stop(&mut self) {}

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

impl InspectMut for SlowRollbackStartUnit {
    fn inspect_mut(&mut self, req: inspect::Request<'_>) {
        req.respond();
    }
}

struct StopCountingUnit {
    stops: Arc<AtomicUsize>,
}

impl StateUnit for StopCountingUnit {
    async fn start(&mut self) -> anyhow::Result<()> {
        Ok(())
    }

    async fn stop(&mut self) {
        self.stops.fetch_add(1, Ordering::Relaxed);
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

impl InspectMut for StopCountingUnit {
    fn inspect_mut(&mut self, req: inspect::Request<'_>) {
        req.respond();
    }
}

#[derive(Default)]
struct InputCounts {
    quiesces: AtomicUsize,
    resumes: AtomicUsize,
}

struct InputTestUnit {
    driver: DefaultDriver,
    counts: Arc<InputCounts>,
    delay: Option<Duration>,
    fail_start: bool,
    fail_quiesce: bool,
    fail_resume: bool,
}

impl StateUnit for InputTestUnit {
    async fn start(&mut self) -> anyhow::Result<()> {
        anyhow::ensure!(!self.fail_start, "intentional start failure");
        Ok(())
    }

    async fn stop(&mut self) {}

    async fn quiesce_input(&mut self) -> anyhow::Result<()> {
        self.counts.quiesces.fetch_add(1, Ordering::Relaxed);
        if let Some(delay) = self.delay {
            pal_async::timer::PolledTimer::new(&self.driver)
                .sleep(delay)
                .await;
        }
        anyhow::ensure!(!self.fail_quiesce, "intentional input quiesce failure");
        Ok(())
    }

    async fn resume_input(&mut self) -> anyhow::Result<()> {
        self.counts.resumes.fetch_add(1, Ordering::Relaxed);
        anyhow::ensure!(!self.fail_resume, "intentional input resume failure");
        Ok(())
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

impl InspectMut for InputTestUnit {
    fn inspect_mut(&mut self, req: inspect::Request<'_>) {
        req.respond();
    }
}

fn input_unit(driver: &DefaultDriver, counts: Arc<InputCounts>) -> InputTestUnit {
    InputTestUnit {
        driver: driver.clone(),
        counts,
        delay: None,
        fail_start: false,
        fail_quiesce: false,
        fail_resume: false,
    }
}

#[async_test]
async fn test_input_hooks_run_only_for_running_units(driver: DefaultDriver) {
    let units = StateUnits::new();
    let running_counts = Arc::new(InputCounts::default());
    let _running = units
        .add("running")
        .spawn(&driver, |recv| {
            run_unit(input_unit(&driver, running_counts.clone()), recv)
        })
        .unwrap();
    let mut units = units;
    units.start().await.unwrap();

    let stopped_counts = Arc::new(InputCounts::default());
    let _stopped = units
        .add("stopped")
        .spawn(&driver, |recv| {
            run_unit(input_unit(&driver, stopped_counts.clone()), recv)
        })
        .unwrap();

    units
        .quiesce_input_for_save(Duration::from_secs(1))
        .await
        .unwrap();
    units
        .resume_input_after_save(Duration::from_secs(1))
        .await
        .unwrap();

    assert_eq!(running_counts.quiesces.load(Ordering::Relaxed), 1);
    assert_eq!(running_counts.resumes.load(Ordering::Relaxed), 1);
    assert_eq!(stopped_counts.quiesces.load(Ordering::Relaxed), 0);
    assert_eq!(stopped_counts.resumes.load(Ordering::Relaxed), 0);
}

#[async_test]
async fn test_input_hook_errors_are_reported_per_unit(driver: DefaultDriver) {
    let mut units = StateUnits::new();
    let successful_counts = Arc::new(InputCounts::default());
    let _successful = units
        .add("successful")
        .spawn(&driver, |recv| {
            run_unit(input_unit(&driver, successful_counts.clone()), recv)
        })
        .unwrap();
    let failing_counts = Arc::new(InputCounts::default());
    let _failing = units
        .add("failing")
        .spawn(&driver, |recv| {
            let mut unit = input_unit(&driver, failing_counts.clone());
            unit.fail_quiesce = true;
            unit.fail_resume = true;
            run_unit(unit, recv)
        })
        .unwrap();
    units.start().await.unwrap();

    let error = units
        .quiesce_input_for_save(Duration::from_secs(1))
        .await
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("failing: intentional input quiesce failure")
    );
    let error = units
        .resume_input_after_save(Duration::from_secs(1))
        .await
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("failing: intentional input resume failure")
    );
    assert_eq!(successful_counts.quiesces.load(Ordering::Relaxed), 1);
    assert_eq!(successful_counts.resumes.load(Ordering::Relaxed), 1);
    assert_eq!(failing_counts.quiesces.load(Ordering::Relaxed), 1);
    assert_eq!(failing_counts.resumes.load(Ordering::Relaxed), 1);
}

#[async_test]
async fn test_input_hook_closed_channel_is_reported(driver: DefaultDriver) {
    let mut units = StateUnits::new();
    let _closed = units
        .add("closed")
        .spawn(&driver, |mut recv| async move {
            let request = recv.next().await.unwrap();
            let StateRequest::Start(response) = request else {
                panic!("unexpected request");
            };
            response.handle_failable_sync(|()| anyhow::Ok(()));
        })
        .unwrap();
    units.start().await.unwrap();

    let error = units
        .quiesce_input_for_save(Duration::from_secs(1))
        .await
        .unwrap_err();
    assert!(error.to_string().contains("quiesce_input failed: closed:"));
}

#[async_test]
async fn test_input_hook_timeout_is_bounded(driver: DefaultDriver) {
    let mut units = StateUnits::new();
    let failing_counts = Arc::new(InputCounts::default());
    let _failing = units
        .add("failing")
        .spawn(&driver, |recv| {
            let mut unit = input_unit(&driver, failing_counts.clone());
            unit.fail_quiesce = true;
            run_unit(unit, recv)
        })
        .unwrap();
    let counts = Arc::new(InputCounts::default());
    let _slow = units
        .add("slow")
        .spawn(&driver, |recv| {
            let mut unit = input_unit(&driver, counts.clone());
            unit.delay = Some(Duration::from_secs(1));
            run_unit(unit, recv)
        })
        .unwrap();
    units.start().await.unwrap();

    let started = std::time::Instant::now();
    let error = units
        .quiesce_input_for_save(Duration::from_millis(10))
        .await
        .unwrap_err();
    let error = error.to_string();
    assert!(error.contains("failing: intentional input quiesce failure"));
    assert!(error.contains("slow: quiesce_input timed out"));
    assert!(started.elapsed() < Duration::from_millis(500));
    assert_eq!(failing_counts.quiesces.load(Ordering::Relaxed), 1);
    assert_eq!(counts.quiesces.load(Ordering::Relaxed), 1);
}

#[async_test]
async fn test_input_hooks_reject_terminal_units(driver: DefaultDriver) {
    let mut units = StateUnits::new();
    let _running = units
        .add("running")
        .spawn(&driver, |recv| {
            run_unit(input_unit(&driver, Arc::new(InputCounts::default())), recv)
        })
        .unwrap();
    units.start().await.unwrap();

    let terminal_counts = Arc::new(InputCounts::default());
    let _terminal = units
        .add("terminal")
        .spawn(&driver, |recv| {
            let mut unit = input_unit(&driver, terminal_counts.clone());
            unit.fail_start = true;
            run_unit(unit, recv)
        })
        .unwrap();
    units.start_stopped_units().await.unwrap_err();

    let error = units
        .quiesce_input_for_save(Duration::from_secs(1))
        .await
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("terminal: unit is in terminal state StartFailedStopped")
    );
    assert_eq!(terminal_counts.quiesces.load(Ordering::Relaxed), 0);
}

#[async_test]
async fn test_quiesce_and_resume_after_failed_save(driver: DefaultDriver) {
    let mut units = StateUnits::new();
    let _unit = units
        .add("unit")
        .spawn(&driver, |recv| run_unit(TestUnit::default(), recv))
        .unwrap();
    units.start().await.unwrap();

    units
        .quiesce_for_save(Duration::from_secs(1))
        .await
        .unwrap();
    assert!(!units.is_running());

    units
        .resume_after_failed_save(Duration::from_secs(1))
        .await
        .unwrap();
    assert!(units.is_running());
}

#[async_test]
async fn test_quiesce_skips_stopped_units(driver: DefaultDriver) {
    let mut units = StateUnits::new();
    let _running = units
        .add("running")
        .spawn(&driver, |recv| run_unit(TestUnit::default(), recv))
        .unwrap();
    units.start().await.unwrap();

    let stops = Arc::new(AtomicUsize::new(0));
    let _stopped = units
        .add("stopped")
        .spawn(&driver, |recv| {
            run_unit(
                StopCountingUnit {
                    stops: stops.clone(),
                },
                recv,
            )
        })
        .unwrap();

    units
        .quiesce_for_save(Duration::from_secs(1))
        .await
        .unwrap();
    assert_eq!(stops.load(Ordering::Relaxed), 0);
    units
        .resume_after_failed_save(Duration::from_secs(1))
        .await
        .unwrap();
    assert!(
        units
            .inner
            .lock()
            .units
            .values()
            .any(|unit| unit.name.as_ref() == "stopped" && unit.state == State::Stopped)
    );
}

#[async_test]
async fn test_quiesce_timeout_is_uncertain(driver: DefaultDriver) {
    let mut units = StateUnits::new();
    let _unit = units
        .add("slow")
        .spawn(&driver, {
            let driver = driver.clone();
            |recv| {
                run_unit(
                    SlowStopUnit {
                        driver,
                        delay: Duration::from_secs(1),
                    },
                    recv,
                )
            }
        })
        .unwrap();
    units.start().await.unwrap();

    let started = std::time::Instant::now();
    let error = units
        .quiesce_for_save(Duration::from_millis(10))
        .await
        .unwrap_err();
    assert!(error.has_uncertain_state());
    assert!(started.elapsed() < Duration::from_millis(500));
    assert!(
        units
            .resume_after_failed_save(Duration::from_secs(1))
            .await
            .is_err()
    );
}

#[async_test]
async fn test_quiesce_readiness_timeout_is_bounded(driver: DefaultDriver) {
    let mut units = StateUnits::new();
    let a = units
        .add("a")
        .spawn(&driver, |recv| run_unit(TestUnit::default(), recv))
        .unwrap();
    units.start().await.unwrap();
    let _b = units
        .add("b")
        .depends_on(a.handle())
        .dependency_of(a.handle())
        .spawn(&driver, |recv| run_unit(TestUnit::default(), recv))
        .unwrap();
    units.start_stopped_units().await.unwrap();

    let started = std::time::Instant::now();
    let error = units
        .quiesce_for_save(Duration::from_millis(10))
        .await
        .unwrap_err();
    assert!(!error.has_uncertain_state());
    assert!(started.elapsed() < Duration::from_millis(500));
    assert!(units.is_running());
    assert!(
        units
            .inner
            .lock()
            .units
            .values()
            .all(|unit| unit.state == State::Running)
    );
}

#[async_test]
async fn test_quiesce_partial_success_is_uncertain(driver: DefaultDriver) {
    let mut units = StateUnits::new();
    let a = units
        .add("a")
        .spawn(&driver, |recv| run_unit(TestUnit::default(), recv))
        .unwrap();
    units.start().await.unwrap();
    let _b = units
        .add("b")
        .depends_on(a.handle())
        .dependency_of(a.handle())
        .spawn(&driver, |recv| run_unit(TestUnit::default(), recv))
        .unwrap();
    let stops = Arc::new(AtomicUsize::new(0));
    let _independent = units
        .add("independent")
        .spawn(&driver, |recv| {
            run_unit(
                StopCountingUnit {
                    stops: stops.clone(),
                },
                recv,
            )
        })
        .unwrap();
    units.start_stopped_units().await.unwrap();

    let started = std::time::Instant::now();
    let error = units
        .quiesce_for_save(Duration::from_millis(10))
        .await
        .unwrap_err();
    assert!(error.has_uncertain_state());
    assert!(started.elapsed() < Duration::from_millis(500));
    assert_eq!(stops.load(Ordering::Relaxed), 1);
    assert!(
        units
            .inner
            .lock()
            .units
            .values()
            .any(|unit| unit.name.as_ref() == "independent" && unit.state == State::Stopped)
    );
}

#[async_test]
async fn test_rollback_start_timeout_is_bounded(driver: DefaultDriver) {
    let mut units = StateUnits::new();
    let _unit = units
        .add("slow-rollback")
        .spawn(&driver, {
            let driver = driver.clone();
            |recv| run_unit(SlowRollbackStartUnit { driver, starts: 0 }, recv)
        })
        .unwrap();
    units.start().await.unwrap();
    units
        .quiesce_for_save(Duration::from_secs(1))
        .await
        .unwrap();

    let started = std::time::Instant::now();
    let error = units
        .resume_after_failed_save(Duration::from_millis(10))
        .await
        .unwrap_err();
    assert!(error.to_string().contains("snapshot rollback"));
    assert!(started.elapsed() < Duration::from_millis(500));
    assert!(!units.is_running());
}

#[async_test]
async fn test_rollback_readiness_timeout_is_bounded(driver: DefaultDriver) {
    let mut units = StateUnits::new();
    let a = units
        .add("a")
        .spawn(&driver, |recv| run_unit(TestUnit::default(), recv))
        .unwrap();
    units.start().await.unwrap();
    units
        .quiesce_for_save(Duration::from_secs(1))
        .await
        .unwrap();
    let b = units
        .add("b")
        .depends_on(a.handle())
        .dependency_of(a.handle())
        .spawn(&driver, |recv| run_unit(TestUnit::default(), recv))
        .unwrap();
    // Exercise the deadline against a cyclic rollback set directly.
    units
        .save_quiesced_units
        .as_mut()
        .unwrap()
        .push(b.handle().id.id);

    let started = std::time::Instant::now();
    let error = units
        .resume_after_failed_save(Duration::from_millis(10))
        .await
        .unwrap_err();
    assert!(error.to_string().contains("snapshot rollback"));
    assert!(started.elapsed() < Duration::from_millis(500));
    assert!(!units.is_running());
    assert!(
        units
            .inner
            .lock()
            .units
            .values()
            .all(|unit| unit.state == State::Stopped)
    );
}
