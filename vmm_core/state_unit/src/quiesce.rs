// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Snapshot quiesce transactions: gating host input around the snapshot
//! vCPU boundary, bounded quiesce of every unit for save, and restart after a
//! failed save.

use super::PendingInput;
use super::PendingStart;
use super::PendingStop;
use super::State;
use super::StateRequest;
use super::StateUnits;
use super::state_change;
use futures::FutureExt;
use futures::future::join_all;
use mesh::CancelContext;
use mesh::rpc::FailableRpc;
use std::time::Duration;
use std::time::Instant;
use thiserror::Error;

/// A bounded quiesce failed and the VM must not be saved.
#[derive(Debug, Error)]
#[error("failed to quiesce state units for save: {failures:?}")]
pub struct QuiesceForSaveError {
    failures: Vec<(String, String)>,
    uncertain: bool,
}

impl QuiesceForSaveError {
    /// Returns whether any unit changed state or may have partially changed
    /// ownership or state.
    ///
    /// When true, the VM cannot be resumed safely and must be terminated.
    /// When false, no unit changed state during the failed transaction.
    pub fn has_uncertain_state(&self) -> bool {
        self.uncertain
    }
}

impl StateUnits {
    /// Stops host-input producers in running units before establishing a
    /// snapshot vCPU boundary.
    ///
    /// Stopped units are skipped, and terminal lifecycle states fail the
    /// transaction. Timed-out handlers are retained so a later input or
    /// lifecycle transition waits for their completion.
    pub async fn quiesce_input_for_save(&mut self, timeout: Duration) -> anyhow::Result<()> {
        self.run_input_op("quiesce_input", timeout, StateRequest::QuiesceInput)
            .await
    }

    /// Resumes host-input producers in running units before releasing a failed
    /// snapshot boundary.
    ///
    /// Stopped units are skipped, and terminal lifecycle states fail the
    /// transaction. Timed-out handlers are retained so later transitions
    /// cannot overlap them.
    pub async fn resume_input_after_save(&mut self, timeout: Duration) -> anyhow::Result<()> {
        self.run_input_op("resume_input", timeout, StateRequest::ResumeInput)
            .await
    }

    async fn run_input_op(
        &mut self,
        operation: &'static str,
        timeout: Duration,
        request: impl 'static + Copy + Send + FnOnce(FailableRpc<(), ()>) -> StateRequest,
    ) -> anyhow::Result<()> {
        enum InputResult {
            Completed(anyhow::Result<()>),
            TimedOut {
                reason: anyhow::Error,
                pending: PendingInput,
                waiting_for_previous: bool,
            },
        }

        let context = CancelContext::new().with_timeout(timeout);
        let mut operations = Vec::new();
        let mut failures = Vec::new();
        {
            let mut inner = self.inner.lock();
            for (&id, unit) in &mut inner.units {
                match unit.state {
                    State::Running => {
                        let name = unit.name.clone();
                        let previous = unit.pending_input.take();
                        let mut current = PendingInput {
                            future: state_change(name.clone(), unit, request, Some(()))
                                .map(|result| -> anyhow::Result<()> {
                                    match result {
                                        Ok(Some(Ok(()))) => Ok(()),
                                        Ok(Some(Err(error))) => Err(error.into()),
                                        Ok(None) => Err(anyhow::anyhow!("request was not sent")),
                                        Err(error) => Err(error.into()),
                                    }
                                })
                                .boxed(),
                        };
                        let mut context = context.clone();
                        operations.push(async move {
                            if let Some(mut previous) = previous {
                                if let Err(reason) =
                                    context.until_cancelled(previous.future.as_mut()).await
                                {
                                    return (
                                        name,
                                        id,
                                        InputResult::TimedOut {
                                            reason: reason.into(),
                                            pending: previous,
                                            waiting_for_previous: true,
                                        },
                                    );
                                }
                            }

                            let result =
                                match context.until_cancelled(current.future.as_mut()).await {
                                    Ok(result) => InputResult::Completed(result),
                                    Err(reason) => InputResult::TimedOut {
                                        reason: reason.into(),
                                        pending: current,
                                        waiting_for_previous: false,
                                    },
                                };
                            (name, id, result)
                        });
                    }
                    State::Stopped => {}
                    State::StartUncertain
                    | State::StartFailedStopped
                    | State::QuiesceUncertain
                    | State::QuiesceFailedStopped => {
                        failures.push(format!(
                            "{}: unit is in terminal state {:?}",
                            unit.name, unit.state
                        ));
                    }
                    state => {
                        failures.push(format!(
                            "{}: unit is in unexpected state {state:?}",
                            unit.name
                        ));
                    }
                }
            }
        }

        let results = join_all(operations).await;
        let mut inner = self.inner.lock();
        for (name, id, result) in results {
            match result {
                InputResult::Completed(Ok(())) => {}
                InputResult::Completed(Err(error)) => failures.push(format!("{name}: {error}")),
                InputResult::TimedOut {
                    reason,
                    pending,
                    waiting_for_previous,
                } => {
                    if let Some(unit) = inner.units.get_mut(&id) {
                        assert!(unit.pending_input.is_none());
                        unit.pending_input = Some(pending);
                    }
                    let waiting = if waiting_for_previous {
                        " waiting for previous input operation"
                    } else {
                        ""
                    };
                    failures.push(format!("{name}: {operation} timed out{waiting}: {reason}"));
                }
            }
        }
        drop(inner);

        anyhow::ensure!(
            failures.is_empty(),
            "{operation} failed: {}",
            failures.join("; ")
        );
        Ok(())
    }

