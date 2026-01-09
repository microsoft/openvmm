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

use crate::Disk;
use crate::Firmware;
use crate::ModifyFn;
use crate::OpenHclServicingFlags;
use crate::OpenvmmLogConfig;
use crate::PetriLogFile;
use crate::PetriVmConfig;
use crate::PetriVmResources;
use crate::PetriVmRuntimeConfig;
use crate::PetriVmgsDisk;
use crate::PetriVmgsResource;
use crate::PetriVmmBackend;
use crate::VmmQuirks;
use crate::linux_direct_serial_agent::LinuxDirectSerialAgent;
use crate::vm::PetriVmProperties;
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
use petri_artifacts_core::ArtifactResolver;
use petri_artifacts_core::ResolvedArtifact;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tempfile::TempPath;
use unix_socket::UnixListener;
use vm_resource::IntoResource;
use vm_resource::Resource;
use vm_resource::kind::DiskHandleKind;
use vmgs_resources::VmgsDisk;
use vmgs_resources::VmgsResource;

/// The instance guid for the MANA nic automatically added when specifying `PetriVmConfigOpenVmm::with_nic`
const MANA_INSTANCE: Guid = guid::guid!("f9641cf4-d915-4743-a7d8-efa75db7b85a");

/// The MAC address used by the NIC assigned with [`PetriVmConfigOpenVmm::with_nic`].
pub const NIC_MAC_ADDRESS: MacAddress = MacAddress::new([0x00, 0x15, 0x5D, 0x12, 0x12, 0x12]);

/// OpenVMM Petri Backend
#[derive(Debug)]
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
            enable_mana_keepalive: false,
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
        modify_vmm_config: Option<ModifyFn<Self::VmmConfig>>,
        resources: &PetriVmResources,
        properties: PetriVmProperties,
    ) -> anyhow::Result<(Self::VmRuntime, PetriVmRuntimeConfig)> {
<<<<<<< HEAD
        let mut config = PetriVmConfigOpenVmm::new(&self.openvmm_path, config, resources)?;
=======
        let mut config =
            PetriVmConfigOpenVmm::new(&self.openvmm_path, config, resources, properties).await?;
>>>>>>> 5b9004e7 (petri: backend agnostic additional disk configuration (#2551))

        if let Some(f) = modify_vmm_config {
            config = f.0(config);
        }

        config.run().await
    }
}

/// Configuration state for a test VM.
pub struct PetriVmConfigOpenVmm {
    // Direct configuration related information.
    runtime_config: PetriVmRuntimeConfig,
    arch: MachineArch,
    host_log_levels: Option<OpenvmmLogConfig>,
    config: Config,

    // Runtime resources
    resources: PetriVmResourcesOpenVmm,

    // Logging
    openvmm_log_file: PetriLogFile,

    // Resources that are only used during startup.
    ged: Option<get_resources::ged::GuestEmulationDeviceHandle>,
    framebuffer_view: Option<framebuffer::View>,
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
    openvmm_path: ResolvedArtifact,
    output_dir: PathBuf,

    // TempPaths that cannot be dropped until the end.
    vtl2_vsock_path: Option<TempPath>,
    _vmbus_vsock_path: TempPath,

    // properties needed at runtime
    properties: PetriVmProperties,
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
            disk: petri_disk_to_openvmm(&disk.disk)?,
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

fn petri_disk_to_openvmm(disk: &Disk) -> anyhow::Result<Resource<DiskHandleKind>> {
    Ok(match disk {
        Disk::Memory(len) => {
            LayeredDiskHandle::single_layer(RamDiskLayerHandle { len: Some(*len) }).into_resource()
        }
        Disk::Differencing(path) => memdiff_disk(path)?,
        Disk::Persistent(path) => open_disk_type(path.as_ref(), false)?,
        Disk::Temporary(path) => open_disk_type(path.as_ref(), false)?,
    })
}
