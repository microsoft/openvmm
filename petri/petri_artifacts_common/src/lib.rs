// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! `petri` test artifact declarations used by all petri-based tests, no matter
//! what VMM backend is being used.

#![forbid(unsafe_code)]

/// Artifact declarations
pub mod artifacts {
    use petri_artifacts_core::declare_artifacts;

    declare_artifacts! {
        /// Pipette windows x86_64 executable
        PIPETTE_WINDOWS_X64,
        /// Pipette linux x86_64 executable
        PIPETTE_LINUX_X64,
        /// Pipette windows aarch64 executable
        PIPETTE_WINDOWS_AARCH64,
        /// Pipette linux aarch64 executable
        PIPETTE_LINUX_AARCH64,
        /// Directory to put petri test logs in
        TEST_LOG_DIRECTORY,
    }
}

/// Artifact tag trait declarations
pub mod tags {
    use petri_artifacts_core::ArtifactId;

    /// A coarse-grained label used to differentiate between different OS
    /// environments.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    #[expect(missing_docs)] // Self-describing names.
    pub enum OsFlavor {
        Windows,
        Linux,
        FreeBsd,
        Uefi,
    }

    /// The machine architecture supported by the artifact or VM.
    #[derive(Copy, Clone, Debug, PartialEq, Eq)]
    #[expect(missing_docs)] // Self describing names
    pub enum MachineArch {
        X86_64,
        Aarch64,
    }

    impl MachineArch {
        /// Returns the host's architecture.
        pub fn host() -> Self {
            // xtask-fmt allow-target-arch oneoff-petri-host-arch
            if cfg!(target_arch = "x86_64") {
                Self::X86_64
            }
            // xtask-fmt allow-target-arch oneoff-petri-host-arch
            else if cfg!(target_arch = "aarch64") {
                Self::Aarch64
            } else {
                panic!("unsupported host architecture")
            }
        }
    }

    /// Quirks needed to boot a guest.
    #[derive(Default, Clone, Debug)]
    pub struct GuestQuirksInner {
        /// How long to wait after the shutdown IC reports ready before sending
        /// the shutdown command.
        ///
        /// This is necessary because some guests will ignore shutdown requests
        /// that arrive too early in the boot process.
        pub hyperv_shutdown_ic_sleep: Option<std::time::Duration>,
        /// Some guests reboot automatically soon after first boot.
        pub initial_reboot: Option<InitialRebootCondition>,
    }

    /// Some guests may automatically reboot only in certain configurations
    #[derive(Clone, Copy, Debug)]
    pub enum InitialRebootCondition {
        /// This guest always reboots on this VMM.
        Always,
        /// This guest only reboots when the TPM is enabled.
        WithTpm,
    }

    /// Quirks needed to boot a guest, allowing for differences based on backend
    #[derive(Default, Clone, Debug)]
    pub struct GuestQuirks {
        /// Quirks when running in OpenVMM
        pub openvmm: GuestQuirksInner,
        /// Quirks when running in Hyper-V
        pub hyperv: GuestQuirksInner,
    }

    impl GuestQuirks {
        /// Use the same quirks for all backends
        pub fn for_all_backends(quirks: GuestQuirksInner) -> GuestQuirks {
            GuestQuirks {
                openvmm: quirks.clone(),
                hyperv: quirks,
            }
        }
    }

    /// Artifact is a OpenHCL IGVM file
    pub trait IsOpenhclIgvm: IsLoadable + ArtifactId {}

    /// Artifact is a bootable test VHD file
    pub trait IsTestVhd: ArtifactId {
        /// What [`OsFlavor`] this image boots into.
        const OS_FLAVOR: OsFlavor;

        /// What [`MachineArch`] this image supports.
        const ARCH: MachineArch;

        /// Declare any "quirks" needed to boot the image.
        fn quirks() -> GuestQuirks {
            GuestQuirks::default()
        }
    }

    /// Artifact is a bootable test ISO file
    pub trait IsTestIso: ArtifactId {
        /// What [`OsFlavor`] this image boots into.
        const OS_FLAVOR: OsFlavor;

        /// What [`MachineArch`] this image supports.
        const ARCH: MachineArch;

        /// Declare any "quirks" needed to boot the image.
        fn quirks() -> GuestQuirks {
            GuestQuirks::default()
        }
    }

    /// Artifact is a binary that can be loaded into a VM
    pub trait IsLoadable: ArtifactId {
        /// What [`MachineArch`] this artifact supports.
        const ARCH: MachineArch;
    }

    /// Artifact is a test VMGS file
    pub trait IsTestVmgs: ArtifactId {}

    /// Artifact is a VmgsTool binary
    pub trait IsVmgsTool: ArtifactId {}
}
