// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Integration tests for OpenHCL servicing.
//! OpenHCL servicing is supported on x86-64 and aarch64.
//! For x86-64, it is supported using both Hyper-V and OpenVMM.
//! For aarch64, it is supported using Hyper-V.

use disk_backend_resources::LayeredDiskHandle;
use disk_backend_resources::layer::RamDiskLayerHandle;
use guid::Guid;
use hvlite_defs::config::DeviceVtl;
use hvlite_defs::config::VpciDeviceConfig;
use mesh::CancelContext;
use mesh::CellUpdater;
use mesh::rpc::RpcSend;
use nvme_resources::NamespaceDefinition;
use nvme_resources::NvmeFaultControllerHandle;
use nvme_resources::fault::AdminQueueFaultConfig;
use nvme_resources::fault::FaultConfiguration;
use nvme_resources::fault::NamespaceChange;
use nvme_resources::fault::NamespaceFaultConfig;
use nvme_resources::fault::QueueFaultBehavior;
use nvme_test::command_match::CommandMatchBuilder;
use petri::OpenHclServicingFlags;
use petri::PetriGuestStateLifetime;
use petri::PetriVmBuilder;
use petri::PetriVmmBackend;
use petri::ResolvedArtifact;
use petri::openvmm::OpenVmmPetriBackend;
use petri::pipette::cmd;
use petri::vtl2_settings::ControllerType;
use petri::vtl2_settings::Vtl2LunBuilder;
use petri::vtl2_settings::Vtl2StorageBackingDeviceBuilder;
use petri::vtl2_settings::Vtl2StorageControllerBuilder;
#[allow(unused_imports)]
use petri_artifacts_vmm_test::artifacts::openhcl_igvm::LATEST_LINUX_DIRECT_TEST_X64;
#[allow(unused_imports)]
use petri_artifacts_vmm_test::artifacts::openhcl_igvm::LATEST_STANDARD_AARCH64;
#[allow(unused_imports)]
use petri_artifacts_vmm_test::artifacts::openhcl_igvm::LATEST_STANDARD_X64;
#[allow(unused_imports)]
use petri_artifacts_vmm_test::artifacts::openhcl_igvm::RELEASE_25_05_LINUX_DIRECT_X64;
#[allow(unused_imports)]
use petri_artifacts_vmm_test::artifacts::openhcl_igvm::RELEASE_25_05_STANDARD_AARCH64;
use pipette_client::PipetteClient;
use scsidisk_resources::SimpleScsiDiskHandle;
use std::time::Duration;
use storvsp_resources::ScsiControllerHandle;
use storvsp_resources::ScsiDeviceAndPath;
use storvsp_resources::ScsiPath;
use vm_resource::IntoResource;
use vmm_test_macros::openvmm_test;
use vmm_test_macros::vmm_test;
use zerocopy::IntoBytes;

const DEFAULT_SERVICING_COUNT: u8 = 3;
const KEEPALIVE_VTL2_NSID: u32 = 37; // Pick any namespace ID as long as it doesn't conflict with other namespaces in the controller

async fn openhcl_servicing_core<T: PetriVmmBackend>(
    config: PetriVmBuilder<T>,
    openhcl_cmdline: &str,
    new_openhcl: ResolvedArtifact<impl petri_artifacts_common::tags::IsOpenhclIgvm>,
    flags: OpenHclServicingFlags,
    servicing_count: u8,
) -> anyhow::Result<()> {
    let (mut vm, agent) = config
        .with_openhcl_command_line(openhcl_cmdline)
        .run()
        .await?;

    for _ in 0..servicing_count {
        agent.ping().await?;

        // Test that inspect serialization works with the old version.
        vm.test_inspect_openhcl().await?;

        vm.restart_openhcl(new_openhcl.clone(), flags).await?;

        agent.ping().await?;

        // Test that inspect serialization works with the new version.
        vm.test_inspect_openhcl().await?;
    }

    agent.power_off().await?;
    vm.wait_for_clean_teardown().await?;

    Ok(())
}

