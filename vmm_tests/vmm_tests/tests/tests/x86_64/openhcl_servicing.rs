// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Integration tests for x86_64 OpenHCL servicing.

use disk_backend_resources::LayeredDiskHandle;
use disk_backend_resources::layer::RamDiskLayerHandle;
use hvlite_defs::config::DeviceVtl;
use petri::OpenHclServicingFlags;
use petri::ResolvedArtifact;
use petri::openvmm::PetriVmConfigOpenVmm;
use petri::pipette::cmd;
use petri_artifacts_vmm_test::artifacts::openhcl_igvm::LATEST_LINUX_DIRECT_TEST_X64;
use scsidisk_resources::SimpleScsiDiskHandle;
use storvsp_resources::ScsiControllerHandle;
use storvsp_resources::ScsiDeviceAndPath;
use storvsp_resources::ScsiPath;
use vm_resource::IntoResource;
use vmm_core_defs::HaltReason;
use vmm_test_macros::openvmm_test;

async fn openhcl_servicing_core(
    config: PetriVmConfigOpenVmm,
    openhcl_cmdline: &str,
    new_openhcl: ResolvedArtifact<impl petri_artifacts_common::tags::IsOpenhclIgvm>,
    flags: OpenHclServicingFlags,
) -> anyhow::Result<()> {
    let (mut vm, agent) = config
        .with_openhcl_command_line(openhcl_cmdline)
        .run()
        .await?;

    agent.ping().await?;

    // Test that inspect serialization works with the old version.
    vm.test_inspect_openhcl().await?;

    vm.restart_openhcl(new_openhcl, flags).await?;

    agent.ping().await?;

    // Test that inspect serialization works with the new version.
    vm.test_inspect_openhcl().await?;

    agent.power_off().await?;
    assert_eq!(vm.wait_for_teardown().await?, HaltReason::PowerOff);

    Ok(())
}

/// Test servicing an OpenHCL VM from the current version to itself.
#[openvmm_test(openhcl_linux_direct_x64 [LATEST_LINUX_DIRECT_TEST_X64])]
async fn openhcl_servicing(
    config: PetriVmConfigOpenVmm,
    (igvm_file,): (ResolvedArtifact<impl petri_artifacts_common::tags::IsOpenhclIgvm>,),
) -> Result<(), anyhow::Error> {
    openhcl_servicing_core(config, "", igvm_file, OpenHclServicingFlags::default()).await
}

/// Test servicing an OpenHCL VM from the current version to itself
/// with VF keepalive support.
#[openvmm_test(openhcl_linux_direct_x64 [LATEST_LINUX_DIRECT_TEST_X64])]
async fn openhcl_servicing_keepalive(
    config: PetriVmConfigOpenVmm,
    (igvm_file,): (ResolvedArtifact<impl petri_artifacts_common::tags::IsOpenhclIgvm>,),
) -> Result<(), anyhow::Error> {
    openhcl_servicing_core(
        config,
        "OPENHCL_ENABLE_VTL2_GPA_POOL=512 OPENHCL_SIDECAR=off", // disable sidecar until #1345 is fixed
        igvm_file,
        OpenHclServicingFlags {
            enable_nvme_keepalive: true,
        },
    )
    .await
}

#[openvmm_test(openhcl_linux_direct_x64 [LATEST_LINUX_DIRECT_TEST_X64])]
async fn openhcl_servicing_shutdown_ic(
    config: PetriVmConfigOpenVmm,
    (igvm_file,): (ResolvedArtifact<impl petri_artifacts_common::tags::IsOpenhclIgvm>,),
) -> Result<(), anyhow::Error> {
    let (mut vm, agent) = config
        .with_vmbus_redirect()
        .with_custom_config(|c| {
            // Add a disk so that we can make sure (non-intercepted) relay
            // channels are also functional.
            c.vmbus_devices.push((
                DeviceVtl::Vtl0,
                ScsiControllerHandle {
                    instance_id: guid::Guid::new_random(),
                    max_sub_channel_count: 1,
                    devices: vec![ScsiDeviceAndPath {
                        path: ScsiPath {
                            path: 0,
                            target: 0,
                            lun: 0,
                        },
                        device: SimpleScsiDiskHandle {
                            disk: LayeredDiskHandle::single_layer(RamDiskLayerHandle {
                                len: Some(256 * 1024),
                            })
                            .into_resource(),
                            read_only: false,
                            parameters: Default::default(),
                        }
                        .into_resource(),
                    }],
                    io_queue_depth: None,
                    requests: None,
                }
                .into_resource(),
            ));
        })
        .run()
        .await?;
    agent.ping().await?;
    let sh = agent.unix_shell();

    // Make sure the disk showed up.
    cmd!(sh, "ls /dev/sda").run().await?;

    let shutdown_ic = vm.wait_for_enlightened_shutdown_ready().await?;
    vm.restart_openhcl(igvm_file, OpenHclServicingFlags::default())
        .await?;
    // VTL2 will disconnect and then reconnect the shutdown IC across a servicing event.
    tracing::info!("waiting for shutdown IC to close");
    shutdown_ic.await.unwrap_err();
    vm.wait_for_enlightened_shutdown_ready().await?;

    // Make sure the VTL0 disk is still present by reading it.
    agent.read_file("/dev/sda").await?;

    vm.send_enlightened_shutdown(petri::ShutdownKind::Shutdown)
        .await?;
    assert_eq!(vm.wait_for_teardown().await?, HaltReason::PowerOff);
    Ok(())
}

// TODO: add tests with guest workloads while doing servicing.
// TODO: add tests from previous release branch to current.
