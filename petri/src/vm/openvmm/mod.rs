// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Code managing the lifetime of a `PetriVmOpenVmm`. All VMs live the same lifecycle:
//! * A `PetriVmConfigOpenVmm` is built for the given firmware and architecture in `construct`.
//! * The configuration is optionally modified from the defaults using the helpers in `modify`.
//! * The `PetriVmOpenVmm` is started by the code in `start`.
//! * The VM is interacted with through the methods in `runtime`.
//! * The VM is either shut down by the code in `runtime`, or gets dropped and cleaned up automatically.

mod construct;
mod modify;
mod runtime;
mod start;

pub use runtime::OpenVmmFramebufferAccess;
pub use runtime::OpenVmmInspector;
pub use runtime::PetriVmOpenVmm;

use crate::BootDeviceType;
use crate::Firmware;
use crate::OpenHclServicingFlags;
use crate::OpenvmmLogConfig;
use crate::PetriDiskType;
use crate::PetriLogFile;
use crate::PetriVmConfig;
use crate::PetriVmResources;
use crate::PetriVmRuntimeConfig;
use crate::PetriVmgsDisk;
use crate::PetriVmgsResource;
use crate::PetriVmmBackend;
use crate::VmmQuirks;
use crate::disk_image::AgentImage;
use crate::linux_direct_serial_agent::LinuxDirectSerialAgent;
use anyhow::Context;
use async_trait::async_trait;
use disk_backend_resources::LayeredDiskHandle;
use disk_backend_resources::layer::DiskLayerHandle;
use disk_backend_resources::layer::RamDiskLayerHandle;
use get_resources::ged::FirmwareEvent;
use guid::Guid;
use hyperv_ic_resources::shutdown::ShutdownRpc;
use mesh::Receiver;
use mesh::Sender;
use net_backend_resources::mac_address::MacAddress;
use openvmm_defs::config::Config;
use openvmm_helpers::disk::open_disk_type;
use pal_async::DefaultDriver;
use pal_async::socket::PolledSocket;
use pal_async::task::Task;
use petri_artifacts_common::tags::GuestQuirksInner;
use petri_artifacts_common::tags::MachineArch;
use petri_artifacts_common::tags::OsFlavor;
use petri_artifacts_core::ArtifactResolver;
use petri_artifacts_core::ResolvedArtifact;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use storvsp_resources::ScsiControllerHandle;
use tempfile::TempPath;
use unix_socket::UnixListener;
use vm_resource::IntoResource;
use vm_resource::Resource;
use vm_resource::kind::DiskHandleKind;
use vmgs_resources::VmgsDisk;
use vmgs_resources::VmgsResource;
use vtl2_settings_proto::Vtl2Settings;

/// The instance guid used for all of our SCSI drives.
pub(crate) const SCSI_INSTANCE: Guid = guid::guid!("27b553e8-8b39-411b-a55f-839971a7884f");

/// The instance guid for the NVMe controller automatically added for boot media
/// for paravisor storage translation.
pub(crate) const PARAVISOR_BOOT_NVME_INSTANCE: Guid =
    guid::guid!("92bc8346-718b-449a-8751-edbf3dcd27e4");

/// The instance guid for the NVMe controller automatically added for boot media.
pub(crate) const BOOT_NVME_INSTANCE: Guid = guid::guid!("e23a04e2-90f5-4852-bc9d-e7ac691b756c");

/// The instance guid for the MANA nic automatically added when specifying `PetriVmConfigOpenVmm::with_nic`
const MANA_INSTANCE: Guid = guid::guid!("f9641cf4-d915-4743-a7d8-efa75db7b85a");

/// The namespace ID for the NVMe controller automatically added for boot media.
pub(crate) const BOOT_NVME_NSID: u32 = 37;

/// The LUN ID for the NVMe controller automatically added for boot media.
pub(crate) const BOOT_NVME_LUN: u32 = 1;

/// The MAC address used by the NIC assigned with [`PetriVmConfigOpenVmm::with_nic`].
pub const NIC_MAC_ADDRESS: MacAddress = MacAddress::new([0x00, 0x15, 0x5D, 0x12, 0x12, 0x12]);

/// OpenVMM Petri Backend
pub struct OpenVmmPetriBackend {
    openvmm_path: ResolvedArtifact,
}

#[async_trait]
impl PetriVmmBackend for OpenVmmPetriBackend {
    type VmmConfig = PetriVmConfigOpenVmm;
    type VmRuntime = PetriVmOpenVmm;

    fn check_compat(firmware: &Firmware, arch: MachineArch) -> bool {
        arch == MachineArch::host()
            && !(firmware.is_openhcl() && (!cfg!(windows) || arch == MachineArch::Aarch64))
            && !(firmware.is_pcat() && arch == MachineArch::Aarch64)
    }

    fn quirks(firmware: &Firmware) -> (GuestQuirksInner, VmmQuirks) {
        (
            firmware.quirks().openvmm,
            VmmQuirks {
                // Workaround for #1684
                flaky_boot: firmware.is_pcat().then_some(Duration::from_secs(15)),
            },
        )
    }

