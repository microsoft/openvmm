// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Integration tests for OpenHCL servicing.
//! OpenHCL servicing is supported on x86-64 and aarch64.
//! For x86-64, it is supported using both Hyper-V and OpenVMM.
//! For aarch64, it is supported using Hyper-V.

use crate::utils::ExpectedGuestDevice;
use crate::utils::get_device_paths;
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
use nvme_resources::fault::HardwareConfigFaultConfig;
use nvme_resources::fault::IoQueueFaultBehavior;
use nvme_resources::fault::IoQueueFaultConfig;
use nvme_resources::fault::NamespaceChange;
use nvme_resources::fault::NamespaceFaultConfig;
use nvme_resources::fault::PciFaultBehavior;
use nvme_resources::fault::PciFaultConfig;
use nvme_test::command_match::CommandMatchBuilder;
use openvmm_defs::config::DeviceVtl;
use openvmm_defs::config::VpciDeviceConfig;
use petri::MemoryConfig;
use petri::OpenHclServicingFlags;
use petri::PetriGuestStateLifetime;
use petri::PetriVm;
use petri::PetriVmBuilder;
use petri::PetriVmmBackend;
use petri::ProcessorTopology;
use petri::ResolvedArtifact;
use petri::openvmm::OpenVmmPetriBackend;
use petri::pipette::cmd;
use petri::vtl2_settings::ControllerType;
use petri::vtl2_settings::Vtl2LunBuilder;
use petri::vtl2_settings::Vtl2StorageBackingDeviceBuilder;
use petri::vtl2_settings::Vtl2StorageControllerBuilder;
use petri_artifacts_vmm_test::artifacts::openhcl_igvm::LATEST_LINUX_DIRECT_TEST_X64;
use petri_artifacts_vmm_test::artifacts::openhcl_igvm::LATEST_RELEASE_LINUX_DIRECT_X64;
#[cfg(windows)]
use petri_artifacts_vmm_test::artifacts::openhcl_igvm::LATEST_RELEASE_STANDARD_AARCH64;
#[cfg(windows)]
use petri_artifacts_vmm_test::artifacts::openhcl_igvm::LATEST_RELEASE_STANDARD_X64;
#[cfg(windows)]
use petri_artifacts_vmm_test::artifacts::openhcl_igvm::LATEST_STANDARD_AARCH64;
use petri_artifacts_vmm_test::artifacts::openhcl_igvm::LATEST_STANDARD_X64;
use pipette_client::PipetteClient;
use pipette_client::process::Child;
use pipette_client::process::Stdio;
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
const VTL0_NVME_LUN: u32 = 1; // LUN 0 is reserved for the boot device
const DEFAULT_DISK_SIZE: u64 = 256 * 1024; // 256 KiB
const SCSI_SECTOR_SIZE: u64 = 512;

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
        config.with_openhcl_command_line(
            "OPENHCL_ENABLE_VTL2_GPA_POOL=512 OPENHCL_DISABLE_NVME_KEEP_ALIVE=0",
        ),
        igvm_file,
        flags,
        DEFAULT_SERVICING_COUNT,
    )
    .await
}

/// Test servicing an OpenHCL VM with a multi-NUMA pool split.
/// The pool is split across NUMA nodes via `OPENHCL_VTL2_GPA_POOL_NUMA=split`,
/// and pool ranges must be preserved identically across the service boundary.
#[openvmm_test(openhcl_linux_direct_x64[LATEST_LINUX_DIRECT_TEST_X64])]
async fn servicing_numa_private_pool<T: PetriVmmBackend>(
    config: PetriVmBuilder<T>,
    (igvm_file,): (ResolvedArtifact<impl petri_artifacts_common::tags::IsOpenhclIgvm>,),
) -> anyhow::Result<()> {
    let mut flags = config.default_servicing_flags();
    flags.override_version_checks = true;
    openhcl_servicing_core(
        config
            .with_processor_topology(ProcessorTopology {
                vp_count: 4,
                vps_per_socket: Some(2),
                ..Default::default()
            })
            .with_memory(MemoryConfig {
                numa_mem_sizes: Some(vec![2 * 1024 * 1024 * 1024, 2 * 1024 * 1024 * 1024]),
                ..Default::default()
            })
            .with_openhcl_command_line(
                "OPENHCL_VTL2_GPA_POOL_NUMA=split OPENHCL_ENABLE_VTL2_GPA_POOL=512 OPENHCL_DISABLE_NVME_KEEP_ALIVE=0",
            ),
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
            .with_openhcl_command_line(
                "OPENHCL_ENABLE_VTL2_GPA_POOL=512 OPENHCL_DISABLE_NVME_KEEP_ALIVE=0",
            )
            .with_boot_device_type(petri::BootDeviceType::ScsiViaNvme)
            .with_vmbus_redirect(true), // Need this to attach the NVMe device
        igvm_file,
        flags,
        1, // Test is slow with NVMe device, so only do one loop to avoid timeout
    )
    .await
}

