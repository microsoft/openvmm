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
use std::collections::BTreeSet;
use std::time::Instant;
use tracing::Instrument;

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
    /// The units that did start are stopped again in reverse dependency order.
    /// A started unit that a unit that is not stopped depends on, such as a
    /// unit whose start is uncertain, is not stopped, since that unit may
    /// still use it, and a started unit whose task does not answer its stop
    /// request may still run. Either is left in the terminal state too, and
    /// the units then stay running, so that [`StateUnits::stop`] can stop
    /// them.
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
            let rollback_failures = self.stop_after_failed_start(&started_ids).await;
            // A unit that the rollback did not stop may still run.
            self.running = was_running || !rollback_failures.is_empty();
            failures.extend(rollback_failures);
            anyhow::bail!("state units could not start: {}", failures.join("; "))
        }
    }

    /// Stops the units in `started_ids` again in reverse dependency order.
    ///
    /// Every other unit that is not stopped, such as one that was already
    /// running or one whose start is uncertain, blocks the stop of its
    /// dependencies without receiving a stop request. A unit whose stop is
    /// blocked or not answered is left in a terminal state. Returns the
    /// failures.
    async fn stop_after_failed_start(&self, started_ids: &[u64]) -> Vec<String> {
        const OP: &str = "failed_start_rollback";
        let started_ids = started_ids.iter().copied().collect::<BTreeSet<_>>();
        let mut operations = Vec::new();
        {
            let mut inner = self.inner.lock();
            let ready_set = inner.ready_set(None);
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
                let units = self.inner.clone();
                let stop = state_change(name.clone(), unit, StateRequest::Stop, Some(()));
                operations.push(async move {
                    if !ready_set.wait(OP, id, &dependents).await {
                        // A unit removed while it waits no longer uses its
                        // dependencies, so they can stop.
                        ready_set.done(id, !units.lock().units.contains_key(&id));
                        return (name, id, None);
                    }
                    let result = stop.await;
                    // Removing a unit ends its task, which then cannot answer.
                    // A removed unit no longer uses its dependencies, so they
                    // can stop.
                    let stopped =
                        matches!(result, Ok(Some(()))) || !units.lock().units.contains_key(&id);
                    ready_set.done(id, stopped);
                    (name, id, Some(result))
                });
                unit.state = State::Stopping;
            }
        }
        let results = async {
            let start = Instant::now();
            let results = join_all(operations).await;
            tracing::info!(duration = ?Instant::now() - start, "state change complete");
            results
        }
        .instrument(tracing::info_span!("state_change", operation = OP))
        .await;

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
                    failures.push(format!(
                        "{name}: {OP} stop failed: stop request was not sent"
                    ));
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