/// Test servicing an OpenHCL VM from the current version to itself.
///
/// N.B. These Hyper-V tests fail in CI for x64. Tracked by #1652.
#[vmm_test(
    openvmm_openhcl_linux_direct_x64 [LATEST_LINUX_DIRECT_TEST_X64],
    //hyperv_openhcl_uefi_x64(vhd(ubuntu_2504_server_x64))[LATEST_STANDARD_X64],
    hyperv_openhcl_uefi_aarch64(vhd(ubuntu_2404_server_aarch64))[LATEST_STANDARD_AARCH64]
)]
async fn basic_servicing<T: PetriVmmBackend>(
    config: PetriVmBuilder<T>,
    (igvm_file,): (ResolvedArtifact<impl petri_artifacts_common::tags::IsOpenhclIgvm>,),
) -> anyhow::Result<()> {
    openhcl_servicing_core(
        config,
        "",
        igvm_file,
        OpenHclServicingFlags {
            override_version_checks: true,
            ..Default::default()
        },
        DEFAULT_SERVICING_COUNT,
    )
    .await
}

/// Test servicing an OpenHCL VM from the current version to itself
/// with NVMe keepalive support and no vmbus redirect.
#[openvmm_test(openhcl_linux_direct_x64[LATEST_LINUX_DIRECT_TEST_X64])]
async fn servicing_keepalive_no_device<T: PetriVmmBackend>(
    config: PetriVmBuilder<T>,
    (igvm_file,): (ResolvedArtifact<impl petri_artifacts_common::tags::IsOpenhclIgvm>,),
) -> anyhow::Result<()> {
    openhcl_servicing_core(
        config,
        "OPENHCL_ENABLE_VTL2_GPA_POOL=512",
        igvm_file,
        OpenHclServicingFlags {
            enable_nvme_keepalive: true,
            ..Default::default()
        },
        DEFAULT_SERVICING_COUNT,
    )
    .await
}

/// Test servicing an OpenHCL VM from the current version to itself
/// with NVMe keepalive support.
#[openvmm_test(openhcl_uefi_x64[nvme](vhd(ubuntu_2504_server_x64))[LATEST_STANDARD_X64])]
async fn servicing_keepalive_with_device<T: PetriVmmBackend>(
    config: PetriVmBuilder<T>,
    (igvm_file,): (ResolvedArtifact<impl petri_artifacts_common::tags::IsOpenhclIgvm>,),
) -> anyhow::Result<()> {
    openhcl_servicing_core(
        config.with_vmbus_redirect(true), // Need this to attach the NVMe device
        "OPENHCL_ENABLE_VTL2_GPA_POOL=512",
        igvm_file,
        OpenHclServicingFlags {
            enable_nvme_keepalive: true,
            ..Default::default()
        },
        1, // Test is slow with NVMe device, so only do one loop to avoid timeout
    )
    .await
}

#[vmm_test(
    openvmm_openhcl_linux_direct_x64 [LATEST_LINUX_DIRECT_TEST_X64, RELEASE_25_05_LINUX_DIRECT_X64],
    hyperv_openhcl_uefi_aarch64(vhd(ubuntu_2404_server_aarch64))[RELEASE_25_05_STANDARD_AARCH64, LATEST_STANDARD_AARCH64]
)]
async fn servicing_upgrade<T: PetriVmmBackend>(
    config: PetriVmBuilder<T>,
    (to_igvm, from_igvm): (
        ResolvedArtifact<impl petri_artifacts_common::tags::IsOpenhclIgvm>,
        ResolvedArtifact<impl petri_artifacts_common::tags::IsOpenhclIgvm>,
    ),
) -> anyhow::Result<()> {
    // TODO: remove .with_guest_state_lifetime(PetriGuestStateLifetime::Disk). The default (ephemeral) does not exist in the 2505 release.
    openhcl_servicing_core(
        config
            .with_custom_openhcl(from_igvm)
            .with_guest_state_lifetime(PetriGuestStateLifetime::Disk),
        "",
        to_igvm,
        OpenHclServicingFlags::default(),
        DEFAULT_SERVICING_COUNT,
    )
    .await
}

