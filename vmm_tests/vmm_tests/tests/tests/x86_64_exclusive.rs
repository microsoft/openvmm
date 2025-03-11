// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Integration tests for x86_64 guests.

use hvlite_defs::config::ProcessorTopologyConfig;
use hvlite_defs::config::X2ApicConfig;
use hvlite_defs::config::X86TopologyConfig;
use petri::openvmm::PetriVmConfigOpenVmm;
use vmm_core_defs::HaltReason;
use vmm_test_macros::openvmm_test;

/// Validate we can run with VP index != APIC ID.
#[openvmm_test(linux_direct_x64)]
async fn apicid_offset(config: PetriVmConfigOpenVmm) -> Result<(), anyhow::Error> {
    let (vm, agent) = config
        .with_custom_config(|c| c.processor_topology.arch.apic_id_offset = 16)
        .run()
        .await?;

    agent.ping().await?;

    agent.power_off().await?;
    assert_eq!(vm.wait_for_teardown().await?, HaltReason::PowerOff);

    Ok(())
}

/// Boot Linux with legacy xapic with 2 VPs and apic_ids of 253 and 254, the maximum.
#[openvmm_test(linux_direct_x64)]
async fn legacy_xapic(config: PetriVmConfigOpenVmm) -> Result<(), anyhow::Error> {
    let (vm, agent) = config
        .with_custom_config(|c| {
            c.processor_topology = ProcessorTopologyConfig {
                proc_count: 2,
                vps_per_socket: Some(1),
                enable_smt: None,
                arch: X86TopologyConfig {
                    x2apic: X2ApicConfig::Unsupported,
                    apic_id_offset: 253,
                },
            }
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
    assert_eq!(vm.wait_for_teardown().await?, HaltReason::PowerOff);

    Ok(())
}
