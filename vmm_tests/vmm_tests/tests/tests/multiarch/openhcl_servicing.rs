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
use mesh::CellUpdater;
use nvme_resources::NamespaceDefinition;
use nvme_resources::NvmeFaultControllerHandle;
use nvme_resources::fault::AdminQueueFaultConfig;
use nvme_resources::fault::FaultConfiguration;
use nvme_resources::fault::PciFaultConfig;
use nvme_resources::fault::QueueFaultBehavior;
use petri::OpenHclServicingFlags;
use petri::PetriVmBuilder;
use petri::PetriVmmBackend;
use petri::ResolvedArtifact;
use petri::openvmm::OpenVmmPetriBackend;
use petri::pipette::cmd;
#[allow(unused_imports)]
use petri_artifacts_vmm_test::artifacts::openhcl_igvm::LATEST_LINUX_DIRECT_TEST_X64;
#[allow(unused_imports)]
use petri_artifacts_vmm_test::artifacts::openhcl_igvm::LATEST_STANDARD_AARCH64;
#[allow(unused_imports)]
use petri_artifacts_vmm_test::artifacts::openhcl_igvm::LATEST_STANDARD_X64;
use scsidisk_resources::SimpleScsiDiskHandle;
use storvsp_resources::ScsiControllerHandle;
use storvsp_resources::ScsiDeviceAndPath;
use storvsp_resources::ScsiPath;
use vm_resource::IntoResource;
use vmm_test_macros::openvmm_test;
use vmm_test_macros::vmm_test;

// TODO: Move this host query logic into common code so that we can instead
// filter tests based on host capabilities.
pub(crate) fn host_supports_servicing() -> bool {
    cfg_if::cfg_if! {
        // xtask-fmt allow-target-arch cpu-intrinsic
        if #[cfg(all(target_arch = "x86_64", target_os = "windows"))] {
            // Check if this is a nested host and AMD. WHP partition scrub has a bug
            // on AMD nested which can result in flakey tests. Query this via CPUID.
            !is_amd_nested_via_cpuid()
        } else {
            true
        }
    }
}

// xtask-fmt allow-target-arch cpu-intrinsic
#[cfg(all(target_arch = "x86_64", target_os = "windows"))]
fn is_amd_nested_via_cpuid() -> bool {
    let is_nested = {
        let result =
            safe_intrinsics::cpuid(hvdef::HV_CPUID_FUNCTION_MS_HV_ENLIGHTENMENT_INFORMATION, 0);
        hvdef::HvEnlightenmentInformation::from(
            result.eax as u128
                | (result.ebx as u128) << 32
                | (result.ecx as u128) << 64
                | (result.edx as u128) << 96,
        )
        .nested()
    };

    let vendor = {
        let result =
            safe_intrinsics::cpuid(x86defs::cpuid::CpuidFunction::VendorAndMaxFunction.0, 0);
        x86defs::cpuid::Vendor::from_ebx_ecx_edx(result.ebx, result.ecx, result.edx)
    };

    is_nested && vendor.is_amd_compatible()
}

async fn openhcl_servicing_core<T: PetriVmmBackend>(
    config: PetriVmBuilder<T>,
    openhcl_cmdline: &str,
    new_openhcl: ResolvedArtifact<impl petri_artifacts_common::tags::IsOpenhclIgvm>,
    flags: OpenHclServicingFlags,
) -> anyhow::Result<()> {
    let (mut vm, agent) = config
        .with_openhcl_command_line(openhcl_cmdline)
        .run()
        .await?;

    for _ in 0..3 {
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
    //hyperv_openhcl_uefi_x64(vhd(ubuntu_2204_server_x64))[LATEST_STANDARD_X64],
    hyperv_openhcl_uefi_aarch64(vhd(ubuntu_2404_server_aarch64))[LATEST_STANDARD_AARCH64]
)]
async fn basic<T: PetriVmmBackend>(
    config: PetriVmBuilder<T>,
    (igvm_file,): (ResolvedArtifact<impl petri_artifacts_common::tags::IsOpenhclIgvm>,),
) -> Result<(), anyhow::Error> {
    if !host_supports_servicing() {
        tracing::info!("skipping OpenHCL servicing test on unsupported host");
        return Ok(());
    }

    openhcl_servicing_core(
        config,
        "",
        igvm_file,
        OpenHclServicingFlags {
            override_version_checks: true,
            ..Default::default()
        },
    )
    .await
}