    /// Stops all units in reverse dependency order within `timeout`.
    ///
    /// A timeout or communication failure after a stop request is sent leaves
    /// ownership uncertain. The returned error reports this via
    /// [`QuiesceForSaveError::has_uncertain_state`]; callers must terminate the
    /// VM rather than attempt rollback in that case. A completed stop followed
    /// by another unit's failure is also classified as uncertain, so a clean
    /// error guarantees that no unit changed state.
    pub async fn quiesce_for_save(&mut self, timeout: Duration) -> Result<(), QuiesceForSaveError> {
        assert!(self.running);

        enum UnitResult {
            Stopped,
            DependencyFailed,
            NotSent(anyhow::Error),
            InputPending(anyhow::Error, PendingInput),
            Uncertain(anyhow::Error, Option<PendingStop>),
        }

        let context = CancelContext::new().with_timeout(timeout);
        let mut operations = Vec::new();
        let mut failures = Vec::new();
        let mut uncertain = false;
        let mut quiesced_units = Vec::new();
        let ready_set;
        {
            let mut inner = self.inner.lock();
            ready_set = inner.ready_set(None);
            for (&id, unit) in &mut inner.units {
                match unit.state {
                    State::Stopped => {
                        ready_set.done(id, true);
                        continue;
                    }
                    State::Running => {}
                    State::StartFailedStopped => {
                        ready_set.done(id, false);
                        failures.push((
                            unit.name.to_string(),
                            "unit is in terminal state StartFailedStopped".to_owned(),
                        ));
                        continue;
                    }
                    State::QuiesceFailedStopped => {
                        ready_set.done(id, false);
                        uncertain = true;
                        failures.push((
                            unit.name.to_string(),
                            "unit is in terminal state QuiesceFailedStopped".to_owned(),
                        ));
                        continue;
                    }
                    State::StartUncertain | State::QuiesceUncertain => {
                        ready_set.done(id, false);
                        uncertain = true;
                        failures.push((
                            unit.name.to_string(),
                            format!("unit is in terminal state {:?}", unit.state),
                        ));
                        continue;
                    }
                    state => {
                        ready_set.done(id, false);
                        uncertain = true;
                        failures.push((
                            unit.name.to_string(),
                            format!("unit is in unexpected state {state:?}"),
                        ));
                        continue;
                    }
                }
                quiesced_units.push(id);
                let name = unit.name.clone();
                let dependencies = unit.dependents.iter().copied().collect::<Vec<_>>();
                let pending_input = unit.pending_input.take();
                let ready_set = ready_set.clone();
                let mut context = context.clone();
                let mut stop = PendingStop {
                    future: state_change(name.clone(), unit, StateRequest::Stop, Some(())).boxed(),
                };
                operations.push(async move {
                    if let Some(mut pending_input) = pending_input {
                        if let Err(reason) =
                            context.until_cancelled(pending_input.future.as_mut()).await
                        {
                            ready_set.done(id, false);
                            return (
                                name,
                                id,
                                UnitResult::InputPending(reason.into(), pending_input),
                            );
                        }
                    }

                    let ready = match context
                        .until_cancelled(ready_set.wait("quiesce_for_save", id, &dependencies))
                        .await
                    {
                        Ok(ready) => ready,
                        Err(reason) => {
                            ready_set.done(id, false);
                            return (name, id, UnitResult::NotSent(reason.into()));
                        }
                    };
                    if !ready {
                        ready_set.done(id, false);
                        return (name, id, UnitResult::DependencyFailed);
                    }

                    let result = match context.until_cancelled(stop.future.as_mut()).await {
                        Ok(Ok(Some(()))) => UnitResult::Stopped,
                        Ok(Ok(None)) => UnitResult::Uncertain(
                            anyhow::anyhow!("stop request was not sent"),
                            None,
                        ),
                        Ok(Err(error)) => UnitResult::Uncertain(error.into(), None),
                        Err(reason) => UnitResult::Uncertain(reason.into(), Some(stop)),
                    };
                    ready_set.done(id, matches!(result, UnitResult::Stopped));
                    (name, id, result)
                });
                unit.state = State::Stopping;
            }
        }

        let start = Instant::now();
        let results = join_all(operations).await;
        tracing::info!(duration = ?Instant::now() - start, "save quiesce complete");

        let mut inner = self.inner.lock();
        let mut stopped_ids = Vec::new();
        for (name, id, result) in results {
            let Some(unit) = inner.units.get_mut(&id) else {
                continue;
            };
            match result {
                UnitResult::Stopped => {
                    unit.state = State::Stopped;
                    stopped_ids.push(id);
                }
                UnitResult::DependencyFailed => {
                    unit.state = State::Running;
                    failures.push((
                        name.to_string(),
                        "a dependent unit did not quiesce".to_owned(),
                    ));
                }
                UnitResult::NotSent(error) => {
                    unit.state = State::Running;
                    failures.push((name.to_string(), format!("{error:#}")));
                }
                UnitResult::InputPending(error, pending_input) => {
                    unit.state = State::Running;
                    assert!(unit.pending_input.is_none());
                    unit.pending_input = Some(pending_input);
                    failures.push((
                        name.to_string(),
                        format!("pending input operation did not complete: {error:#}"),
                    ));
                }
                UnitResult::Uncertain(error, pending_stop) => {
                    unit.state = State::QuiesceUncertain;
                    assert!(unit.pending_stop.is_none());
                    unit.pending_stop = pending_stop;
                    uncertain = true;
                    failures.push((name.to_string(), format!("{error:#}")));
                }
            }
        }
        if !failures.is_empty() {
            uncertain |= !stopped_ids.is_empty();
            for id in stopped_ids {
                let Some(unit) = inner.units.get_mut(&id) else {
                    continue;
                };
                assert_eq!(unit.state, State::Stopped);
                unit.state = State::QuiesceFailedStopped;
            }
        }
        drop(inner);

        if failures.is_empty() {
            self.running = false;
            self.save_quiesced_units = Some(quiesced_units);
            Ok(())
        } else {
            Err(QuiesceForSaveError {
                failures,
                uncertain,
            })
        }
    }

