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
use parking_lot::Mutex;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::time::Duration;
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
