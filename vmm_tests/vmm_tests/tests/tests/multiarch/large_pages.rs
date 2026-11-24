// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Tests for Windows large-page (2 MB SLAT) guest RAM backing.

use anyhow::Context;
use petri::MemoryConfig;
use petri::PetriVmBuilder;
use petri::openvmm::OpenVmmPetriBackend;
use vmm_test_macros::openvmm_test;

/// Verify that backing OpenVMM guest RAM with 2 MB large pages on Windows (WHP)
/// actually produces 2 MB SLAT (nested page table) entries.
///
/// This boots a guest with `hugepages` enabled (a `SEC_LARGE_PAGES` section on
/// Windows), which forces `prefetch` on so the guest RAM mappings are
/// pre-populated in the SLAT at startup, then reads the WHP partition memory
/// counters through the OpenVMM inspect tree and asserts that all guest memory
/// has been backed by 2 MB pages.
///
/// Requires the "Lock pages in memory" privilege (`SeLockMemoryPrivilege`) so
/// that the large-page section allocation succeeds; the test fails (rather than
/// skips) if large-page backing is unavailable, per design.
#[openvmm_test(linux_direct_x64, linux_direct_aarch64)]
async fn whp_large_pages_slat(config: PetriVmBuilder<OpenVmmPetriBackend>) -> anyhow::Result<()> {
    const HUGEPAGE_SIZE: u64 = 2 * 1024 * 1024;
    const GIGAPAGE_SIZE: u64 = 1024 * 1024 * 1024;
    // 1 GiB of guest RAM, an exact multiple of the 2 MB hugepage size.
    const STARTUP_BYTES: u64 = 512 * HUGEPAGE_SIZE;

    let (vm, agent) = config
        .with_memory(MemoryConfig {
            startup_bytes: STARTUP_BYTES,
            ..Default::default()
        })
        .modify_backend(|b| b.with_hugepages(None))
        .run()
        .await?;

    let node = vm.inspect_vmm(SLAT_INSPECT_PATH).await?;
    let (mapped_2m, mapped_1g) =
        read_slat_counters(&node).context("could not read WHP SLAT counters from inspect tree")?;
    tracing::info!(mapped_2m, mapped_1g, "WHP SLAT page counts");

    // Count large SLAT entries in units of 2 MB pages (each 1 GB entry covers
    // 512). It won't exactly equal guest RAM / 2 MB: the guest places a few 4 KB
    // hypervisor overlay pages (hypercall, SynIC, monitor, etc.) inside RAM,
    // each demoting its containing 2 MB region to 4 KB entries. Allow a small
    // tolerance; a real regression would drop the count by hundreds.
    const MAX_OVERLAY_DEMOTIONS: u64 = 16;
    let expected_2m = STARTUP_BYTES / HUGEPAGE_SIZE;
    let mapped_2m_equiv = mapped_2m + mapped_1g * (GIGAPAGE_SIZE / HUGEPAGE_SIZE);
    tracing::info!(mapped_2m_equiv, expected_2m, "large-page SLAT coverage");
    assert!(
        mapped_2m_equiv >= expected_2m - MAX_OVERLAY_DEMOTIONS,
        "large-page backing: got {mapped_2m_equiv} 2 MB pages, expected ~{expected_2m}"
    );

    agent.power_off().await?;
    vm.wait_for_clean_teardown().await?;
    Ok(())
}

/// The inspect path to the WHP partition SLAT (nested page table) mapping
/// counters for VTL0. `linux_direct` is not an OpenHCL config, so only the
/// VTL0 partition exists.
const SLAT_INSPECT_PATH: &str = "partition/vtl0/memory";

/// Read the `(mapped_2m, mapped_1g)` counters from the `memory` inspect node
/// (as returned by inspecting [`SLAT_INSPECT_PATH`]).
///
/// Fails if the expected fields are missing, including when WHP could not
/// retrieve the memory counter set and therefore omitted them from inspect.
fn read_slat_counters(node: &inspect::Node) -> anyhow::Result<(u64, u64)> {
    let json: serde_json::Value = serde_json::from_str(&node.json().to_string())
        .context("failed to parse inspect output as JSON")?;
    let mapped_2m = json["mapped_2m"]
        .as_u64()
        .context("memory.mapped_2m missing or not an integer")?;
    let mapped_1g = json["mapped_1g"]
        .as_u64()
        .context("memory.mapped_1g missing or not an integer")?;
    Ok((mapped_2m, mapped_1g))
}
