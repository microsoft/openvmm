// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Start with rollback. A unit's start cannot fail, but its task can stop
//! answering: then its dependents do not start, and the units that did start
//! are stopped again, rather than the whole process panicking.

use super::State;
use super::StateRequest;
use super::StateUnits;
use super::state_change;
use futures::future::join_all;
use std::time::Instant;

impl StateUnits {
    /// Starts all stopped units in dependency order.
    ///
    /// Fails without starting any unit if one is in a terminal state. A unit
    /// does not start if one of its dependencies
    /// did not start. If a unit's task does not answer its start request, the
    /// units that did start are stopped again, and the unit is left in a
    /// terminal state that fails later starts. A started unit that cannot be
    /// stopped again, because a unit that depends on it was not stopped, is
    /// left running in that terminal state too, and the units stay running so
    /// that [`StateUnits::stop`] can stop them later.
    pub(super) async fn start_with_rollback(&mut self) -> anyhow::Result<()> {
        enum StartResult {
            Started,
            DependencyFailed,
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
                let dependencies = unit.dependencies.iter().copied().collect::<Vec<_>>();
                let ready_set = ready_set.clone();
                let start = state_change(name.clone(), unit, StateRequest::Start, Some(()));
                operations.push(async move {
                    if !ready_set.wait("start", id, &dependencies).await {
                        ready_set.done(id, false);
                        return (name, id, StartResult::DependencyFailed);
                    }

                    let result = match start.await {
                        Ok(Some(())) => StartResult::Started,
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
                    StartResult::Uncertain(error) => {
                        unit.state = State::StartUncertain;
                        failures.push(format!("{name}: {error:#}"));
                    }
                }
            }
        }

        if failures.is_empty() {
            self.running = true;
            Ok(())
        } else {
            let cleanup_failures = self.stop_after_failed_start(&started_ids).await;
            self.running = was_running || !cleanup_failures.is_empty();
            failures.extend(cleanup_failures);
            anyhow::bail!("state units could not start: {}", failures.join("; "))
        }
    }

    /// Stops the units in `started_ids` again in reverse dependency order.
    ///
    /// Every other unit that is not stopped, such as one that was already
    /// running or one whose start is uncertain, blocks the stop of its
    /// dependencies without receiving a stop request. A unit whose stop is
    /// blocked or not confirmed is left in a terminal state.
    async fn stop_after_failed_start(&self, started_ids: &[u64]) -> Vec<String> {
        const OP: &str = "failed_start_rollback";
        let mut operations = Vec::new();
        let ready_set;
        {
            let mut inner = self.inner.lock();
            ready_set = inner.ready_set(None);
            for (&id, unit) in &inner.units {
                if !started_ids.contains(&id) {
                    ready_set.done(id, unit.state == State::Stopped);
                }
            }
            for (&id, unit) in inner
                .units
                .iter_mut()
                .filter(|(id, _)| started_ids.contains(id))
            {
                assert_eq!(unit.state, State::Running);
                let name = unit.name.clone();
                let dependents = unit.dependents.iter().copied().collect::<Vec<_>>();
                let ready_set = ready_set.clone();
                let stop = state_change(name.clone(), unit, StateRequest::Stop, Some(()));
                operations.push(async move {
                    if !ready_set.wait(OP, id, &dependents).await {
                        ready_set.done(id, false);
                        return (name, id, None);
                    }
                    let result = stop.await;
                    ready_set.done(id, matches!(&result, Ok(Some(()))));
                    (name, id, Some(result))
                });
                unit.state = State::Stopping;
            }
        }

        let results = join_all(operations).await;
        let mut failures = Vec::new();
        let mut inner = self.inner.lock();
        for (name, id, result) in results {
            let Some(unit) = inner.units.get_mut(&id) else {
                continue;
            };
            assert_eq!(unit.state, State::Stopping);
            match result {
                Some(Ok(Some(()))) => unit.state = State::Stopped,
                Some(Ok(None)) => {
                    unit.state = State::StartUncertain;
                    failures.push(format!("{name}: {OP} stop request was not sent"));
                }
                Some(Err(error)) => {
                    unit.state = State::StartUncertain;
                    failures.push(format!("{name}: {OP} stop failed: {error:#}"));
                }
                None => {
                    unit.state = State::StartUncertain;
                    failures.push(format!(
                        "{name}: {OP} stop skipped because a dependent was not stopped"
                    ));
                }
            }
        }
        failures
    }
}