    /// Restarts every unit after a save failure that occurred after a fully
    /// successful quiesce but before publication committed. Restart and
    /// cleanup share `timeout`; timed-out transitions are retained for later
    /// termination cleanup.
    pub async fn resume_after_failed_save(&mut self, timeout: Duration) -> anyhow::Result<()> {
        anyhow::ensure!(!self.running, "VM was not successfully quiesced for save");
        let quiesced_units = self
            .save_quiesced_units
            .clone()
            .ok_or_else(|| anyhow::anyhow!("VM was not successfully quiesced for save"))?;
        anyhow::ensure!(
            self.inner
                .lock()
                .units
                .values()
                .all(|unit| unit.state == State::Stopped),
            "state-unit rollback is unsafe because not every unit is cleanly stopped"
        );

        enum StartResult {
            Started,
            DependencyFailed,
            NotSent(anyhow::Error),
            Failed(anyhow::Error),
            Uncertain(anyhow::Error, Option<PendingStart>),
        }

        let context = CancelContext::new().with_timeout(timeout);
        let mut operations = Vec::new();
        let ready_set;
        {
            let mut inner = self.inner.lock();
            ready_set = inner.ready_set(None);
            for (&id, unit) in &mut inner.units {
                if !quiesced_units.contains(&id) {
                    ready_set.done(id, true);
                    continue;
                }
                let name = unit.name.clone();
                let dependencies = unit.dependencies.iter().copied().collect::<Vec<_>>();
                let ready_set = ready_set.clone();
                let mut context = context.clone();
                let mut start =
                    state_change(name.clone(), unit, StateRequest::Start, Some(())).boxed();
                operations.push(async move {
                    let ready = match context
                        .until_cancelled(ready_set.wait("snapshot_rollback", id, &dependencies))
                        .await
                    {
                        Ok(ready) => ready,
                        Err(reason) => {
                            ready_set.done(id, false);
                            return (name, id, StartResult::NotSent(reason.into()));
                        }
                    };
                    if !ready {
                        ready_set.done(id, false);
                        return (name, id, StartResult::DependencyFailed);
                    }

                    let result = match context.until_cancelled(start.as_mut()).await {
                        Ok(Ok(Some(Ok(())))) => StartResult::Started,
                        Ok(Ok(Some(Err(error)))) => StartResult::Failed(error.into()),
                        Ok(Ok(None)) => StartResult::Uncertain(
                            anyhow::anyhow!("start request was not sent"),
                            None,
                        ),
                        Ok(Err(error)) => StartResult::Uncertain(error.into(), None),
                        Err(reason) => StartResult::Uncertain(
                            reason.into(),
                            Some(PendingStart {
                                future: start.map(|result| result.map(|_| ())).boxed(),
                            }),
                        ),
                    };
                    ready_set.done(id, matches!(result, StartResult::Started));
                    (name, id, result)
                });
                unit.state = State::Starting;
            }
        }

        let results = join_all(operations).await;
        let mut failures = Vec::new();
        let mut started_ids = Vec::new();
        let mut failed_or_uncertain_ids = Vec::new();
        {
            let mut inner = self.inner.lock();
            for (name, id, result) in results {
                let Some(unit) = inner.units.get_mut(&id) else {
                    continue;
                };
                match result {
                    StartResult::Started => {
                        unit.state = State::Running;
                        started_ids.push(id);
                    }
                    StartResult::DependencyFailed => {
                        unit.state = State::Stopped;
                        failures.push(format!("{name}: a dependency did not restart"));
                    }
                    StartResult::NotSent(error) => {
                        unit.state = State::Stopped;
                        failures.push(format!("{name}: {error:#}"));
                    }
                    StartResult::Failed(error) => {
                        unit.state = State::StartUncertain;
                        failed_or_uncertain_ids.push(id);
                        failures.push(format!("{name}: {error:#}"));
                    }
                    StartResult::Uncertain(error, pending_start) => {
                        unit.state = State::StartUncertain;
                        assert!(unit.pending_start.is_none());
                        unit.pending_start = pending_start;
                        failed_or_uncertain_ids.push(id);
                        failures.push(format!("{name}: {error:#}"));
                    }
                }
            }
        }

        if !failures.is_empty() {
            let cleanup_failures = self
                .stop_after_failed_snapshot_restart(context, &failed_or_uncertain_ids, &started_ids)
                .await;
            self.running = !cleanup_failures.is_empty();
            failures.extend(cleanup_failures);
            anyhow::bail!(
                "snapshot rollback could not safely restart all state units: {}",
                failures.join("; ")
            );
        }
        self.running = true;
        self.save_quiesced_units = None;
        Ok(())
    }