#[vmm_test(
    openvmm_openhcl_linux_direct_x64 [RELEASE_25_05_LINUX_DIRECT_X64, LATEST_LINUX_DIRECT_TEST_X64],
    hyperv_openhcl_uefi_aarch64(vhd(ubuntu_2404_server_aarch64))[RELEASE_25_05_STANDARD_AARCH64, LATEST_STANDARD_AARCH64]
)]
async fn servicing_downgrade<T: PetriVmmBackend>(
    config: PetriVmBuilder<T>,
    (to_igvm, from_igvm): (
        ResolvedArtifact<impl petri_artifacts_common::tags::IsOpenhclIgvm>,
        ResolvedArtifact<impl petri_artifacts_common::tags::IsOpenhclIgvm>,
    ),
) -> anyhow::Result<()> {
    // TODO: remove .with_guest_state_lifetime(PetriGuestStateLifetime::Disk). The default (ephemeral) does not exist in the 2505 release.
    openhcl_servicing_core(
        config
            .with_custom_openhcl(from_igvm)
            .with_guest_state_lifetime(PetriGuestStateLifetime::Disk),
        "",
        to_igvm,
        OpenHclServicingFlags::default(),
        DEFAULT_SERVICING_COUNT,
    )
    .await
}

#[openvmm_test(openhcl_linux_direct_x64 [LATEST_LINUX_DIRECT_TEST_X64])]
async fn servicing_shutdown_ic(
    config: PetriVmBuilder<OpenVmmPetriBackend>,
    (igvm_file,): (ResolvedArtifact<impl petri_artifacts_common::tags::IsOpenhclIgvm>,),
) -> anyhow::Result<()> {
    let (mut vm, agent) = config
        .with_vmbus_redirect(true)
        .modify_backend(move |b| {
            b.with_custom_config(|c| {
                // Add a disk so that we can make sure (non-intercepted) relay
                // channels are also functional.
                c.vmbus_devices.push((
                    DeviceVtl::Vtl0,
                    ScsiControllerHandle {
                        instance_id: Guid::new_random(),
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
                        poll_mode_queue_depth: None,
                    }
                    .into_resource(),
                ));
            })
        })
        .run()
        .await?;
    agent.ping().await?;
    let sh = agent.unix_shell();

    // Make sure the disk showed up.
    cmd!(sh, "ls /dev/sda").run().await?;

    let shutdown_ic = vm.backend().wait_for_enlightened_shutdown_ready().await?;
    vm.restart_openhcl(igvm_file, OpenHclServicingFlags::default())
        .await?;
    // VTL2 will disconnect and then reconnect the shutdown IC across a servicing event.
    tracing::info!("waiting for shutdown IC to close");
    shutdown_ic.await.unwrap_err();
    vm.backend().wait_for_enlightened_shutdown_ready().await?;

    // Make sure the VTL0 disk is still present by reading it.
    agent.read_file("/dev/sda").await?;

    vm.send_enlightened_shutdown(petri::ShutdownKind::Shutdown)
        .await?;
    vm.wait_for_clean_teardown().await?;
    Ok(())
}

// TODO: add tests with guest workloads while doing servicing.
// TODO: add tests from previous release branch to current.

