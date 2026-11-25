// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Tests for gating host input around the snapshot boundary.

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
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::time::Duration;
use std::time::Instant;
use test_with_tracing::test;
use vmcore::device_state::InputHookCompletion;
use vmcore::save_restore::RestoreError;
use vmcore::save_restore::SaveError;
use vmcore::save_restore::SavedStateBlob;

#[derive(Default)]
struct InputCounts {
    quiesces: AtomicUsize,
    resumes: AtomicUsize,
}

struct InputTestUnit {
    driver: DefaultDriver,
    counts: Arc<InputCounts>,
    delay: Option<Duration>,
    fail_quiesce: bool,
    fail_resume: bool,
}

impl StateUnit for InputTestUnit {
    async fn start(&mut self) {}

    async fn stop(&mut self) {}

    fn quiesce_input(&mut self) -> InputHookCompletion {
        self.counts.quiesces.fetch_add(1, Ordering::Relaxed);
        let fail = self.fail_quiesce;
        let delay = self.delay.map(|delay| (self.driver.clone(), delay));
        InputHookCompletion::new(async move {
            if let Some((driver, delay)) = delay {
                pal_async::timer::PolledTimer::new(&driver)
                    .sleep(delay)
                    .await;
            }
            anyhow::ensure!(!fail, "intentional input quiesce failure");
            Ok(())
        })
    }

