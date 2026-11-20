// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Snapshot input gating: stopping host input before the snapshot vCPU
//! boundary, and resuming it after a failed snapshot transaction.

use super::State;
use super::StateRequest;
use super::StateUnits;
use super::state_change;
use futures::future::join_all;
use mesh::rpc::FailableRpc;

impl StateUnits {
    /// Stops host-input producers in running units before establishing a
    /// snapshot vCPU boundary.
    ///
    /// Waits for every running unit, without a deadline, so a unit that does
    /// not answer stalls the transaction. Stopped units are skipped, and
    /// terminal lifecycle states fail the transaction. Drive the returned
    /// future to completion: if it is dropped, the units still run the hooks
    /// that it requested, and a unit's next request may arrive before its hook
    /// completes.
    pub async fn quiesce_input_for_save(&mut self) -> anyhow::Result<()> {
        self.run_input_op("quiesce_input", StateRequest::QuiesceInput)
            .await
    }

    /// Resumes host-input producers in running units before releasing a failed
    /// snapshot boundary.
    ///
    /// Waits for every running unit, without a deadline, so a unit that does
    /// not answer stalls the transaction. Stopped units are skipped, and
    /// terminal lifecycle states fail the transaction. As for
    /// [`StateUnits::quiesce_input_for_save`], drive the returned future to
    /// completion.
    pub async fn resume_input_after_save(&mut self) -> anyhow::Result<()> {
        self.run_input_op("resume_input", StateRequest::ResumeInput)
            .await
    }

    async fn run_input_op(
        &mut self,
        operation: &'static str,
        request: impl Copy + FnOnce(FailableRpc<(), ()>) -> StateRequest,
    ) -> anyhow::Result<()> {
        let mut operations = Vec::new();
        let mut failures = Vec::new();
        {
            let inner = self.inner.lock();
            for unit in inner.units.values() {
                match unit.state {
                    State::Running => {
                        let name = unit.name.clone();
                        let operation = state_change(name.clone(), unit, request, Some(()));
                        operations.push(async move { (name, operation.await) });
                    }
                    State::Stopped => {}
                    State::StartUncertain => {
                        failures.push(format!(
                            "{}: unit is in terminal state {:?}",
                            unit.name, unit.state
                        ));
                    }
                    state => {
                        failures.push(format!(
                            "{}: unit is in unexpected state {state:?}",
                            unit.name
                        ));
                    }
                }
            }
        }

        for (name, result) in join_all(operations).await {
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
}