/// Updates the namespace during servicing and verifies rescan events after servicing.
#[openvmm_test(openhcl_linux_direct_x64 [LATEST_LINUX_DIRECT_TEST_X64])]
async fn servicing_keepalive_with_namespace_update(
    config: PetriVmBuilder<OpenVmmPetriBackend>,
    (igvm_file,): (ResolvedArtifact<impl petri_artifacts_common::tags::IsOpenhclIgvm>,),
) -> Result<(), anyhow::Error> {
    let mut fault_start_updater = CellUpdater::new(false);
    let (ns_change_send, ns_change_recv) = mesh::channel::<NamespaceChange>();
    let (aer_verify_send, aer_verify_recv) = mesh::oneshot::<()>();
    let (log_verify_send, log_verify_recv) = mesh::oneshot::<()>();

    let fault_configuration = FaultConfiguration::new(fault_start_updater.cell())
        .with_namespace_fault(NamespaceFaultConfig::new(ns_change_recv))
        .with_admin_queue_fault(
            AdminQueueFaultConfig::new()
                .with_submission_queue_fault(
                    CommandMatchBuilder::new()
                        .match_cdw0_opcode(nvme_spec::AdminOpcode::ASYNCHRONOUS_EVENT_REQUEST.0)
                        .build(),
                    QueueFaultBehavior::Verify(Some(aer_verify_send)),
                )
                .with_submission_queue_fault(
                    CommandMatchBuilder::new()
                        .match_cdw0_opcode(nvme_spec::AdminOpcode::GET_LOG_PAGE.0)
                        .build(),
                    QueueFaultBehavior::Verify(Some(log_verify_send)),
                ),
        );

    let (mut vm, agent) = create_keepalive_test_config(config, fault_configuration).await?;

    agent.ping().await?;
    let sh = agent.unix_shell();

    // Make sure the disk showed up.
    cmd!(sh, "ls /dev/sda").run().await?;

    fault_start_updater.set(true).await;
    vm.save_openhcl(
        igvm_file.clone(),
        OpenHclServicingFlags {
            enable_nvme_keepalive: true,
            ..Default::default()
        },
    )
    .await?;
    ns_change_send
        .call(NamespaceChange::ChangeNotification, KEEPALIVE_VTL2_NSID)
        .await?;
    vm.restore_openhcl().await?;

    let _ = CancelContext::new()
        .with_timeout(Duration::from_secs(10))
        .until_cancelled(aer_verify_recv)
        .await
        .expect("AER command was not observed within 10 seconds of vm restore after servicing with namespace change");

    let _ = CancelContext::new()
        .with_timeout(Duration::from_secs(10))
        .until_cancelled(log_verify_recv)
        .await
        .expect("GET_LOG_PAGE command was not observed within 10 seconds of vm restore after servicing with namespace change");

    fault_start_updater.set(false).await;
    agent.ping().await?;

    Ok(())
}

/// Test servicing an OpenHCL VM from the current version to itself
/// with NVMe keepalive support and a faulty controller that drops CREATE_IO_COMPLETION_QUEUE commands
#[openvmm_test(openhcl_linux_direct_x64 [LATEST_LINUX_DIRECT_TEST_X64])]
async fn servicing_keepalive_with_nvme_fault(
    config: PetriVmBuilder<OpenVmmPetriBackend>,
    (igvm_file,): (ResolvedArtifact<impl petri_artifacts_common::tags::IsOpenhclIgvm>,),
) -> Result<(), anyhow::Error> {
    let mut fault_start_updater = CellUpdater::new(false);

    let fault_configuration = FaultConfiguration::new(fault_start_updater.cell())
        .with_admin_queue_fault(
            AdminQueueFaultConfig::new().with_submission_queue_fault(
                CommandMatchBuilder::new().match_cdw0_opcode(nvme_spec::AdminOpcode::CREATE_IO_COMPLETION_QUEUE.0).build(),
                QueueFaultBehavior::Panic("Received a CREATE_IO_COMPLETION_QUEUE command during servicing with keepalive enabled. THERE IS A BUG SOMEWHERE.".to_string()),
            ),
        );

    apply_fault_with_keepalive(config, fault_configuration, fault_start_updater, igvm_file).await
}