    async fn stop_after_failed_snapshot_restart(
        &self,
        context: CancelContext,
        failed_or_uncertain_ids: &[u64],
        started_ids: &[u64],
    ) -> Vec<String> {
        enum CleanupResult {
            Stopped(State),
            DependencyFailed,
            NotSent(anyhow::Error),
            Uncertain(anyhow::Error, Option<PendingStop>),
        }

        let unit_ids = failed_or_uncertain_ids
            .iter()
            .chain(started_ids)
            .copied()
            .collect::<Vec<_>>();
        let mut operations = Vec::new();
        let mut failures = Vec::new();
        let ready_set;
        {
            let mut inner = self.inner.lock();
            ready_set = inner.ready_set(Some(&unit_ids));
            for (&id, unit) in inner
                .units
                .iter_mut()
                .filter(|(id, _)| ready_set.0.contains_key(id))
            {
                if unit.pending_start.is_some() {
                    assert!(failed_or_uncertain_ids.contains(&id));
                    assert_eq!(unit.state, State::StartUncertain);
                    ready_set.done(id, false);
                    failures.push(format!(
                        "{}: failed_start_cleanup deferred until pending start completes",
                        unit.name
                    ));
                    continue;
                }

                let (op, old_state, stopped_state) = if failed_or_uncertain_ids.contains(&id) {
                    (
                        "failed_start_cleanup",
                        State::StartUncertain,
                        State::StartFailedStopped,
                    )
                } else {
                    ("failed_start_rollback", State::Running, State::Stopped)
                };
                assert_eq!(unit.state, old_state);
                let name = unit.name.clone();
                let dependents = unit.dependents.iter().copied().collect::<Vec<_>>();
                let ready_set = ready_set.clone();
                let mut context = context.clone();
                let mut stop = PendingStop {
                    future: state_change(name.clone(), unit, StateRequest::Stop, Some(())).boxed(),
                };
                operations.push(async move {
                    let ready = match context
                        .until_cancelled(ready_set.wait(op, id, &dependents))
                        .await
                    {
                        Ok(ready) => ready,
                        Err(reason) => {
                            ready_set.done(id, false);
                            return (name, id, op, CleanupResult::NotSent(reason.into()));
                        }
                    };
                    if !ready {
                        ready_set.done(id, false);
                        return (name, id, op, CleanupResult::DependencyFailed);
                    }

                    let result = match context.until_cancelled(stop.future.as_mut()).await {
                        Ok(Ok(Some(()))) => CleanupResult::Stopped(stopped_state),
                        Ok(Ok(None)) => CleanupResult::Uncertain(
                            anyhow::anyhow!("stop request was not sent"),
                            None,
                        ),
                        Ok(Err(error)) => CleanupResult::Uncertain(error.into(), None),
                        Err(reason) => CleanupResult::Uncertain(reason.into(), Some(stop)),
                    };
                    ready_set.done(id, matches!(result, CleanupResult::Stopped(_)));
                    (name, id, op, result)
                });
                unit.state = State::Stopping;
            }
        }

        let results = join_all(operations).await;
        let mut inner = self.inner.lock();
        for (name, id, op, result) in results {
            let Some(unit) = inner.units.get_mut(&id) else {
                continue;
            };
            assert_eq!(unit.state, State::Stopping);
            match result {
                CleanupResult::Stopped(stopped_state) => unit.state = stopped_state,
                CleanupResult::DependencyFailed => {
                    unit.state = State::StartUncertain;
                    failures.push(format!(
                        "{name}: {op} stop skipped because a dependent was not stopped"
                    ));
                }
                CleanupResult::NotSent(error) => {
                    unit.state = State::StartUncertain;
                    failures.push(format!("{name}: {op} stop was not sent: {error:#}"));
                }
                CleanupResult::Uncertain(error, pending_stop) => {
                    unit.state = State::StartUncertain;
                    assert!(unit.pending_stop.is_none());
                    unit.pending_stop = pending_stop;
                    failures.push(format!("{name}: {op} stop failed: {error:#}"));
                }
            }
        }
        failures
    }
}
