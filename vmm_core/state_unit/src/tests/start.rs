// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Tests for fallible start.

use crate::StateUnit;
use crate::StateUnits;
use crate::run_unit;
use inspect::InspectMut;
use pal_async::DefaultDriver;
use pal_async::async_test;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use test_with_tracing::test;
use vmcore::save_restore::RestoreError;
use vmcore::save_restore::SaveError;
use vmcore::save_restore::SavedStateBlob;

struct StartResultUnit {
    started: Arc<AtomicBool>,
    stopped: Arc<AtomicBool>,
    fail: bool,
}

impl StateUnit for StartResultUnit {
    async fn start(&mut self) -> anyhow::Result<()> {
        anyhow::ensure!(!self.fail, "intentional start failure");
        self.started.store(true, Ordering::Relaxed);
        Ok(())
    }

    async fn stop(&mut self) {
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

impl InspectMut for StartResultUnit {
    fn inspect_mut(&mut self, req: inspect::Request<'_>) {
        req.respond();
    }
}

#[async_test]
async fn test_start_failure_blocks_dependent(driver: DefaultDriver) {
    let mut units = StateUnits::new();
    let independent_started = Arc::new(AtomicBool::new(false));
    let independent_stopped = Arc::new(AtomicBool::new(false));
    let _independent = units
        .add("independent")
        .spawn(&driver, |recv| {
            run_unit(
                StartResultUnit {
                    started: independent_started.clone(),
                    stopped: independent_stopped.clone(),
                    fail: false,
                },
                recv,
            )
        })
        .unwrap();
    let failed = units
        .add("failed")
        .spawn(&driver, |recv| {
            run_unit(
                StartResultUnit {
                    started: Arc::new(AtomicBool::new(false)),
                    stopped: Arc::new(AtomicBool::new(false)),
                    fail: true,
                },
                recv,
            )
        })
        .unwrap();
    let dependent_started = Arc::new(AtomicBool::new(false));
    let _dependent = units
        .add("dependent")
        .depends_on(failed.handle())
        .spawn(&driver, |recv| {
            run_unit(
                StartResultUnit {
                    started: dependent_started.clone(),
                    stopped: Arc::new(AtomicBool::new(false)),
                    fail: false,
                },
                recv,
            )
        })
        .unwrap();

    let error = units.start().await.unwrap_err();
    assert!(error.to_string().contains("intentional start failure"));
    assert!(independent_started.load(Ordering::Relaxed));
    assert!(independent_stopped.load(Ordering::Relaxed));
    assert!(!dependent_started.load(Ordering::Relaxed));
    assert!(!units.is_running());
    let retry_error = units.start().await.unwrap_err();
    assert!(retry_error.to_string().contains("terminal state"));
}
