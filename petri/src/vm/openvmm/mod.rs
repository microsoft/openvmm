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

pub use runtime::PetriVmOpenVmm;

use crate::disk_image::AgentImage;
use crate::linux_direct_serial_agent::LinuxDirectSerialAgent;
use crate::openhcl_diag::OpenHclDiagHandler;
use crate::Firmware;
use crate::PetriLogFile;
use crate::PetriLogSource;
use crate::PetriVm;
use crate::PetriVmConfig;
use async_trait::async_trait;
use framebuffer::FramebufferAccess;
use get_resources::ged::FirmwareEvent;
use guid::Guid;
use hvlite_defs::config::Config;
use hyperv_ic_resources::shutdown::ShutdownRpc;
use mesh::MpscReceiver;
use mesh::Sender;
use pal_async::socket::PolledSocket;
use pal_async::task::Task;
use pal_async::DefaultDriver;
use petri_artifacts_common::tags::MachineArch;
use petri_artifacts_common::tags::OsFlavor;
use petri_artifacts_core::ArtifactResolver;
use petri_artifacts_core::ResolvedArtifact;
use pipette_client::PipetteClient;
use std::path::PathBuf;
use std::sync::Arc;
use tempfile::TempPath;
use unix_socket::UnixListener;
use vtl2_settings_proto::Vtl2Settings;

/// The instance guid used for all of our SCSI drives.
pub(crate) const SCSI_INSTANCE: Guid =
    Guid::from_static_str("27b553e8-8b39-411b-a55f-839971a7884f");

/// The instance guid for the NVMe controller automatically added for boot media.
pub(crate) const BOOT_NVME_INSTANCE: Guid =
    Guid::from_static_str("92bc8346-718b-449a-8751-edbf3dcd27e4");

/// The instance guid for the MANA nic automatically added when specifying `PetriVmConfigOpenVmm::with_nic`
const MANA_INSTANCE: Guid = Guid::from_static_str("f9641cf4-d915-4743-a7d8-efa75db7b85a");

/// The namespace ID for the NVMe controller automatically added for boot media.
pub(crate) const BOOT_NVME_NSID: u32 = 37;

/// The LUN ID for the NVMe controller automatically added for boot media.
pub(crate) const BOOT_NVME_LUN: u32 = 1;

/// The set of artifacts and resources needed to instantiate a
/// [`PetriVmConfigOpenVmm`].
pub struct PetriVmArtifactsOpenVmm {
    firmware: Firmware,
    arch: MachineArch,
    agent_image: AgentImage,
    openhcl_agent_image: Option<AgentImage>,
    openvmm_path: ResolvedArtifact,
    openhcl_dump_directory: ResolvedArtifact,
}

impl PetriVmArtifactsOpenVmm {
    /// Resolves the artifacts needed to instantiate a [`PetriVmConfigOpenVmm`].
    pub fn new(resolver: &ArtifactResolver<'_>, firmware: Firmware, arch: MachineArch) -> Self {
        let agent_image = AgentImage::new(resolver, arch, firmware.os_flavor());
        let openhcl_agent_image = if firmware.is_openhcl() {
            Some(AgentImage::new(resolver, arch, OsFlavor::Linux))
        } else {
            None
        };
        Self {
            firmware,
            arch,
            agent_image,
            openhcl_agent_image,
            openvmm_path: resolver
                .require(petri_artifacts_vmm_test::artifacts::OPENVMM_NATIVE)
                .erase(),
            openhcl_dump_directory: resolver
                .require(petri_artifacts_vmm_test::artifacts::OPENHCL_DUMP_DIRECTORY)
                .erase(),
        }
    }
}

/// Configuration state for a test VM.
pub struct PetriVmConfigOpenVmm {
    // Direct configuration related information.
    firmware: Firmware,
    arch: MachineArch,
    config: Config,

    // Runtime resources
    resources: PetriVmResourcesOpenVmm,

    // Logging
    openvmm_log_file: PetriLogFile,

    // Resources that are only used during startup.
    ged: Option<get_resources::ged::GuestEmulationDeviceHandle>,
    vtl2_settings: Option<Vtl2Settings>,
    framebuffer_access: Option<FramebufferAccess>,
}

#[async_trait]
impl PetriVmConfig for PetriVmConfigOpenVmm {
    async fn run_without_agent(self: Box<Self>) -> anyhow::Result<Box<dyn PetriVm>> {
        Ok(Box::new(Self::run_without_agent(*self).await?))
    }

    async fn run_with_lazy_pipette(mut self: Box<Self>) -> anyhow::Result<Box<dyn PetriVm>> {
        Ok(Box::new(Self::run_with_lazy_pipette(*self).await?))
    }

    async fn run(self: Box<Self>) -> anyhow::Result<(Box<dyn PetriVm>, PipetteClient)> {
        let (vm, client) = Self::run(*self).await?;
        Ok((Box::new(vm), client))
    }
}

/// Various channels and resources used to interact with the VM while it is running.
struct PetriVmResourcesOpenVmm {
    serial_tasks: Vec<Task<anyhow::Result<()>>>,
    firmware_event_recv: MpscReceiver<FirmwareEvent>,
    shutdown_ic_send: Sender<ShutdownRpc>,
    expected_boot_event: Option<FirmwareEvent>,
    ged_send: Option<Arc<Sender<get_resources::ged::GuestEmulationRequest>>>,
    pipette_listener: PolledSocket<UnixListener>,
    vtl2_pipette_listener: Option<PolledSocket<UnixListener>>,
    openhcl_diag_handler: Option<OpenHclDiagHandler>,
    linux_direct_serial_agent: Option<LinuxDirectSerialAgent>,

    // Externally injected management stuff also needed at runtime.
    driver: DefaultDriver,
    agent_image: AgentImage,
    openhcl_agent_image: Option<AgentImage>,
    openvmm_path: ResolvedArtifact,
    output_dir: PathBuf,
    log_source: PetriLogSource,

    // Resources that are only kept so they can be dropped at the end.
    _vsock_temp_paths: Vec<TempPath>,
}

impl PetriVmConfigOpenVmm {
    /// Get the OS that the VM will boot into.
    pub fn os_flavor(&self) -> OsFlavor {
        self.firmware.os_flavor()
    }
}
