// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Fallible start with rollback. If a unit fails to start, its dependents do
//! not start, and the units that did start are stopped again.

use super::State;
use super::StateRequest;
use super::StateUnits;
use super::state_change;
use futures::future::join_all;
use std::time::Instant;

impl StateUnits {
    /// Starts all stopped units in dependency order.
    ///
    /// A unit does not start if one of its dependencies fails to start. On any
    /// failure, the units that did start are stopped again, and a unit whose
    /// start failed or is uncertain is left in a terminal state that fails
    /// later starts.
    pub(super) async fn start_with_rollback(&mut self) -> anyhow::Result<()> {
        enum StartResult {
            Started,
            DependencyFailed,
            Failed(anyhow::Error),
            Uncertain(anyhow::Error),
        }

        let was_running = self.running;
        let mut operations = Vec::new();
        let ready_set;
        {
            let mut inner = self.inner.lock();
            if let Some(unit) = inner
                .units
                .values()
                .find(|unit| !matches!(unit.state, State::Stopped | State::Running))
            {
                anyhow::bail!(
                    "state unit '{}' cannot be started from terminal state {:?}",
                    unit.name,
                    unit.state
                );
            }
            ready_set = inner.ready_set(None);
            for (&id, unit) in &mut inner.units {
                if unit.state == State::Running {
                    ready_set.done(id, true);
                    continue;
                }
                assert_eq!(
                    unit.state,
                    State::Stopped,
                    "unit {} is not stopped before start",
                    unit.name,
                );
                let name = unit.name.clone();
                let dependencies = unit.dependencies.clone();
                let ready_set = ready_set.clone();
                let start = state_change(name.clone(), unit, StateRequest::Start, Some(()));
                operations.push(async move {
                    if !ready_set.wait("start", id, &dependencies).await {
                        ready_set.done(id, false);
                        return (name, id, StartResult::DependencyFailed);
                    }

                    let result = match start.await {
                        Ok(Some(Ok(()))) => StartResult::Started,
                        Ok(Some(Err(error))) => StartResult::Failed(error.into()),
                        Ok(None) => {
                            StartResult::Uncertain(anyhow::anyhow!("start request was not sent"))
                        }
                        Err(error) => StartResult::Uncertain(error.into()),
                    };
                    ready_set.done(id, matches!(result, StartResult::Started));
                    (name, id, result)
                });
                unit.state = State::Starting;
            }
        }

        let start = Instant::now();
        let results = join_all(operations).await;
        tracing::info!(duration = ?Instant::now() - start, "state-unit start complete");

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
                        failures.push(format!("{name}: a dependency did not start"));
                    }
                    StartResult::Failed(error) => {
                        unit.state = State::StartUncertain;
                        failed_or_uncertain_ids.push(id);
                        failures.push(format!("{name}: {error:#}"));
                    }
                    StartResult::Uncertain(error) => {
                        unit.state = State::StartUncertain;
                        failed_or_uncertain_ids.push(id);
                        failures.push(format!("{name}: {error:#}"));
                    }
                }
            }
        }

        if failures.is_empty() {
            self.running = true;
            Ok(())
        } else {
            failures.extend(
                self.stop_after_failed_start(&failed_or_uncertain_ids, &started_ids)
                    .await,
            );
            self.running = was_running;
            anyhow::bail!("state units could not start: {}", failures.join("; "))
        }
    }

    async fn stop_after_failed_start(
        &self,
        failed_or_uncertain_ids: &[u64],
        started_ids: &[u64],
    ) -> Vec<String> {
        let unit_ids = failed_or_uncertain_ids
            .iter()
            .chain(started_ids)
            .copied()
            .collect::<Vec<_>>();
        let mut operations = Vec::new();
        let ready_set;
        {
            let mut inner = self.inner.lock();
            ready_set = inner.ready_set(Some(&unit_ids));
            for (&id, unit) in inner
                .units
                .iter_mut()
                .filter(|(id, _)| ready_set.0.contains_key(id))
            {
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
                let dependents = unit.dependents.clone();
                let ready_set = ready_set.clone();
                let stop = state_change(name.clone(), unit, StateRequest::Stop, Some(()));
                operations.push(async move {
                    if !ready_set.wait(op, id, &dependents).await {
                        ready_set.done(id, false);
                        return (name, id, op, stopped_state, None);
                    }
                    let result = stop.await;
                    ready_set.done(id, matches!(&result, Ok(Some(()))));
                    (name, id, op, stopped_state, Some(result))
                });
                unit.state = State::Stopping;
            }
        }

        let results = join_all(operations).await;
        let mut failures = Vec::new();
        let mut inner = self.inner.lock();
        for (name, id, op, stopped_state, result) in results {
            let Some(unit) = inner.units.get_mut(&id) else {
                continue;
            };
            assert_eq!(unit.state, State::Stopping);
            match result {
                Some(Ok(Some(()))) => unit.state = stopped_state,
                Some(Ok(None)) => {
                    unit.state = State::StartUncertain;
                    failures.push(format!("{name}: {op} stop request was not sent"));
                }
                Some(Err(error)) => {
                    unit.state = State::StartUncertain;
                    failures.push(format!("{name}: {op} stop failed: {error:#}"));
                }
                None => {
                    unit.state = State::StartUncertain;
                    failures.push(format!(
                        "{name}: {op} stop skipped because a dependent was not stopped"
                    ));
                }
            }
        }
        failures
    }
}
