// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Start with rollback. A unit's start cannot fail, but its confirmation can,
//! and its task can stop answering: then its dependents do not start, and the
//! units that did start are stopped again, rather than the whole process
//! panicking.

use super::State;
use super::StateRequest;
use super::StateUnits;
use super::state_change;
use futures::future::join_all;
use std::time::Instant;

impl StateUnits {
    /// Starts all stopped units in dependency order.
    ///
    /// Fails without starting any unit if one is in a terminal state. Each
    /// unit confirms its start, with [`StateUnit::confirm_start`], before its
    /// dependents start. A unit does not start if one of its dependencies did
    /// not start or did not confirm its start. If a unit fails to confirm its
    /// start, or its task does not answer its start or confirmation request,
    /// the units that did start, the failed unit among them, are stopped
    /// again. A unit whose task does not answer is left in a terminal state:
    /// while it is in that state, later starts, resets, saves, restores, and
    /// time advances fail, and a stop stops it, since it may be running, but
    /// leaves it in that state. A unit that failed to confirm its start can
    /// start again.
    ///
    /// [`StateUnit::confirm_start`]: crate::StateUnit::confirm_start
    pub(super) async fn start_with_rollback(&mut self) -> anyhow::Result<()> {
        enum StartResult {
            Started,
            NotConfirmed(anyhow::Error),
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
                let confirm =
                    state_change(name.clone(), unit, StateRequest::ConfirmStart, Some(()));
                operations.push(async move {
                    if !ready_set.wait("start", id, &dependencies).await {
                        ready_set.done(id, false);
                        return (name, id, StartResult::DependencyFailed);
                    }

                    let result = match start.await {
                        Ok(Some(())) => match confirm.await {
                            Ok(Some(Ok(()))) => StartResult::Started,
                            Ok(Some(Err(error))) => StartResult::NotConfirmed(error.into()),
                            Ok(None) => StartResult::Uncertain(anyhow::anyhow!(
                                "confirmation request was not sent"
                            )),
                            Err(error) => StartResult::Uncertain(error.into()),
                        },
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
                    StartResult::NotConfirmed(error) => {
                        unit.state = State::Running;
                        started_ids.push(id);
                        failures.push(format!("{name}: {error:#}"));
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
            if !started_ids.is_empty() {
                self.run_op(
                    "failed_start_rollback",
                    Some(&started_ids),
                    State::Running,
                    State::Stopping,
                    State::Stopped,
                    StateRequest::Stop,
                    |_, _| Some(()),
                    |unit| Some(&unit.dependents),
                )
                .await;
            }
            self.running = was_running;
            anyhow::bail!("state units could not start: {}", failures.join("; "))
        }
    }
}
