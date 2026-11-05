// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Ordered stop. A unit's stop cannot fail, but its task can stop answering:
//! then the units that it depends on are not stopped, since it may still use
//! them, and the stop fails, rather than the whole process panicking.

use super::State;
use super::StateRequest;
use super::StateUnits;
use super::state_change;
use futures::future::join_all;
use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::Instant;
use tracing::Instrument;

/// The outcome of a unit's stop.
pub(super) enum StopResult {
    /// The unit answered its stop request.
    Stopped,
    /// The unit was not sent a stop request, because a unit that depends on
    /// it was not stopped.
    Blocked,
    /// The unit's task did not answer its stop request, so the unit may still
    /// be running.
    Uncertain(anyhow::Error),
}

/// The outcome of a unit's stop, with the unit's name and ID.
pub(super) type UnitStopResult = (Arc<str>, u64, StopResult);

impl StateUnits {
    /// Stops the units that are not stopped in reverse dependency order.
    ///
    /// A unit in a terminal state is sent a stop request too, and keeps its
    /// terminal marker. A unit whose task does not answer its stop request is
    /// left in a terminal state, and the units that it depends on keep their
    /// states. The units stay running until every unit has stopped.
    pub(super) async fn stop_in_order(&mut self) -> anyhow::Result<()> {
        const OP: &str = "stop";
        // Make the units in a terminal state receive a stop request, then
        // restore their terminal markers.
        let mut ids = BTreeSet::new();
        let mut terminal_states = BTreeMap::new();
        for (&id, unit) in &mut self.inner.lock().units {
            match unit.state {
                State::Stopped => continue,
                State::Running => {}
                State::StartUncertain | State::StopUncertain => {
                    terminal_states.insert(id, unit.state);
                    unit.state = State::Running;
                }
                state => panic!("unit {} in {state:?} state before stop", unit.name),
            }
            ids.insert(id);
        }

        let results = async {
            let start = Instant::now();
            let results = self.stop_units(OP, &ids).await;
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
            let terminal_state = terminal_states.get(&id).copied();
            unit.state = match result {
                StopResult::Stopped => terminal_state.unwrap_or(State::Stopped),
                StopResult::Uncertain(error) => {
                    failures.push(format!("{name}: {error:#}"));
                    State::StopUncertain
                }
                StopResult::Blocked => {
                    failures.push(format!(
                        "{name}: stop skipped because a dependent was not stopped"
                    ));
                    terminal_state.unwrap_or(State::Running)
                }
            };
        }
        if failures.is_empty() {
            self.running = false;
            Ok(())
        } else {
            anyhow::bail!("state units could not stop: {}", failures.join("; "))
        }
    }

    /// Sends a stop request to each running unit in `ids` in reverse
    /// dependency order, and returns the outcome of each unit's stop.
    ///
    /// A unit is sent its request once every unit that depends on it has
    /// stopped. A unit that is not in `ids` has stopped only if it is in the
    /// stopped state, and a unit in `ids` once it answers its request or is
    /// removed. The units in `ids` are left stopping, for the caller to move
    /// each to the state that the outcome of its stop leaves it in.
    pub(super) async fn stop_units(
        &self,
        op: &'static str,
        ids: &BTreeSet<u64>,
    ) -> Vec<UnitStopResult> {
        let mut operations = Vec::new();
        {
            let mut inner = self.inner.lock();
            let ready_set = inner.ready_set(None);
            for (&id, unit) in &inner.units {
                if !ids.contains(&id) {
                    ready_set.done(id, unit.state == State::Stopped);
                }
            }
            for (&id, unit) in inner.units.iter_mut().filter(|(id, _)| ids.contains(id)) {
                assert_eq!(unit.state, State::Running);
                let name = unit.name.clone();
                let dependents = unit.dependents.iter().copied().collect::<Vec<_>>();
                let ready_set = ready_set.clone();
                let units = self.inner.clone();
                let stop = state_change(name.clone(), unit, StateRequest::Stop, Some(()));
                operations.push(async move {
                    if !ready_set.wait(op, id, &dependents).await {
                        ready_set.done(id, false);
                        return (name, id, StopResult::Blocked);
                    }
                    let result = match stop.await {
                        Ok(Some(())) => StopResult::Stopped,
                        Ok(None) => {
                            StopResult::Uncertain(anyhow::anyhow!("stop request was not sent"))
                        }
                        Err(error) => StopResult::Uncertain(error.into()),
                    };
                    // Removing a unit ends its task, which then cannot answer.
                    // A removed unit no longer uses its dependencies, so they
                    // can stop.
                    let stopped = matches!(result, StopResult::Stopped)
                        || !units.lock().units.contains_key(&id);
                    ready_set.done(id, stopped);
                    (name, id, result)
                });
                unit.state = State::Stopping;
            }
        }
        join_all(operations).await
    }
}