/// Test servicing an OpenHCL VM from the current version to itself
/// with NVMe keepalive support and a faulty controller that panics when
/// IDENTIFY commands are received. This verifies namespace save/restore functionality.
#[openvmm_test(openhcl_linux_direct_x64 [LATEST_LINUX_DIRECT_TEST_X64])]
async fn servicing_keepalive_with_nvme_namespace_fault(
    config: PetriVmBuilder<OpenVmmPetriBackend>,
    (igvm_file,): (ResolvedArtifact<impl petri_artifacts_common::tags::IsOpenhclIgvm>,),
) -> Result<(), anyhow::Error> {
    let mut fault_start_updater = CellUpdater::new(false);

    let fault_configuration = FaultConfiguration::new(fault_start_updater.cell())
        .with_admin_queue_fault(
            AdminQueueFaultConfig::new().with_submission_queue_fault(
                CommandMatchBuilder::new().match_cdw0_opcode(nvme_spec::AdminOpcode::IDENTIFY.0).match_cdw10(nvme_spec::Cdw10Identify::new().with_cns(nvme_spec::Cns::NAMESPACE.0).into(), nvme_spec::Cdw10Identify::new().with_cns(u8::MAX).into()).build(),
                QueueFaultBehavior::Panic("Received an IDENTIFY command during servicing with keepalive enabled (And no namespaces were updated). THERE IS A BUG SOMEWHERE.".to_string()),
            ),
        );

    apply_fault_with_keepalive(config, fault_configuration, fault_start_updater, igvm_file).await
}

/// Verifies that the driver awaits an existing AER instead of issuing a new one after servicing.
#[openvmm_test(openhcl_linux_direct_x64 [LATEST_LINUX_DIRECT_TEST_X64])]
async fn servicing_keepalive_verify_no_duplicate_aers(
    config: PetriVmBuilder<OpenVmmPetriBackend>,
    (igvm_file,): (ResolvedArtifact<impl petri_artifacts_common::tags::IsOpenhclIgvm>,),
) -> Result<(), anyhow::Error> {
    let mut fault_start_updater = CellUpdater::new(false);

    let fault_configuration = FaultConfiguration::new(fault_start_updater.cell())
        .with_admin_queue_fault(
            AdminQueueFaultConfig::new().with_submission_queue_fault(
                CommandMatchBuilder::new().match_cdw0_opcode(nvme_spec::AdminOpcode::ASYNCHRONOUS_EVENT_REQUEST.0).build(),
                QueueFaultBehavior::Panic("Received a duplicate ASYNCHRONOUS_EVENT_REQUEST command during servicing with keepalive enabled. THERE IS A BUG SOMEWHERE.".to_string()),
            )
        );

    apply_fault_with_keepalive(config, fault_configuration, fault_start_updater, igvm_file).await
}

/// Test servicing an OpenHCL VM from the current version to itself with NVMe keepalive support
/// and a faulty controller that responds incorrectly to the IDENTIFY:NAMESPACE command after servicing.
/// TODO: For now this test will succeed because the driver currently requeries the namespace size and only checks that the size is non-zero.
/// Once AER support is added to the driver the checks will be more stringent and this test will need updating
#[openvmm_test(openhcl_linux_direct_x64 [LATEST_LINUX_DIRECT_TEST_X64])]
async fn servicing_keepalive_with_nvme_identify_fault(
    config: PetriVmBuilder<OpenVmmPetriBackend>,
    (igvm_file,): (ResolvedArtifact<impl petri_artifacts_common::tags::IsOpenhclIgvm>,),
) -> Result<(), anyhow::Error> {
    let mut fault_start_updater = CellUpdater::new(false);

    // The first 8bytes of the response buffer correspond to the nsze field of the Identify Namespace data structure.
    // Reduce the reported size of the namespace to 256 blocks instead of the original 512.
    let mut buf: u64 = 256;
    let buf = buf.as_mut_bytes();
    let fault_configuration = FaultConfiguration::new(fault_start_updater.cell())
        .with_admin_queue_fault(
            AdminQueueFaultConfig::new().with_completion_queue_fault(
                CommandMatchBuilder::new()
                    .match_cdw0_opcode(nvme_spec::AdminOpcode::IDENTIFY.0)
                    .match_cdw10(
                        nvme_spec::Cdw10Identify::new()
                            .with_cns(nvme_spec::Cns::NAMESPACE.0)
                            .into(),
                        nvme_spec::Cdw10Identify::new().with_cns(u8::MAX).into(),
                    )
                    .build(),
                QueueFaultBehavior::CustomPayload(buf.to_vec()),
            ),
        );

    apply_fault_with_keepalive(config, fault_configuration, fault_start_updater, igvm_file).await
}

