// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Snapshot quiesce transactions: gating host input around the snapshot
//! vCPU boundary, bounded quiesce of every unit for save, and restart after a
//! failed save.

use super::State;
use super::StateRequest;
use super::StateUnits;
use super::state_change;
use anyhow::Context as _;
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
    /// Returns whether any unit may have partially changed ownership or state.
    ///
    /// When true, the VM cannot be resumed safely and must be terminated.
    pub fn has_uncertain_state(&self) -> bool {
        self.uncertain
    }
}

impl StateUnits {
    /// Stops host-input producers before establishing a snapshot vCPU boundary.
    pub async fn quiesce_input_for_save(&self, timeout: Duration) -> anyhow::Result<()> {
        self.run_input_op("quiesce_input", timeout, StateRequest::QuiesceInput)
            .await
    }

    /// Resumes host-input producers before releasing a failed snapshot boundary.
    pub async fn resume_input_after_save(&self, timeout: Duration) -> anyhow::Result<()> {
        self.run_input_op("resume_input", timeout, StateRequest::ResumeInput)
            .await
    }

    async fn run_input_op(
        &self,
        operation: &'static str,
        timeout: Duration,
        request: impl Copy + FnOnce(FailableRpc<(), ()>) -> StateRequest,
    ) -> anyhow::Result<()> {
        let operations = {
            let inner = self.inner.lock();
            inner
                .units
                .values()
                .map(|unit| {
                    let name = unit.name.clone();
                    let operation = state_change(name.clone(), unit, request, Some(()));
                    async move { (name, operation.await) }
                })
                .collect::<Vec<_>>()
        };
        let mut context = CancelContext::new().with_timeout(timeout);
        let results = context
            .until_cancelled(join_all(operations))
            .await
            .with_context(|| format!("{operation} timed out"))?;
        let mut failures = Vec::new();
        for (name, result) in results {
            match result {
                Ok(Some(Ok(()))) => {}
                Ok(Some(Err(error))) => failures.push(format!("{name}: {error}")),
                Ok(None) => failures.push(format!("{name}: request was not sent")),
                Err(error) => failures.push(format!("{name}: {error}")),
            }
        }
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
    /// VM rather than attempt rollback in that case.
    pub async fn quiesce_for_save(&mut self, timeout: Duration) -> Result<(), QuiesceForSaveError> {
        assert!(self.running);

        enum UnitResult {
            Stopped,
            DependencyFailed,
            Uncertain(anyhow::Error),
        }

        let context = CancelContext::new().with_timeout(timeout);
        let mut operations = Vec::new();
        let ready_set;
        {
            let mut inner = self.inner.lock();
            ready_set = inner.ready_set(None);
            for (&id, unit) in &mut inner.units {
                assert_eq!(
                    unit.state,
                    State::Running,
                    "unit {} is not running before save quiesce",
                    unit.name,
                );
                let name = unit.name.clone();
                let dependencies = unit.dependents.clone();
                let ready_set = ready_set.clone();
                let mut context = context.clone();
                let stop = state_change(name.clone(), unit, StateRequest::Stop, Some(()));
                operations.push(async move {
                    if !ready_set.wait("quiesce_for_save", id, &dependencies).await {
                        ready_set.done(id, false);
                        return (name, id, UnitResult::DependencyFailed);
                    }

                    let result = match context.until_cancelled(stop).await {
                        Ok(Ok(Some(()))) => UnitResult::Stopped,
                        Ok(Ok(None)) => {
                            UnitResult::Uncertain(anyhow::anyhow!("stop request was not sent"))
                        }
                        Ok(Err(error)) => UnitResult::Uncertain(error.into()),
                        Err(reason) => UnitResult::Uncertain(reason.into()),
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

        let mut failures = Vec::new();
        let mut uncertain = false;
        let mut inner = self.inner.lock();
        for (name, id, result) in results {
            let Some(unit) = inner.units.get_mut(&id) else {
                continue;
            };
            match result {
                UnitResult::Stopped => unit.state = State::Stopped,
                UnitResult::DependencyFailed => {
                    unit.state = State::Running;
                    failures.push((
                        name.to_string(),
                        "a dependent unit did not quiesce".to_owned(),
                    ));
                }
                UnitResult::Uncertain(error) => {
                    unit.state = State::QuiesceUncertain;
                    uncertain = true;
                    failures.push((name.to_string(), format!("{error:#}")));
                }
            }
        }
        drop(inner);

        if failures.is_empty() {
            self.running = false;
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
            Failed(anyhow::Error),
            Uncertain(anyhow::Error),
        }

        let context = CancelContext::new().with_timeout(timeout);
        let mut operations = Vec::new();
        let ready_set;
        {
            let mut inner = self.inner.lock();
            ready_set = inner.ready_set(None);
            for (&id, unit) in &mut inner.units {
                let name = unit.name.clone();
                let dependencies = unit.dependencies.clone();
                let ready_set = ready_set.clone();
                let mut context = context.clone();
                let start = state_change(name.clone(), unit, StateRequest::Start, Some(()));
                operations.push(async move {
                    if !ready_set.wait("snapshot_rollback", id, &dependencies).await {
                        ready_set.done(id, false);
                        return (name, id, StartResult::DependencyFailed);
                    }

                    let result = match context.until_cancelled(start).await {
                        Ok(Ok(Some(Ok(())))) => StartResult::Started,
                        Ok(Ok(Some(Err(error)))) => StartResult::Failed(error.into()),
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
            let Some(unit) = inner.units.get_mut(&id) else {
                continue;
            };
            match result {
                StartResult::Started => unit.state = State::Running,
                StartResult::DependencyFailed => {
                    unit.state = State::Stopped;
                    failures.push(format!("{name}: a dependency did not restart"));
                }
                StartResult::Failed(error) => {
                    unit.state = State::QuiesceUncertain;
                    failures.push(format!("{name}: {error:#}"));
                }
                StartResult::Uncertain(error) => {
                    unit.state = State::QuiesceUncertain;
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
        Ok(())
    }
}
