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
    #[derive(Debug, Clone, Copy)]
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

    /// Quirks needed to boot a guest.
    #[derive(Default, Copy, Clone, Debug)]
    pub struct GuestQuirks {
        /// How long to wait after the shutdown IC reports ready before sending
        /// the shutdown command.
        pub hyperv_shutdown_ic_sleep: Option<std::time::Duration>,
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
}
