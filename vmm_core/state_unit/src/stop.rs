// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Ordered stop. A unit's stop cannot fail, but its task can stop answering:
//! then the units that it depends on are not stopped, since it may still use
//! them.

use super::State;
use super::StateRequest;
use super::StateUnits;
use super::state_change;
use futures::future::join_all;
use std::collections::BTreeSet;
use std::sync::Arc;

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
    /// Sends a stop request to each running unit in `ids` in reverse
    /// dependency order, and returns the outcome of each unit's stop.
    ///
    /// A unit is sent its request once every unit that depends on it has
    /// stopped. A unit that is not in `ids` has stopped only if it is in the
    /// stopped state, and a unit in `ids` once it answers its request. The
    /// units in `ids` are left stopping, for the caller to move each to the
    /// state that the outcome of its stop leaves it in.
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
                    ready_set.done(id, matches!(result, StopResult::Stopped));
                    (name, id, result)
                });
                unit.state = State::Stopping;
            }
        }
        join_all(operations).await
    }
}