/// Test servicing with sidecar and per-CPU override for outstanding IO.
/// Uses 24 VPs across 2 NUMA nodes with sidecar enabled. Delays IO
/// completions, then saves while IO is in-flight. On restore, the CPUs
/// with delayed IO should appear in cpus_with_outstanding_io, triggering
/// the per-CPU sidecar override: those CPUs are started by the kernel,
/// the remaining VPs go through sidecar's parallel startup.
#[openvmm_test(openhcl_linux_direct_x64 [LATEST_LINUX_DIRECT_TEST_X64])]
async fn servicing_keepalive_sidecar_with_outstanding_io_very_heavy(
    config: PetriVmBuilder<OpenVmmPetriBackend>,
    (igvm_file,): (ResolvedArtifact<impl petri_artifacts_common::tags::IsOpenhclIgvm>,),
) -> Result<(), anyhow::Error> {
    use petri::ApicMode;

    let mut flags = config.default_servicing_flags();
    flags.enable_nvme_keepalive = true;
    let mut fault_start_updater = CellUpdater::new(false);
    let cell = fault_start_updater.cell();

    // Delay IO completions (10s) so IO is still in-flight during save.
    // With max_io_queues=2, exactly 2 NVMe IO queues are created, each
    // bound to a specific CPU. Those 2 CPUs appear in cpus_with_outstanding_io
    // while the remaining 22 VPs stay sidecar-eligible.
    let fault_configuration = FaultConfiguration::new(cell.clone()).with_io_queue_fault(
        IoQueueFaultConfig::new(cell.clone()).with_completion_queue_fault(
            CommandMatchBuilder::new().match_cdw0(0, 0).build(),
            IoQueueFaultBehavior::Delay(Duration::from_secs(10)),
        ),
    );

    let scsi_controller_guid = Guid::new_random();
    let disk_size = 4 * 1024 * 1024; // 4 MiB
    let vp_count: u32 = 24;

    // Use 2 NUMA nodes (vps_per_socket=12). Sidecar requires >1 VP per
    // node to activate.
    let (mut vm, agent) = create_keepalive_test_config_custom(
        config,
        fault_configuration,
        VTL0_NVME_LUN,
        scsi_controller_guid,
        disk_size,
        vp_count,
        Some(ProcessorTopology {
            vp_count,
            vps_per_socket: Some(12),
            enable_smt: Some(false),
            apic_mode: Some(ApicMode::X2apicSupported),
        }),
        &["OPENHCL_SIDECAR=log"],
        3, // msix_count: 1 admin + 2 IO
        2, // max_io_queues
    )
    .await?;

    agent.ping().await?;

    // Find the disk path.
    let device_paths = get_device_paths(
        &agent,
        scsi_controller_guid,
        vec![ExpectedGuestDevice {
            lun: VTL0_NVME_LUN,
            disk_size_sectors: (disk_size / SCSI_SECTOR_SIZE) as usize,
            friendly_name: "nvme_disk".to_string(),
        }],
    )
    .await?;
    assert!(device_paths.len() == 1);
    let disk_path = &device_paths[0];

    // Start delayed IO — this creates outstanding IO on the issuing CPUs.
    fault_start_updater.set(true).await;
    let _io_child = large_read_from_disk(&agent, disk_path).await?;

    // Wait briefly so the IO reaches the NVMe controller and gets delayed.
    std::thread::sleep(Duration::from_secs(2));

    // Save while IO is in-flight. The CPUs with delayed IO will appear
    // in cpus_with_outstanding_io in the persisted state.
    CancelContext::new()
        .with_timeout(Duration::from_secs(60))
        .until_cancelled(vm.save_openhcl(igvm_file.clone(), flags))
        .await
        .expect("VM save should complete within 60 seconds")
        .expect("VM save failed");

    // Restore exercises per-CPU sidecar override: CPUs with outstanding IO
    // are started by the kernel, remaining VPs go through sidecar.
    vm.restore_openhcl().await?;

    // Verify the per-CPU override fired by checking openhcl_boot logs.
    let boot_logs = vm
        .inspect_openhcl("vm/runtime_params/bootshim_logs", Some(2), None)
        .await?;
    let boot_logs_str = format!("{}", boot_logs.json());
    assert!(
        boot_logs_str.contains("excluding CPUs"),
        "per-CPU sidecar override did not fire on restore; \
         cpus_with_outstanding_io was likely empty. Boot logs: {}",
        boot_logs_str
    );

    // Disable faults and verify guest is functional after restore.
    fault_start_updater.set(false).await;
    agent.ping().await?;

    // Verify all VPs came online after restore.
    let sh = agent.unix_shell();
    let online = cmd!(sh, "cat /sys/devices/system/cpu/online")
        .read()
        .await?;
    assert_eq!(
        online.trim(),
        format!("0-{}", vp_count - 1),
        "not all VPs came online after restore"
    );

    agent.power_off().await?;
    vm.wait_for_clean_teardown().await?;
    Ok(())
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
                                    sector_size: None,
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

    let (mut vm, agent) = create_keepalive_test_config_default(
        config,
        fault_configuration,
        VTL0_NVME_LUN,
        Guid::new_random(),
        DEFAULT_DISK_SIZE,
    )
    .await?;

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

/// Verifies behavior when a GET_LOG_PAGE command is delayed during servicing, simulating a
/// scenario where an AER could be missed after OpenHCL restart.
// #[openvmm_test(openhcl_linux_direct_x64 [LATEST_LINUX_DIRECT_TEST_X64])]
async fn _servicing_keepalive_with_missed_get_log_page(
    config: PetriVmBuilder<OpenVmmPetriBackend>,
    (igvm_file,): (ResolvedArtifact<impl petri_artifacts_common::tags::IsOpenhclIgvm>,),
) -> Result<(), anyhow::Error> {
    let flags = config.default_servicing_flags();
    let mut fault_start_updater = CellUpdater::new(false);
    let (ns_change_send, ns_change_recv) = mesh::channel::<NamespaceChange>();
    let (identify_verify_send, identify_verify_recv) = mesh::oneshot::<()>();

    let fault_configuration = FaultConfiguration::new(fault_start_updater.cell())
        .with_namespace_fault(NamespaceFaultConfig::new(ns_change_recv))
        .with_admin_queue_fault(
            AdminQueueFaultConfig::new()
                .with_submission_queue_fault(
                    CommandMatchBuilder::new()
                        .match_cdw0_opcode(nvme_spec::AdminOpcode::GET_LOG_PAGE.0)
                        .build(),
                    AdminQueueFaultBehavior::Delay(Duration::from_secs(10)),
                )
                .with_submission_queue_fault(
                    CommandMatchBuilder::new()
                        .match_cdw0_opcode(nvme_spec::AdminOpcode::IDENTIFY.0)
                        .match_cdw10(
                            nvme_spec::Cdw10Identify::new()
                                .with_cns(nvme_spec::Cns::NAMESPACE.0)
                                .into(),
                            nvme_spec::Cdw10Identify::new().with_cns(u8::MAX).into(),
                        )
                        .build(),
                    AdminQueueFaultBehavior::Verify(Some(identify_verify_send)),
                ),
        );

    let (mut vm, agent) = create_keepalive_test_config_default(
        config,
        fault_configuration,
        VTL0_NVME_LUN,
        Guid::new_random(),
        DEFAULT_DISK_SIZE,
    )
    .await?;

    agent.ping().await?;
    let sh = agent.unix_shell();

    // Make sure the disk showed up.
    cmd!(sh, "ls /dev/sda").run().await?;

    fault_start_updater.set(true).await;
    ns_change_send
        .call(NamespaceChange::ChangeNotification, KEEPALIVE_VTL2_NSID)
        .await?;

    vm.restart_openhcl(igvm_file.clone(), flags).await?;

    CancelContext::new()
        .with_timeout(Duration::from_secs(30))
        .until_cancelled(identify_verify_recv)
        .await
        .expect("IDENTIFY should be observed within 30 seconds of vm restore after servicing with namespace change")
        .expect("IDENTIFY verification should pass and return a valid result.");

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

/// Test that disabling keepalive through inspect actually disables it.
/// We test this by disabling keepalive and waiting for IDENTIFY.
/// We should only receive IDENTIFY if (and only if) keepalive is disabled.
#[openvmm_test(openhcl_linux_direct_x64 [LATEST_LINUX_DIRECT_TEST_X64])]
async fn servicing_test_keepalive_disable_through_inspect(
    config: PetriVmBuilder<OpenVmmPetriBackend>,
    (igvm_file,): (ResolvedArtifact<impl petri_artifacts_common::tags::IsOpenhclIgvm>,),
) -> Result<(), anyhow::Error> {
    let mut fault_start_updater = CellUpdater::new(false);

    let (identify_verify_send, identify_verify_recv) = mesh::oneshot::<()>();

    let fault_configuration = FaultConfiguration::new(fault_start_updater.cell())
        .with_admin_queue_fault(
            AdminQueueFaultConfig::new().with_submission_queue_fault(
                CommandMatchBuilder::new()
                    .match_cdw0_opcode(nvme_spec::AdminOpcode::IDENTIFY.0)
                    .build(),
                AdminQueueFaultBehavior::Verify(Some(identify_verify_send)),
            ),
        );

    let mut flags = config.default_servicing_flags();
    // Enable keepalive, then disable it later via inspect
    flags.enable_nvme_keepalive = true;
    // We need to disabled MANA KA since if either of the KA flasgs in on, DMA manager will save its state
    // which includes NVMe regions and restore verification will fail ("unrestored allocations found"),
    // since NVMe KA is off and we don't save anything).
    flags.enable_mana_keepalive = false;
    let (mut vm, agent) = create_keepalive_test_config_default(
        config,
        fault_configuration,
        VTL0_NVME_LUN,
        Guid::new_random(),
        DEFAULT_DISK_SIZE,
    )
    .await?;

    agent.ping().await?;
    let sh = agent.unix_shell();

    // Make sure the disk showed up.
    cmd!(sh, "ls /dev/sda").run().await?;

    fault_start_updater.set(true).await;

    // Disable keepalive via inspect
    vm.inspect_update_openhcl("vm/nvme_keepalive_mode", "disabled")
        .await?;

    vm.restart_openhcl(igvm_file.clone(), flags).await?;

    agent.ping().await?;

    CancelContext::new()
        .with_timeout(Duration::from_secs(30))
        .until_cancelled(identify_verify_recv)
        .await
        .expect("IDENTIFY should be observed within 30 seconds of vm restore after servicing with keepalive disabled")
        .expect("IDENTIFY verification should pass and return a valid result.");

    fault_start_updater.set(false).await;

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

/// Verifies behavior when a submission queue is full and we try to service. The
/// servicing should still succeed (i.e. the queue pairs should still be
/// listening for save commands).
#[openvmm_test(openhcl_linux_direct_x64 [LATEST_LINUX_DIRECT_TEST_X64])]
async fn servicing_keepalive_with_io_queue_full(
    config: PetriVmBuilder<OpenVmmPetriBackend>,
    (igvm_file,): (ResolvedArtifact<impl petri_artifacts_common::tags::IsOpenhclIgvm>,),
) -> Result<(), anyhow::Error> {
    let flags = config.default_servicing_flags();
    let mut fault_start_updater = CellUpdater::new(false);
    let cell = fault_start_updater.cell();

    // Delay excessively (100s) to cause the queue to fill up. Don't start fault
    // immediately. There will be some IO during guest boot that we don't want to
    // interfere with.
    // DEV NOTE: Reduced mqes is required to make sure the queue fills up during
    // the test. dd is single threaded and there seems to be a guest limitation
    // that prevents more than 16 concurrent SCSI requests. This limit can
    // probably be lifted if/when fio is available in the guest.
    let fault_configuration = FaultConfiguration::new(cell.clone())
        .with_io_queue_fault(
            IoQueueFaultConfig::new(cell.clone()).with_completion_queue_fault(
                CommandMatchBuilder::new().match_cdw0(0, 0).build(),
                IoQueueFaultBehavior::Delay(Duration::from_secs(100)),
            ),
        )
        .with_pci_fault(PciFaultConfig::new().with_max_queue_size(8));

    let scsi_controller_guid = Guid::new_random();
    let disk_size = 100 * 1024 * 1024; // 100 MiB

    let (mut vm, agent) = create_keepalive_test_config_default(
        config,
        fault_configuration,
        VTL0_NVME_LUN,
        scsi_controller_guid,
        disk_size,
    )
    .await?;

    agent.ping().await?;

    // Fetch the correct disk path for the VTL0 NVMe disk. Petri may assign it
    // to /dev/sda or /dev/sdb depending on timing.
    let device_paths = get_device_paths(
        &agent,
        scsi_controller_guid,
        vec![ExpectedGuestDevice {
            lun: VTL0_NVME_LUN,
            disk_size_sectors: (disk_size / SCSI_SECTOR_SIZE) as usize,
            friendly_name: "nvme_disk".to_string(),
        }],
    )
    .await?;
    assert!(device_paths.len() == 1);
    let disk_path = &device_paths[0];

    // At this point the guest should be booted and the disk should be stable
    // with no other ongoing IO. Start some large reads to fill up the IO queue.
    fault_start_updater.set(true).await;
    let _io_child = large_read_from_disk(&agent, disk_path).await?;

    // 60 seconds should be plenty of time for the save to complete. If
    // save is stuck it will be exposed here.
    CancelContext::new()
        .with_timeout(Duration::from_secs(60))
        .until_cancelled(vm.save_openhcl(igvm_file.clone(), flags))
        .await
        .expect("VM save did not complete within 60 seconds, even though it should have. Save is stuck.")
        .expect("VM save failed");

    vm.restore_openhcl().await?;

    fault_start_updater.set(false).await;
    agent.ping().await?;

    Ok(())
}

/// Verifies behavior when device io is slow/stuck and we repeatedly
/// try to service. When draining IO queues after restore, nvme_driver should
/// still be responsive on Save commands.
#[openvmm_test(openhcl_linux_direct_x64 [LATEST_LINUX_DIRECT_TEST_X64])]
async fn servicing_keepalive_with_unresponsive_io(
    config: PetriVmBuilder<OpenVmmPetriBackend>,
    (igvm_file,): (ResolvedArtifact<impl petri_artifacts_common::tags::IsOpenhclIgvm>,),
) -> Result<(), anyhow::Error> {
    let flags = config.default_servicing_flags();
    let mut fault_start_updater = CellUpdater::new(false);
    let cell = fault_start_updater.cell();

    // Delay (120s). Draining IO after restore will now be excessively slow.
    let fault_configuration = FaultConfiguration::new(cell.clone()).with_io_queue_fault(
        IoQueueFaultConfig::new(cell.clone()).with_completion_queue_fault(
            CommandMatchBuilder::new().match_cdw0(0, 0).build(),
            IoQueueFaultBehavior::Delay(Duration::from_secs(120)),
        ),
    );

    let scsi_controller_guid = Guid::new_random();
    let (mut vm, agent) = create_keepalive_test_config_default(
        config,
        fault_configuration,
        VTL0_NVME_LUN,
        scsi_controller_guid,
        DEFAULT_DISK_SIZE,
    )
    .await?;

    agent.ping().await?;

    // Fetch the correct disk path for the VTL0 NVMe disk. Petri may assign it
    // to /dev/sda or /dev/sdb depending on timing.
    let device_paths = get_device_paths(
        &agent,
        scsi_controller_guid,
        vec![ExpectedGuestDevice {
            lun: VTL0_NVME_LUN,
            disk_size_sectors: (DEFAULT_DISK_SIZE / SCSI_SECTOR_SIZE) as usize,
            friendly_name: "nvme_disk".to_string(),
        }],
    )
    .await?;
    assert!(device_paths.len() == 1);
    let disk_path = &device_paths[0];

    // At this point the guest should be booted and the disk should be stable
    // with no other ongoing IO. Start some reads.
    fault_start_updater.set(true).await;
    let _io_child = large_read_from_disk(&agent, disk_path).await?;

    // 60 seconds should be plenty of time for the save to complete. Save should
    // NEVER get stuck. Keeping a timeout to avoid long running tests.
    CancelContext::new()
        .with_timeout(Duration::from_secs(60))
        .until_cancelled(vm.save_openhcl(igvm_file.clone(), flags))
        .await
        .expect("VM save did not complete within 60 seconds, even though it should have. Stuck on first save attempt.")
        .expect("VM save failed");
    vm.restore_openhcl().await?;
    agent.ping().await?;

    CancelContext::new()
        .with_timeout(Duration::from_secs(60))
        .until_cancelled(vm.save_openhcl(igvm_file.clone(), flags))
        .await
        .expect("VM save did not complete within 60 seconds, even though it should have. Save is stuck when draining after restore.")
        .expect("VM save failed");
    vm.restore_openhcl().await?;

    fault_start_updater.set(false).await;
    agent.ping().await?;

    Ok(())
}

/// Verifies that `create_io_queue` works correctly after keepalive restore
/// when IO is directed to a CPU that had no IO queue at save time.
///
/// Uses a 4-CPU VM so that some CPUs are unlikely to have IO queues from
/// boot-time activity. After boot, inspect is used to find a CPU that has
/// no IO issuer — this makes the test deterministic. After keepalive restore
/// (with a phantom AER from a namespace change), IO is directed to that CPU
/// via a cpuset cgroup (the minimal linux direct initrd has no `taskset`), which
/// forces the NVMe driver's `create_io_queue` path — including CREATE_IO_CQ
/// and CREATE_IO_SQ admin commands sent to the kept-alive controller. A
/// second servicing cycle validates that the new queue state is fully
/// consistent.
///
/// NOTE: This test validates driver-side correctness only. The emulated
/// controller handles interrupts via in-process signals, so it cannot catch
/// host-side interrupt routing issues that only manifest with real SR-IOV
/// devices.
#[openvmm_test(openhcl_linux_direct_x64 [LATEST_LINUX_DIRECT_TEST_X64])]
async fn servicing_keepalive_create_io_queue_on_new_cpu(
    config: PetriVmBuilder<OpenVmmPetriBackend>,
    (igvm_file,): (ResolvedArtifact<impl petri_artifacts_common::tags::IsOpenhclIgvm>,),
) -> Result<(), anyhow::Error> {
    let mut flags = config.default_servicing_flags();
    flags.enable_nvme_keepalive = true;
    let mut fault_start_updater = CellUpdater::new(false);
    let (ns_change_send, ns_change_recv) = mesh::channel::<NamespaceChange>();

    // No admin queue faults — we want create_io_queue admin commands to
    // succeed normally. The namespace fault injects a phantom AER during
    // the keepalive window, matching the production scenario.
    let fault_configuration = FaultConfiguration::new(fault_start_updater.cell())
        .with_namespace_fault(NamespaceFaultConfig::new(ns_change_recv));

    let scsi_controller_guid = Guid::new_random();
    let disk_size = 4 * 1024 * 1024; // 4 MiB — enough for dd reads
    let vp_count = 4;

    let (mut vm, agent) = create_keepalive_test_config_custom_vps(
        config,
        fault_configuration,
        VTL0_NVME_LUN,
        scsi_controller_guid,
        disk_size,
        vp_count,
    )
    .await?;

    agent.ping().await?;

    let cpus_with_issuers = find_cpus_with_io_issuers(&vm).await?;
    let target_cpu = (0u32..vp_count)
        .find(|cpu| !cpus_with_issuers.contains(cpu))
        .unwrap_or_else(|| {
            panic!(
                "all {vp_count} CPUs already have IO issuers after boot — \
             test cannot exercise create_io_queue. Consider increasing vp_count."
            )
        });
    tracing::info!(
        target_cpu,
        existing_issuers = ?cpus_with_issuers,
        "selected target CPU with no IO issuer"
    );

    // Resolve the disk path before save. The device might appear as /dev/sda
    // or /dev/sdb depending on timing.
    let device_paths = get_device_paths(
        &agent,
        scsi_controller_guid,
        vec![ExpectedGuestDevice {
            lun: VTL0_NVME_LUN,
            disk_size_sectors: (disk_size / SCSI_SECTOR_SIZE) as usize,
            friendly_name: "nvme_disk".to_string(),
        }],
    )
    .await?;
    assert!(device_paths.len() == 1);
    let disk_path = &device_paths[0];

    // ── First servicing cycle: phantom AER + create_io_queue ──
    // Save, inject a namespace change (phantom AER), then restore.
    fault_start_updater.set(true).await;
    vm.save_openhcl(igvm_file.clone(), flags).await?;
    ns_change_send
        .call(NamespaceChange::ChangeNotification, KEEPALIVE_VTL2_NSID)
        .await?;
    vm.restore_openhcl().await?;
    fault_start_updater.set(false).await;

    // This should trigger creation of a new io queue.
    run_cpu_pinned_io(&agent, disk_path, target_cpu).await?;

    let cpus_with_issuers = find_cpus_with_io_issuers(&vm).await?;
    assert!(
        cpus_with_issuers.contains(&target_cpu),
        "target CPU should have an IO issuer on CPU {target_cpu} after pinning IO. CPUs with issuers: {cpus_with_issuers:?}"
    );
    agent.ping().await?;

    // ── Second servicing cycle: verify queue state consistency ──
    // If the newly created queue has inconsistent state, this save/restore
    // will expose it.
    vm.save_openhcl(igvm_file.clone(), flags).await?;
    vm.restore_openhcl().await?;
    agent.ping().await?;

    // Issue IO again on the same CPU to confirm the queue survived the
    // second servicing cycle.
    run_cpu_pinned_io(&agent, disk_path, target_cpu).await?;

    agent.ping().await?;

    Ok(())
}

/// Verifies that save works correctly when a create_io_queue command
/// is stuck. The `DriverWorkerTask` run loop should still be able to process
/// save commands when the stuck create_io_queue command completes, even when
/// that happens after save has been issued.
#[openvmm_test(openhcl_linux_direct_x64 [LATEST_LINUX_DIRECT_TEST_X64])]
async fn servicing_keepalive_slow_create_io_queue(
    config: PetriVmBuilder<OpenVmmPetriBackend>,
    (igvm_file,): (ResolvedArtifact<impl petri_artifacts_common::tags::IsOpenhclIgvm>,),
) -> Result<(), anyhow::Error> {
    const QUEUE_CREATION_DELAY: Duration = Duration::from_secs(10);
    const TRIGGER_CREATE_IO_QUEUE_TIMEOUT: Duration = Duration::from_secs(5);
    const TOTAL_SAVE_TIMEOUT: Duration = Duration::from_secs(30);

    let mut flags = config.default_servicing_flags();
    flags.enable_nvme_keepalive = true;
    let mut fault_start_updater = CellUpdater::new(false);

    let fault_configuration = FaultConfiguration::new(fault_start_updater.cell())
        .with_admin_queue_fault(
            AdminQueueFaultConfig::new().with_submission_queue_fault(
                CommandMatchBuilder::new()
                    .match_cdw0_opcode(nvme_spec::AdminOpcode::CREATE_IO_COMPLETION_QUEUE.0)
                    .build(),
                AdminQueueFaultBehavior::Delay(QUEUE_CREATION_DELAY),
            ),
        );

    let scsi_controller_guid = Guid::new_random();
    let disk_size = 4 * 1024 * 1024; // 4 MiB — enough for dd reads
    let vp_count = 4;

    let (mut vm, agent) = create_keepalive_test_config_custom_vps(
        config,
        fault_configuration,
        VTL0_NVME_LUN,
        scsi_controller_guid,
        disk_size,
        vp_count,
    )
    .await?;
    agent.ping().await?;

    let cpus_with_issuers = find_cpus_with_io_issuers(&vm).await?;
    let target_cpu = (0u32..vp_count)
        .find(|cpu| !cpus_with_issuers.contains(cpu))
        .unwrap_or_else(|| {
            panic!(
                "all {vp_count} CPUs already have IO issuers after boot — \
             test cannot exercise create_io_queue. Consider increasing vp_count."
            )
        });
    tracing::info!(
        target_cpu,
        existing_issuers = ?cpus_with_issuers,
        "selected target CPU with no IO issuer"
    );

    // Resolve the disk path before save. The device might appear as /dev/sda
    // or /dev/sdb depending on timing.
    let device_paths = get_device_paths(
        &agent,
        scsi_controller_guid,
        vec![ExpectedGuestDevice {
            lun: VTL0_NVME_LUN,
            disk_size_sectors: (disk_size / SCSI_SECTOR_SIZE) as usize,
            friendly_name: "nvme_disk".to_string(),
        }],
    )
    .await?;
    assert!(device_paths.len() == 1);
    let disk_path = &device_paths[0];

    // DEV NOTE: `run_cpu_pinned_io` only needs to be run for a duration that
    // guarantees the create_io_queue command getting stuck. Ideally this should
    // be event driven instead of time driven, but the infrastructure for that
    // is not in place yet.
    // Even though the dd command will timeout, the run loop will be stuck until
    // the create_io_queue command completes.
    fault_start_updater.set(true).await;
    let io_result = CancelContext::new()
        .with_timeout(TRIGGER_CREATE_IO_QUEUE_TIMEOUT)
        .until_cancelled(run_cpu_pinned_io(&agent, disk_path, target_cpu))
        .await;

    assert!(
        io_result.is_err(),
        "IO command should have timed out. This likely means the create_io_queue command did not get injected correctly."
    );

    CancelContext::new()
        .with_timeout(TOTAL_SAVE_TIMEOUT)
        .until_cancelled(vm.save_openhcl(igvm_file.clone(), flags))
        .await
        .expect("VM save did not complete within the given timeout, even though it should have. Save is stuck when draining after restore with slow create_io_queue.")
        .expect("Save failed");

    fault_start_updater.set(false).await;
    vm.restore_openhcl().await?;
    agent.ping().await?;
    Ok(())
}

/// Verifies that save works correctly when a create_io_queue command
/// is still in flight and inspect is called on the device. Previously we saw
/// inspect calls inadvertently throwing away create_io_issuer futures and then
/// save being serviced with CREATE_IO_COMPLETION_QUEUE commands still pending.
#[openvmm_test(openhcl_linux_direct_x64 [LATEST_LINUX_DIRECT_TEST_X64])]
async fn servicing_keepalive_slow_create_io_queue_with_inspect(
    config: PetriVmBuilder<OpenVmmPetriBackend>,
    (igvm_file,): (ResolvedArtifact<impl petri_artifacts_common::tags::IsOpenhclIgvm>,),
) -> Result<(), anyhow::Error> {
    const QUEUE_CREATION_DELAY: Duration = Duration::from_secs(60);
    const TRIGGER_CREATE_IO_QUEUE_TIMEOUT: Duration = Duration::from_secs(5);
    const TOTAL_SAVE_TIMEOUT: Duration = Duration::from_secs(15);

    let mut flags = config.default_servicing_flags();
    flags.enable_nvme_keepalive = true;
    let mut fault_start_updater = CellUpdater::new(false);

    let fault_configuration = FaultConfiguration::new(fault_start_updater.cell())
        .with_admin_queue_fault(
            AdminQueueFaultConfig::new().with_submission_queue_fault(
                CommandMatchBuilder::new()
                    .match_cdw0_opcode(nvme_spec::AdminOpcode::CREATE_IO_COMPLETION_QUEUE.0)
                    .build(),
                AdminQueueFaultBehavior::Delay(QUEUE_CREATION_DELAY),
            ),
        );

    let scsi_controller_guid = Guid::new_random();
    let disk_size = 4 * 1024 * 1024; // 4 MiB — enough for dd reads
    let vp_count = 4;

    let (mut vm, agent) = create_keepalive_test_config_custom_vps(
        config,
        fault_configuration,
        VTL0_NVME_LUN,
        scsi_controller_guid,
        disk_size,
        vp_count,
    )
    .await?;
    agent.ping().await?;

    let cpus_with_issuers = find_cpus_with_io_issuers(&vm).await?;
    let target_cpu = (0u32..vp_count)
        .find(|cpu| !cpus_with_issuers.contains(cpu))
        .unwrap_or_else(|| {
            panic!(
                "all {vp_count} CPUs already have IO issuers after boot — \
             test cannot exercise create_io_queue. Consider increasing vp_count."
            )
        });
    tracing::info!(
        target_cpu,
        existing_issuers = ?cpus_with_issuers,
        "selected target CPU with no IO issuer"
    );

    // Resolve the disk path before save. The device might appear as /dev/sda
    // or /dev/sdb depending on timing.
    let device_paths = get_device_paths(
        &agent,
        scsi_controller_guid,
        vec![ExpectedGuestDevice {
            lun: VTL0_NVME_LUN,
            disk_size_sectors: (disk_size / SCSI_SECTOR_SIZE) as usize,
            friendly_name: "nvme_disk".to_string(),
        }],
    )
    .await?;
    assert!(device_paths.len() == 1);
    let disk_path = &device_paths[0];

    // DEV NOTE: `run_cpu_pinned_io` only needs to be run for a duration that
    // guarantees the create_io_queue command getting stuck. Ideally this should
    // be event driven instead of time driven, but the infrastructure for that
    // is not in place yet.
    // Even though the dd command will timeout, the run loop will be stuck until
    // the create_io_queue command completes.
    fault_start_updater.set(true).await;
    let io_result = CancelContext::new()
        .with_timeout(TRIGGER_CREATE_IO_QUEUE_TIMEOUT)
        .until_cancelled(run_cpu_pinned_io(&agent, disk_path, target_cpu))
        .await;

    assert!(
        io_result.is_err(),
        "IO command should have timed out. This likely means the create_io_queue command did not get injected correctly."
    );

    // In previous versions invoking inspect would cause the DriverWorkerTask to
    // just drop the stuck create io queue command and service the save with
    // pending admin commands (not good)
    let nvme_device_inspect = vm.inspect_openhcl("vm/nvme/devices", None, None).await?;
    tracing::info!(nvme_device_inspect = ?nvme_device_inspect, "nvme device inspected");

    let entries = match &nvme_device_inspect {
        inspect::Node::Dir(entries) => entries,
        _ => panic!(
            "expected dir for 'vm/nvme/devices' but found {}",
            nvme_device_inspect.json()
        ),
    };
    assert_eq!(
        entries.len(),
        1,
        "expected exactly 1 NVMe device under 'vm/nvme/devices', found {}",
        entries.len()
    );
    let nvme_device_name = entries[0].name.clone();

    CancelContext::new()
        .with_timeout(TOTAL_SAVE_TIMEOUT)
        .until_cancelled(vm.save_openhcl(igvm_file.clone(), flags))
        .await
        .expect("VM save did not complete within the given timeout, even though it should have. Save is stuck when draining after restore with slow create_io_queue.")
        .expect("Save failed");

    fault_start_updater.set(false).await;

    vm.restore_openhcl().await?;
    agent.ping().await?;

    let vm_inspect = vm
        .inspect_openhcl(
            &format!("vm/nvme/devices/{nvme_device_name}/driver/driver/admin/commands/commands"),
            None,
            None,
        )
        .await?;

    tracing::info!("vm inspected {}", vm_inspect.json());
    let entries = match &vm_inspect {
        inspect::Node::Dir(entries) => entries,
        _ => {
            panic!(
                "expected list of pending commands but found {}",
                vm_inspect.json()
            );
        }
    };
    assert_eq!(entries.len(), 1, "expected only 1 entry, the AER command");

    Ok(())
}

/// Verifies the per-device NVMe keepalive gate.
///
/// Two NVMe controllers are attached to a single VM:
/// * A keepalive-compatible controller. Keepalive must be honored for
///   this device — its controller is not reset across servicing, so
///   `CREATE_IO_COMPLETION_QUEUE` must NOT be issued after servicing.
///   The fault panics if the opcode is observed.
/// * A keepalive-incompatible controller with VendorID = 0x1414 and DeviceID =
///   0xb111. The test forces this path via the hardware-config fault override
///   used by the compatibility check. Keepalive must be downgraded to
///   reset-on-servicing for this device, so `CREATE_IO_COMPLETION_QUEUE` must
///   be issued around servicing.
///
/// Keepalive is enabled VM-wide; the gate is expected to apply per
/// device based on its Vendor/Device IDs.
#[openvmm_test(openhcl_linux_direct_x64 [LATEST_LINUX_DIRECT_TEST_X64])]
async fn servicing_keepalive_per_device_gate(
    config: PetriVmBuilder<OpenVmmPetriBackend>,
    (igvm_file,): (ResolvedArtifact<impl petri_artifacts_common::tags::IsOpenhclIgvm>,),
) -> Result<(), anyhow::Error> {
    let mut flags = config.default_servicing_flags();
    flags.enable_nvme_keepalive = true;

    const WITH_KEEPALIVE_NVME_INSTANCE: Guid = guid::guid!("00000000-c05b-0000-0000-000000000001");
    const NO_KEEPALIVE_NVME_INSTANCE: Guid = guid::guid!("dce4ebad-182f-46c0-8d30-8446c1c62ab3");

    const WITH_KEEPALIVE_NSID: u32 = KEEPALIVE_VTL2_NSID;
    const NO_KEEPALIVE_NSID: u32 = KEEPALIVE_VTL2_NSID + 1;
    const WITH_KEEPALIVE_LUN: u32 = VTL0_NVME_LUN;
    const NO_KEEPALIVE_LUN: u32 = VTL0_NVME_LUN + 1;

    // Two independent fault start cells — one per device.
    let mut with_keepalive_fault_updater = CellUpdater::new(false);
    let mut no_keepalive_fault_updater = CellUpdater::new(false);

    let (no_keepalive_create_seen_send, no_keepalive_create_seen_recv) = mesh::oneshot::<()>();

    // c05b device: keepalive must be honored — fail loudly if any
    // CREATE_IO_COMPLETION_QUEUE is observed after the fault is armed.
    let with_keepalive_fault_config = FaultConfiguration::new(with_keepalive_fault_updater.cell())
        .with_admin_queue_fault(
            AdminQueueFaultConfig::new().with_submission_queue_fault(
                CommandMatchBuilder::new()
                    .match_cdw0_opcode(nvme_spec::AdminOpcode::CREATE_IO_COMPLETION_QUEUE.0)
                    .build(),
                AdminQueueFaultBehavior::Panic(
                    "c05b device received CREATE_IO_COMPLETION_QUEUE after servicing — \
                     keepalive should have been honored for this device but the controller \
                     was reset."
                        .to_string(),
                ),
            ),
        );

    // Non c05b device: keepalive must be downgraded —
    // verify CREATE_IO_COMPLETION_QUEUE IS issued after servicing.
    let no_keepalive_fault_config = FaultConfiguration::new(no_keepalive_fault_updater.cell())
        .with_admin_queue_fault(
            AdminQueueFaultConfig::new().with_submission_queue_fault(
                CommandMatchBuilder::new()
                    .match_cdw0_opcode(nvme_spec::AdminOpcode::CREATE_IO_COMPLETION_QUEUE.0)
                    .build(),
                AdminQueueFaultBehavior::Verify(Some(no_keepalive_create_seen_send)),
            ),
        )
        .with_hardware_config_fault(
            HardwareConfigFaultConfig::new()
                .with_vendor_id(0x1414)
                .with_device_id(0xb111),
        );

    let scsi_instance = Guid::new_random();

    let (mut vm, agent) = config
        .with_vmbus_redirect(true)
        .with_openhcl_command_line(
            "OPENHCL_ENABLE_VTL2_GPA_POOL=512 OPENHCL_DISABLE_NVME_KEEP_ALIVE=0",
        )
        .modify_backend(move |b| {
            b.with_custom_config(move |c| {
                c.vpci_devices.push(VpciDeviceConfig {
                    vtl: DeviceVtl::Vtl2,
                    instance_id: NO_KEEPALIVE_NVME_INSTANCE,
                    resource: NvmeFaultControllerHandle {
                        subsystem_id: Guid::new_random(),
                        msix_count: 10,
                        max_io_queues: 10,
                        namespaces: vec![NamespaceDefinition {
                            nsid: NO_KEEPALIVE_NSID,
                            read_only: false,
                            disk: LayeredDiskHandle::single_layer(RamDiskLayerHandle {
                                len: Some(DEFAULT_DISK_SIZE),
                                sector_size: None,
                            })
                            .into_resource(),
                        }],
                        fault_config: no_keepalive_fault_config,
                        enable_tdisp_tests: false,
                    }
                    .into_resource(),
                });
                c.vpci_devices.push(VpciDeviceConfig {
                    vtl: DeviceVtl::Vtl2,
                    instance_id: WITH_KEEPALIVE_NVME_INSTANCE,
                    resource: NvmeFaultControllerHandle {
                        subsystem_id: Guid::new_random(),
                        msix_count: 10,
                        max_io_queues: 10,
                        namespaces: vec![NamespaceDefinition {
                            nsid: WITH_KEEPALIVE_NSID,
                            read_only: false,
                            disk: LayeredDiskHandle::single_layer(RamDiskLayerHandle {
                                len: Some(DEFAULT_DISK_SIZE),
                                sector_size: None,
                            })
                            .into_resource(),
                        }],
                        fault_config: with_keepalive_fault_config,
                        enable_tdisp_tests: false,
                    }
                    .into_resource(),
                });
            })
        })
        .add_vtl2_storage_controller(
            Vtl2StorageControllerBuilder::new(ControllerType::Scsi)
                .with_instance_id(scsi_instance)
                .add_lun(
                    Vtl2LunBuilder::disk()
                        .with_location(NO_KEEPALIVE_LUN)
                        .with_physical_device(Vtl2StorageBackingDeviceBuilder::new(
                            ControllerType::Nvme,
                            NO_KEEPALIVE_NVME_INSTANCE,
                            NO_KEEPALIVE_NSID,
                        )),
                )
                .add_lun(
                    Vtl2LunBuilder::disk()
                        .with_location(WITH_KEEPALIVE_LUN)
                        .with_physical_device(Vtl2StorageBackingDeviceBuilder::new(
                            ControllerType::Nvme,
                            WITH_KEEPALIVE_NVME_INSTANCE,
                            WITH_KEEPALIVE_NSID,
                        )),
                )
                .build(),
        )
        .run()
        .await?;

    agent.ping().await?;

    // Arm both faults BEFORE servicing so that the post-servicing
    // CREATE_IO_COMPLETION_QUEUE is what each fault matches.
    with_keepalive_fault_updater.set(true).await;
    no_keepalive_fault_updater.set(true).await;

    vm.restart_openhcl(igvm_file.clone(), flags).await?;

    agent.ping().await?;

    // The non-c05b device must issue CREATE_IO_COMPLETION_QUEUE after
    // servicing because its keepalive was downgraded to reset.
    CancelContext::new()
        .with_timeout(Duration::from_secs(60))
        .until_cancelled(no_keepalive_create_seen_recv)
        .await
        .expect(
            "non-c05b NVMe device did not issue CREATE_IO_COMPLETION_QUEUE within 60s after \
             servicing — the per-device keepalive gate did not downgrade keepalive for this \
             device.",
        )
        .expect("CREATE_IO_COMPLETION_QUEUE verification on non-c05b device failed");

    // If the c05b device had received CREATE_IO_COMPLETION_QUEUE, the
    // panic fault would have crashed the VM and failed this test. We
    // disarm the faults defensively before tearing down.
    with_keepalive_fault_updater.set(false).await;
    no_keepalive_fault_updater.set(false).await;

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
    let (mut vm, agent) = create_keepalive_test_config_default(
        config,
        fault_configuration,
        VTL0_NVME_LUN,
        Guid::new_random(),
        DEFAULT_DISK_SIZE,
    )
    .await?;

    agent.ping().await?;
    let sh = agent.unix_shell();

    // Make sure the disk showed up.
    cmd!(sh, "ls /dev/sda").run().await?;

    fault_start_updater.set(true).await;

    if let Some(cmdline) = new_cmdline {
        vm.update_command_line(cmdline).await?;
    }

    vm.restart_openhcl(igvm_file.clone(), flags).await?;

    // Ensure the agent is responsive after the restart before returning.
    agent.ping().await?;

    fault_start_updater.set(false).await;
    Ok(vm)
}

async fn create_keepalive_test_config_default(
    config: PetriVmBuilder<OpenVmmPetriBackend>,
    fault_configuration: FaultConfiguration,
    vtl0_nvme_lun: u32,
    scsi_instance: Guid,
    disk_size: u64,
) -> Result<(PetriVm<OpenVmmPetriBackend>, PipetteClient), anyhow::Error> {
    const NVME_INSTANCE: Guid = guid::guid!("dce4ebad-182f-46c0-8d30-8446c1c62ab3");

    config
        .with_vmbus_redirect(true)
        .with_openhcl_command_line(
            "OPENHCL_ENABLE_VTL2_GPA_POOL=512 OPENHCL_DISABLE_NVME_KEEP_ALIVE=0",
        )
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
                                len: Some(disk_size),
                                sector_size: None,
                            })
                            .into_resource(),
                        }],
                        fault_config: fault_configuration,
                        enable_tdisp_tests: false,
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

