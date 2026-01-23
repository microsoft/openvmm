// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Integration tests for OpenHCL servicing.
//! OpenHCL servicing is supported on x86-64 and aarch64.
//! For x86-64, it is supported using both Hyper-V and OpenVMM.
//! For aarch64, it is supported using Hyper-V.

use disk_backend_resources::LayeredDiskHandle;
use disk_backend_resources::layer::RamDiskLayerHandle;
use guid::Guid;
use mesh::CancelContext;
use mesh::CellUpdater;
use mesh::rpc::RpcSend;
use nvme_resources::NamespaceDefinition;
use nvme_resources::NvmeFaultControllerHandle;
use nvme_resources::fault::AdminQueueFaultBehavior;
use nvme_resources::fault::AdminQueueFaultConfig;
use nvme_resources::fault::FaultConfiguration;
use nvme_resources::fault::NamespaceChange;
use nvme_resources::fault::NamespaceFaultConfig;
use nvme_resources::fault::PciFaultBehavior;
use nvme_resources::fault::PciFaultConfig;
use nvme_test::command_match::CommandMatchBuilder;
use openvmm_defs::config::DeviceVtl;
use openvmm_defs::config::VpciDeviceConfig;
use petri::OpenHclServicingFlags;
use petri::PetriGuestStateLifetime;
use petri::PetriVm;
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
use petri_artifacts_vmm_test::artifacts::openhcl_igvm::LATEST_RELEASE_LINUX_DIRECT_X64;
#[allow(unused_imports)]
use petri_artifacts_vmm_test::artifacts::openhcl_igvm::LATEST_RELEASE_STANDARD_AARCH64;
#[allow(unused_imports)]
use petri_artifacts_vmm_test::artifacts::openhcl_igvm::LATEST_RELEASE_STANDARD_X64;
#[allow(unused_imports)]
use petri_artifacts_vmm_test::artifacts::openhcl_igvm::LATEST_STANDARD_AARCH64;
#[allow(unused_imports)]
use petri_artifacts_vmm_test::artifacts::openhcl_igvm::LATEST_STANDARD_X64;
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
    new_openhcl: ResolvedArtifact<impl petri_artifacts_common::tags::IsOpenhclIgvm>,
    flags: OpenHclServicingFlags,
    servicing_count: u8,
) -> anyhow::Result<()> {
    let (mut vm, agent) = config.run().await?;

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
#[vmm_test(
    openvmm_openhcl_linux_direct_x64 [LATEST_LINUX_DIRECT_TEST_X64],
    hyperv_openhcl_pcat_x64(vhd(ubuntu_2504_server_x64))[LATEST_STANDARD_X64],
    hyperv_openhcl_uefi_x64(vhd(ubuntu_2504_server_x64))[LATEST_STANDARD_X64],
    hyperv_openhcl_uefi_aarch64(vhd(ubuntu_2404_server_aarch64))[LATEST_STANDARD_AARCH64]
)]
async fn basic_servicing<T: PetriVmmBackend>(
    config: PetriVmBuilder<T>,
    (igvm_file,): (ResolvedArtifact<impl petri_artifacts_common::tags::IsOpenhclIgvm>,),
) -> anyhow::Result<()> {
    let mut flags = config.default_servicing_flags();
    flags.override_version_checks = true;
    openhcl_servicing_core(config, igvm_file, flags, DEFAULT_SERVICING_COUNT).await
}

