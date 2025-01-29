// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! `petri` test artifacts used by in-tree VMM tests

#![forbid(unsafe_code)]
#![warn(missing_docs)]

/// Artifact declarations
pub mod artifacts {
    use petri_artifacts_core::declare_artifacts;

    macro_rules! openvmm_native {
        ($id_ty:ty, $os:literal, $arch:literal) => {
            /// openvmm "native" executable (i.e:
            /// [`OPENVMM_WIN_X64`](const@OPENVMM_WIN_X64) when compiled on windows x86_64,
            /// [`OPENVMM_LINUX_AARCH64`](const@OPENVMM_LINUX_AARCH64) when compiled on linux aarch64,
            /// etc...)
            // xtask-fmt allow-target-arch oneoff-petri-native-openvmm
            #[cfg(all(target_os = $os, target_arch = $arch))]
            pub const OPENVMM_NATIVE: petri_artifacts_core::ArtifactHandle<$id_ty> =
                petri_artifacts_core::ArtifactHandle::new();
        };
    }

    openvmm_native!(OPENVMM_WIN_X64, "windows", "x86_64");
    openvmm_native!(OPENVMM_LINUX_X64, "linux", "x86_64");
    openvmm_native!(OPENVMM_WIN_AARCH64, "windows", "aarch64");
    openvmm_native!(OPENVMM_LINUX_AARCH64, "linux", "aarch64");
    openvmm_native!(OPENVMM_MACOS_AARCH64, "macos", "aarch64");

    declare_artifacts! {
        /// openvmm windows x86_64 executable
        OPENVMM_WIN_X64,
        /// openvmm linux x86_64 executable
        OPENVMM_LINUX_X64,
        /// openvmm windows aarch64 executable
        OPENVMM_WIN_AARCH64,
        /// openvmm linux aarch64 executable
        OPENVMM_LINUX_AARCH64,
        /// openvmm macos aarch64 executable
        OPENVMM_MACOS_AARCH64,
        /// Directory to put OpenHCL dumps in
        OPENHCL_DUMP_DIRECTORY,
    }

    /// Loadable artifacts
    pub mod loadable {
        use petri_artifacts_common::tags::IsLoadable;
        use petri_artifacts_common::tags::MachineArch;
        use petri_artifacts_core::declare_artifacts;

        declare_artifacts! {
            /// Test linux direct kernel (from OpenVMM deps)
            LINUX_DIRECT_TEST_KERNEL_X64,
            /// Test linux direct initrd (from OpenVMM deps)
            LINUX_DIRECT_TEST_INITRD_X64,
            /// Test linux direct kernel (from OpenVMM deps)
            LINUX_DIRECT_TEST_KERNEL_AARCH64,
            /// Test linux direct initrd (from OpenVMM deps)
            LINUX_DIRECT_TEST_INITRD_AARCH64,
            /// PCAT firmware DLL
            PCAT_FIRMWARE_X64,
            /// SVGA firmware DLL
            SVGA_FIRMWARE_X64,
            /// UEFI firmware for x64
            UEFI_FIRMWARE_X64,
            /// UEFI firmware for aarch64
            UEFI_FIRMWARE_AARCH64,
        }

        impl IsLoadable for LINUX_DIRECT_TEST_KERNEL_X64 {
            const ARCH: MachineArch = MachineArch::X86_64;
        }

        impl IsLoadable for LINUX_DIRECT_TEST_INITRD_X64 {
            const ARCH: MachineArch = MachineArch::X86_64;
        }

        impl IsLoadable for LINUX_DIRECT_TEST_KERNEL_AARCH64 {
            const ARCH: MachineArch = MachineArch::Aarch64;
        }

        impl IsLoadable for LINUX_DIRECT_TEST_INITRD_AARCH64 {
            const ARCH: MachineArch = MachineArch::Aarch64;
        }

        impl IsLoadable for PCAT_FIRMWARE_X64 {
            const ARCH: MachineArch = MachineArch::X86_64;
        }

        impl IsLoadable for SVGA_FIRMWARE_X64 {
            const ARCH: MachineArch = MachineArch::X86_64;
        }

        impl IsLoadable for UEFI_FIRMWARE_X64 {
            const ARCH: MachineArch = MachineArch::X86_64;
        }

        impl IsLoadable for UEFI_FIRMWARE_AARCH64 {
            const ARCH: MachineArch = MachineArch::Aarch64;
        }
    }

