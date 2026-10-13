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
async fn test_stop_cleans_up_uncertain_quiesce(driver: DefaultDriver) {
    let mut units = StateUnits::new();
    let stops = Arc::new(AtomicUsize::new(0));
    let stop_requests = Arc::new(AtomicUsize::new(0));
    let _unit = units
        .add("uncertain")
        .spawn(&driver, |mut recv| {
            let stop_requests = stop_requests.clone();
            let mut unit = StopCountingUnit {
                stops: stops.clone(),
            };
            async move {
                while let Some(request) = recv.next().await {
                    match request {
                        StateRequest::Stop(response) => {
                            if stop_requests.fetch_add(1, Ordering::Relaxed) == 0 {
                                drop(response);
                            } else {
                                StateRequest::Stop(response).apply(&mut unit).await;
                            }
                        }
                        request => request.apply(&mut unit).await,
                    }
                }
                unit
            }
        })
        .unwrap();
    units.start().await.unwrap();

    let error = units
        .quiesce_for_save(Duration::from_secs(1))
        .await
        .unwrap_err();
    assert!(error.has_uncertain_state());
    assert_eq!(stop_requests.load(Ordering::Relaxed), 1);
    assert_eq!(stops.load(Ordering::Relaxed), 0);

    units.stop().await;
    assert_eq!(stop_requests.load(Ordering::Relaxed), 2);
    assert_eq!(stops.load(Ordering::Relaxed), 1);
    assert!(!units.is_running());
    assert!(
        units
            .inner
            .lock()
            .units
            .values()
            .any(|unit| unit.state == State::QuiesceUncertain)
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