/// Creates a keepalive test config with a custom number of VPs with 1 VP per
/// socket. It also creates an appropriate number of scsi sub-channels to ensure
/// IO can be issued on all VPs.
async fn create_keepalive_test_config_custom_vps(
    config: PetriVmBuilder<OpenVmmPetriBackend>,
    fault_configuration: FaultConfiguration,
    vtl0_nvme_lun: u32,
    scsi_instance: Guid,
    disk_size: u64,
    vp_count: u32,
) -> Result<(PetriVm<OpenVmmPetriBackend>, PipetteClient), anyhow::Error> {
    create_keepalive_test_config_custom(
        config,
        fault_configuration,
        vtl0_nvme_lun,
        scsi_instance,
        disk_size,
        vp_count,
        None,
        &[],
        10,
        10,
    )
    .await
}

/// Creates a keepalive test config with full control over topology, NVMe queue
/// counts, and extra OpenHCL command line arguments.
async fn create_keepalive_test_config_custom(
    config: PetriVmBuilder<OpenVmmPetriBackend>,
    fault_configuration: FaultConfiguration,
    vtl0_nvme_lun: u32,
    scsi_instance: Guid,
    disk_size: u64,
    vp_count: u32,
    topology: Option<ProcessorTopology>,
    extra_cmdlines: &[&str],
    msix_count: u16,
    max_io_queues: u16,
) -> Result<(PetriVm<OpenVmmPetriBackend>, PipetteClient), anyhow::Error> {
    const NVME_INSTANCE: Guid = guid::guid!("dce4ebad-182f-46c0-8d30-8446c1c62ab3");

    let mut builder = config.with_vmbus_redirect(true).with_openhcl_command_line(
        "OPENHCL_ENABLE_VTL2_GPA_POOL=512 OPENHCL_DISABLE_NVME_KEEP_ALIVE=0",
    );

    for cmdline in extra_cmdlines {
        builder = builder.with_openhcl_command_line(cmdline);
    }

    builder
        .with_processor_topology(topology.unwrap_or(ProcessorTopology {
            vp_count,
            vps_per_socket: Some(1),
            ..Default::default()
        }))
        .modify_backend(move |b| {
            b.with_custom_config(|c| {
                c.vpci_devices.push(VpciDeviceConfig {
                    vtl: DeviceVtl::Vtl2,
                    instance_id: NVME_INSTANCE,
                    resource: NvmeFaultControllerHandle {
                        subsystem_id: Guid::new_random(),
                        msix_count,
                        max_io_queues,
                        namespaces: vec![NamespaceDefinition {
                            nsid: KEEPALIVE_VTL2_NSID,
                            read_only: false,
                            disk: LayeredDiskHandle::single_layer(RamDiskLayerHandle {
                                len: Some(disk_size),
                                sector_size: None,
                            })
                            .into_resource(),
                        }],
                        fault_config: fault_configuration,
                        enable_tdisp_tests: false,
                    }
                    .into_resource(),
                })
            })
        })
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
        .with_custom_vtl2_settings(move |v| {
            if v.fixed.is_none() {
                v.fixed = Some(Default::default());
            }

            // Configure SCSI so there are as many total channels as vCPUs to
            // allow IO on all CPUs. The scsi_sub_channels counts beyond the first
            // channel which is always present. so vp_count - 1 yields a total
            // of vp_count channels.
            assert!(
                vp_count >= 1,
                "vp_count must be at least 1 when configuring SCSI sub-channels"
            );
            v.fixed.as_mut().unwrap().scsi_sub_channels = Some(vp_count - 1);
        })
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

// Reads a large chunk from the disk, generating lots of concurrent IOs on the
// submission queue.
async fn large_read_from_disk(
    agent: &PipetteClient,
    disk_path: &str,
) -> Result<Child, anyhow::Error> {
    let mut io_cmd = agent.command("sh");

    let cmd = format!(
        "dd if={} of=/dev/null bs=10M iflag=direct status=none",
        disk_path
    );

    io_cmd
        .args(["-c", &cmd])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let io_child = io_cmd.spawn().await?;
    Ok(io_child)
}

// Runs IO using dd that is pinned to a specific target CPU using cpuset
// cgroups.
// DEV NOTE: This approach fails when there is more than 1 processor assigned
// per socket. The failure itself is seen as an off-by-one error when observing
// which CPU gets the pinned IO.
async fn run_cpu_pinned_io(
    agent: &PipetteClient,
    disk_path: &str,
    target_cpu: u32,
) -> Result<(), anyhow::Error> {
    let sh = agent.unix_shell();

    // Mount sysfs and cgroup2, then create a cpuset cgroup to pin IO to
    // the target CPU. The minimal Alpine initrd has neither `taskset` nor
    // sysfs mounted, so we set this up manually.
    let setup_cpuset = format!(
        "mount -t sysfs none /sys 2>/dev/null || true; \
         mkdir -p /sys/fs/cgroup; \
         mount -t cgroup2 none /sys/fs/cgroup 2>/dev/null || true; \
         echo '+cpuset' > /sys/fs/cgroup/cgroup.subtree_control 2>/dev/null || true; \
         mkdir -p /sys/fs/cgroup/pin{target_cpu}; \
         echo {target_cpu} > /sys/fs/cgroup/pin{target_cpu}/cpuset.cpus; \
         echo 0 > /sys/fs/cgroup/pin{target_cpu}/cpuset.mems"
    );
    cmd!(sh, "sh -c {setup_cpuset}").run().await?;

    // Force IO on the target CPU. This CPU has no IO queue, so the NVMe
    // driver will call create_io_queue() to create one via admin commands.
    // The dd command must complete successfully — a hang here means the admin
    // command didn't complete (the exact production failure scenario).
    let pinned_dd = format!(
        "echo $$ > /sys/fs/cgroup/pin{target_cpu}/cgroup.procs; \
         dd if={disk_path} of=/dev/null bs=4k count=256 iflag=direct status=none"
    );
    CancelContext::new()
        .with_timeout(Duration::from_secs(60))
        .until_cancelled(cmd!(sh, "sh -c {pinned_dd}").run())
        .await
        .expect("IO on target CPU did not complete within 60 seconds after keepalive restore. create_io_queue may be stuck.")
        .expect("dd command failed");

    Ok(())
}

// Uses inspect to find and return the CPU indices (per-CPU map keys) that have
// IO issuers in the NVMe driver. Proto queues are not reported in the returned
// value.
async fn find_cpus_with_io_issuers(
    vm: &PetriVm<OpenVmmPetriBackend>,
) -> Result<Vec<u32>, anyhow::Error> {
    // Query inspect to find which CPUs have IO issuers after boot.
    // Only CPUs with initialized issuers appear in the per_cpu map (unset
    // OnceLock entries are absent). This makes the test deterministic —
    // we guarantee we target a CPU that triggers create_io_queue().
    let devices = vm.inspect_openhcl("vm/nvme/devices", None, None).await?;
    let devices: serde_json::Value = serde_json::from_str(&format!("{}", devices.json()))?;
    let device = devices
        .as_object()
        .expect("inspect path 'vm/nvme/devices' did not yield a JSON object; NVMe inspect schema may have changed")
        .values()
        .next()
        .expect("no NVMe devices found under inspect path 'vm/nvme/devices'; device list is empty");
    let per_cpu = &device["driver"]["driver"]["io_issuers"]["per_cpu"];
    let per_cpu_map = per_cpu
        .as_object()
        .expect("inspect field 'driver.driver.io_issuers.per_cpu' is not a JSON object; NVMe inspect schema may have changed");
    let cpu_indices = per_cpu_map
        .keys()
        .map(|key| key.parse::<u32>())
        .collect::<Result<Vec<u32>, _>>()?;
    Ok(cpu_indices)
}