    fn resume_input(&mut self) -> InputHookCompletion {
        self.counts.resumes.fetch_add(1, Ordering::Relaxed);
        let fail = self.fail_resume;
        InputHookCompletion::new(async move {
            anyhow::ensure!(!fail, "intentional input resume failure");
            Ok(())
        })
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

impl InspectMut for InputTestUnit {
    fn inspect_mut(&mut self, req: inspect::Request<'_>) {
        req.respond();
    }
}

fn input_unit(driver: &DefaultDriver, counts: Arc<InputCounts>) -> InputTestUnit {
    InputTestUnit {
        driver: driver.clone(),
        counts,
        delay: None,
        fail_quiesce: false,
        fail_resume: false,
    }
}

/// Runs `unit` like [`run_unit`], but after answering `answered_starts` start
/// requests, starts the unit for each later one without answering it, so that
/// the start is uncertain while the unit still answers other requests.
async fn run_unit_leaving_starts_unanswered<T: StateUnit>(
    mut unit: T,
    mut recv: Receiver<StateRequest>,
    answered_starts: usize,
) -> T {
    let mut starts = 0;
    while let Some(request) = recv.next().await {
        match request {
            StateRequest::Start(response) if starts >= answered_starts => {
                unit.start().await;
                drop(response);
            }
            request => {
                if matches!(request, StateRequest::Start(_)) {
                    starts += 1;
                }
                request.apply(&mut unit).await;
            }
        }
    }
    unit
}

#[async_test]
async fn test_input_hooks_run_only_for_running_units(driver: DefaultDriver) {
    let units = StateUnits::new();
    let running_counts = Arc::new(InputCounts::default());
    let _running = units
        .add("running")
        .spawn(&driver, |recv| {
            run_unit(input_unit(&driver, running_counts.clone()), recv)
        })
        .unwrap();
    let mut units = units;
    units.start().await.unwrap();

    let stopped_counts = Arc::new(InputCounts::default());
    let _stopped = units
        .add("stopped")
        .spawn(&driver, |recv| {
            run_unit(input_unit(&driver, stopped_counts.clone()), recv)
        })
        .unwrap();

    units.quiesce_input_for_save().await.unwrap();
    units.resume_input_after_save().await.unwrap();

    assert_eq!(running_counts.quiesces.load(Ordering::Relaxed), 1);
    assert_eq!(running_counts.resumes.load(Ordering::Relaxed), 1);
    assert_eq!(stopped_counts.quiesces.load(Ordering::Relaxed), 0);
    assert_eq!(stopped_counts.resumes.load(Ordering::Relaxed), 0);
}

#[async_test]
async fn test_input_hook_errors_are_reported_per_unit(driver: DefaultDriver) {
    let mut units = StateUnits::new();
    let successful_counts = Arc::new(InputCounts::default());
    let _successful = units
        .add("successful")
        .spawn(&driver, |recv| {
            run_unit(input_unit(&driver, successful_counts.clone()), recv)
        })
        .unwrap();
    let failing_counts = Arc::new(InputCounts::default());
    let _failing = units
        .add("failing")
        .spawn(&driver, |recv| {
            let mut unit = input_unit(&driver, failing_counts.clone());
            unit.fail_quiesce = true;
            unit.fail_resume = true;
            run_unit(unit, recv)
        })
        .unwrap();
    units.start().await.unwrap();

    let error = units.quiesce_input_for_save().await.unwrap_err();
    assert!(
        error
            .to_string()
            .contains("failing: intentional input quiesce failure")
    );
    let error = units.resume_input_after_save().await.unwrap_err();
    assert!(
        error
            .to_string()
            .contains("failing: intentional input resume failure")
    );
    assert_eq!(successful_counts.quiesces.load(Ordering::Relaxed), 1);
    assert_eq!(successful_counts.resumes.load(Ordering::Relaxed), 1);
    assert_eq!(failing_counts.quiesces.load(Ordering::Relaxed), 1);
    assert_eq!(failing_counts.resumes.load(Ordering::Relaxed), 1);
}

#[async_test]
async fn test_input_hook_closed_channel_is_reported(driver: DefaultDriver) {
    let mut units = StateUnits::new();
    let _closed = units
        .add("closed")
        .spawn(&driver, |mut recv| async move {
            let request = recv.next().await.unwrap();
            let StateRequest::Start(response) = request else {
                panic!("unexpected request");
            };
            response.complete(());
        })
        .unwrap();
    units.start().await.unwrap();

    let error = units.quiesce_input_for_save().await.unwrap_err();
    assert!(error.to_string().contains("quiesce_input failed: closed:"));
}

#[async_test]
async fn test_slow_input_hook_is_awaited(driver: DefaultDriver) {
    let delay = Duration::from_millis(200);
    let mut units = StateUnits::new();
    let failing_counts = Arc::new(InputCounts::default());
    let _failing = units
        .add("failing")
        .spawn(&driver, |recv| {
            let mut unit = input_unit(&driver, failing_counts.clone());
            unit.fail_quiesce = true;
            run_unit(unit, recv)
        })
        .unwrap();
    let counts = Arc::new(InputCounts::default());
    let _slow = units
        .add("slow")
        .spawn(&driver, |recv| {
            let mut unit = input_unit(&driver, counts.clone());
            unit.delay = Some(delay);
            run_unit(unit, recv)
        })
        .unwrap();
    units.start().await.unwrap();

    // The transaction has no deadline: it waits for the slow hook, which
    // succeeds, and reports only the hook that failed.
    let started = Instant::now();
    let error = units.quiesce_input_for_save().await.unwrap_err();
    assert!(started.elapsed() >= delay, "{:?}", started.elapsed());
    let error = error.to_string();
    assert!(error.contains("failing: intentional input quiesce failure"));
    assert!(!error.contains("slow"), "{error}");
    assert_eq!(failing_counts.quiesces.load(Ordering::Relaxed), 1);
    assert_eq!(counts.quiesces.load(Ordering::Relaxed), 1);

    // Resuming input reaches both units, and a stop follows.
    units.resume_input_after_save().await.unwrap();
    assert_eq!(failing_counts.resumes.load(Ordering::Relaxed), 1);
    assert_eq!(counts.resumes.load(Ordering::Relaxed), 1);
    units.stop().await;
}

#[async_test]
async fn test_input_hooks_reject_terminal_units(driver: DefaultDriver) {
    let mut units = StateUnits::new();
    let _running = units
        .add("running")
        .spawn(&driver, |recv| {
            run_unit(input_unit(&driver, Arc::new(InputCounts::default())), recv)
        })
        .unwrap();
    units.start().await.unwrap();

    let terminal_counts = Arc::new(InputCounts::default());
    let _terminal = units
        .add("terminal")
        .spawn(&driver, |recv| {
            run_unit_leaving_starts_unanswered(
                input_unit(&driver, terminal_counts.clone()),
                recv,
                0,
            )
        })
        .unwrap();
    units.start_stopped_units().await.unwrap_err();

    let error = units.quiesce_input_for_save().await.unwrap_err();
    assert!(
        error
            .to_string()
            .contains("terminal: unit is in terminal state StartUncertain")
    );
    assert_eq!(terminal_counts.quiesces.load(Ordering::Relaxed), 0);
}
