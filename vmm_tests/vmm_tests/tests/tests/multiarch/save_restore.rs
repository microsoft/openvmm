// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Save/restore integration tests that exercise OpenVMM's snapshot machinery.

use anyhow::Context;
use petri::PetriHaltReason;
use petri::PetriVmBuilder;
use petri::PetriVmRuntime;
use petri::openvmm::OpenVmmPetriBackend;
use vmm_test_macros::openvmm_test;

/// Regression test for UEFI cross-process restore.
///
/// The in-process pulse (`verify_save_restore`) restores into the *same*
/// worker, whose UEFI resolvers were already registered during cold boot, so it
/// cannot reproduce the bug where restoring a UEFI snapshot onto a *fresh*
/// worker panicked with `no resolver for uefi_logger:platform`.
#[openvmm_test(unstable(
    reason = "cross-process restore is new infrastructure; gate until it has soaked in CI",
    uefi_x64(vhd(alpine_3_23_x64))
))]
async fn uefi_cross_process_restore(
    config: PetriVmBuilder<OpenVmmPetriBackend>,
) -> anyhow::Result<()> {
    verify_openvmm_cross_process_restore(config).await
}

/// Verify Linux direct cross-process restore.
#[openvmm_test(unstable(
    reason = "cross-process restore is new infrastructure; gate until it has soaked in CI",
    linux_direct_x64
))]
async fn linux_direct_cross_process_restore(
    config: PetriVmBuilder<OpenVmmPetriBackend>,
) -> anyhow::Result<()> {
    verify_openvmm_cross_process_restore(config).await
}

async fn verify_openvmm_cross_process_restore(
    config: PetriVmBuilder<OpenVmmPetriBackend>,
) -> anyhow::Result<()> {
    let config = config.prepare_openvmm_relaunch()?;

    // Shared backing file carries guest RAM across the process boundary.
    let backing =
        tempfile::NamedTempFile::new().context("failed to create guest memory backing file")?;
    let backing_path = backing.into_temp_path();

    // Worker #1: cold boot, reach steady state, save.
    tracing::info!("cross-process restore: booting worker #1");
    let mut vm1 = config.launch(backing_path.to_path_buf()).await?;

    let client = vm1.wait_for_agent(false).await?;
    client.ping().await?;
    drop(client);
    vm1.pause().await?;
    let saved_state = vm1.save_state().await?;
    vm1.teardown().await?;

    // Worker #2: fresh worker restoring the snapshot (the regression gate).
    tracing::info!("cross-process restore: launching worker #2 from saved state");
    let mut vm2 = config
        .restore(backing_path.to_path_buf(), &saved_state)
        .await
        .context("restoring snapshot onto a fresh worker failed")?;

    let client = vm2.wait_for_agent(false).await?;
    client.ping().await?;

    // A guest reboot executes the retained recipe rather than turning into
    // the old Restore/None no-op.
    client.reboot().await?;
    drop(client);
    let halt_reason = vm2.wait_for_halt(true).await?;
    anyhow::ensure!(
        halt_reason.reason == PetriHaltReason::Reset,
        "expected reset after guest reboot, got {halt_reason:?}"
    );
    let client = vm2.wait_for_agent(false).await?;
    client.ping().await?;
    drop(client);

    // Save the restored worker and restore that state on another fresh worker.
    // The recipe must remain wrapped exactly once.
    vm2.pause().await?;
    let saved_state = vm2.save_state().await?;
    vm2.teardown().await?;

    tracing::info!("cross-process restore: launching worker #3 from saved state");
    let mut vm3 = config
        .restore(backing_path.to_path_buf(), &saved_state)
        .await
        .context("restoring a snapshot produced by a restored worker failed")?;

    let client = vm3.wait_for_agent(false).await?;
    client.ping().await?;
    drop(client);

    vm3.teardown().await?;
    Ok(())
}
