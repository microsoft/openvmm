// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

/// Hyper-V VM management
#[cfg(windows)]
pub mod hyperv;
/// OpenVMM VM management
pub mod openvmm;

use anyhow::Context;
use async_trait::async_trait;
use petri_artifacts_common::tags::GuestQuirks;
use petri_artifacts_common::tags::MachineArch;
use petri_artifacts_common::tags::OsFlavor;
use petri_artifacts_core::ArtifactHandle;
use petri_artifacts_core::AsArtifactHandle;
use petri_artifacts_core::ErasedArtifactHandle;
use petri_artifacts_vmm_test::artifacts as hvlite_artifacts;
use pipette_client::PipetteClient;
use vmm_core_defs::HaltReason;

/// Configuration state for a test VM.
///
/// R is the type of the struct used to interact with the VM once it is created
#[async_trait]
pub trait PetriVmConfig: Send {
    /// Build and boot the requested VM. Does not configure and start pipette.
    /// Should only be used for testing platforms that pipette does not support.
    async fn run_without_agent(self: Box<Self>) -> anyhow::Result<Box<dyn PetriVm>>;
    /// Run the VM, configuring pipette to automatically start, but do not wait
    /// for it to connect. This is useful for tests where the first boot attempt
    /// is expected to not succeed, but pipette functionality is still desired.
    async fn run_with_lazy_pipette(self: Box<Self>) -> anyhow::Result<Box<dyn PetriVm>>;
    /// Run the VM, launching pipette and returning a client to it.
    async fn run(self: Box<Self>) -> anyhow::Result<(Box<dyn PetriVm>, PipetteClient)>;
}

/// A running VM that tests can interact with.
#[async_trait]
pub trait PetriVm: Send {
    /// Wait for the VM to halt, returning the reason for the halt.
    async fn wait_for_halt(&mut self) -> anyhow::Result<HaltReason>;
    /// Wait for the VM to halt, returning the reason for the halt,
    /// and cleanly tear down the VM.
    async fn wait_for_teardown(self: Box<Self>) -> anyhow::Result<HaltReason>;
    /// Test that we are able to inspect OpenHCL.
    async fn test_inspect_openhcl(&mut self) -> anyhow::Result<()>;
    /// Wait for a connection from a pipette agent running in the guest.
    /// Useful if you've rebooted the vm or are otherwise expecting a fresh connection.
    async fn wait_for_agent(&mut self) -> anyhow::Result<PipetteClient>;
    /// Wait for VTL 2 to report that it is ready to respond to commands.
    /// Will fail if the VM is not running OpenHCL.
    ///
    /// This should only be necessary if you're doing something manual. All
    /// Petri-provided methods will wait for VTL 2 to be ready automatically.
    async fn wait_for_vtl2_ready(&mut self) -> anyhow::Result<()>;
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

/// Isolation type
#[derive(Debug, Clone, Copy)]
pub enum IsolationType {
    /// VBS
    Vbs,
    /// SNP
    Snp,
    /// TDX
    Tdx,
}

/// Generates a name for the petri test based on the thread name
pub fn get_test_name() -> anyhow::Result<String> {
    // Use the current thread name for the test name, both cargo-test and
    // cargo-nextest set this.
    // FUTURE: If we ever want to use petri outside a testing context this
    // will need to be revisited.
    let current_thread = std::thread::current();
    let test_name = current_thread.name().context("no thread name configured")?;
    if test_name.is_empty() {
        anyhow::bail!("thread name is empty");
    }
    if test_name == "main" {
        anyhow::bail!("thread name is 'main', not running from test thread");
    }
    // Windows paths can't include colons, replace them.
    Ok(test_name.replace("::", "__"))
}
