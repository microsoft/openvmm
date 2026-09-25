// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Boot the SEV-SNP Linux IGVM with virtio-vsock pipette on MSHV.

use petri::MemoryConfig;
use petri::PetriVmBuilder;
use petri::ProcessorTopology;
use petri::openvmm::OpenVmmPetriBackend;
use petri::pipette::cmd;
use vmm_test_macros::vmm_test_with;

#[vmm_test_with(openvmm, configs(snp_linux_direct_x64))]
async fn boot(config: PetriVmBuilder<OpenVmmPetriBackend>) -> anyhow::Result<()> {
    let (vm, agent) = config
        .with_processor_topology(ProcessorTopology {
            vp_count: 1,
            vps_per_socket: Some(1),
            ..Default::default()
        })
        .with_memory(MemoryConfig {
            startup_bytes: 160 * petri::SIZE_1_MB,
            ..Default::default()
        })
        .modify_backend(|b| {
            b.with_pcie_root_topology(1, 1, 1)
                .with_custom_config(|c| c.pcie_ecam_below_4gb = true)
        })
        .run()
        .await?;

    let shell = agent.unix_shell();
    let dmesg = cmd!(shell, "dmesg").read().await?;
    for marker in [
        "SEV: Status: SEV SEV-ES SEV-SNP",
        "x2apic enabled",
        "smp: Brought up 1 node, 1 CPU",
    ] {
        anyhow::ensure!(dmesg.contains(marker), "guest dmesg missing {marker}");
    }

    let output = cmd!(shell, "echo petri-snp-ok").read().await?;
    anyhow::ensure!(
        output.trim() == "petri-snp-ok",
        "unexpected shell output: {output:?}"
    );
    let kernel = cmd!(shell, "uname -r").read().await?;
    anyhow::ensure!(
        kernel.trim().starts_with("6.18.53"),
        "unexpected SNP guest kernel: {kernel:?}"
    );

    vm.teardown().await
}
