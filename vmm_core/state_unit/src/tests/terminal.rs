// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Tests for the requests that reach a set with a unit in a terminal state.

use crate::StateRequest;
use crate::StateTransitionError;
use crate::StateTransitionErrorKind;
use crate::StateUnit;
use crate::StateUnits;
use crate::UnitErrorSet;
use crate::run_unit;
use futures::StreamExt;
use inspect::InspectMut;
use mesh::Receiver;
use pal_async::DefaultDriver;
use pal_async::async_test;
use pal_async::timer::Instant;
use parking_lot::Mutex;
use std::sync::Arc;
use std::time::Duration;
use test_with_tracing::test;
use vmcore::save_restore::RestoreError;
use vmcore::save_restore::SaveError;
use vmcore::save_restore::SavedStateBlob;

/// The requests that [`LoggingUnit`]s handled, in order.
#[derive(Clone, Default)]
struct Log(Arc<Mutex<Vec<String>>>);

impl Log {
    fn push(&self, unit: &str, request: &str) {
        self.0.lock().push(format!("{unit} {request}"));
    }

    fn take(&self) -> Vec<String> {
        std::mem::take(&mut *self.0.lock())
    }
}

/// A unit that logs each state change request that it handles.
struct LoggingUnit {
    name: &'static str,
    log: Log,
}

impl LoggingUnit {
    fn new(name: &'static str, log: &Log) -> Self {
        Self {
            name,
            log: log.clone(),
        }
    }
}

impl StateUnit for LoggingUnit {
    async fn start(&mut self) {
        self.log.push(self.name, "start");
    }

    async fn stop(&mut self) {
        self.log.push(self.name, "stop");
    }

    async fn reset(&mut self) -> anyhow::Result<()> {
        self.log.push(self.name, "reset");
        Ok(())
    }

    async fn save(&mut self) -> Result<Option<SavedStateBlob>, SaveError> {
        self.log.push(self.name, "save");
        Ok(None)
    }

    async fn restore(&mut self, _state: SavedStateBlob) -> Result<(), RestoreError> {
        self.log.push(self.name, "restore");
        Ok(())
    }

    async fn advance_time(&mut self, _duration: Duration, _resumed: Instant) -> anyhow::Result<()> {
        self.log.push(self.name, "advance_time");
        Ok(())
    }
}

impl InspectMut for LoggingUnit {
    fn inspect_mut(&mut self, req: inspect::Request<'_>) {
        req.respond();
    }
}

/// Runs `unit`, but drops each start request after starting the unit, without
/// answering it, so that the start is uncertain while the unit runs and
/// answers later requests.
async fn run_unanswering_start(
    mut unit: LoggingUnit,
    mut recv: Receiver<StateRequest>,
) -> LoggingUnit {
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

/// Starts a set with a unit named `dependency`, then adds a unit named
/// `uncertain` that depends on it, as a hot-add does, and whose start is
/// uncertain, and returns the set, which still runs, with the handles of both
/// units.
async fn running_set_with_an_uncertain_unit(
    driver: &DefaultDriver,
    log: &Log,
) -> (StateUnits, Vec<crate::SpawnedUnit<LoggingUnit>>) {
    let mut units = StateUnits::new();
    let dependency = units
        .add("dependency")
        .spawn(driver, |recv| {
            run_unit(LoggingUnit::new("dependency", log), recv)
        })
        .unwrap();
    units.start().await.unwrap();

    let uncertain = units
        .add("uncertain")
        .depends_on(dependency.handle())
        .spawn(driver, |recv| {
            run_unanswering_start(LoggingUnit::new("uncertain", log), recv)
        })
        .unwrap();
    let error = units.start_stopped_units().await.unwrap_err();
    assert!(format!("{error:#}").contains("uncertain"), "{error:#}");
    assert!(units.is_running());
    assert_eq!(log.take(), ["dependency start", "uncertain start"]);
    (units, vec![dependency, uncertain])
}

/// Checks that `error` is the failure of `op` because of the unit named
/// `name` alone.
fn assert_terminal_unit_error(error: StateTransitionError, op: &str, name: &str) {
    let StateTransitionErrorKind::Units(UnitErrorSet(errors)) = &error.error else {
        panic!("{op} failed for another reason: {error:?}");
    };
    let names = errors
        .iter()
        .map(|(name, _)| name.as_ref())
        .collect::<Vec<_>>();
    assert_eq!(
        (error.op, names.as_slice()),
        (op, [name].as_slice()),
        "{error:?}"
    );
}

/// Checks that a reset, save, restore, and time advance of `units` each fail
/// because of the unit named `name`, which is in a terminal state, without
/// sending a unit a request.
async fn assert_requests_fail(units: &mut StateUnits, name: &str, log: &Log) {
    assert_terminal_unit_error(units.reset().await.unwrap_err(), "reset", name);
    let Err(error) = units.save().await else {
        panic!("save succeeded");
    };
    assert_terminal_unit_error(error, "save", name);
    assert_terminal_unit_error(
        units.restore(Vec::new()).await.unwrap_err(),
        "restore",
        name,
    );
    assert_terminal_unit_error(
        units
            .advance_time(Duration::from_secs(1), Instant::now())
            .await
            .unwrap_err(),
        "advance_time",
        name,
    );
    assert_eq!(log.take(), Vec::<String>::new());
}

#[async_test]
async fn a_stop_stops_a_unit_whose_start_is_uncertain(driver: DefaultDriver) {
    let log = Log::default();
    let (mut units, _handles) = running_set_with_an_uncertain_unit(&driver, &log).await;

    // The unit whose start is uncertain may be running, so the stop stops it,
    // before the unit that it depends on.
    units.stop().await.unwrap();
    assert!(!units.is_running());
    assert_eq!(log.take(), ["uncertain stop", "dependency stop"]);

    // The unit stays in its terminal state.
    let error = units.start().await.unwrap_err();
    assert!(error.to_string().contains("terminal state"), "{error:#}");
    assert_requests_fail(&mut units, "uncertain", &log).await;
}

#[async_test]
async fn requests_fail_after_a_start_leaves_a_terminal_unit(driver: DefaultDriver) {
    let mut units = StateUnits::new();
    let log = Log::default();
    let _healthy = units
        .add("healthy")
        .advances_time()
        .spawn(&driver, |recv| {
            run_unit(LoggingUnit::new("healthy", &log), recv)
        })
        .unwrap();
    // A unit whose task exits at its start request without answering it, as
    // a task that has failed would.
    let _lost = units
        .add("lost")
        .spawn(&driver, |mut recv| async move {
            while let Some(request) = recv.next().await {
                if matches!(request, StateRequest::Start(_)) {
                    break;
                }
            }
        })
        .unwrap();

    let error = units.start().await.unwrap_err();
    assert!(format!("{error:#}").contains("lost"), "{error:#}");
    // The rollback stopped the unit that started.
    assert_eq!(log.take(), ["healthy start", "healthy stop"]);

    assert_requests_fail(&mut units, "lost", &log).await;
}

#[async_test]
async fn requests_fail_while_a_terminal_unit_keeps_the_set_running(driver: DefaultDriver) {
    let log = Log::default();
    let (mut units, _handles) = running_set_with_an_uncertain_unit(&driver, &log).await;

    // The terminal unit fails the requests before the set's running state
    // matters, so that a caller that considers the VM stopped while such a
    // unit keeps the set running gets an error rather than a panic.
    assert_requests_fail(&mut units, "uncertain", &log).await;
    assert!(units.is_running());
}
