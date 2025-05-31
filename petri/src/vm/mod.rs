// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

/// Hyper-V VM management
#[cfg(windows)]
pub mod hyperv;
/// OpenVMM VM management
pub mod openvmm;

use crate::ShutdownKind;
use async_trait::async_trait;
use get_resources::ged::FirmwareEvent;
use petri_artifacts_common::tags::GuestQuirks;
use petri_artifacts_common::tags::IsTestVmgs;
use petri_artifacts_common::tags::MachineArch;
use petri_artifacts_common::tags::OsFlavor;
use petri_artifacts_core::ArtifactResolver;
use petri_artifacts_core::ResolvedArtifact;
use petri_artifacts_core::ResolvedOptionalArtifact;
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

    /// Inject Windows secure boot templates into the VM's UEFI.
    fn with_windows_secure_boot_template(self: Box<Self>) -> Box<dyn PetriVmConfig>;
    /// Set the VM to use the specified processor topology.
    fn with_processor_topology(
        self: Box<Self>,
        topology: ProcessorTopology,
    ) -> Box<dyn PetriVmConfig>;

    /// Sets a custom OpenHCL IGVM file to use.
    fn with_custom_openhcl(self: Box<Self>, artifact: ResolvedArtifact) -> Box<dyn PetriVmConfig>;
    /// Sets the command line for the paravisor.
    fn with_openhcl_command_line(self: Box<Self>, command_line: &str) -> Box<dyn PetriVmConfig>;
    /// Adds a file to the VM's pipette agent image.
    fn with_agent_file(
        self: Box<Self>,
        name: &str,
        artifact: ResolvedArtifact,
    ) -> Box<dyn PetriVmConfig>;
    /// Adds a file to the paravisor's pipette agent image.
    fn with_openhcl_agent_file(
        self: Box<Self>,
        name: &str,
        artifact: ResolvedArtifact,
    ) -> Box<dyn PetriVmConfig>;
    /// Sets whether UEFI frontpage is enabled.
    fn with_uefi_frontpage(self: Box<Self>, enable: bool) -> Box<dyn PetriVmConfig>;
    /// Run the VM with Enable VMBus relay enabled
    fn with_vmbus_relay(self: Box<Self>, enable: bool) -> Box<dyn PetriVmConfig>;
}

/// Common processor topology information for the VM.
pub struct ProcessorTopology {
    /// The number of virtual processors.
    pub vp_count: u32,
    /// Whether SMT (hyperthreading) is enabled.
    pub enable_smt: Option<bool>,
    /// The number of virtual processors per socket.
    pub vps_per_socket: Option<u32>,
    /// The APIC configuration (x86-64 only).
    pub apic_mode: Option<ApicMode>,
}

impl Default for ProcessorTopology {
    fn default() -> Self {
        Self {
            vp_count: 2,
            enable_smt: None,
            vps_per_socket: None,
            apic_mode: None,
        }
    }
}

/// The APIC mode for the VM.
#[derive(Debug, Clone, Copy)]
pub enum ApicMode {
    /// xAPIC mode only.
    Xapic,
    /// x2APIC mode supported but not enabled at boot.
    X2apicSupported,
    /// x2APIC mode enabled at boot.
    X2apicEnabled,
}

/// A running VM that tests can interact with.
#[async_trait]
pub trait PetriVm: Send {
    /// Returns the guest architecture.
    fn arch(&self) -> MachineArch;
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
    /// Wait for a connection from a pipette agent running in VTL 2.
    /// Useful if you've reset VTL 2 or are otherwise expecting a fresh connection.
    /// Will fail if the VM is not running OpenHCL.
    async fn wait_for_vtl2_agent(&mut self) -> anyhow::Result<PipetteClient>;
    /// Wait for VTL 2 to report that it is ready to respond to commands.
    /// Will fail if the VM is not running OpenHCL.
    ///
    /// This should only be necessary if you're doing something manual. All
    /// Petri-provided methods will wait for VTL 2 to be ready automatically.
    async fn wait_for_vtl2_ready(&mut self) -> anyhow::Result<()>;
    /// Waits for an event emitted by the firmware about its boot status, and
    /// verifies that it is the expected success value.
    ///
    /// * Linux Direct guests do not emit a boot event, so this method immediately returns Ok.
    /// * PCAT guests may not emit an event depending on the PCAT version, this
    /// method is best effort for them.
    async fn wait_for_successful_boot_event(&mut self) -> anyhow::Result<()>;
    /// Waits for an event emitted by the firmware about its boot status, and
    /// returns that status.
    async fn wait_for_boot_event(&mut self) -> anyhow::Result<FirmwareEvent>;
    /// Instruct the guest to shutdown via the Hyper-V shutdown IC.
    async fn send_enlightened_shutdown(&mut self, kind: ShutdownKind) -> anyhow::Result<()>;
}

