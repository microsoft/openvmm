// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! `petri` test artifacts used by in-tree VMM tests

#![forbid(unsafe_code)]

/// Artifact declarations
pub mod artifacts {
    use petri_artifacts_common::tags::IsVmgsTool;
    use petri_artifacts_core::declare_artifacts;

    macro_rules! openvmm_native {
        ($id_ty:ty, $os:literal, $arch:literal) => {
            /// openvmm "native" executable (i.e:
            /// [`OPENVMM_WIN_X64`](const@OPENVMM_WIN_X64) when compiled on windows x86_64,
            /// [`OPENVMM_LINUX_AARCH64`](const@OPENVMM_LINUX_AARCH64) when compiled on linux aarch64,
            /// etc...)
            // xtask-fmt allow-target-arch oneoff-petri-native-test-deps
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
    }

    /// Guest-side tools used by the VMM tests.
    pub mod guest_tools {
        use petri_artifacts_core::declare_artifacts;

        declare_artifacts! {
            /// Windows x86_64 build of the `tpm_guest_tests` utility.
            TPM_GUEST_TESTS_WINDOWS_X64,
            /// Linux x86_64 build of the `tpm_guest_tests` utility.
            TPM_GUEST_TESTS_LINUX_X64,
        }
    }

    /// Loadable artifacts
    pub mod loadable {
        use petri_artifacts_common::tags::IsLoadable;
        use petri_artifacts_common::tags::MachineArch;
        use petri_artifacts_core::declare_artifacts;

        macro_rules! linux_direct_native {
            ($id_kernel_ty:ty, $id_initrd_ty:ty, $arch:literal) => {
                /// Test linux direct kernel (from OpenVMM deps) for the target architecture
                // xtask-fmt allow-target-arch oneoff-petri-native-test-deps
                #[cfg(target_arch = $arch)]
                pub const LINUX_DIRECT_TEST_KERNEL_NATIVE: petri_artifacts_core::ArtifactHandle<
                    $id_kernel_ty,
                > = petri_artifacts_core::ArtifactHandle::new();
                /// Test linux direct initrd (from OpenVMM deps) for the target architecture
                // xtask-fmt allow-target-arch oneoff-petri-native-test-deps
                #[cfg(target_arch = $arch)]
                pub const LINUX_DIRECT_TEST_INITRD_NATIVE: petri_artifacts_core::ArtifactHandle<
                    $id_initrd_ty,
                > = petri_artifacts_core::ArtifactHandle::new();
            };
        }

        linux_direct_native!(
            LINUX_DIRECT_TEST_KERNEL_X64,
            LINUX_DIRECT_TEST_INITRD_X64,
            "x86_64"
        );
        linux_direct_native!(
            LINUX_DIRECT_TEST_KERNEL_AARCH64,
            LINUX_DIRECT_TEST_INITRD_AARCH64,
            "aarch64"
        );

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
            /// OpenHCL IGVM last release (standard)
            LATEST_RELEASE_STANDARD_X64,
            /// OpenHCL IGVM (standard, with VTL2 dev kernel)
            LATEST_STANDARD_DEV_KERNEL_X64,
            /// OpenHCL IGVM (for CVM)
            LATEST_CVM_X64,
            /// OpenHCL IGVM (using a linux direct-boot test image instead of UEFI)
            LATEST_LINUX_DIRECT_TEST_X64,
            /// OpenHCL IGVM last release (using a linux direct-boot test image instead of UEFI)
            LATEST_RELEASE_LINUX_DIRECT_X64,
            /// OpenHCL IGVM (standard AARCH64)
            LATEST_STANDARD_AARCH64,
            /// OpenHCL IGVM last release (standard AARCH64)
            LATEST_RELEASE_STANDARD_AARCH64,
            /// OpenHCL IGVM (standard AARCH64, with VTL2 dev kernel)
            LATEST_STANDARD_DEV_KERNEL_AARCH64,
        }

        impl IsLoadable for LATEST_STANDARD_X64 {
            const ARCH: MachineArch = MachineArch::X86_64;
        }
        impl IsOpenhclIgvm for LATEST_STANDARD_X64 {}

        impl IsLoadable for LATEST_RELEASE_STANDARD_X64 {
            const ARCH: MachineArch = MachineArch::X86_64;
        }
        impl IsOpenhclIgvm for LATEST_RELEASE_STANDARD_X64 {}

        impl IsLoadable for LATEST_STANDARD_DEV_KERNEL_X64 {
            const ARCH: MachineArch = MachineArch::X86_64;
        }
        impl IsOpenhclIgvm for LATEST_STANDARD_DEV_KERNEL_X64 {}

        impl IsLoadable for LATEST_CVM_X64 {
            const ARCH: MachineArch = MachineArch::X86_64;
        }
        impl IsOpenhclIgvm for LATEST_CVM_X64 {}

        impl IsLoadable for LATEST_LINUX_DIRECT_TEST_X64 {
            const ARCH: MachineArch = MachineArch::X86_64;
        }
        impl IsOpenhclIgvm for LATEST_LINUX_DIRECT_TEST_X64 {}

        impl IsLoadable for LATEST_RELEASE_LINUX_DIRECT_X64 {
            const ARCH: MachineArch = MachineArch::X86_64;
        }
        impl IsOpenhclIgvm for LATEST_RELEASE_LINUX_DIRECT_X64 {}

        impl IsLoadable for LATEST_STANDARD_AARCH64 {
            const ARCH: MachineArch = MachineArch::Aarch64;
        }
        impl IsOpenhclIgvm for LATEST_STANDARD_AARCH64 {}

        impl IsLoadable for LATEST_RELEASE_STANDARD_AARCH64 {
            const ARCH: MachineArch = MachineArch::Aarch64;
        }
        impl IsOpenhclIgvm for LATEST_RELEASE_STANDARD_AARCH64 {}

        impl IsLoadable for LATEST_STANDARD_DEV_KERNEL_AARCH64 {
            const ARCH: MachineArch = MachineArch::Aarch64;
        }
        impl IsOpenhclIgvm for LATEST_STANDARD_DEV_KERNEL_AARCH64 {}

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
        use petri_artifacts_common::tags::GuestQuirksInner;
        use petri_artifacts_common::tags::InitialRebootCondition;
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
            /// Generation 2 windows test image
            GEN2_WINDOWS_DATA_CENTER_CORE2025_X64
        }