    /// OpenHCL IGVM artifacts
    pub mod openhcl_igvm {
        use petri_artifacts_common::tags::IsLoadable;
        use petri_artifacts_common::tags::IsOpenhclIgvm;
        use petri_artifacts_common::tags::MachineArch;
        use petri_artifacts_core::declare_artifacts;

        declare_artifacts! {
            /// OpenHCL IGVM (standard)
            LATEST_STANDARD_X64,
            /// OpenHCL IGVM (for CVM)
            LATEST_CVM_X64,
            /// OpenHCL IGVM (using a linux direct-boot test image instead of UEFI)
            LATEST_LINUX_DIRECT_TEST_X64,
            /// OpenHCL IGVM (standard AARCH64)
            LATEST_STANDARD_AARCH64,
        }

        impl IsLoadable for LATEST_STANDARD_X64 {
            const ARCH: MachineArch = MachineArch::X86_64;
        }
        impl IsOpenhclIgvm for LATEST_STANDARD_X64 {}

        impl IsLoadable for LATEST_CVM_X64 {
            const ARCH: MachineArch = MachineArch::X86_64;
        }
        impl IsOpenhclIgvm for LATEST_CVM_X64 {}

        impl IsLoadable for LATEST_LINUX_DIRECT_TEST_X64 {
            const ARCH: MachineArch = MachineArch::X86_64;
        }
        impl IsOpenhclIgvm for LATEST_LINUX_DIRECT_TEST_X64 {}

        impl IsLoadable for LATEST_STANDARD_AARCH64 {
            const ARCH: MachineArch = MachineArch::Aarch64;
        }
        impl IsOpenhclIgvm for LATEST_STANDARD_AARCH64 {}

        /// OpenHCL usermode binary
        pub mod um_bin {
            use petri_artifacts_core::declare_artifacts;

            declare_artifacts! {
                /// Usermode binary for Linux direct
                LATEST_LINUX_DIRECT_TEST_X64
            }
        }

        /// OpenHCL debugging symbols for the usermode binary
        pub mod um_dbg {
            use petri_artifacts_core::declare_artifacts;

            declare_artifacts! {
                /// Usermode symbols for Linux direct
                LATEST_LINUX_DIRECT_TEST_X64
            }
        }
    }

    /// Test VHD artifacts
    pub mod test_vhd {
        use crate::tags::IsHostedOnHvliteAzureBlobStore;
        use petri_artifacts_common::tags::GuestQuirks;
        use petri_artifacts_common::tags::IsTestVhd;
        use petri_artifacts_common::tags::MachineArch;
        use petri_artifacts_common::tags::OsFlavor;
        use petri_artifacts_core::declare_artifacts;

        declare_artifacts! {
            /// guest_test_uefi.img, built for x86_64 from the in-tree `guest_test_uefi` codebase.
            GUEST_TEST_UEFI_X64,
            /// guest_test_uefi.img, built for aarch64 from the in-tree `guest_test_uefi` codebase.
            GUEST_TEST_UEFI_AARCH64,
        }

        impl IsTestVhd for GUEST_TEST_UEFI_X64 {
            const OS_FLAVOR: OsFlavor = OsFlavor::Uefi;
            const ARCH: MachineArch = MachineArch::X86_64;
        }

        impl IsTestVhd for GUEST_TEST_UEFI_AARCH64 {
            const OS_FLAVOR: OsFlavor = OsFlavor::Uefi;
            const ARCH: MachineArch = MachineArch::Aarch64;
        }

        // NOTE: GUEST_TEST_UEFI is not hosted on the HvLite Azure Blob Store. It is
        // built just-in-time, using the code that is present in-tree, under
        // `guest_test_uefi`.

        declare_artifacts! {
            /// Generation 1 windows test image
            GEN1_WINDOWS_DATA_CENTER_CORE2022_X64
        }

        impl IsTestVhd for GEN1_WINDOWS_DATA_CENTER_CORE2022_X64 {
            const OS_FLAVOR: OsFlavor = OsFlavor::Windows;
            const ARCH: MachineArch = MachineArch::X86_64;
        }

        impl IsHostedOnHvliteAzureBlobStore for GEN1_WINDOWS_DATA_CENTER_CORE2022_X64 {
            const FILENAME: &'static str =
                "WindowsServer-2022-datacenter-core-smalldisk-20348.1906.230803.vhd";
            const SIZE: u64 = 32214352384;
        }

        declare_artifacts! {
            /// Generation 2 windows test image
            GEN2_WINDOWS_DATA_CENTER_CORE2022_X64
        }