/// Firmware to load into the test VM.
#[derive(Debug)]
pub enum Firmware {
    /// Boot Linux directly, without any firmware.
    LinuxDirect {
        /// The kernel to boot.
        kernel: ResolvedArtifact,
        /// The initrd to use.
        initrd: ResolvedArtifact,
    },
    /// Boot Linux directly, without any firmware, with OpenHCL in VTL2.
    OpenhclLinuxDirect {
        /// The path to the IGVM file to use.
        igvm_path: ResolvedArtifact,
    },
    /// Boot a PCAT-based VM.
    Pcat {
        /// The guest OS the VM will boot into.
        guest: PcatGuest,
        /// The firmware to use.
        bios_firmware: ResolvedOptionalArtifact,
        /// The SVGA firmware to use.
        svga_firmware: ResolvedOptionalArtifact,
    },
    /// Boot a UEFI-based VM.
    Uefi {
        /// The guest OS the VM will boot into.
        guest: UefiGuest,
        /// The firmware to use.
        uefi_firmware: ResolvedArtifact,
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
        /// The path to the IGVM file to use.
        igvm_path: ResolvedArtifact,
    },
}

impl Firmware {
    /// Constructs a standard [`Firmware::LinuxDirect`] configuration.
    pub fn linux_direct(resolver: &ArtifactResolver<'_>, arch: MachineArch) -> Self {
        use petri_artifacts_vmm_test::artifacts::loadable::*;
        match arch {
            MachineArch::X86_64 => Firmware::LinuxDirect {
                kernel: resolver.require(LINUX_DIRECT_TEST_KERNEL_X64).erase(),
                initrd: resolver.require(LINUX_DIRECT_TEST_INITRD_X64).erase(),
            },
            MachineArch::Aarch64 => Firmware::LinuxDirect {
                kernel: resolver.require(LINUX_DIRECT_TEST_KERNEL_AARCH64).erase(),
                initrd: resolver.require(LINUX_DIRECT_TEST_INITRD_AARCH64).erase(),
            },
        }
    }

    /// Constructs a standard [`Firmware::OpenhclLinuxDirect`] configuration.
    pub fn openhcl_linux_direct(resolver: &ArtifactResolver<'_>, arch: MachineArch) -> Self {
        use petri_artifacts_vmm_test::artifacts::openhcl_igvm::*;
        match arch {
            MachineArch::X86_64 => Firmware::OpenhclLinuxDirect {
                igvm_path: resolver.require(LATEST_LINUX_DIRECT_TEST_X64).erase(),
            },
            MachineArch::Aarch64 => todo!("Linux direct not yet supported on aarch64"),
        }
    }

    /// Constructs a standard [`Firmware::Pcat`] configuration.
    pub fn pcat(resolver: &ArtifactResolver<'_>, guest: PcatGuest) -> Self {
        use petri_artifacts_vmm_test::artifacts::loadable::*;
        Firmware::Pcat {
            guest,
            bios_firmware: resolver.try_require(PCAT_FIRMWARE_X64).erase(),
            svga_firmware: resolver.try_require(SVGA_FIRMWARE_X64).erase(),
        }
    }

    /// Constructs a standard [`Firmware::Uefi`] configuration.
    pub fn uefi(resolver: &ArtifactResolver<'_>, arch: MachineArch, guest: UefiGuest) -> Self {
        use petri_artifacts_vmm_test::artifacts::loadable::*;
        let uefi_firmware = match arch {
            MachineArch::X86_64 => resolver.require(UEFI_FIRMWARE_X64).erase(),
            MachineArch::Aarch64 => resolver.require(UEFI_FIRMWARE_AARCH64).erase(),
        };
        Firmware::Uefi {
            guest,
            uefi_firmware,
        }
    }

