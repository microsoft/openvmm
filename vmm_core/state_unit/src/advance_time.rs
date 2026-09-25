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
    pub async fn advance_time(&mut self, duration: Duration) -> Result<(), StateTransitionError> {
        assert!(!self.running);
        let results = self
            .run_op(
                "advance_time",
                None,
                State::Stopped,
                State::AdvancingTime,
                State::Stopped,
                StateRequest::AdvanceTime,
                |_, _| Some(duration),
                |unit| &unit.dependencies,
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