        impl IsTestVhd for GEN2_WINDOWS_DATA_CENTER_CORE2022_X64 {
            const OS_FLAVOR: OsFlavor = OsFlavor::Windows;
            const ARCH: MachineArch = MachineArch::X86_64;
        }

        impl IsHostedOnHvliteAzureBlobStore for GEN2_WINDOWS_DATA_CENTER_CORE2022_X64 {
            const FILENAME: &'static str =
                "WindowsServer-2022-datacenter-core-smalldisk-g2-20348.1906.230803.vhd";
            const SIZE: u64 = 32214352384;
        }

        declare_artifacts! {
            /// FreeBSD 13.2
            FREE_BSD_13_2_X64
        }

        impl IsTestVhd for FREE_BSD_13_2_X64 {
            const OS_FLAVOR: OsFlavor = OsFlavor::FreeBsd;
            const ARCH: MachineArch = MachineArch::X86_64;

            fn quirks() -> GuestQuirks {
                GuestQuirks {
                    // FreeBSD will ignore shutdown requests that arrive too
                    // early in the boot process.
                    hyperv_shutdown_ic_sleep: Some(std::time::Duration::from_secs(15)),
                }
            }
        }

        impl IsHostedOnHvliteAzureBlobStore for FREE_BSD_13_2_X64 {
            const FILENAME: &'static str = "FreeBSD-13.2-RELEASE-amd64.vhd";
            const SIZE: u64 = 6477005312;
        }

        declare_artifacts! {
            /// Ubuntu 2204 Server
            UBUNTU_2204_SERVER_X64
        }

        impl IsTestVhd for UBUNTU_2204_SERVER_X64 {
            const OS_FLAVOR: OsFlavor = OsFlavor::Linux;
            const ARCH: MachineArch = MachineArch::X86_64;
        }

        impl IsHostedOnHvliteAzureBlobStore for UBUNTU_2204_SERVER_X64 {
            const FILENAME: &'static str = "ubuntu-22.04-server-cloudimg-amd64.vhd";
            const SIZE: u64 = 2361655808;
        }

        declare_artifacts! {
            /// Ubuntu 24.04 Server Aarch64
            UBUNTU_2404_SERVER_AARCH64
        }

        impl IsTestVhd for UBUNTU_2404_SERVER_AARCH64 {
            const OS_FLAVOR: OsFlavor = OsFlavor::Linux;
            const ARCH: MachineArch = MachineArch::Aarch64;
        }

        impl IsHostedOnHvliteAzureBlobStore for UBUNTU_2404_SERVER_AARCH64 {
            const FILENAME: &'static str = "ubuntu-24.04-server-cloudimg-arm64.vhd";
            const SIZE: u64 = 3758211584;
        }
    }

    /// Test ISO artifacts
    pub mod test_iso {
        use crate::tags::IsHostedOnHvliteAzureBlobStore;
        use petri_artifacts_common::tags::GuestQuirks;
        use petri_artifacts_common::tags::IsTestIso;
        use petri_artifacts_common::tags::MachineArch;
        use petri_artifacts_common::tags::OsFlavor;
        use petri_artifacts_core::declare_artifacts;

        declare_artifacts! {
            /// FreeBSD 13.2
            FREE_BSD_13_2_X64
        }

        impl IsTestIso for FREE_BSD_13_2_X64 {
            const OS_FLAVOR: OsFlavor = OsFlavor::FreeBsd;
            const ARCH: MachineArch = MachineArch::X86_64;

            fn quirks() -> GuestQuirks {
                GuestQuirks {
                    // FreeBSD will ignore shutdown requests that arrive too
                    // early in the boot process.
                    //
                    // Time is set to 5s longer than the VHD, to account for ISO
                    // boot being slower.
                    hyperv_shutdown_ic_sleep: Some(std::time::Duration::from_secs(20)),
                }
            }
        }

        impl IsHostedOnHvliteAzureBlobStore for FREE_BSD_13_2_X64 {
            const FILENAME: &'static str = "FreeBSD-13.2-RELEASE-amd64-dvd1.iso";
            const SIZE: u64 = 4245487616;
        }
    }
}

/// Artifact tag trait declarations
pub mod tags {
    use petri_artifacts_core::ArtifactId;

    /// Artifact is associated with a file hosted in HvLite's microsoft-internal
    /// Azure Blob Store.
    pub trait IsHostedOnHvliteAzureBlobStore: ArtifactId {
        /// Filename in the blob store
        const FILENAME: &'static str;
        /// Size of the file in bytes
        const SIZE: u64;
    }
}
