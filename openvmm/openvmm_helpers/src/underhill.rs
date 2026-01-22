// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Helpers for managing the Underhill firmware.

use anyhow::Context;
use get_resources::ged::GuestEmulationRequest;
use get_resources::ged::GuestServicingFlags;
use mesh::rpc::RpcSend;
use openvmm_defs::rpc::VmRpc;

/// Save the running state of Underhill and stage the new version.
pub async fn save_underhill(
    vm_send: &mesh::Sender<VmRpc>,
    send: &mesh::Sender<GuestEmulationRequest>,
    flags: GuestServicingFlags,
    file: std::fs::File,
) -> anyhow::Result<()> {
    // Stage the IGVM file in the VM worker.
    tracing::debug!("staging new IGVM file");
    vm_send
        .call_failable(VmRpc::StartReloadIgvm, file)
        .await
        .context("failed to stage new IGVM file")?;

    // Block waiting for the guest to send saved state.
    //
    // TODO: make this event driven instead so that other operations are not
    // blocked while waiting for the guest.
    tracing::debug!("waiting for guest to send saved state");
    let r = send
        .call_failable(GuestEmulationRequest::SaveGuestVtl2State, flags)
        .await
        .context("failed to save VTL2 state");

    if r.is_err() {
        // Clear the staged IGVM file.
        tracing::debug!(?r, "save state failed, clearing staged IGVM file");
        let _ = vm_send.call(VmRpc::CompleteReloadIgvm, false).await;
    }

    r
}

/// Restore Underhill from a previously saved state. This should always be called after save_underhill.
pub async fn restore_underhill(
    vm_send: &mesh::Sender<VmRpc>,
    send: &mesh::Sender<GuestEmulationRequest>,
) -> anyhow::Result<()> {
    // Reload the IGVM file and reset VTL2 state.
    tracing::debug!("reloading IGVM file");
    vm_send
        .call_failable(VmRpc::CompleteReloadIgvm, true)
        .await
        .context("failed to reload VTL2 firmware")?;

    // Wait for VTL0 to start.
    //
    // TODO: event driven, cancellable.
    tracing::debug!("waiting for VTL0 to start");
    send.call_failable(GuestEmulationRequest::WaitForVtl0Start, ())
        .await
        .context("vtl0 start failed")?;

    Ok(())
}