async fn apply_fault_with_keepalive(
    config: PetriVmBuilder<OpenVmmPetriBackend>,
    fault_configuration: FaultConfiguration,
    mut fault_start_updater: CellUpdater<bool>,
    igvm_file: ResolvedArtifact<impl petri_artifacts_common::tags::IsOpenhclIgvm>,
) -> Result<(), anyhow::Error> {
    let (mut vm, agent) = create_keepalive_test_config(config, fault_configuration).await?;

    agent.ping().await?;
    let sh = agent.unix_shell();

    // Make sure the disk showed up.
    cmd!(sh, "ls /dev/sda").run().await?;

    fault_start_updater.set(true).await;
    vm.restart_openhcl(
        igvm_file.clone(),
        OpenHclServicingFlags {
            enable_nvme_keepalive: true,
            ..Default::default()
        },
    )
    .await?;

    fault_start_updater.set(false).await;
    agent.ping().await?;

    Ok(())
}

async fn create_keepalive_test_config(
    config: PetriVmBuilder<OpenVmmPetriBackend>,
    fault_configuration: FaultConfiguration,
) -> Result<(petri::PetriVm<OpenVmmPetriBackend>, PipetteClient), anyhow::Error> {
    const NVME_INSTANCE: Guid = guid::guid!("dce4ebad-182f-46c0-8d30-8446c1c62ab3");
    let vtl0_nvme_lun = 1;
    let scsi_instance = Guid::new_random();

    config
        .with_vmbus_redirect(true)
        .with_openhcl_command_line("OPENHCL_ENABLE_VTL2_GPA_POOL=512")
        .modify_backend(move |b| {
            b.with_custom_config(|c| {
                // Add a fault controller to test the nvme controller functionality
                c.vpci_devices.push(VpciDeviceConfig {
                    vtl: DeviceVtl::Vtl2,
                    instance_id: NVME_INSTANCE,
                    resource: NvmeFaultControllerHandle {
                        subsystem_id: Guid::new_random(),
                        msix_count: 10,
                        max_io_queues: 10,
                        namespaces: vec![NamespaceDefinition {
                            nsid: KEEPALIVE_VTL2_NSID,
                            read_only: false,
                            disk: LayeredDiskHandle::single_layer(RamDiskLayerHandle {
                                len: Some(256 * 1024),
                            })
                            .into_resource(),
                        }],
                        fault_config: fault_configuration,
                    }
                    .into_resource(),
                })
            })
            // Assign the fault controller to VTL2
            .with_custom_vtl2_settings(|v| {
                v.dynamic.as_mut().unwrap().storage_controllers.push(
                    Vtl2StorageControllerBuilder::scsi()
                        .with_instance_id(scsi_instance)
                        .add_lun(
                            Vtl2LunBuilder::disk()
                                .with_location(vtl0_nvme_lun)
                                .with_physical_device(Vtl2StorageBackingDeviceBuilder::new(
                                    ControllerType::Nvme,
                                    NVME_INSTANCE,
                                    KEEPALIVE_VTL2_NSID,
                                )),
                        )
                        .build(),
                );
            })
        })
        .run()
        .await
}