/// Test servicing an OpenHCL VM from the current version to itself
/// with NVMe keepalive support.
#[openvmm_test(openhcl_linux_direct_x64 [LATEST_LINUX_DIRECT_TEST_X64])]
async fn keepalive<T: PetriVmmBackend>(
    config: PetriVmBuilder<T>,
    (igvm_file,): (ResolvedArtifact<impl petri_artifacts_common::tags::IsOpenhclIgvm>,),
) -> Result<(), anyhow::Error> {
    if !host_supports_servicing() {
        tracing::info!("skipping OpenHCL servicing test on unsupported host");
        return Ok(());
    }

    openhcl_servicing_core(
        config,
        "OPENHCL_ENABLE_VTL2_GPA_POOL=512 OPENHCL_SIDECAR=off", // disable sidecar until #1345 is fixed
        igvm_file,
        OpenHclServicingFlags {
            enable_nvme_keepalive: true,
            ..Default::default()
        },
    )
    .await
}

#[openvmm_test(openhcl_linux_direct_x64 [LATEST_LINUX_DIRECT_TEST_X64])]
async fn shutdown_ic(
    config: PetriVmBuilder<OpenVmmPetriBackend>,
    (igvm_file,): (ResolvedArtifact<impl petri_artifacts_common::tags::IsOpenhclIgvm>,),
) -> Result<(), anyhow::Error> {
    if !host_supports_servicing() {
        tracing::info!("skipping OpenHCL servicing test on unsupported host");
        return Ok(());
    }

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

/// Test servicing an OpenHCL VM from the current version to itself
/// with NVMe keepalive support and a faulty controller that drops CREATE_IO_COMPLETION_QUEUE commands
#[openvmm_test(openhcl_linux_direct_x64 [LATEST_LINUX_DIRECT_TEST_X64])]
async fn keepalive_with_nvme_fault(
    config: PetriVmBuilder<OpenVmmPetriBackend>,
    (igvm_file,): (ResolvedArtifact<impl petri_artifacts_common::tags::IsOpenhclIgvm>,),
) -> Result<(), anyhow::Error> {
    const NVME_INSTANCE: Guid = guid::guid!("dce4ebad-182f-46c0-8d30-8446c1c62ab3");
    let vtl0_nvme_lun = 1;
    let vtl2_nsid = 37; // Pick any namespace ID as long as it doesn't conflict with other namespaces in the controller
    let scsi_instance = Guid::new_random();

    if !host_supports_servicing() {
        tracing::info!("skipping OpenHCL servicing test on unsupported host");
        return Ok(());
    }

    let mut fault_start_updater = CellUpdater::new(false);

    let fault_configuration = FaultConfiguration {
        fault_active: fault_start_updater.cell(),
        admin_fault: AdminQueueFaultConfig::new().with_submission_queue_fault(
            nvme_spec::AdminOpcode::CREATE_IO_COMPLETION_QUEUE.0,
            QueueFaultBehavior::Panic("Received a CREATE_IO_COMPLETION_QUEUE command during servicing with keepalive enabled. THERE IS A BUG SOMEWHERE.".to_string()),
        ),
        pci_fault: PciFaultConfig::new(),
    };

    let (mut vm, agent) = config
        .with_vmbus_redirect(true)
        .with_openhcl_command_line("OPENHCL_ENABLE_VTL2_GPA_POOL=512 OPENHCL_SIDECAR=off") // disable sidecar until #1345 is fixed
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
                            nsid: vtl2_nsid,
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
                    vtl2_settings_proto::StorageController {
                        instance_id: scsi_instance.to_string(),
                        protocol: vtl2_settings_proto::storage_controller::StorageProtocol::Scsi
                            .into(),
                        luns: vec![vtl2_settings_proto::Lun {
                            location: vtl0_nvme_lun,
                            device_id: Guid::new_random().to_string(),
                            vendor_id: "OpenVMM".to_string(),
                            product_id: "Disk".to_string(),
                            product_revision_level: "1.0".to_string(),
                            serial_number: "0".to_string(),
                            model_number: "1".to_string(),
                            physical_devices: Some(vtl2_settings_proto::PhysicalDevices {
                                r#type: vtl2_settings_proto::physical_devices::BackingType::Single
                                    .into(),
                                device: Some(vtl2_settings_proto::PhysicalDevice {
                                    device_type:
                                        vtl2_settings_proto::physical_device::DeviceType::Nvme
                                            .into(),
                                    device_path: NVME_INSTANCE.to_string(),
                                    sub_device_path: vtl2_nsid,
                                }),
                                devices: Vec::new(),
                            }),
                            ..Default::default()
                        }],
                        io_queue_depth: None,
                    },
                )
            })
        })
        .run()
        .await?;
    agent.ping().await?;
    let sh = agent.unix_shell();

    // Make sure the disk showed up.
    cmd!(sh, "ls /dev/sda").run().await?;

    // CREATE_IO_COMPLETION_QUEUE is blocked. This will time out without keepalive enabled.
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
