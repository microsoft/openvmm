// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Snapshot input gating: stopping host input before the snapshot vCPU
//! boundary, and resuming it when the VM continues past that boundary.

use super::StateRequest;
use super::StateUnits;
use super::state_change;
use futures::future::join_all;
use mesh::rpc::FailableRpc;

impl StateUnits {
    /// Stops host-input producers before establishing a snapshot vCPU boundary.
    ///
    /// Waits for every unit, without a deadline: a unit that does not answer
    /// stalls the gate, and the log names it while it waits. Fails, once every
    /// unit has answered, if any reported an error; the units that did stop
    /// their input resume it with [`Self::resume_input_after_save`].
    pub async fn quiesce_input_for_save(&self) -> anyhow::Result<()> {
        self.run_input_op("quiesce_input", StateRequest::QuiesceInput)
            .await
    }

    /// Resumes host-input producers before releasing a failed snapshot boundary.
    ///
    /// Waits for every unit, without a deadline, as
    /// [`Self::quiesce_input_for_save`] does.
    pub async fn resume_input_after_save(&self) -> anyhow::Result<()> {
        self.run_input_op("resume_input", StateRequest::ResumeInput)
            .await
    }

    async fn run_input_op(
        &self,
        operation: &'static str,
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
        let results = join_all(operations).await;
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
}
