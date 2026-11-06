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
use parking_lot::Mutex;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::time::Duration;
use test_with_tracing::test;
use vmcore::save_restore::RestoreError;
use vmcore::save_restore::SaveError;
use vmcore::save_restore::SavedStateBlob;

pub(super) struct RecordingUnit {
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

pub(super) fn recording_unit(
    started: &Arc<AtomicBool>,
    stopped: &Arc<AtomicBool>,
) -> RecordingUnit {
    RecordingUnit {
        started: started.clone(),
        stopped: stopped.clone(),
    }
}

pub(super) fn unit_state(units: &StateUnits, name: &str) -> State {
    units
        .inner
        .lock()
        .units
        .values()
        .find(|unit| unit.name.as_ref() == name)
        .unwrap()
        .state
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

/// The record of the starts, confirmations, and stops of [`ConfirmUnit`]s.
#[derive(Clone, Default)]
struct Events(Arc<Mutex<Vec<String>>>);

impl Events {
    fn push(&self, unit: &str, event: &str) {
        self.0.lock().push(format!("{unit} {event}"));
    }

    fn take(&self) -> Vec<String> {
        std::mem::take(&mut *self.0.lock())
    }
}

/// A unit whose start confirmation takes `delay`, then fails while `fail`
/// is set, as one whose device failed to start would.
struct ConfirmUnit {
    name: &'static str,
    driver: DefaultDriver,
    delay: Duration,
    fail: Arc<AtomicBool>,
    events: Events,
}

impl StateUnit for ConfirmUnit {
    async fn start(&mut self) {
        self.events.push(self.name, "started");
    }

    async fn confirm_start(&mut self) -> anyhow::Result<()> {
        pal_async::timer::PolledTimer::new(&self.driver)
            .sleep(self.delay)
            .await;
        anyhow::ensure!(
            !self.fail.load(Ordering::Relaxed),
            "the unit's device did not start"
        );
        self.events.push(self.name, "confirmed");
        Ok(())
    }

    async fn stop(&mut self) {
        self.events.push(self.name, "stopped");
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

impl InspectMut for ConfirmUnit {
    fn inspect_mut(&mut self, req: inspect::Request<'_>) {
        req.respond();
    }
}

/// Adds a [`ConfirmUnit`] named `name`, which depends on `dependency` if
/// one is given.
fn add_confirm_unit(
    units: &StateUnits,
    driver: &DefaultDriver,
    name: &'static str,
    dependency: Option<&crate::UnitHandle>,
    delay: Duration,
    fail: &Arc<AtomicBool>,
    events: &Events,
) -> crate::SpawnedUnit<ConfirmUnit> {
    let mut builder = units.add(name);
    if let Some(dependency) = dependency {
        builder = builder.depends_on(dependency);
    }
    let unit = ConfirmUnit {
        name,
        driver: driver.clone(),
        delay,
        fail: fail.clone(),
        events: events.clone(),
    };
    builder.spawn(driver, |recv| run_unit(unit, recv)).unwrap()
}

#[async_test]
async fn a_failed_confirmation_rolls_back_and_blocks_dependents(driver: DefaultDriver) {
    let mut units = StateUnits::new();
    let events = Events::default();
    let no_failure = Arc::new(AtomicBool::new(false));
    let fail = Arc::new(AtomicBool::new(true));
    let _independent = add_confirm_unit(
        &units,
        &driver,
        "independent",
        None,
        Duration::ZERO,
        &no_failure,
        &events,
    );
    let failing = add_confirm_unit(
        &units,
        &driver,
        "failing",
        None,
        Duration::ZERO,
        &fail,
        &events,
    );
    let _dependent = add_confirm_unit(
        &units,
        &driver,
        "dependent",
        Some(failing.handle()),
        Duration::ZERO,
        &no_failure,
        &events,
    );

    let error = format!("{:#}", units.start().await.unwrap_err());
    assert!(
        error.contains("failing: the unit's device did not start"),
        "{error}"
    );
    assert!(
        error.contains("dependent: a dependency did not start"),
        "{error}"
    );
    assert!(!units.is_running());
    // The unit whose confirmation failed did start, so it stops again with
    // the others, and its dependent never starts.
    let first = events.take();
    for event in ["independent stopped", "failing started", "failing stopped"] {
        assert!(first.iter().any(|e| e == event), "{first:?}");
    }
    assert!(
        !first.iter().any(|e| e.starts_with("dependent")),
        "{first:?}"
    );

    // Unlike a unit that does not answer, the unit can start again.
    fail.store(false, Ordering::Relaxed);
    units.start().await.unwrap();
    assert!(units.is_running());
    let second = events.take();
    assert!(
        second.iter().any(|e| e == "dependent confirmed"),
        "{second:?}"
    );
}

#[async_test]
async fn dependents_start_once_their_dependencies_confirm(driver: DefaultDriver) {
    let mut units = StateUnits::new();
    let events = Events::default();
    let no_failure = Arc::new(AtomicBool::new(false));
    let dependency = add_confirm_unit(
        &units,
        &driver,
        "dependency",
        None,
        Duration::from_millis(50),
        &no_failure,
        &events,
    );
    let _dependent = add_confirm_unit(
        &units,
        &driver,
        "dependent",
        Some(dependency.handle()),
        Duration::ZERO,
        &no_failure,
        &events,
    );

    units.start().await.unwrap();
    assert_eq!(
        events.take(),
        [
            "dependency started",
            "dependency confirmed",
            "dependent started",
            "dependent confirmed"
        ]
    );
}

#[async_test]
async fn an_unanswered_confirmation_leaves_a_terminal_unit(driver: DefaultDriver) {
    let mut units = StateUnits::new();
    // A unit whose task starts, then drops its confirmation request without
    // answering, as a task that has failed would.
    let _unanswered = units
        .add("unanswered")
        .spawn(&driver, |mut recv| async move {
            while let Some(request) = recv.next().await {
                match request {
                    StateRequest::Start(rpc) => rpc.complete(()),
                    StateRequest::ConfirmStart(_) => break,
                    _ => {}
                }
            }
        })
        .unwrap();

    let error = units.start().await.unwrap_err();
    assert!(error.to_string().contains("unanswered"), "{error:#}");
    assert!(!units.is_running());
    let retry_error = units.start().await.unwrap_err();
    assert!(retry_error.to_string().contains("terminal state"));
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
        error.contains("dependency: failed_start_rollback stop skipped"),
        "{error}"
    );
    assert!(dependency_started.load(Ordering::Relaxed));
    assert!(unanswered_started.load(Ordering::Relaxed));
    // The unit whose start is uncertain may use its dependency, so the
    // rollback leaves the dependency running, and the units stay running.
    assert!(!dependency_stopped.load(Ordering::Relaxed));
    assert!(units.is_running());
    assert_eq!(unit_state(&units, "dependency"), State::StartUncertain);
    assert_eq!(unit_state(&units, "unanswered"), State::StartUncertain);

    // A later stop stops both, the dependency last, and both stay terminal.
    units.stop().await;
    assert!(unanswered_stopped.load(Ordering::Relaxed));
    assert!(dependency_stopped.load(Ordering::Relaxed));
    assert!(!units.is_running());
    assert_eq!(unit_state(&units, "dependency"), State::StartUncertain);
    assert_eq!(unit_state(&units, "unanswered"), State::StartUncertain);
    let retry_error = units.start().await.unwrap_err();
    assert!(retry_error.to_string().contains("terminal state"));
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
    // A unit whose task answers its start and the confirmation of that start,
    // and then exits, as a task that has failed would.
    let exited = units
        .add("exited")
        .depends_on(dependency.handle())
        .spawn(&driver, |mut recv| async move {
            while let Some(request) = recv.next().await {
                match request {
                    StateRequest::Start(response) => response.complete(()),
                    StateRequest::ConfirmStart(response) => {
                        response.complete(Ok(()));
                        break;
                    }
                    request => panic!("unexpected request {request:?}"),
                }
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
        error.contains("exited: failed_start_rollback stop failed"),
        "{error}"
    );
    assert!(
        error.contains("dependency: failed_start_rollback stop skipped"),
        "{error}"
    );
    assert!(dependency_started.load(Ordering::Relaxed));
    assert!(!dependency_stopped.load(Ordering::Relaxed));
    // The dependency is still running, so a later stop must be able to stop it.
    assert!(units.is_running());
    assert_eq!(unit_state(&units, "exited"), State::StartUncertain);
    assert_eq!(unit_state(&units, "dependency"), State::StartUncertain);

    // Once the exited unit is removed, a stop stops the others.
    exited.remove().await;
    units.stop().await;
    assert!(dependency_stopped.load(Ordering::Relaxed));
    assert!(unanswered_stopped.load(Ordering::Relaxed));
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
        error.contains("new-dependency: failed_start_rollback stop skipped"),
        "{error}"
    );
    // A running unit depends on the new unit, so the rollback leaves it
    // running.
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
}

#[async_test]
async fn test_unanswered_rollback_stop_can_be_retried(driver: DefaultDriver) {
    let mut units = StateUnits::new();
    let started = Arc::new(AtomicBool::new(false));
    let stopped = Arc::new(AtomicBool::new(false));
    let stop_requests = Arc::new(AtomicUsize::new(0));
    // A unit that drops its first stop request without stopping or
    // answering.
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
        error.contains("rollback-retry: failed_start_rollback stop failed"),
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