    /// Constructs a standard [`Firmware::OpenhclUefi`] configuration.
    pub fn openhcl_uefi(
        resolver: &ArtifactResolver<'_>,
        arch: MachineArch,
        guest: UefiGuest,
        isolation: Option<IsolationType>,
        vtl2_nvme_boot: bool,
    ) -> Self {
        use petri_artifacts_vmm_test::artifacts::openhcl_igvm::*;
        let igvm_path = match arch {
            MachineArch::X86_64 if isolation.is_some() => resolver.require(LATEST_CVM_X64).erase(),
            MachineArch::X86_64 => resolver.require(LATEST_STANDARD_X64).erase(),
            MachineArch::Aarch64 => resolver.require(LATEST_STANDARD_AARCH64).erase(),
        };
        Firmware::OpenhclUefi {
            guest,
            isolation,
            vtl2_nvme_boot,
            igvm_path,
        }
    }

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
                guest: UefiGuest::GuestTestUefi { .. } | UefiGuest::None,
                ..
            }
            | Firmware::OpenhclUefi {
                guest: UefiGuest::GuestTestUefi { .. } | UefiGuest::None,
                ..
            } => OsFlavor::Uefi,
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

    fn expected_boot_event(&self) -> Option<FirmwareEvent> {
        match self {
            Firmware::LinuxDirect { .. } | Firmware::OpenhclLinuxDirect { .. } => None,
            Firmware::Pcat { .. } => {
                // TODO: Handle older PCAT versions that don't fire the event
                Some(FirmwareEvent::BootAttempt)
            }
            Firmware::Uefi {
                guest: UefiGuest::None,
                ..
            }
            | Firmware::OpenhclUefi {
                guest: UefiGuest::None,
                ..
            } => Some(FirmwareEvent::NoBootDevice),
            Firmware::Uefi { .. } | Firmware::OpenhclUefi { .. } => {
                Some(FirmwareEvent::BootSuccess)
            }
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
    fn artifact(&self) -> &ResolvedArtifact {
        match self {
            PcatGuest::Vhd(disk) => &disk.artifact,
            PcatGuest::Iso(disk) => &disk.artifact,
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
    GuestTestUefi(ResolvedArtifact),
    /// No guest, just the firmware.
    None,
}

impl UefiGuest {
    /// Construct a standard [`UefiGuest::GuestTestUefi`] configuration.
    pub fn guest_test_uefi(resolver: &ArtifactResolver<'_>, arch: MachineArch) -> Self {
        use petri_artifacts_vmm_test::artifacts::test_vhd::*;
        let artifact = match arch {
            MachineArch::X86_64 => resolver.require(GUEST_TEST_UEFI_X64).erase(),
            MachineArch::Aarch64 => resolver.require(GUEST_TEST_UEFI_AARCH64).erase(),
        };
        UefiGuest::GuestTestUefi(artifact)
    }

    fn artifact(&self) -> Option<&ResolvedArtifact> {
        match self {
            UefiGuest::Vhd(vhd) => Some(&vhd.artifact),
            UefiGuest::GuestTestUefi(p) => Some(p),
            UefiGuest::None => None,
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
    artifact: ResolvedArtifact,
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
    pub fn from_vhd<A>(artifact: ResolvedArtifact<A>) -> Self
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
    pub fn from_iso<A>(artifact: ResolvedArtifact<A>) -> Self
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

/// Flags controlling servicing behavior.
#[derive(Default, Debug, Clone, Copy)]
pub struct OpenHclServicingFlags {
    /// Preserve DMA memory for NVMe devices if supported.
    pub enable_nvme_keepalive: bool,
}

/// Virtual machine guest state resource
pub enum PetriVmgsResource<T: IsTestVmgs> {
    /// Use disk to store guest state
    Disk(ResolvedArtifact<T>),
    /// Use disk to store guest state, reformatting if corrupted.
    ReprovisionOnFailure(ResolvedArtifact<T>),
    /// Format and use disk to store guest state
    Reprovision(ResolvedArtifact<T>),
    /// Store guest state in memory
    Ephemeral,
}
