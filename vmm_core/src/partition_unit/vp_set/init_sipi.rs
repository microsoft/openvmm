// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! The snapshot capture's check that no stopped VP has an INIT or a SIPI that
//! it has not accepted yet.
//!
//! When another VP sends an INIT or a SIPI to a VP that has already stopped, a
//! backend such as KVM may hold the event until the VP runs again or its state
//! is read. A save reads a VP's registers before its activity, and reading the
//! activity may accept the event: the saved registers would be from before the
//! event, and the rest of the saved state from after it. A VP can also hold an
//! INIT that it cannot accept yet, which the saved state cannot represent. A
//! capture therefore checks every VP after all the state units stopped, before
//! it saves any.

use super::StateEvent;
use super::VpEvent;
use super::VpSet;
use anyhow::Context as _;
use futures::future::TryJoinAll;
use mesh::rpc::RpcSend;
use virt::Processor;

/// Checks that the stopped VP `vp` has no INIT or SIPI that it has not
/// accepted yet.
pub(super) fn check_no_pending_init_sipi(vp: &mut impl Processor) -> anyhow::Result<()> {
    let pending = vp
        .has_pending_init_sipi()
        .context("failed to read the stopped VP's pending INIT and SIPI")?;
    anyhow::ensure!(
        !pending,
        "the VP holds an INIT or a SIPI that it has not accepted yet, which a \
         save would either split into state from before the event and state \
         from after it or lose; the capture fails, and the guest can request \
         the snapshot again"
    );
    Ok(())
}

impl VpSet {
    /// Checks that no stopped VP has an INIT or a SIPI that it has not
    /// accepted yet, before a capture saves any VP.
    pub async fn check_no_pending_init_sipi(&mut self) -> anyhow::Result<()> {
        anyhow::ensure!(
            !self.started,
            "vCPUs must be stopped before checking for a pending INIT or SIPI"
        );
        self.vps
            .iter()
            .enumerate()
            .map(|(index, vp)| async move {
                vp.send
                    .call_failable(|rpc| VpEvent::State(StateEvent::CheckInitSipi(rpc)), ())
                    .await
                    .with_context(|| format!("vp{index} pending INIT or SIPI"))
            })
            .collect::<TryJoinAll<_>>()
            .await?;
        Ok(())
    }
}