    fn default_servicing_flags() -> OpenHclServicingFlags {
        OpenHclServicingFlags {
            enable_nvme_keepalive: true,
            enable_mana_keepalive: true,
            override_version_checks: false,
            stop_timeout_hint_secs: None,
        }
    }

    fn create_guest_dump_disk() -> anyhow::Result<
        Option<(
            Arc<TempPath>,
            Box<dyn FnOnce() -> anyhow::Result<Box<dyn fatfs::ReadWriteSeek>>>,
        )>,
    > {
        Ok(None) // TODO #2403
    }

    fn new(resolver: &ArtifactResolver<'_>) -> Self {
        OpenVmmPetriBackend {
            openvmm_path: resolver
                .require(petri_artifacts_vmm_test::artifacts::OPENVMM_NATIVE)
                .erase(),
        }
    }

    async fn run(
        self,
        config: PetriVmConfig,
        modify_vmm_config: Option<impl FnOnce(PetriVmConfigOpenVmm) -> PetriVmConfigOpenVmm + Send>,
        resources: &PetriVmResources,
    ) -> anyhow::Result<(Self::VmRuntime, PetriVmRuntimeConfig)> {
        let mut config = PetriVmConfigOpenVmm::new(&self.openvmm_path, config, resources)?;

        if let Some(f) = modify_vmm_config {
            config = f(config);
        }

        config.run().await
    }
}

/// Configuration state for a test VM.
pub struct PetriVmConfigOpenVmm {
    // Direct configuration related information.
    firmware: Firmware,
    arch: MachineArch,
    host_log_levels: Option<OpenvmmLogConfig>,
    config: Config,
    boot_device_type: BootDeviceType,

    // Runtime resources
    resources: PetriVmResourcesOpenVmm,

    // Logging
    openvmm_log_file: PetriLogFile,

    // Resources that are only used during startup.
    /// Single VMBus SCSI controller shared for all VTL0 disks added by petri.
    petri_vtl0_scsi: ScsiControllerHandle,

    ged: Option<get_resources::ged::GuestEmulationDeviceHandle>,
    framebuffer_view: Option<framebuffer::View>,

    vtl2_settings: Option<Vtl2Settings>,
}
/// Various channels and resources used to interact with the VM while it is running.
struct PetriVmResourcesOpenVmm {
    log_stream_tasks: Vec<Task<anyhow::Result<()>>>,
    firmware_event_recv: Receiver<FirmwareEvent>,
    shutdown_ic_send: Sender<ShutdownRpc>,
    kvp_ic_send: Sender<hyperv_ic_resources::kvp::KvpConnectRpc>,
    ged_send: Option<Sender<get_resources::ged::GuestEmulationRequest>>,
    pipette_listener: PolledSocket<UnixListener>,
    vtl2_pipette_listener: Option<PolledSocket<UnixListener>>,
    linux_direct_serial_agent: Option<LinuxDirectSerialAgent>,

    // Externally injected management stuff also needed at runtime.
    driver: DefaultDriver,
    agent_image: Option<AgentImage>,
    openhcl_agent_image: Option<AgentImage>,
    openvmm_path: ResolvedArtifact,
    output_dir: PathBuf,

    // TempPaths that cannot be dropped until the end.
    vtl2_vsock_path: Option<TempPath>,
    _vmbus_vsock_path: TempPath,
}

impl PetriVmConfigOpenVmm {
    /// Get the OS that the VM will boot into.
    pub fn os_flavor(&self) -> OsFlavor {
        self.firmware.os_flavor()
    }
}

fn memdiff_disk(path: &Path) -> anyhow::Result<Resource<DiskHandleKind>> {
    let disk = open_disk_type(path, true)
        .with_context(|| format!("failed to open disk: {}", path.display()))?;
    Ok(LayeredDiskHandle {
        layers: vec![
            RamDiskLayerHandle { len: None }.into_resource().into(),
            DiskLayerHandle(disk).into_resource().into(),
        ],
    }
    .into_resource())
}

fn memdiff_vmgs(vmgs: &PetriVmgsResource) -> anyhow::Result<VmgsResource> {
    let convert_disk = |disk: &PetriVmgsDisk| -> anyhow::Result<VmgsDisk> {
        Ok(VmgsDisk {
            disk: match &disk.disk {
                PetriDiskType::Memory => LayeredDiskHandle::single_layer(RamDiskLayerHandle {
                    len: Some(vmgs_format::VMGS_DEFAULT_CAPACITY),
                })
                .into_resource(),
                PetriDiskType::Differencing(path) => memdiff_disk(path)?,
                PetriDiskType::Persistent(path) => open_disk_type(path, false)?,
            },
            encryption_policy: disk.encryption_policy,
        })
    };

    Ok(match vmgs {
        PetriVmgsResource::Disk(disk) => VmgsResource::Disk(convert_disk(disk)?),
        PetriVmgsResource::ReprovisionOnFailure(disk) => {
            VmgsResource::ReprovisionOnFailure(convert_disk(disk)?)
        }
        PetriVmgsResource::Reprovision(disk) => VmgsResource::Reprovision(convert_disk(disk)?),
        PetriVmgsResource::Ephemeral => VmgsResource::Ephemeral,
    })
}
