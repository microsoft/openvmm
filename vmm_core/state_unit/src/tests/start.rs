// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Tests for start with rollback.

use crate::State;
use crate::StateRequest;
use crate::StateUnit;
use crate::StateUnits;
use crate::run_unit;
use futures::StreamExt;
use inspect::InspectMut;
use mesh::Receiver;
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

/// Runs `unit`, but drops each start request after starting the unit, without
/// answering it, so that the start is uncertain while the unit still answers
/// later requests.
async fn run_unanswering_start(
    mut unit: RecordingUnit,
    mut recv: Receiver<StateRequest>,
) -> RecordingUnit {
    while let Some(request) = recv.next().await {
        match request {
            StateRequest::Start(response) => {
                unit.start().await;
                drop(response);
            }
            request => request.apply(&mut unit).await,
        }
    }
    unit
}

fn recording_unit(started: &Arc<AtomicBool>, stopped: &Arc<AtomicBool>) -> RecordingUnit {
    RecordingUnit {
        started: started.clone(),
        stopped: stopped.clone(),
    }
}

fn unit_state(units: &StateUnits, name: &str) -> State {
    units
        .inner
        .lock()
        .units
        .values()
        .find(|unit| unit.name.as_ref() == name)
        .unwrap()
        .state
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

#[async_test]
async fn test_failed_hot_add_does_not_stop_dependency_of_running_unit(driver: DefaultDriver) {
    let mut units = StateUnits::new();
    let running_dependent_started = Arc::new(AtomicBool::new(false));
    let running_dependent_stopped = Arc::new(AtomicBool::new(false));
    let running_dependent = units
        .add("running-dependent")
        .spawn(&driver, |recv| {
            run_unit(
                recording_unit(&running_dependent_started, &running_dependent_stopped),
                recv,
            )
        })
        .unwrap();
    units.start().await.unwrap();

    let dependency_started = Arc::new(AtomicBool::new(false));
    let dependency_stopped = Arc::new(AtomicBool::new(false));
    let _dependency = units
        .add("new-dependency")
        .dependency_of(running_dependent.handle())
        .spawn(&driver, |recv| {
            run_unit(
                recording_unit(&dependency_started, &dependency_stopped),
                recv,
            )
        })
        .unwrap();
    let unanswered_started = Arc::new(AtomicBool::new(false));
    let unanswered_stopped = Arc::new(AtomicBool::new(false));
    let _unanswered = units
        .add("unanswered")
        .spawn(&driver, |recv| {
            run_unanswering_start(
                recording_unit(&unanswered_started, &unanswered_stopped),
                recv,
            )
        })
        .unwrap();

    let error = units.start_stopped_units().await.unwrap_err().to_string();
    assert!(
        error.contains("failed_start_rollback stop skipped"),
        "{error}"
    );
    assert!(dependency_started.load(Ordering::Relaxed));
    assert!(!dependency_stopped.load(Ordering::Relaxed));
    assert!(!running_dependent_stopped.load(Ordering::Relaxed));
    assert!(unanswered_started.load(Ordering::Relaxed));
    assert!(!unanswered_stopped.load(Ordering::Relaxed));
    assert!(units.is_running());
    assert_eq!(unit_state(&units, "new-dependency"), State::StartUncertain);
    assert_eq!(unit_state(&units, "unanswered"), State::StartUncertain);

    units.stop().await;
    assert!(dependency_stopped.load(Ordering::Relaxed));
    assert!(running_dependent_stopped.load(Ordering::Relaxed));
    assert!(unanswered_stopped.load(Ordering::Relaxed));
    assert!(!units.is_running());
    assert_eq!(unit_state(&units, "new-dependency"), State::StartUncertain);
    let retry_error = units.start().await.unwrap_err();
    assert!(retry_error.to_string().contains("terminal state"));
    assert!(units.reset().await.is_err());
    assert!(units.save().await.is_err());
    assert!(units.restore(Vec::new()).await.is_err());
}

#[async_test]
async fn test_started_unit_exit_during_rollback_returns_error(driver: DefaultDriver) {
    let mut units = StateUnits::new();
    let dependency_started = Arc::new(AtomicBool::new(false));
    let dependency_stopped = Arc::new(AtomicBool::new(false));
    let dependency = units
        .add("dependency")
        .spawn(&driver, |recv| {
            run_unit(
                recording_unit(&dependency_started, &dependency_stopped),
                recv,
            )
        })
        .unwrap();
    // A unit whose task answers its start request and then exits.
    let exited = units
        .add("exited")
        .depends_on(dependency.handle())
        .spawn(&driver, |mut recv| async move {
            let Some(StateRequest::Start(response)) = recv.next().await else {
                panic!("unexpected request");
            };
            response.complete(());
        })
        .unwrap();
    let unanswered_stopped = Arc::new(AtomicBool::new(false));
    let _unanswered = units
        .add("unanswered")
        .spawn(&driver, |recv| {
            run_unanswering_start(
                recording_unit(&Arc::new(AtomicBool::new(false)), &unanswered_stopped),
                recv,
            )
        })
        .unwrap();

    let error = units.start().await.unwrap_err().to_string();
    assert!(
        error.contains("failed_start_rollback stop failed"),
        "{error}"
    );
    assert!(
        error.contains("failed_start_rollback stop skipped"),
        "{error}"
    );
    assert!(dependency_started.load(Ordering::Relaxed));
    assert!(!dependency_stopped.load(Ordering::Relaxed));
    // The dependency is still running, so a later stop must be able to stop it.
    assert!(units.is_running());

    exited.remove().await;
    units.stop().await;
    assert!(dependency_stopped.load(Ordering::Relaxed));
    assert!(unanswered_stopped.load(Ordering::Relaxed));
    assert!(!units.is_running());
    let retry_error = units.start().await.unwrap_err();
    assert!(retry_error.to_string().contains("terminal state"));
    assert!(units.reset().await.is_err());
    assert!(units.save().await.is_err());
    assert!(units.restore(Vec::new()).await.is_err());
}

#[async_test]
async fn test_uncertain_start_blocks_dependency_rollback(driver: DefaultDriver) {
    let mut units = StateUnits::new();
    let dependency_started = Arc::new(AtomicBool::new(false));
    let dependency_stopped = Arc::new(AtomicBool::new(false));
    let dependency = units
        .add("dependency")
        .spawn(&driver, |recv| {
            run_unit(
                recording_unit(&dependency_started, &dependency_stopped),
                recv,
            )
        })
        .unwrap();
    let unanswered_started = Arc::new(AtomicBool::new(false));
    let unanswered_stopped = Arc::new(AtomicBool::new(false));
    let _unanswered = units
        .add("unanswered")
        .depends_on(dependency.handle())
        .spawn(&driver, |recv| {
            run_unanswering_start(
                recording_unit(&unanswered_started, &unanswered_stopped),
                recv,
            )
        })
        .unwrap();

    let error = units.start().await.unwrap_err().to_string();
    assert!(
        error.contains("failed_start_rollback stop skipped"),
        "{error}"
    );
    assert!(dependency_started.load(Ordering::Relaxed));
    assert!(unanswered_started.load(Ordering::Relaxed));
    assert!(!dependency_stopped.load(Ordering::Relaxed));
    assert!(units.is_running());
    assert_eq!(unit_state(&units, "dependency"), State::StartUncertain);

    units.stop().await;
    assert!(unanswered_stopped.load(Ordering::Relaxed));
    assert!(dependency_stopped.load(Ordering::Relaxed));
    assert!(!units.is_running());
    assert_eq!(unit_state(&units, "dependency"), State::StartUncertain);
    assert!(units.reset().await.is_err());
    assert!(units.save().await.is_err());
    assert!(units.restore(Vec::new()).await.is_err());
}

#[async_test]
async fn test_initial_start_rollback_can_be_retried(driver: DefaultDriver) {
    let mut units = StateUnits::new();
    let started = Arc::new(AtomicBool::new(false));
    let stopped = Arc::new(AtomicBool::new(false));
    let stop_requests = Arc::new(AtomicUsize::new(0));
    // A unit that loses the response to its first stop request without
    // stopping.
    let _unit = units
        .add("rollback-retry")
        .spawn(&driver, |mut recv| {
            let mut unit = recording_unit(&started, &stopped);
            let stop_requests = stop_requests.clone();
            async move {
                while let Some(request) = recv.next().await {
                    match request {
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
    let unanswered_stopped = Arc::new(AtomicBool::new(false));
    let _unanswered = units
        .add("unanswered")
        .spawn(&driver, |recv| {
            run_unanswering_start(
                recording_unit(&Arc::new(AtomicBool::new(false)), &unanswered_stopped),
                recv,
            )
        })
        .unwrap();

    let error = units.start().await.unwrap_err().to_string();
    assert!(
        error.contains("failed_start_rollback stop failed"),
        "{error}"
    );
    assert!(started.load(Ordering::Relaxed));
    assert!(!stopped.load(Ordering::Relaxed));
    assert_eq!(stop_requests.load(Ordering::Relaxed), 1);
    assert!(units.is_running());

    units.stop().await;
    assert!(stopped.load(Ordering::Relaxed));
    assert_eq!(stop_requests.load(Ordering::Relaxed), 2);
    assert!(unanswered_stopped.load(Ordering::Relaxed));
    assert!(!units.is_running());
}

#[async_test]
async fn test_teardown_stop_after_unit_exits_before_answering_start(driver: DefaultDriver) {
    let mut units = StateUnits::new();
    let dependency_started = Arc::new(AtomicBool::new(false));
    let dependency_stopped = Arc::new(AtomicBool::new(false));
    let dependency = units
        .add("dependency")
        .spawn(&driver, |recv| {
            run_unit(
                recording_unit(&dependency_started, &dependency_stopped),
                recv,
            )
        })
        .unwrap();
    // A unit whose task answers a start and a stop, and then exits before it
    // answers the next start, as a task that fails while the VM restarts after
    // a failed save would.
    let _exited = units
        .add("exited")
        .depends_on(dependency.handle())
        .spawn(&driver, |mut recv| async move {
            for _ in 0..2 {
                match recv.next().await {
                    Some(StateRequest::Start(response)) => response.complete(()),
                    Some(StateRequest::Stop(response)) => response.complete(()),
                    _ => panic!("unexpected request"),
                }
            }
        })
        .unwrap();
    units.start().await.unwrap();
    units.stop().await;
    dependency_stopped.store(false, Ordering::Relaxed);

    let error = units.start().await.unwrap_err().to_string();
    assert!(error.contains("exited"), "{error}");
    assert!(dependency_started.load(Ordering::Relaxed));
    assert!(!dependency_stopped.load(Ordering::Relaxed));
    assert!(units.is_running());

    // Tearing the VM down stops the units without the exited unit answering.
    units.stop_for_teardown().await;
    assert!(dependency_stopped.load(Ordering::Relaxed));
    assert!(!units.is_running());
    assert_eq!(unit_state(&units, "exited"), State::StartUncertain);
    assert_eq!(unit_state(&units, "dependency"), State::StartUncertain);
}
