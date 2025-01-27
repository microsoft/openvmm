// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Code managing the lifetime of a `PetriVm`. All VMs live the same lifecycle:
//! * A `PetriVmConfig` is built for the given firmware and architecture in `construct`.
//! * The configuration is optionally modified from the defaults using the helpers in `modify`.
//! * The `PetriVm` is started by the code in `start`.
//! * The VM is interacted with through the methods in `runtime`.
//! * The VM is either shut down by the code in `runtime`, or gets dropped and cleaned up automatically.

mod construct;
mod modify;
mod runtime;
mod start;

pub use runtime::PetriVm;

use crate::linux_direct_serial_agent::LinuxDirectSerialAgent;
use crate::openhcl_diag::OpenHclDiagHandler;
use framebuffer::FramebufferAccess;
use fs_err::File;
use get_resources::ged::FirmwareEvent;
use guid::Guid;
use hvlite_defs::config::Config;
use hvlite_defs::config::IsolationType;
use hyperv_ic_resources::shutdown::ShutdownRpc;
use mesh::MpscReceiver;
use mesh::Sender;
use pal_async::socket::PolledSocket;
use pal_async::task::Task;
use pal_async::DefaultDriver;
use petri_artifacts_common::tags::GuestQuirks;
use petri_artifacts_common::tags::MachineArch;
use petri_artifacts_common::tags::OsFlavor;
use petri_artifacts_core::ArtifactHandle;
use petri_artifacts_core::AsArtifactHandle;
use petri_artifacts_core::ErasedArtifactHandle;
use petri_artifacts_core::TestArtifacts;
use petri_artifacts_vmm_test::artifacts as hvlite_artifacts;
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

/// The namespace ID for the NVMe controller automatically added for boot media.
pub(crate) const BOOT_NVME_NSID: u32 = 37;

/// The LUN ID for the NVMe controller automatically added for boot media.
pub(crate) const BOOT_NVME_LUN: u32 = 1;

/// Configuration state for a test VM.
pub struct PetriVmConfig {
    // Direct configuration related information.
    firmware: Firmware,
    arch: MachineArch,
    config: Config,

    // Runtime resources
    resources: PetriVmResources,

    // Logging
    hvlite_log_file: File,

    // Resources that are only used during startup.
    ged: Option<get_resources::ged::GuestEmulationDeviceHandle>,
    vtl2_settings: Option<Vtl2Settings>,
    framebuffer_access: Option<FramebufferAccess>,
}

/// Various channels and resources used to interact with the VM while it is running.
struct PetriVmResources {
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
    resolver: TestArtifacts,
    output_dir: PathBuf,

    // Resources that are only kept so they can be dropped at the end.
    _vsock_temp_paths: Vec<TempPath>,
}

/// Firmware to load into the test VM.
#[derive(Debug)]
pub enum Firmware {
    /// Boot Linux directly, without any firmware.
    LinuxDirect,
    /// Boot Linux directly, without any firmware, with OpenHCL in VTL2.
    OpenhclLinuxDirect,
    /// Boot a PCAT-based VM.
    Pcat {
        /// The guest OS the VM will boot into.
        guest: PcatGuest,
    },
    /// Boot a UEFI-based VM.
    Uefi {
        /// The guest OS the VM will boot into.
        guest: UefiGuest,
    },
    /// Boot a UEFI-based VM with OpenHCL in VTL2.
    OpenhclUefi {
        /// The guest OS the VM will boot into.
        guest: UefiGuest,
        /// The isolation type of the VM.
        isolation: Option<IsolationType>,
        /// Emulate SCSI via NVME to VTL2, with the provided namespace ID on
        /// the controller with `BOOT_NVME_INSTANCE`.
        vtl2_nvme_boot: bool,
    },
}

impl Firmware {
    fn is_openhcl(&self) -> bool {
        match self {
            Firmware::OpenhclLinuxDirect { .. } | Firmware::OpenhclUefi { .. } => true,
            Firmware::LinuxDirect { .. } | Firmware::Pcat { .. } | Firmware::Uefi { .. } => false,
        }
    }

    fn isolation(&self) -> Option<IsolationType> {
        match self {
            Firmware::OpenhclUefi { isolation, .. } => *isolation,
            Firmware::LinuxDirect { .. }
            | Firmware::Pcat { .. }
            | Firmware::Uefi { .. }
            | Firmware::OpenhclLinuxDirect { .. } => None,
        }
    }

    fn is_linux_direct(&self) -> bool {
        match self {
            Firmware::LinuxDirect { .. } | Firmware::OpenhclLinuxDirect { .. } => true,
            Firmware::Pcat { .. } | Firmware::Uefi { .. } | Firmware::OpenhclUefi { .. } => false,
        }
    }

    fn is_uefi(&self) -> bool {
        match self {
            Firmware::Uefi { .. } | Firmware::OpenhclUefi { .. } => true,
            Firmware::LinuxDirect { .. }
            | Firmware::OpenhclLinuxDirect { .. }
            | Firmware::Pcat { .. } => false,
        }
    }

