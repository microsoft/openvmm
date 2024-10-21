// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Integration tests for aarch64 guests.

use petri::PetriVmConfig;
use vmm_core_defs::HaltReason;
use vmm_test_macros::vmm_test;

/// Boot through the UEFI firmware, it will shut itself down after booting.
#[vmm_test(uefi_aarch64(none))]
async fn frontpage(config: PetriVmConfig) -> anyhow::Result<()> {
    let mut vm = config.with_single_processor().run_without_agent().await?;
    vm.wait_for_successful_boot_event().await?;
    assert_eq!(vm.wait_for_teardown().await?, HaltReason::PowerOff);
    Ok(())
}
