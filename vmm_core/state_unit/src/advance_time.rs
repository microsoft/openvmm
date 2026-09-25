// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Advancing guest-visible time by the snapshot downtime after restore.

use super::State;
use super::StateRequest;
use super::StateTransitionError;
use super::StateUnits;
use super::check;
use std::time::Duration;

impl StateUnits {
    /// Advances guest-visible time on all stopped units in dependency order.
    /// Only units added with [`UnitBuilder::advances_time`](crate::UnitBuilder::advances_time)
    /// receive the request; the others pass through the operation without a
    /// round trip.
    ///
    /// Fails without sending a request while a unit is in a terminal state,
    /// whether or not the units run. Otherwise panics if running.
    pub async fn advance_time(&mut self, duration: Duration) -> Result<(), StateTransitionError> {
        self.check_no_terminal_units("advance_time")?;
        assert!(!self.running);
        let results = self
            .run_op(
                "advance_time",
                None,
                State::Stopped,
                State::AdvancingTime,
                State::Stopped,
                StateRequest::AdvanceTime,
                |_, unit| unit.advances_time.then_some(duration),
                |unit| Some(&unit.dependencies),
            )
            .await;
        check(
            "advance_time",
            results
                .into_iter()
                .map(|(name, result)| (name, result.map_err(anyhow::Error::from))),
        )?;
        Ok(())
    }
}