    fn os_flavor(&self) -> OsFlavor {
        match self {
            Firmware::LinuxDirect { .. } | Firmware::OpenhclLinuxDirect { .. } => OsFlavor::Linux,
            Firmware::Uefi {
                guest: UefiGuest::GuestTestUefi(_) | UefiGuest::None,
            }
            | Firmware::OpenhclUefi {
                guest: UefiGuest::GuestTestUefi(_) | UefiGuest::None,
                ..
            } => OsFlavor::Uefi,
            Firmware::Pcat {
                guest: PcatGuest::Vhd(cfg),
            }
            | Firmware::Uefi {
                guest: UefiGuest::Vhd(cfg),
            }
            | Firmware::OpenhclUefi {
                guest: UefiGuest::Vhd(cfg),
                ..
            } => cfg.os_flavor,
            Firmware::Pcat {
                guest: PcatGuest::Iso(cfg),
                ..
            } => cfg.os_flavor,
        }
    }

    fn quirks(&self) -> GuestQuirks {
        match self {
            Firmware::Pcat {
                guest: PcatGuest::Vhd(cfg),
                ..
            }
            | Firmware::Uefi {
                guest: UefiGuest::Vhd(cfg),
                ..
            }
            | Firmware::OpenhclUefi {
                guest: UefiGuest::Vhd(cfg),
                ..
            } => cfg.quirks,
            Firmware::Pcat {
                guest: PcatGuest::Iso(cfg),
                ..
            } => cfg.quirks,
            _ => Default::default(),
        }
    }
}

/// The guest the VM will boot into. A boot drive with the chosen setup
/// will be automatically configured.
#[derive(Debug)]
pub enum PcatGuest {
    /// Mount a VHD as the boot drive.
    Vhd(BootImageConfig<boot_image_type::Vhd>),
    /// Mount an ISO as the CD/DVD drive.
    Iso(BootImageConfig<boot_image_type::Iso>),
}

impl PcatGuest {
    fn artifact(&self) -> ErasedArtifactHandle {
        match self {
            PcatGuest::Vhd(disk) => disk.artifact,
            PcatGuest::Iso(disk) => disk.artifact,
        }
    }
}

/// The guest the VM will boot into. A boot drive with the chosen setup
/// will be automatically configured.
#[derive(Debug)]
pub enum UefiGuest {
    /// Mount a VHD as the boot drive.
    Vhd(BootImageConfig<boot_image_type::Vhd>),
    /// The UEFI test image produced by our guest-test infrastructure.
    GuestTestUefi(MachineArch),
    /// No guest, just the firmware.
    None,
}

impl UefiGuest {
    fn artifact(&self) -> ErasedArtifactHandle {
        match self {
            UefiGuest::Vhd(vhd) => vhd.artifact,
            UefiGuest::GuestTestUefi(a) => match a {
                MachineArch::X86_64 => hvlite_artifacts::test_vhd::GUEST_TEST_UEFI_X64.erase(),
                MachineArch::Aarch64 => hvlite_artifacts::test_vhd::GUEST_TEST_UEFI_AARCH64.erase(),
            },

            UefiGuest::None => unreachable!(),
        }
    }
}

/// Type-tags for [`BootImageConfig`](super::BootImageConfig)
pub mod boot_image_type {
    mod private {
        pub trait Sealed {}
        impl Sealed for super::Vhd {}
        impl Sealed for super::Iso {}
    }

    /// Private trait use to seal the set of artifact types BootImageType
    /// supports.
    pub trait BootImageType: private::Sealed {}

    /// BootImageConfig for a VHD file
    #[derive(Debug)]
    pub enum Vhd {}

    /// BootImageConfig for an ISO file
    #[derive(Debug)]
    pub enum Iso {}

    impl BootImageType for Vhd {}
    impl BootImageType for Iso {}
}

/// Configuration information for the boot drive of the VM.
#[derive(Debug)]
pub struct BootImageConfig<T: boot_image_type::BootImageType> {
    /// Artifact handle corresponding to the boot media.
    artifact: ErasedArtifactHandle,
    /// The OS flavor.
    os_flavor: OsFlavor,
    /// Any quirks needed to boot the guest.
    ///
    /// Most guests should not need any quirks, and can use `Default`.
    quirks: GuestQuirks,
    /// Marker denoting what type of media `artifact` corresponds to
    _type: core::marker::PhantomData<T>,
}

impl BootImageConfig<boot_image_type::Vhd> {
    /// Create a new BootImageConfig from a VHD artifact handle
    pub fn from_vhd<A>(artifact: ArtifactHandle<A>) -> Self
    where
        A: petri_artifacts_common::tags::IsTestVhd,
    {
        BootImageConfig {
            artifact: artifact.erase(),
            os_flavor: A::OS_FLAVOR,
            quirks: A::quirks(),
            _type: std::marker::PhantomData,
        }
    }
}

impl BootImageConfig<boot_image_type::Iso> {
    /// Create a new BootImageConfig from an ISO artifact handle
    pub fn from_iso<A>(artifact: ArtifactHandle<A>) -> Self
    where
        A: petri_artifacts_common::tags::IsTestIso,
    {
        BootImageConfig {
            artifact: artifact.erase(),
            os_flavor: A::OS_FLAVOR,
            quirks: A::quirks(),
            _type: std::marker::PhantomData,
        }
    }
}

impl PetriVmConfig {
    /// Get the OS that the VM will boot into.
    pub fn os_flavor(&self) -> OsFlavor {
        self.firmware.os_flavor()
    }
}