/// Test servicing an OpenHCL VM from the current version to itself, with a tpm.
#[vmm_test(
    openvmm_openhcl_uefi_x64(vhd(ubuntu_2504_server_x64))[LATEST_STANDARD_X64],
    hyperv_openhcl_uefi_x64(vhd(ubuntu_2504_server_x64))[LATEST_STANDARD_X64],
    hyperv_openhcl_uefi_aarch64(vhd(ubuntu_2404_server_aarch64))[LATEST_STANDARD_AARCH64]
)]
async fn tpm_servicing<T: PetriVmmBackend>(
    config: PetriVmBuilder<T>,
    (igvm_file,): (ResolvedArtifact<impl petri_artifacts_common::tags::IsOpenhclIgvm>,),
) -> anyhow::Result<()> {
    let mut flags = config.default_servicing_flags();
    flags.override_version_checks = true;
    openhcl_servicing_core(
        config
            .with_tpm(true)
            .with_tpm_state_persistence(true)
            .with_guest_state_lifetime(PetriGuestStateLifetime::Disk),
        igvm_file,
        flags,
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
    let flags = config.default_servicing_flags();
    openhcl_servicing_core(
        config.with_openhcl_command_line("OPENHCL_ENABLE_VTL2_GPA_POOL=512"),
        igvm_file,
        flags,
        DEFAULT_SERVICING_COUNT,
    )
    .await
}

/// Test servicing an OpenHCL VM from the current version to itself
/// with NVMe keepalive support.
#[openvmm_test(openhcl_uefi_x64(vhd(ubuntu_2504_server_x64))[LATEST_STANDARD_X64])]
async fn servicing_keepalive_with_device<T: PetriVmmBackend>(
    config: PetriVmBuilder<T>,
    (igvm_file,): (ResolvedArtifact<impl petri_artifacts_common::tags::IsOpenhclIgvm>,),
) -> anyhow::Result<()> {
    let flags = config.default_servicing_flags();
    openhcl_servicing_core(
        config
            .with_openhcl_command_line("OPENHCL_ENABLE_VTL2_GPA_POOL=512")
            .with_boot_device_type(petri::BootDeviceType::ScsiViaNvme)
            .with_vmbus_redirect(true), // Need this to attach the NVMe device
        igvm_file,
        flags,
        1, // Test is slow with NVMe device, so only do one loop to avoid timeout
    )
    .await
}

#[vmm_test(
    openvmm_openhcl_linux_direct_x64 [LATEST_LINUX_DIRECT_TEST_X64, LATEST_RELEASE_LINUX_DIRECT_X64],
    hyperv_openhcl_pcat_x64(vhd(ubuntu_2504_server_x64))[LATEST_STANDARD_X64, LATEST_RELEASE_STANDARD_X64],
    hyperv_openhcl_uefi_x64(vhd(ubuntu_2504_server_x64))[LATEST_STANDARD_X64, LATEST_RELEASE_STANDARD_X64],
    hyperv_openhcl_uefi_aarch64(vhd(ubuntu_2404_server_aarch64))[LATEST_STANDARD_AARCH64, LATEST_RELEASE_STANDARD_AARCH64]
)]
async fn servicing_upgrade<T: PetriVmmBackend>(
    config: PetriVmBuilder<T>,
    (to_igvm, from_igvm): (
        ResolvedArtifact<impl petri_artifacts_common::tags::IsOpenhclIgvm>,
        ResolvedArtifact<impl petri_artifacts_common::tags::IsOpenhclIgvm>,
    ),
) -> anyhow::Result<()> {
    let mut flags = config.default_servicing_flags();
    flags.enable_mana_keepalive = false; // MANA keepalive not supported until current main

    // TODO: remove .with_guest_state_lifetime(PetriGuestStateLifetime::Disk). The default (ephemeral) does not exist in the 2505 release.
    openhcl_servicing_core(
        config
            .with_custom_openhcl(from_igvm)
            .with_guest_state_lifetime(PetriGuestStateLifetime::Disk),
        to_igvm,
        flags,
        DEFAULT_SERVICING_COUNT,
    )
    .await
}

