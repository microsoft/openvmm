// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Boot the SEV-SNP Linux IGVM with virtio-vsock pipette on MSHV.

use petri::MemoryConfig;
use petri::PetriVmBuilder;
use petri::ProcessorTopology;
use petri::openvmm::OpenVmmPetriBackend;
use petri::pipette::cmd;
use vmm_test_macros::vmm_test_with;

#[vmm_test_with(openvmm, configs(linux_direct_x64[snp]))]
async fn boot_1vp(config: PetriVmBuilder<OpenVmmPetriBackend>) -> anyhow::Result<()> {
    boot_with_vps(config, 1).await
}

#[vmm_test_with(openvmm, configs(linux_direct_x64[snp]))]
async fn boot_2vp(config: PetriVmBuilder<OpenVmmPetriBackend>) -> anyhow::Result<()> {
    boot_with_vps(config, 2).await
}

async fn boot_with_vps(
    config: PetriVmBuilder<OpenVmmPetriBackend>,
    vp_count: u32,
) -> anyhow::Result<()> {
    let (vm, agent) = config
        .with_processor_topology(ProcessorTopology {
            vp_count,
            vps_per_socket: Some(vp_count),
            ..Default::default()
        })
        .with_memory(MemoryConfig {
            startup_bytes: 160 * petri::SIZE_1_MB,
            ..Default::default()
        })
        // The ECAM stays at its default placement above 4 GiB. Linux accepts
        // it because the shim boots the EFI stub, so the kernel reads the
        // SMBIOS BIOS date.
        .modify_backend(|b| b.with_pcie_root_topology(1, 1, 1))
        .run()
        .await?;

    let shell = agent.unix_shell();
    let dmesg = cmd!(shell, "dmesg").read().await?;
    for marker in [
        "SEV: Status: SEV SEV-ES SEV-SNP".to_string(),
        "x2apic enabled".to_string(),
        format!("smp: Brought up 1 node, {vp_count} CPU"),
        // The shim's UEFI environment booted the kernel's EFI stub.
        "by OpenVMM".to_string(),
        "PCI: ECAM [mem ".to_string(),
    ] {
        anyhow::ensure!(dmesg.contains(&marker), "guest dmesg missing {marker}");
    }
    anyhow::ensure!(
        !dmesg.contains("above 4GB, ignored"),
        "guest ignored the PCIe ECAM above 4 GiB"
    );

    let bios_date = cmd!(shell, "cat /sys/class/dmi/id/bios_date")
        .read()
        .await?;
    anyhow::ensure!(
        bios_date.trim() == "06/19/2026",
        "unexpected SMBIOS BIOS date: {bios_date:?}"
    );
    // The image leaves most RAM unaccepted, and Linux accepts it lazily.
    let meminfo = cmd!(shell, "cat /proc/meminfo").read().await?;
    anyhow::ensure!(
        meminfo.contains("Unaccepted:"),
        "guest does not report unaccepted memory"
    );

    let output = cmd!(shell, "echo petri-snp-ok").read().await?;
    anyhow::ensure!(
        output.trim() == "petri-snp-ok",
        "unexpected shell output: {output:?}"
    );

    vm.teardown().await
}
