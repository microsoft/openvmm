// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Tests for fallible start.

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
use std::sync::atomic::AtomicUsize;
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
        self.started.store(true, Ordering::Relaxed);
        anyhow::ensure!(!self.fail, "intentional start failure");
        Ok(())
    }

    async fn stop(&mut self) {
        assert!(!self.stopped.swap(true, Ordering::Relaxed));
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
    let failed_started = Arc::new(AtomicBool::new(false));
    let failed_stopped = Arc::new(AtomicBool::new(false));
    let failed = units
        .add("failed")
        .spawn(&driver, |recv| {
            run_unit(
                StartResultUnit {
                    started: failed_started.clone(),
                    stopped: failed_stopped.clone(),
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
    assert!(failed_started.load(Ordering::Relaxed));
    assert!(failed_stopped.load(Ordering::Relaxed));
    assert!(!dependent_started.load(Ordering::Relaxed));
    assert!(!units.is_running());
    let retry_error = units.start().await.unwrap_err();
    assert!(retry_error.to_string().contains("terminal state"));
    assert!(units.reset().await.is_err());
    assert!(units.save().await.is_err());
    assert!(units.restore(Vec::new()).await.is_err());
}

#[async_test]
async fn test_stop_cleans_up_failed_start_while_running(driver: DefaultDriver) {
    let mut units = StateUnits::new();
    let _running = units
        .add("running")
        .spawn(&driver, |recv| {
            run_unit(
                StartResultUnit {
                    started: Arc::new(AtomicBool::new(false)),
                    stopped: Arc::new(AtomicBool::new(false)),
                    fail: false,
                },
                recv,
            )
        })
        .unwrap();
    units.start().await.unwrap();

    let failed_stopped = Arc::new(AtomicBool::new(false));
    let _failed = units
        .add("failed")
        .spawn(&driver, |recv| {
            run_unit(
                StartResultUnit {
                    started: Arc::new(AtomicBool::new(false)),
                    stopped: failed_stopped.clone(),
                    fail: true,
                },
                recv,
            )
        })
        .unwrap();

    units.start_stopped_units().await.unwrap_err();
    assert!(units.is_running());
    let retry_error = units.start_stopped_units().await.unwrap_err();
    assert!(retry_error.to_string().contains("terminal state"));
    units.stop().await;
    assert!(failed_stopped.load(Ordering::Relaxed));
    assert!(!units.is_running());
    let retry_error = units.start().await.unwrap_err();
    assert!(retry_error.to_string().contains("terminal state"));
    assert!(units.reset().await.is_err());
    assert!(units.save().await.is_err());
    assert!(units.restore(Vec::new()).await.is_err());
}

#[async_test]
async fn test_uncertain_start_is_cleaned_once_and_remains_terminal(driver: DefaultDriver) {
    let mut units = StateUnits::new();
    let _running = units
        .add("running")
        .spawn(&driver, |recv| {
            run_unit(
                StartResultUnit {
                    started: Arc::new(AtomicBool::new(false)),
                    stopped: Arc::new(AtomicBool::new(false)),
                    fail: false,
                },
                recv,
            )
        })
        .unwrap();
    units.start().await.unwrap();

    let uncertain_started = Arc::new(AtomicBool::new(false));
    let uncertain_stopped = Arc::new(AtomicBool::new(false));
    let stop_requests = Arc::new(AtomicUsize::new(0));
    let _uncertain = units
        .add("uncertain")
        .spawn(&driver, |mut recv| {
            let stop_requests = stop_requests.clone();
            let mut unit = StartResultUnit {
                started: uncertain_started.clone(),
                stopped: uncertain_stopped.clone(),
                fail: false,
            };
            async move {
                while let Some(request) = recv.next().await {
                    match request {
                        StateRequest::Start(response) => {
                            unit.started.store(true, Ordering::Relaxed);
                            drop(response);
                        }
                        StateRequest::Stop(response)
                            if stop_requests.fetch_add(1, Ordering::Relaxed) == 0 =>
                        {
                            drop(response);
                        }
                        request => request.apply(&mut unit).await,
                    }
                }
                unit
            }
        })
        .unwrap();

    units.start_stopped_units().await.unwrap_err();
    assert!(uncertain_started.load(Ordering::Relaxed));
    assert_eq!(stop_requests.load(Ordering::Relaxed), 1);
    assert!(!uncertain_stopped.load(Ordering::Relaxed));
    assert!(units.is_running());
    let retry_error = units.start_stopped_units().await.unwrap_err();
    assert!(retry_error.to_string().contains("terminal state"));

    units.stop().await;
    assert_eq!(stop_requests.load(Ordering::Relaxed), 2);
    assert!(uncertain_stopped.load(Ordering::Relaxed));
    assert!(!units.is_running());
    let retry_error = units.start().await.unwrap_err();
    assert!(retry_error.to_string().contains("terminal state"));
    assert!(units.reset().await.is_err());
    assert!(units.save().await.is_err());
    assert!(units.restore(Vec::new()).await.is_err());
}

#[async_test]
async fn test_initial_uncertain_start_is_cleaned(driver: DefaultDriver) {
    let mut units = StateUnits::new();
    let uncertain_started = Arc::new(AtomicBool::new(false));
    let uncertain_stopped = Arc::new(AtomicBool::new(false));
    let _uncertain = units
        .add("uncertain")
        .spawn(&driver, |mut recv| {
            let mut unit = StartResultUnit {
                started: uncertain_started.clone(),
                stopped: uncertain_stopped.clone(),
                fail: false,
            };
            async move {
                while let Some(request) = recv.next().await {
                    match request {
                        StateRequest::Start(response) => {
                            unit.started.store(true, Ordering::Relaxed);
                            drop(response);
                        }
                        request => request.apply(&mut unit).await,
                    }
                }
                unit
            }
        })
        .unwrap();

    units.start().await.unwrap_err();
    assert!(uncertain_started.load(Ordering::Relaxed));
    assert!(uncertain_stopped.load(Ordering::Relaxed));
    assert!(!units.is_running());
    let retry_error = units.start().await.unwrap_err();
    assert!(retry_error.to_string().contains("terminal state"));
    assert!(units.reset().await.is_err());
    assert!(units.save().await.is_err());
    assert!(units.restore(Vec::new()).await.is_err());
}

#[async_test]
async fn test_started_unit_exit_during_rollback_returns_error(driver: DefaultDriver) {
    let mut units = StateUnits::new();
    let _exited = units
        .add("exited")
        .spawn(&driver, |mut recv| async move {
            let unit = StartResultUnit {
                started: Arc::new(AtomicBool::new(true)),
                stopped: Arc::new(AtomicBool::new(false)),
                fail: false,
            };
            let request = recv.next().await.unwrap();
            let StateRequest::Start(response) = request else {
                panic!("unexpected request");
            };
            response.handle_failable_sync(|()| anyhow::Ok(()));
            unit
        })
        .unwrap();
    let _failed = units
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

    let error = units.start().await.unwrap_err();
    assert!(
        error
            .to_string()
            .contains("failed_start_rollback stop failed")
    );
    assert!(!units.is_running());
    let retry_error = units.start().await.unwrap_err();
    assert!(retry_error.to_string().contains("terminal state"));
    assert!(units.reset().await.is_err());
    assert!(units.save().await.is_err());
    assert!(units.restore(Vec::new()).await.is_err());
}

#[async_test]
async fn test_rollback_ready_set_ignores_removed_unit(driver: DefaultDriver) {
    let units = StateUnits::new();
    let unit = units
        .add("removed")
        .spawn(&driver, |recv| {
            run_unit(
                StartResultUnit {
                    started: Arc::new(AtomicBool::new(false)),
                    stopped: Arc::new(AtomicBool::new(false)),
                    fail: false,
                },
                recv,
            )
        })
        .unwrap();
    let id = unit.handle().id.id;
    unit.remove().await;

    let ready_set = units.inner.lock().ready_set(Some(&[id]));
    assert!(ready_set.0.is_empty());
}