#[vmm_test(
    openvmm_openhcl_linux_direct_x64 [LATEST_LINUX_DIRECT_TEST_X64, LATEST_RELEASE_LINUX_DIRECT_X64],
    hyperv_openhcl_pcat_x64(vhd(ubuntu_2504_server_x64))[LATEST_STANDARD_X64, LATEST_RELEASE_STANDARD_X64],
    hyperv_openhcl_uefi_x64(vhd(ubuntu_2504_server_x64))[LATEST_STANDARD_X64, LATEST_RELEASE_STANDARD_X64],
    hyperv_openhcl_uefi_aarch64(vhd(ubuntu_2404_server_aarch64))[LATEST_STANDARD_AARCH64, LATEST_RELEASE_STANDARD_AARCH64]
)]
async fn servicing_downgrade<T: PetriVmmBackend>(
    config: PetriVmBuilder<T>,
    (from_igvm, to_igvm): (
        ResolvedArtifact<impl petri_artifacts_common::tags::IsOpenhclIgvm>,
        ResolvedArtifact<impl petri_artifacts_common::tags::IsOpenhclIgvm>,
    ),
) -> anyhow::Result<()> {
    // TODO: remove .with_guest_state_lifetime(PetriGuestStateLifetime::Disk). The default (ephemeral) does not exist in the 2505 release.
    let mut flags = config.default_servicing_flags();
    flags.enable_nvme_keepalive = false; // NVMe keepalive not supported in 2505 release
    flags.enable_mana_keepalive = false; // MANA keepalive not supported until current main
    openhcl_servicing_core(
        config
            .with_custom_openhcl(from_igvm)
            .with_guest_state_lifetime(PetriGuestStateLifetime::Disk),
        to_igvm,
        flags,
        DEFAULT_SERVICING_COUNT,
    )
    .await
}

#[openvmm_test(openhcl_linux_direct_x64 [LATEST_LINUX_DIRECT_TEST_X64])]
async fn servicing_shutdown_ic(
    config: PetriVmBuilder<OpenVmmPetriBackend>,
    (igvm_file,): (ResolvedArtifact<impl petri_artifacts_common::tags::IsOpenhclIgvm>,),
) -> anyhow::Result<()> {
    let flags = config.default_servicing_flags();
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
    vm.restart_openhcl(igvm_file, flags).await?;
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
    let flags = config.default_servicing_flags();
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
                    AdminQueueFaultBehavior::Verify(Some(aer_verify_send)),
                )
                .with_submission_queue_fault(
                    CommandMatchBuilder::new()
                        .match_cdw0_opcode(nvme_spec::AdminOpcode::GET_LOG_PAGE.0)
                        .build(),
                    AdminQueueFaultBehavior::Verify(Some(log_verify_send)),
                ),
        );

    let (mut vm, agent) = create_keepalive_test_config(config, fault_configuration).await?;

    agent.ping().await?;
    let sh = agent.unix_shell();

    // Make sure the disk showed up.
    cmd!(sh, "ls /dev/sda").run().await?;

    fault_start_updater.set(true).await;
    vm.save_openhcl(igvm_file.clone(), flags).await?;
    ns_change_send
        .call(NamespaceChange::ChangeNotification, KEEPALIVE_VTL2_NSID)
        .await?;
    vm.restore_openhcl().await?;

    CancelContext::new()
        .with_timeout(Duration::from_secs(60))
        .until_cancelled(aer_verify_recv)
        .await
        .expect("AER command was not observed within 60 seconds of vm restore after servicing with namespace change")
        .expect("AER verification failed");

    CancelContext::new()
        .with_timeout(Duration::from_secs(60))
        .until_cancelled(log_verify_recv)
        .await
        .expect("GET_LOG_PAGE command was not observed within 60 seconds of vm restore after servicing with namespace change")
        .expect("GET_LOG_PAGE verification failed");

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
                AdminQueueFaultBehavior::Panic("Received a CREATE_IO_COMPLETION_QUEUE command during servicing with keepalive enabled. THERE IS A BUG SOMEWHERE.".to_string()),
            ),
        );

    let _vm = apply_fault_with_keepalive(
        config,
        fault_configuration,
        fault_start_updater,
        igvm_file,
        None,
    )
    .await?;

    Ok(())
}

