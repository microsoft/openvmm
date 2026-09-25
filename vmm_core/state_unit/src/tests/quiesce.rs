// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Tests for gating host input around the snapshot boundary.

use crate::SpawnedUnit;
use crate::StateUnit;
use crate::StateUnits;
use crate::run_unit;
use inspect::InspectMut;
use pal_async::DefaultDriver;
use pal_async::async_test;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::time::Duration;
use test_with_tracing::test;
use vmcore::save_restore::RestoreError;
use vmcore::save_restore::SaveError;
use vmcore::save_restore::SavedStateBlob;

/// A unit whose input quiesce takes `delay` and then fails if `fail` is set,
/// and which records whether its input is quiesced.
struct InputUnit {
    driver: DefaultDriver,
    delay: Duration,
    fail: bool,
    quiesced: Arc<AtomicBool>,
}

impl StateUnit for InputUnit {
    async fn start(&mut self) {}

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

    async fn quiesce_input(&mut self) -> anyhow::Result<()> {
        pal_async::timer::PolledTimer::new(&self.driver)
            .sleep(self.delay)
            .await;
        anyhow::ensure!(!self.fail, "the device refused to quiesce its input");
        self.quiesced.store(true, Ordering::Relaxed);
        Ok(())
    }

    async fn resume_input(&mut self) -> anyhow::Result<()> {
        self.quiesced.store(false, Ordering::Relaxed);
        Ok(())
    }
}

impl InspectMut for InputUnit {
    fn inspect_mut(&mut self, req: inspect::Request<'_>) {
        req.respond();
    }
}

/// Adds an [`InputUnit`] named `name` to `units`, and returns it with its
/// record of whether its input is quiesced.
fn add_input_unit(
    units: &StateUnits,
    driver: &DefaultDriver,
    name: &str,
    delay: Duration,
    fail: bool,
) -> (SpawnedUnit<InputUnit>, Arc<AtomicBool>) {
    let quiesced = Arc::new(AtomicBool::new(false));
    let unit = units
        .add(name)
        .spawn(driver, {
            let driver = driver.clone();
            let quiesced = quiesced.clone();
            |recv| {
                run_unit(
                    InputUnit {
                        driver,
                        delay,
                        fail,
                        quiesced,
                    },
                    recv,
                )
            }
        })
        .unwrap();
    (unit, quiesced)
}

#[async_test]
async fn input_quiesce_and_resume_reach_the_units(driver: DefaultDriver) {
    let mut units = StateUnits::new();
    let (_unit, quiesced) = add_input_unit(&units, &driver, "input", Duration::ZERO, false);
    units.start().await.unwrap();

    units.quiesce_input_for_save().await.unwrap();
    assert!(quiesced.load(Ordering::Relaxed));

    units.resume_input_after_save().await.unwrap();
    assert!(!quiesced.load(Ordering::Relaxed));
    assert!(units.is_running());
}

#[async_test]
async fn a_failed_input_quiesce_rolls_back(driver: DefaultDriver) {
    let mut units = StateUnits::new();
    let (_good, good) = add_input_unit(&units, &driver, "good", Duration::ZERO, false);
    let (_bad, bad) = add_input_unit(&units, &driver, "bad", Duration::ZERO, true);
    units.start().await.unwrap();

    let error = format!("{:#}", units.quiesce_input_for_save().await.unwrap_err());
    assert!(error.contains("quiesce_input failed: bad: "), "{error}");
    assert!(error.contains("refused to quiesce its input"), "{error}");
    assert!(!error.contains("good"), "{error}");
    assert!(good.load(Ordering::Relaxed));
    assert!(!bad.load(Ordering::Relaxed));

    // The snapshot boundary rolls a failed gate back before the guest
    // continues without a snapshot, which resumes the input of every unit.
    units.resume_input_after_save().await.unwrap();
    assert!(!good.load(Ordering::Relaxed));
    assert!(units.is_running());
}
