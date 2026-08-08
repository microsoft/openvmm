// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Save/restore integration tests that exercise OpenVMM's snapshot machinery.

use petri::PetriVmBuilder;
use petri::openvmm::OpenVmmPetriBackend;
use vmm_test_macros::openvmm_test;

/// Regression test for the UEFI-resolver bug on cross-process snapshot restore.
///
/// The in-process pulse (`verify_save_restore`) restores into the *same*
/// worker, whose UEFI resolvers were already registered during cold boot, so it
/// cannot reproduce the bug where restoring a UEFI snapshot onto a *fresh*
/// worker (`load_mode = None`, like `openvmm --restore-snapshot`) panicked with
/// `no resolver for uefi_logger:platform`. This test drives that real
/// cross-process restore path.
#[openvmm_test(unstable(
    reason = "cross-process restore is new infrastructure; gate until it has soaked in CI",
    uefi_x64(vhd(alpine_3_23_x64))
))]
async fn uefi_cross_process_restore(
    config: PetriVmBuilder<OpenVmmPetriBackend>,
) -> anyhow::Result<()> {
    config.verify_openvmm_cross_process_restore().await
}