/// Test servicing an OpenHCL VM from the current version to itself
/// with NVMe keepalive support and a faulty controller that panics when
/// IDENTIFY commands are received. This verifies namespace save/restore functionality.
#[openvmm_test(openhcl_linux_direct_x64 [LATEST_LINUX_DIRECT_TEST_X64])]
async fn servicing_keepalive_fault_if_identify(
    config: PetriVmBuilder<OpenVmmPetriBackend>,
    (igvm_file,): (ResolvedArtifact<impl petri_artifacts_common::tags::IsOpenhclIgvm>,),
) -> Result<(), anyhow::Error> {
    let mut fault_start_updater = CellUpdater::new(false);

    let fault_configuration = FaultConfiguration::new(fault_start_updater.cell())
        .with_admin_queue_fault(
            AdminQueueFaultConfig::new().with_submission_queue_fault(
                CommandMatchBuilder::new().match_cdw0_opcode(nvme_spec::AdminOpcode::IDENTIFY.0).match_cdw10(nvme_spec::Cdw10Identify::new().with_cns(nvme_spec::Cns::NAMESPACE.0).into(), nvme_spec::Cdw10Identify::new().with_cns(u8::MAX).into()).build(),
                AdminQueueFaultBehavior::Panic("Received an IDENTIFY command during servicing with keepalive enabled (And no namespaces were updated). THERE IS A BUG SOMEWHERE.".to_string()),
            ),
        );

    let _vm = apply_fault_with_keepalive(
        config,
        fault_configuration,
        fault_start_updater,
        igvm_file,
        None,
    )
    .await?;

    Ok(())
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
                AdminQueueFaultBehavior::Panic("Received a duplicate ASYNCHRONOUS_EVENT_REQUEST command during servicing with keepalive enabled. THERE IS A BUG SOMEWHERE.".to_string()),
            )
        );

    let _vm = apply_fault_with_keepalive(
        config,
        fault_configuration,
        fault_start_updater,
        igvm_file,
        None,
    )
    .await?;

    Ok(())
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
                AdminQueueFaultBehavior::CustomPayload(buf.to_vec()),
            ),
        );

    let _vm = apply_fault_with_keepalive(
        config,
        fault_configuration,
        fault_start_updater,
        igvm_file,
        None,
    )
    .await?;

    Ok(())
}

async fn apply_fault_with_keepalive(
    config: PetriVmBuilder<OpenVmmPetriBackend>,
    fault_configuration: FaultConfiguration,
    mut fault_start_updater: CellUpdater<bool>,
    igvm_file: ResolvedArtifact<impl petri_artifacts_common::tags::IsOpenhclIgvm>,
    new_cmdline: Option<&str>,
) -> Result<PetriVm<OpenVmmPetriBackend>, anyhow::Error> {
    let mut flags = config.default_servicing_flags();
    flags.enable_nvme_keepalive = true;
    let (mut vm, agent) = create_keepalive_test_config(config, fault_configuration).await?;

    agent.ping().await?;
    let sh = agent.unix_shell();

    // Make sure the disk showed up.
    cmd!(sh, "ls /dev/sda").run().await?;

    fault_start_updater.set(true).await;

    if let Some(cmdline) = new_cmdline {
        vm.update_command_line(cmdline).await?;
    }

    vm.restart_openhcl(igvm_file.clone(), flags).await?;

    fault_start_updater.set(false).await;
    agent.ping().await?;

    Ok(vm)
}

async fn create_keepalive_test_config(
    config: PetriVmBuilder<OpenVmmPetriBackend>,
    fault_configuration: FaultConfiguration,
) -> Result<(PetriVm<OpenVmmPetriBackend>, PipetteClient), anyhow::Error> {
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
        })
        // Assign the fault controller to VTL2
        .add_vtl2_storage_controller(
            Vtl2StorageControllerBuilder::new(ControllerType::Scsi)
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
        )
        .run()
        .await
}

/// Today this only tests that the nic can get an IP address via consomme's DHCP
/// implementation.
///
/// FUTURE: Test traffic on the nic.
async fn validate_mana_nic(agent: &PipetteClient) -> Result<(), anyhow::Error> {
    let sh = agent.unix_shell();
    cmd!(sh, "ifconfig eth0 up").run().await?;
    cmd!(sh, "udhcpc eth0").run().await?;
    let output = cmd!(sh, "ifconfig eth0").read().await?;
    // Validate that we see a mana nic with the expected MAC address and IPs.
    assert!(output.contains("HWaddr 00:15:5D:12:12:12"));
    assert!(output.contains("inet addr:10.0.0.2"));
    assert!(output.contains("inet6 addr: fe80::215:5dff:fe12:1212/64"));

    Ok(())
}

