// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Tests for start with rollback.

use crate::StateRequest;
use crate::StateUnit;
use crate::StateUnits;
use crate::run_unit;
use futures::StreamExt;
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

struct RecordingUnit {
    started: Arc<AtomicBool>,
    stopped: Arc<AtomicBool>,
}

impl StateUnit for RecordingUnit {
    async fn start(&mut self) {
        self.started.store(true, Ordering::Relaxed);
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

impl InspectMut for RecordingUnit {
    fn inspect_mut(&mut self, req: inspect::Request<'_>) {
        req.respond();
    }
}

#[async_test]
async fn an_unanswered_start_rolls_back_and_blocks_dependents(driver: DefaultDriver) {
    let mut units = StateUnits::new();
    let independent_started = Arc::new(AtomicBool::new(false));
    let independent_stopped = Arc::new(AtomicBool::new(false));
    let _independent = units
        .add("independent")
        .spawn(&driver, |recv| {
            run_unit(
                RecordingUnit {
                    started: independent_started.clone(),
                    stopped: independent_stopped.clone(),
                },
                recv,
            )
        })
        .unwrap();
    // A unit whose task drops its start request without answering, as a
    // task that has failed would.
    let unanswered = units
        .add("unanswered")
        .spawn(&driver, |mut recv| async move {
            while let Some(request) = recv.next().await {
                if matches!(request, StateRequest::Start(_)) {
                    break;
                }
            }
        })
        .unwrap();
    let dependent_started = Arc::new(AtomicBool::new(false));
    let _dependent = units
        .add("dependent")
        .depends_on(unanswered.handle())
        .spawn(&driver, |recv| {
            run_unit(
                RecordingUnit {
                    started: dependent_started.clone(),
                    stopped: Arc::new(AtomicBool::new(false)),
                },
                recv,
            )
        })
        .unwrap();

    let error = units.start().await.unwrap_err();
    assert!(error.to_string().contains("unanswered"), "{error:#}");
    assert!(independent_started.load(Ordering::Relaxed));
    assert!(independent_stopped.load(Ordering::Relaxed));
    assert!(!dependent_started.load(Ordering::Relaxed));
    assert!(!units.is_running());
    let retry_error = units.start().await.unwrap_err();
    assert!(retry_error.to_string().contains("terminal state"));
}
