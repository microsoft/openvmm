// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Tests for snapshot quiesce and for restart after a failed save.

use super::TestUnit;
use crate::StateUnit;
use crate::StateUnits;
use crate::run_unit;
use inspect::InspectMut;
use pal_async::DefaultDriver;
use pal_async::async_test;
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
