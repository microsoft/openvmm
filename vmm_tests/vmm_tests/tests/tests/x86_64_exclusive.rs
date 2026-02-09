// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Integration tests for x86_64 guests.

use openvmm_defs::config::ArchTopologyConfig;
use openvmm_defs::config::ProcessorTopologyConfig;
use openvmm_defs::config::X2ApicConfig;
use openvmm_defs::config::X86TopologyConfig;
use petri::PetriVmBuilder;
use petri::openvmm::OpenVmmPetriBackend;
use pipette_client::cmd;
use vmm_test_macros::openvmm_test;

/// Validate we can run with VP index != APIC ID.
#[openvmm_test(linux_direct_x64)]
async fn apicid_offset(config: PetriVmBuilder<OpenVmmPetriBackend>) -> Result<(), anyhow::Error> {
    let (vm, agent) = config
        .modify_backend(|b| {
            b.with_custom_config(|c| {
                let Some(ArchTopologyConfig::X86(arch)) = &mut c.processor_topology.arch else {
                    unreachable!()
                };
                arch.apic_id_offset = 16;
            })
        })
        .run()
        .await?;

    agent.ping().await?;

    agent.power_off().await?;
    vm.wait_for_clean_teardown().await?;

    Ok(())
}

/// Boot Linux with legacy xapic with 2 VPs and apic_ids of 253 and 254, the maximum.
#[openvmm_test(linux_direct_x64)]
async fn legacy_xapic(config: PetriVmBuilder<OpenVmmPetriBackend>) -> Result<(), anyhow::Error> {
    let (vm, agent) = config
        .modify_backend(|b| {
            b.with_custom_config(|c| {
                c.processor_topology = ProcessorTopologyConfig {
                    proc_count: 2,
                    vps_per_socket: Some(1),
                    enable_smt: None,
                    arch: Some(ArchTopologyConfig::X86(X86TopologyConfig {
                        x2apic: X2ApicConfig::Unsupported,
                        apic_id_offset: 253,
                    })),
                }
            })
        })
        .run()
        .await?;

    let output = agent.unix_shell().read_file("/proc/cpuinfo").await?;
    // Validate that all cpus are present
    assert!(output.contains("processor\t: 0"));
    assert!(output.contains("apicid\t\t: 253"));
    assert!(output.contains("processor\t: 1"));
    assert!(output.contains("apicid\t\t: 254"));

    agent.power_off().await?;
    vm.wait_for_clean_teardown().await?;

    Ok(())
}

/// Boot Linux and have it dump MTRR related output.
#[openvmm_test(linux_direct_x64, openhcl_linux_direct_x64)]
async fn mtrrs(config: PetriVmBuilder<OpenVmmPetriBackend>) -> Result<(), anyhow::Error> {
    let (vm, agent) = config.run().await?;

    let sh = agent.unix_shell();
    // Read /proc before dmesg, as reading it can trigger more messages.
    let mtrr_output = sh.read_file("/proc/mtrr").await?;
    let dmesg_output = cmd!(sh, "dmesg").read().await?;

    agent.power_off().await?;
    vm.wait_for_clean_teardown().await?;

    // Validate that output does not contain any MTRR-related errors.
    // If all MTRR registers are zero we get this message.
    assert!(!dmesg_output.contains("CPU MTRRs all blank - virtualized system"));
    // If the BSP and APs have different MTRR values we get "your CPUs had inconsistent (fixed MTRR/variable MTRR/MTRRdefType) settings" messages.
    assert!(!dmesg_output.contains("your CPUs had inconsistent"));
    // If we misread the physical address size we can end up computing incorrect MTRR masks
    assert!(!dmesg_output.contains("your BIOS has configured an incorrect mask"));
    // The Linux kernel may also output general 'something is not right' messages, check for those too.
    assert!(!dmesg_output.contains("probably your BIOS does not setup all CPUs"));
    assert!(!dmesg_output.contains("corrected configuration"));
    assert!(!dmesg_output.contains("BIOS bug"));

    // Validate that the output contains MTRR enablement messages.
    //
    // TODO: these are only output if DEBUG is enabled for Linux's mtrr.c, which
    // it no longer is by default in newer kernel versions.
    // assert!(mtrr_output.contains("default type: uncachable"));
    // assert!(mtrr_output.contains("fixed ranges enabled"));
    // assert!(mtrr_output.contains("variable ranges enabled"));
    assert!(
        mtrr_output
            .contains("reg00: base=0x000000000 (    0MB), size=  128MB, count=1: write-back")
    );
    assert!(
        mtrr_output
            .contains("reg01: base=0x008000000 (  128MB), size= 4096MB, count=1: write-back")
    );

    Ok(())
}
