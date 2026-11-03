// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Snapshot quiesce transactions: bounded quiesce of every unit for save, and
//! restart after a failed save.

use super::PendingStop;
use super::State;
use super::StateRequest;
use super::StateUnits;
use super::state_change;
use futures::FutureExt;
use futures::future::join_all;
use mesh::CancelContext;
use std::collections::BTreeSet;
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
                let ready_set = ready_set.clone();
                let mut context = context.clone();
                let mut stop = PendingStop {
                    future: state_change(name.clone(), unit, StateRequest::Stop, Some(())).boxed(),
                };
                operations.push(async move {
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
                    if context.is_cancelled() {
                        ready_set.done(id, false);
                        return (
                            name,
                            id,
                            UnitResult::NotSent(anyhow::anyhow!(
                                "quiesce deadline expired before stop request was sent"
                            )),
                        );
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
            match result {
                UnitResult::Stopped => {
                    if let Some(unit) = inner.units.get_mut(&id) {
                        unit.state = State::Stopped;
                    }
                    stopped_ids.push(id);
                }
                UnitResult::DependencyFailed => {
                    if let Some(unit) = inner.units.get_mut(&id) {
                        unit.state = State::Running;
                    }
                    failures.push((
                        name.to_string(),
                        "a dependent unit did not quiesce".to_owned(),
                    ));
                }
                UnitResult::NotSent(error) => {
                    if let Some(unit) = inner.units.get_mut(&id) {
                        unit.state = State::Running;
                    }
                    failures.push((name.to_string(), format!("{error:#}")));
                }
                UnitResult::Uncertain(error, mut pending_stop) => {
                    if let Some(unit) = inner.units.get_mut(&id) {
                        unit.state = State::QuiesceUncertain;
                        assert!(unit.pending_stop.is_none());
                        unit.pending_stop = pending_stop.take();
                    } else if let Some(pending_stop) = pending_stop {
                        inner
                            .orphaned_pending_stops
                            .push((name.clone(), pending_stop));
                    }
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
    /// successful quiesce but before publication committed.
    pub async fn resume_after_failed_save(&mut self, timeout: Duration) -> anyhow::Result<()> {
        anyhow::ensure!(!self.running, "VM was not successfully quiesced for save");
        let quiesced_units = self
            .save_quiesced_units
            .clone()
            .ok_or_else(|| anyhow::anyhow!("VM was not successfully quiesced for save"))?;
        {
            let inner = self.inner.lock();
            anyhow::ensure!(
                inner
                    .units
                    .values()
                    .all(|unit| unit.state == State::Stopped),
                "state-unit rollback is unsafe because not every unit is cleanly stopped"
            );
            let quiesced_units = quiesced_units.iter().copied().collect::<BTreeSet<_>>();
            for id in &quiesced_units {
                let Some(unit) = inner.units.get(id) else {
                    continue;
                };
                for dependency in &unit.dependencies {
                    if !quiesced_units.contains(dependency)
                        && inner
                            .units
                            .get(dependency)
                            .is_some_and(|dependency| dependency.state == State::Stopped)
                    {
                        anyhow::bail!(
                            "state-unit rollback cannot restart '{}' because dependency '{}' was not quiesced",
                            unit.name,
                            inner.units[dependency].name
                        );
                    }
                }
            }
        }

        enum StartResult {
            Started,
            DependencyFailed,
            NotSent(anyhow::Error),
            Uncertain(anyhow::Error),
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
                let start = state_change(name.clone(), unit, StateRequest::Start, Some(()));
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
                    if context.is_cancelled() {
                        ready_set.done(id, false);
                        return (
                            name,
                            id,
                            StartResult::NotSent(anyhow::anyhow!(
                                "snapshot rollback deadline expired before start request was sent"
                            )),
                        );
                    }

                    let result = match context.until_cancelled(start).await {
                        Ok(Ok(Some(()))) => StartResult::Started,
                        Ok(Ok(None)) => {
                            StartResult::Uncertain(anyhow::anyhow!("start request was not sent"))
                        }
                        Ok(Err(error)) => StartResult::Uncertain(error.into()),
                        Err(reason) => StartResult::Uncertain(reason.into()),
                    };
                    ready_set.done(id, matches!(result, StartResult::Started));
                    (name, id, result)
                });
                unit.state = State::Starting;
            }
        }

        let results = join_all(operations).await;
        let mut failures = Vec::new();
        let mut inner = self.inner.lock();
        for (name, id, result) in results {
            match result {
                StartResult::Started => {
                    if let Some(unit) = inner.units.get_mut(&id) {
                        unit.state = State::Running;
                    }
                }
                StartResult::DependencyFailed => {
                    if let Some(unit) = inner.units.get_mut(&id) {
                        unit.state = State::Stopped;
                    }
                    failures.push(format!("{name}: a dependency did not restart"));
                }
                StartResult::NotSent(error) => {
                    if let Some(unit) = inner.units.get_mut(&id) {
                        unit.state = State::Stopped;
                    }
                    failures.push(format!("{name}: {error:#}"));
                }
                StartResult::Uncertain(error) => {
                    if let Some(unit) = inner.units.get_mut(&id) {
                        unit.state = State::QuiesceUncertain;
                    }
                    failures.push(format!("{name}: {error:#}"));
                }
            }
        }
        drop(inner);

        anyhow::ensure!(
            failures.is_empty(),
            "snapshot rollback could not safely restart all state units: {}",
            failures.join("; ")
        );
        self.running = true;
        self.save_quiesced_units = None;
        Ok(())
    }
}