/// Test an OpenHCL Linux direct VM with a MANA nic assigned to VTL2 (backed by
/// the MANA emulator), and vmbus relay. Perform servicing and validate that the
/// nic is still functional.
#[openvmm_test(openhcl_linux_direct_x64 [LATEST_LINUX_DIRECT_TEST_X64])]
async fn mana_nic_servicing(
    config: PetriVmBuilder<OpenVmmPetriBackend>,
    (igvm_file,): (ResolvedArtifact<LATEST_LINUX_DIRECT_TEST_X64>,),
) -> Result<(), anyhow::Error> {
    let flags = config.default_servicing_flags();
    let (mut vm, agent) = config
        .with_vmbus_redirect(true)
        .modify_backend(|b| b.with_nic())
        .run()
        .await?;

    validate_mana_nic(&agent).await?;

    vm.restart_openhcl(igvm_file, flags).await?;

    validate_mana_nic(&agent).await?;

    agent.power_off().await?;
    vm.wait_for_clean_teardown().await?;

    Ok(())
}
/// Test an OpenHCL Linux direct VM with a MANA nic assigned to VTL2 (backed by
/// the MANA emulator), and vmbus relay. Perform servicing and validate that the
/// nic is still functional.
#[openvmm_test(openhcl_linux_direct_x64 [LATEST_LINUX_DIRECT_TEST_X64])]
async fn mana_nic_servicing_keepalive(
    config: PetriVmBuilder<OpenVmmPetriBackend>,
    (igvm_file,): (ResolvedArtifact<LATEST_LINUX_DIRECT_TEST_X64>,),
) -> Result<(), anyhow::Error> {
    let default_flags = config.default_servicing_flags();

    let (mut vm, agent) = config
        .with_vmbus_redirect(true)
        .modify_backend(|b| b.with_nic())
        .with_openhcl_command_line("OPENHCL_ENABLE_VTL2_GPA_POOL=512")
        .run()
        .await?;

    validate_mana_nic(&agent).await?;

    vm.restart_openhcl(
        igvm_file,
        OpenHclServicingFlags {
            enable_mana_keepalive: true,
            ..default_flags
        },
    )
    .await?;

    validate_mana_nic(&agent).await?;

    agent.power_off().await?;
    vm.wait_for_clean_teardown().await?;

    Ok(())
}

/// Test servicing an OpenHCL VM when NVME keepalive is enabled but then
/// disabled after servicing.
/// It verifies that the controller is reset during the restore process.
#[openvmm_test(openhcl_linux_direct_x64 [LATEST_LINUX_DIRECT_TEST_X64])]
async fn servicing_with_keepalive_disabled_after_servicing(
    config: PetriVmBuilder<OpenVmmPetriBackend>,
    (igvm_file,): (ResolvedArtifact<impl petri_artifacts_common::tags::IsOpenhclIgvm>,),
) -> Result<(), anyhow::Error> {
    let mut fault_start_updater = CellUpdater::new(false);
    let (cc_enable_verify_send, cc_enable_verify_recv) = mesh::oneshot::<()>();

    let fault_configuration = FaultConfiguration::new(fault_start_updater.cell()).with_pci_fault(
        PciFaultConfig::new()
            .with_cc_enable_fault(PciFaultBehavior::Verify(Some(cc_enable_verify_send))),
    );

    let _vm = apply_fault_with_keepalive(
        config,
        fault_configuration,
        fault_start_updater,
        igvm_file,
        Some("OPENHCL_ENABLE_VTL2_GPA_POOL=512 OPENHCL_DISABLE_NVME_KEEP_ALIVE=1"),
    )
    .await?;

    CancelContext::new()
        .with_timeout(Duration::from_secs(60))
        .until_cancelled(cc_enable_verify_recv)
        .await
        .expect("Controller Enable PCI command was not observed within 60 seconds of vm restore indicating that the controller was not reset, even though it should have been.")
        .expect("Failed to receive completion for CC Enable PCI command verification");

    Ok(())
}