        impl IsTestVhd for GEN2_WINDOWS_DATA_CENTER_CORE2025_X64 {
            const OS_FLAVOR: OsFlavor = OsFlavor::Windows;
            const ARCH: MachineArch = MachineArch::X86_64;

            fn quirks() -> GuestQuirks {
                GuestQuirks::for_all_backends(GuestQuirksInner {
                    initial_reboot: Some(InitialRebootCondition::Always),
                    ..Default::default()
                })
            }
        }

        impl IsHostedOnHvliteAzureBlobStore for GEN2_WINDOWS_DATA_CENTER_CORE2025_X64 {
            const FILENAME: &'static str =
                "WindowsServer-2025-datacenter-core-smalldisk-g2-26100.3476.250306.vhd";
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
                GuestQuirks::for_all_backends(GuestQuirksInner {
                    hyperv_shutdown_ic_sleep: Some(std::time::Duration::from_secs(20)),
                    ..Default::default()
                })
            }
        }

        impl IsHostedOnHvliteAzureBlobStore for FREE_BSD_13_2_X64 {
            const FILENAME: &'static str = "FreeBSD-13.2-RELEASE-amd64.vhd";
            const SIZE: u64 = 6477005312;
        }

        declare_artifacts! {
            /// Ubuntu 24.04 Server X64
            UBUNTU_2404_SERVER_X64
        }

        impl IsTestVhd for UBUNTU_2404_SERVER_X64 {
            const OS_FLAVOR: OsFlavor = OsFlavor::Linux;
            const ARCH: MachineArch = MachineArch::X86_64;
            fn quirks() -> GuestQuirks {
                GuestQuirks::for_all_backends(GuestQuirksInner {
                    hyperv_shutdown_ic_sleep: Some(std::time::Duration::from_secs(20)),
                    initial_reboot: Some(InitialRebootCondition::WithTpm),
                })
            }
        }

        impl IsHostedOnHvliteAzureBlobStore for UBUNTU_2404_SERVER_X64 {
            const FILENAME: &'static str = "ubuntu-24.04-server-cloudimg-amd64.vhd";
            const SIZE: u64 = 3758211584;
        }

        declare_artifacts! {
            /// Ubuntu 25.04 Server X64
            UBUNTU_2504_SERVER_X64
        }

        impl IsTestVhd for UBUNTU_2504_SERVER_X64 {
            const OS_FLAVOR: OsFlavor = OsFlavor::Linux;
            const ARCH: MachineArch = MachineArch::X86_64;
            fn quirks() -> GuestQuirks {
                GuestQuirks::for_all_backends(GuestQuirksInner {
                    hyperv_shutdown_ic_sleep: Some(std::time::Duration::from_secs(20)),
                    initial_reboot: Some(InitialRebootCondition::WithTpm),
                })
            }
        }

        impl IsHostedOnHvliteAzureBlobStore for UBUNTU_2504_SERVER_X64 {
            const FILENAME: &'static str = "ubuntu-25.04-server-cloudimg-amd64.vhd";
            const SIZE: u64 = 3758211584;
        }

        declare_artifacts! {
            /// Alpine Linux 3.23.2 x64 UEFI nocloud cloud-init
            /// NOTE: The image on the alpine website is qcow2 and must be converted to a fixed vhd.
            ALPINE_3_23_X64
        }

        impl IsTestVhd for ALPINE_3_23_X64 {
            const OS_FLAVOR: OsFlavor = OsFlavor::Linux;
            const ARCH: MachineArch = MachineArch::X86_64;
            fn quirks() -> GuestQuirks {
                GuestQuirks::for_all_backends(GuestQuirksInner {
                    hyperv_shutdown_ic_sleep: Some(std::time::Duration::from_secs(20)),
                    ..Default::default()
                })
            }
        }

        impl IsHostedOnHvliteAzureBlobStore for ALPINE_3_23_X64 {
            const FILENAME: &'static str = "nocloud_alpine-3.23.2-x86_64-uefi-cloudinit-r0.vhd";
            const SIZE: u64 = 224494080;
        }

        declare_artifacts! {
            /// Alpine Linux 3.23.2 aarch64 UEFI nocloud cloud-init
            /// NOTE: The image on the alpine website is qcow2 and must be converted to a fixed vhd.
            ALPINE_3_23_AARCH64
        }

        impl IsTestVhd for ALPINE_3_23_AARCH64 {
            const OS_FLAVOR: OsFlavor = OsFlavor::Linux;
            const ARCH: MachineArch = MachineArch::Aarch64;
            fn quirks() -> GuestQuirks {
                GuestQuirks::for_all_backends(GuestQuirksInner {
                    hyperv_shutdown_ic_sleep: Some(std::time::Duration::from_secs(20)),
                    ..Default::default()
                })
            }
        }

        impl IsHostedOnHvliteAzureBlobStore for ALPINE_3_23_AARCH64 {
            const FILENAME: &'static str = "nocloud_alpine-3.23.2-aarch64-uefi-cloudinit-r0.vhd";
            const SIZE: u64 = 258015744;
        }

        declare_artifacts! {
            /// Ubuntu 24.04 Server Aarch64
            UBUNTU_2404_SERVER_AARCH64
        }

        impl IsTestVhd for UBUNTU_2404_SERVER_AARCH64 {
            const OS_FLAVOR: OsFlavor = OsFlavor::Linux;
            const ARCH: MachineArch = MachineArch::Aarch64;
            fn quirks() -> GuestQuirks {
                GuestQuirks::for_all_backends(GuestQuirksInner {
                    hyperv_shutdown_ic_sleep: Some(std::time::Duration::from_secs(20)),
                    initial_reboot: Some(InitialRebootCondition::WithTpm),
                })
            }
        }

        impl IsHostedOnHvliteAzureBlobStore for UBUNTU_2404_SERVER_AARCH64 {
            const FILENAME: &'static str = "ubuntu-24.04-server-cloudimg-arm64.vhd";
            const SIZE: u64 = 3758211584;
        }

        declare_artifacts! {
            /// Windows 11 Enterprise ARM64 24H2
            WINDOWS_11_ENTERPRISE_AARCH64
        }

        impl IsTestVhd for WINDOWS_11_ENTERPRISE_AARCH64 {
            const OS_FLAVOR: OsFlavor = OsFlavor::Windows;
            const ARCH: MachineArch = MachineArch::Aarch64;

            fn quirks() -> GuestQuirks {
                GuestQuirks::for_all_backends(GuestQuirksInner {
                    initial_reboot: Some(InitialRebootCondition::Always),
                    ..Default::default()
                })
            }
        }

        impl IsHostedOnHvliteAzureBlobStore for WINDOWS_11_ENTERPRISE_AARCH64 {
            const FILENAME: &'static str =
                "windows11preview-arm64-win11-24h2-ent-26100.3775.250406-1.vhdx";
            const SIZE: u64 = 24398266368;
        }

        // VHDs that are created by pre-preparation automation

        declare_artifacts! {
            /// Generation 2 windows test image
            GEN2_WINDOWS_DATA_CENTER_CORE2025_X64_PREPPED
        }

        impl IsTestVhd for GEN2_WINDOWS_DATA_CENTER_CORE2025_X64_PREPPED {
            const OS_FLAVOR: OsFlavor = GEN2_WINDOWS_DATA_CENTER_CORE2025_X64::OS_FLAVOR;
            const ARCH: MachineArch = GEN2_WINDOWS_DATA_CENTER_CORE2025_X64::ARCH;

            fn quirks() -> GuestQuirks {
                GEN2_WINDOWS_DATA_CENTER_CORE2025_X64::quirks()
            }
        }
    }

    /// Test ISO artifacts
    pub mod test_iso {
        use crate::tags::IsHostedOnHvliteAzureBlobStore;
        use petri_artifacts_common::tags::GuestQuirks;
        use petri_artifacts_common::tags::GuestQuirksInner;
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
                GuestQuirks::for_all_backends(GuestQuirksInner {
                    hyperv_shutdown_ic_sleep: Some(std::time::Duration::from_secs(20)),
                    ..Default::default()
                })
            }
        }

        impl IsHostedOnHvliteAzureBlobStore for FREE_BSD_13_2_X64 {
            const FILENAME: &'static str = "FreeBSD-13.2-RELEASE-amd64-dvd1.iso";
            const SIZE: u64 = 4245487616;
        }
    }

    /// Test VMGS artifacts
    pub mod test_vmgs {
        use crate::tags::IsHostedOnHvliteAzureBlobStore;
        use petri_artifacts_common::tags::IsTestVmgs;
        use petri_artifacts_core::declare_artifacts;

        declare_artifacts! {
            /// VMGS file containing a UEFI boot entry
            ///
            /// The file was generated by booting an arbitrary Windows VHD
            /// (different from the ones used for testing in CI) in OpenVMM
            /// with a persistent VMGS file enabled. This is useful for testing
            /// whether default_boot_always_attempt works to boot other VHDs.
            VMGS_WITH_BOOT_ENTRY,
        }

        impl IsHostedOnHvliteAzureBlobStore for VMGS_WITH_BOOT_ENTRY {
            const FILENAME: &'static str = "sample-vmgs.vhd";
            const SIZE: u64 = 4194816;
        }

        impl IsTestVmgs for VMGS_WITH_BOOT_ENTRY {}
    }

    /// TMK-related artifacts
    pub mod tmks {
        use petri_artifacts_core::declare_artifacts;

        macro_rules! tmk_native {
            ($id_ty:ty, $os:literal, $arch:literal) => {
                /// tmk_vmm "native" executable
                // xtask-fmt allow-target-arch oneoff-petri-native-test-deps
                #[cfg(all(target_os = $os, target_arch = $arch))]
                pub const TMK_VMM_NATIVE: petri_artifacts_core::ArtifactHandle<$id_ty> =
                    petri_artifacts_core::ArtifactHandle::new();
            };
        }

        tmk_native!(TMK_VMM_WIN_X64, "windows", "x86_64");
        tmk_native!(TMK_VMM_LINUX_X64, "linux", "x86_64");
        tmk_native!(TMK_VMM_WIN_AARCH64, "windows", "aarch64");
        tmk_native!(TMK_VMM_LINUX_AARCH64, "linux", "aarch64");
        tmk_native!(TMK_VMM_MACOS_AARCH64, "macos", "aarch64");

        declare_artifacts! {
            /// TMK VMM for Windows x64
            TMK_VMM_WIN_X64,
            /// TMK VMM for Linux x64
            TMK_VMM_LINUX_X64,
            /// TMK VMM for MacOS x64
            TMK_VMM_WIN_AARCH64,
            /// TMK VMM for Linux aarch64
            TMK_VMM_LINUX_AARCH64,
            /// TMK VMM for MacOS aarch64
            TMK_VMM_MACOS_AARCH64,
            /// TMK VMM for Linux musl x64
            TMK_VMM_LINUX_X64_MUSL,
            /// TMK VMM for Linux musl aarch64
            TMK_VMM_LINUX_AARCH64_MUSL,
            /// TMK binary for x64
            SIMPLE_TMK_X64,
            /// TMK binary for aarch64
            SIMPLE_TMK_AARCH64,
        }
    }

    macro_rules! vmgstool_native {
        ($id_ty:ty, $os:literal, $arch:literal) => {
            /// vmgstool "native" executable (i.e:
            /// [`VMGSTOOL_WIN_X64`](const@VMGSTOOL_WIN_X64) when compiled on windows x86_64,
            /// [`VMGSTOOL_LINUX_AARCH64`](const@VMGSTOOL_LINUX_AARCH64) when compiled on linux aarch64,
            /// etc...)
            // xtask-fmt allow-target-arch oneoff-petri-native-test-deps
            #[cfg(all(target_os = $os, target_arch = $arch))]
            pub const VMGSTOOL_NATIVE: petri_artifacts_core::ArtifactHandle<$id_ty> =
                petri_artifacts_core::ArtifactHandle::new();
        };
    }

    vmgstool_native!(VMGSTOOL_WIN_X64, "windows", "x86_64");
    vmgstool_native!(VMGSTOOL_LINUX_X64, "linux", "x86_64");
    vmgstool_native!(VMGSTOOL_WIN_AARCH64, "windows", "aarch64");
    vmgstool_native!(VMGSTOOL_LINUX_AARCH64, "linux", "aarch64");
    vmgstool_native!(VMGSTOOL_MACOS_AARCH64, "macos", "aarch64");

    declare_artifacts! {
        /// vmgstool windows x86_64 executable
        VMGSTOOL_WIN_X64,
        /// vmgstool linux x86_64 executable
        VMGSTOOL_LINUX_X64,
        /// vmgstool windows aarch64 executable
        VMGSTOOL_WIN_AARCH64,
        /// vmgstool linux aarch64 executable
        VMGSTOOL_LINUX_AARCH64,
        /// vmgstool linux aarch64 executable
        VMGSTOOL_MACOS_AARCH64,
    }

    impl IsVmgsTool for VMGSTOOL_WIN_X64 {}
    impl IsVmgsTool for VMGSTOOL_LINUX_X64 {}
    impl IsVmgsTool for VMGSTOOL_WIN_AARCH64 {}
    impl IsVmgsTool for VMGSTOOL_LINUX_AARCH64 {}
    impl IsVmgsTool for VMGSTOOL_MACOS_AARCH64 {}
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
