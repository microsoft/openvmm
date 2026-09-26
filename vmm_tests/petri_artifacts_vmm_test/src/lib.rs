// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! `petri` test artifacts used by in-tree VMM tests

use petri_artifacts_core::ArtifactHandle;
use petri_artifacts_core::ArtifactId;
use petri_artifacts_core::AsArtifactHandle;
use petri_artifacts_core::ErasedArtifactHandle;

/// A type-erased artifact that holds references to information about a certain
/// test image that implements `IsHostedOnHvliteAzureBlobStore`
#[derive(Copy, Clone)]
pub struct ErasedVmmTestImage {
    artifact_id_str: &'static str,
    filename: &'static str,
    url_fn: fn() -> Option<String>,
    size: u64,
    download_name: &'static str,
}

impl std::fmt::Debug for ErasedVmmTestImage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.global_unique_id())
    }
}

impl serde::Serialize for ErasedVmmTestImage {
    fn serialize<S: serde::Serializer>(&self, ser: S) -> Result<S::Ok, S::Error> {
        ser.serialize_str(self.global_unique_id())
    }
}

impl<'de> serde::Deserialize<'de> for ErasedVmmTestImage {
    fn deserialize<D>(d: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let id = serde::Deserialize::deserialize(d)?;
        vmm_test_image_from_id(id)
            .ok_or_else(|| serde::de::Error::custom(format_args!("invalid artifact id: {}", id)))
    }
}

impl PartialEq<ErasedVmmTestImage> for ErasedVmmTestImage {
    fn eq(&self, other: &ErasedVmmTestImage) -> bool {
        self.global_unique_id() == other.global_unique_id()
    }
}

impl PartialEq<ErasedArtifactHandle> for ErasedVmmTestImage {
    fn eq(&self, other: &ErasedArtifactHandle) -> bool {
        self.global_unique_id() == other.global_unique_id()
    }
}

impl<A: ArtifactId> PartialEq<ArtifactHandle<A>> for ErasedVmmTestImage {
    fn eq(&self, other: &ArtifactHandle<A>) -> bool {
        self == &other.erase()
    }
}

impl PartialOrd for ErasedVmmTestImage {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for ErasedVmmTestImage {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.artifact_id_str.cmp(other.artifact_id_str)
    }
}

impl Eq for ErasedVmmTestImage {}

impl ErasedVmmTestImage {
    /// used to serialize the artifact handle when querying petri for test requirements
    pub fn global_unique_id(&self) -> &'static str {
        self.artifact_id_str
    }

    /// get the filename of the artifact
    pub fn filename(&self) -> &'static str {
        self.filename
    }

    /// get the relative path to the artifact
    pub fn url(&self) -> Option<String> {
        (self.url_fn)()
    }

    /// get the size of the artifact
    pub fn file_size(&self) -> u64 {
        self.size
    }

    /// get the download name of the artifact
    pub fn name(&self) -> &'static str {
        self.download_name
    }

    /// whether the image supports being backed by blob disk
    pub fn supports_blob_disk(&self) -> bool {
        (self.url_fn)().is_some()
    }
}

impl<T: ArtifactId + tags::IsHostedOnHvliteAzureBlobStore> From<ArtifactHandle<T>>
    for ErasedVmmTestImage
{
    fn from(_value: ArtifactHandle<T>) -> Self {
        Self {
            artifact_id_str: T::GLOBAL_UNIQUE_ID,
            filename: T::FILENAME,
            url_fn: T::url,
            size: T::SIZE,
            download_name: T::DOWNLOAD_NAME,
        }
    }
}

/// parse a vmm test image from a string (from the command line, for example)
pub fn parse_vmm_test_image(v: &str) -> Result<ErasedVmmTestImage, String> {
    vmm_test_images()
        .iter()
        .find(|&x| v == x.name() || v == x.filename())
        .ok_or("invalid image name".into())
        .copied()
}

/// Get all the VMM test images
pub fn vmm_test_images() -> &'static [ErasedVmmTestImage] {
    &vmm_test_images_macro_support::VMM_TEST_IMAGES
}

/// Get the vmm test image associated with the id (if any).
pub fn vmm_test_image_from_id(id: &str) -> Option<ErasedVmmTestImage> {
    vmm_test_images_macro_support::VMM_TEST_IMAGES
        .iter()
        .find(|&x| x.artifact_id_str == id)
        .copied()
}

/// Get the vmm test image associated with the id (if any).
pub fn vmm_test_image_from_filename(filename: &str) -> Option<ErasedVmmTestImage> {
    vmm_test_images_macro_support::VMM_TEST_IMAGES
        .iter()
        .find(|&x| x.filename == filename)
        .copied()
}

macro_rules! declare_vmm_test_images {
    (
        $(
            $(#[$doc:meta])*
            $name:ident(
                $filename:literal,
                $size:literal,
                $download_name:literal,
                $blob_storage:ident,
            )
        ),*
        $(,)?
    ) => {
        ::petri_artifacts_core::declare_artifacts_inner!($(
            $(#[$doc])*
            $name(
                $crate::artifacts::blob_disk::$blob_storage,
                $filename,
                ANY
            ),
        )*);

        $(impl $crate::tags::IsHostedOnHvliteAzureBlobStore for $name {
            const SIZE: u64 = $size;
            const DOWNLOAD_NAME: &'static str = $download_name;
        }

        const _: () = {
            use $crate::vmm_test_images_macro_support::linkme;
            use $crate::tags::IsHostedOnHvliteAzureBlobStore;
            use ::petri_artifacts_core::ArtifactId;

            // UNSAFETY: Needed for linkme.
            #[expect(unsafe_code)]
            #[linkme::distributed_slice($crate::vmm_test_images_macro_support::VMM_TEST_IMAGES)]
            #[linkme(crate = linkme)]
            static IMAGE: $crate::ErasedVmmTestImage = $crate::ErasedVmmTestImage {
                artifact_id_str: $name::GLOBAL_UNIQUE_ID,
                filename: $name::FILENAME,
                url_fn: $name::url,
                size: $name::SIZE,
                download_name: $name::DOWNLOAD_NAME,
            };
        };)*
    };
}

macro_rules! declare_prepped_vmm_test_images {
    (
        $(
            $(#[$doc:meta])*
            $name:ident($filename:literal)
        ),*
        $(,)?
    ) => {
        ::petri_artifacts_core::declare_artifacts_inner!($(
            $(#[$doc])*
            $name(::petri_artifacts_core::DOES_NOT_SUPPORT_BLOB_DISK, $filename, ANY),
        )*);
    };
}

/// Artifact declarations
pub mod artifacts {
    use petri_artifacts_core::declare_artifacts;

    macro_rules! openvmm_native {
        ($id_ty:ty, $os:literal, $arch:literal, $env:literal) => {
            /// openvmm "native" executable (i.e:
            /// [`OPENVMM_WINDOWS_X64`](const@OPENVMM_WINDOWS_X64) when compiled on windows x86_64,
            /// [`OPENVMM_LINUX_AARCH64`](const@OPENVMM_LINUX_AARCH64) when compiled on linux aarch64,
            /// etc...)
            // xtask-fmt allow-target-arch oneoff-petri-native-test-deps
            #[cfg(all(target_os = $os, target_arch = $arch, target_env = $env))]
            pub const OPENVMM_NATIVE: petri_artifacts_core::ArtifactHandle<$id_ty> =
                petri_artifacts_core::ArtifactHandle::new();
        };
    }

    openvmm_native!(OPENVMM_WINDOWS_X64, "windows", "x86_64", "msvc");
    openvmm_native!(OPENVMM_LINUX_X64, "linux", "x86_64", "gnu");
    openvmm_native!(OPENVMM_LINUX_X64_MUSL, "linux", "x86_64", "musl");
    openvmm_native!(OPENVMM_WINDOWS_AARCH64, "windows", "aarch64", "msvc");
    openvmm_native!(OPENVMM_LINUX_AARCH64, "linux", "aarch64", "gnu");
    openvmm_native!(OPENVMM_LINUX_AARCH64_MUSL, "linux", "aarch64", "musl");
    /// openvmm "native" executable
    // xtask-fmt allow-target-arch oneoff-petri-native-test-deps
    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    pub const OPENVMM_NATIVE: petri_artifacts_core::ArtifactHandle<OPENVMM_MACOS_AARCH64> =
        petri_artifacts_core::ArtifactHandle::new();

    macro_rules! openvmm_vhost_native {
        ($id_ty:ty, $os:literal, $arch:literal, $env:literal) => {
            /// openvmm_vhost "native" executable — the vhost-user backend binary.
            /// Only available on Linux (vhost-user requires Unix sockets).
            // xtask-fmt allow-target-arch oneoff-petri-native-test-deps
            #[cfg(all(target_os = $os, target_arch = $arch, target_env = $env))]
            pub const OPENVMM_VHOST_NATIVE: petri_artifacts_core::ArtifactHandle<$id_ty> =
                petri_artifacts_core::ArtifactHandle::new();
        };
    }
    openvmm_vhost_native!(OPENVMM_VHOST_LINUX_X64, "linux", "x86_64", "gnu");
    openvmm_vhost_native!(OPENVMM_VHOST_LINUX_X64_MUSL, "linux", "x86_64", "musl");
    openvmm_vhost_native!(OPENVMM_VHOST_LINUX_AARCH64, "linux", "aarch64", "gnu");
    openvmm_vhost_native!(OPENVMM_VHOST_LINUX_AARCH64_MUSL, "linux", "aarch64", "musl");

    declare_artifacts! {
        /// openvmm windows x86_64 executable
        OPENVMM_WINDOWS_X64("openvmm.exe", WINDOWS_X64),
        /// openvmm linux x86_64 executable
        OPENVMM_LINUX_X64("openvmm", LINUX_X64),
        /// openvmm linux x86_64 musl executable
        OPENVMM_LINUX_X64_MUSL("openvmm", LINUX_X64_MUSL),
        /// openvmm windows aarch64 executable
        OPENVMM_WINDOWS_AARCH64("openvmm.exe", WINDOWS_AARCH64),
        /// openvmm linux aarch64 executable
        OPENVMM_LINUX_AARCH64("openvmm", LINUX_AARCH64),
        /// openvmm linux aarch64 musl executable
        OPENVMM_LINUX_AARCH64_MUSL("openvmm", LINUX_AARCH64_MUSL),
        /// openvmm macos aarch64 executable
        OPENVMM_MACOS_AARCH64("openvmm", MACOS_AARCH64),
        /// openvmm_vhost linux x86_64 executable
        OPENVMM_VHOST_LINUX_X64("openvmm_vhost", LINUX_X64),
        /// openvmm_vhost linux x86_64 musl executable
        OPENVMM_VHOST_LINUX_X64_MUSL("openvmm_vhost", LINUX_X64_MUSL),
        /// openvmm_vhost linux aarch64 executable
        OPENVMM_VHOST_LINUX_AARCH64("openvmm_vhost", LINUX_AARCH64),
        /// openvmm_vhost linux aarch64 musl executable
        OPENVMM_VHOST_LINUX_AARCH64_MUSL("openvmm_vhost", LINUX_AARCH64_MUSL),
    }

    declare_artifacts! {
        /// QEMU Aarch64 system emulator for x86_64 Linux
        QEMU_SYSTEM_AARCH64_LINUX_X64("qemu-system-aarch64", LINUX_X64),
    }

    /// Guest-side tools used by the VMM tests.
    pub mod guest_tools {
        use petri_artifacts_core::declare_artifacts;

        declare_artifacts! {
            /// Windows x86_64 build of the `tpm_guest_tests` utility.
            TPM_GUEST_TESTS_WINDOWS_X64("tpm_guest_tests.exe", WINDOWS_X64),
            /// Linux x86_64 build of the `tpm_guest_tests` utility.
            TPM_GUEST_TESTS_LINUX_X64("tpm_guest_tests", LINUX_X64),
        }
    }

    /// Virtio-win driver artifacts from openvmm-deps.
    pub mod virtio_win {
        use petri_artifacts_core::declare_artifacts;

        declare_artifacts! {
            /// Extracted virtio-win driver package (all OS versions and architectures).
            VIRTIO_WINDOWS_DRIVERS("virtio-win", WINDOWS),
        }
    }

    /// Host-side tools used by the VMM tests.
    pub mod host_tools {
        use petri_artifacts_core::declare_artifacts;

        declare_artifacts! {
            /// Windows x86_64 build of the `test_igvm_agent_rpc_server` executable.
            TEST_IGVM_AGENT_RPC_SERVER_WINDOWS_X64(
                "test_igvm_agent_rpc_server.exe",
                WINDOWS_X64
            ),
            /// Windows x86_64 build of `flowey_hvlite`.
            FLOWEY_HVLITE_WINDOWS_X64("flowey_hvlite.exe", WINDOWS_X64),
            /// Linux x86_64 build of `flowey_hvlite`.
            FLOWEY_HVLITE_LINUX_X64("flowey_hvlite", LINUX_X64),
            /// Windows aarch64 build of `flowey_hvlite`.
            FLOWEY_HVLITE_WINDOWS_AARCH64("flowey_hvlite.exe", WINDOWS_AARCH64),
            /// Linux x86_64 build of the `incubator` binary.
            INCUBATOR_LINUX_X64("incubator", LINUX_X64),
            /// Windows x86_64 build of the `prep_steps` binary.
            PREP_STEPS_WINDOWS_X64("prep_steps.exe", WINDOWS_X64),
            /// Linux x86_64 build of the `prep_steps` binary.
            PREP_STEPS_LINUX_X64("prep_steps", LINUX_X64),
            /// Linux musl x86_64 build of the `prep_steps` binary.
            PREP_STEPS_LINUX_X64_MUSL("prep_steps", LINUX_X64_MUSL),
            /// Prebuilt cargo-nextest VMM tests archive for Windows x86_64.
            NEXTEST_VMM_TESTS_ARCHIVE_WINDOWS_X64("vmm_tests.tar.zst", WINDOWS_X64),
            /// Prebuilt cargo-nextest VMM tests archive for Windows aarch64.
            NEXTEST_VMM_TESTS_ARCHIVE_WINDOWS_AARCH64("vmm_tests.tar.zst", WINDOWS_AARCH64),
            /// Prebuilt cargo-nextest VMM tests archive for Linux x86_64.
            NEXTEST_VMM_TESTS_ARCHIVE_LINUX_X64("vmm_tests.tar.zst", LINUX_X64),
            /// Prebuilt cargo-nextest VMM tests archive for Linux musl x86_64.
            NEXTEST_VMM_TESTS_ARCHIVE_LINUX_X64_MUSL(
                "vmm_tests.tar.zst",
                LINUX_X64_MUSL
            ),
            /// Prebuilt cargo-nextest VMM tests archive for Linux musl aarch64.
            NEXTEST_VMM_TESTS_ARCHIVE_LINUX_AARCH64_MUSL(
                "vmm_tests.tar.zst",
                LINUX_AARCH64_MUSL
            ),
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

        /// Test linux direct bzImage kernel (from OpenVMM deps) for x86_64
        // xtask-fmt allow-target-arch oneoff-petri-native-test-deps
        #[cfg(target_arch = "x86_64")]
        pub const LINUX_DIRECT_TEST_BZIMAGE_NATIVE: petri_artifacts_core::ArtifactHandle<
            LINUX_DIRECT_TEST_BZIMAGE_X64,
        > = petri_artifacts_core::ArtifactHandle::new();

        declare_artifacts! {
            /// Test linux direct kernel for x64 (from OpenVMM deps)
            LINUX_DIRECT_TEST_KERNEL_X64("vmlinux", X64),
            /// Test linux direct initrd for x64 (from OpenVMM deps)
            LINUX_DIRECT_TEST_INITRD_X64("initrd", X64),
            /// Test linux direct kernel for aarch64 (from OpenVMM deps)
            LINUX_DIRECT_TEST_KERNEL_AARCH64("Image", AARCH64),
            /// Test linux direct initrd for arch64 (from OpenVMM deps)
            LINUX_DIRECT_TEST_INITRD_AARCH64("initrd", AARCH64),
            /// Test linux direct bzImage kernel for x64 (from OpenVMM deps)
            LINUX_DIRECT_TEST_BZIMAGE_X64("bzImage", X64),
            /// PCAT firmware DLL
            PCAT_FIRMWARE_X64("vmfirmwarepcat.dll", X64),
            /// SVGA firmware DLL
            SVGA_FIRMWARE_X64("VmEmulatedDevices.dll", X64),
            /// UEFI firmware for x64
            UEFI_FIRMWARE_X64("MSVM.fd", X64),
            /// UEFI firmware for aarch64
            UEFI_FIRMWARE_AARCH64("MSVM.fd", AARCH64),
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

        impl IsLoadable for LINUX_DIRECT_TEST_BZIMAGE_X64 {
            const ARCH: MachineArch = MachineArch::X86_64;
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

    /// Petritools disk images
    pub mod petritools {
        use petri_artifacts_core::declare_artifacts;

        declare_artifacts! {
            /// Petritools erofs image (x64)
            PETRITOOLS_EROFS_X64("petritools.erofs", X64),
            /// Petritools erofs image (aarch64)
            PETRITOOLS_EROFS_AARCH64("petritools.erofs", AARCH64),
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
            LATEST_STANDARD_X64("openhcl-x64.bin", X64),
            /// OpenHCL IGVM last release (standard)
            LATEST_RELEASE_STANDARD_X64("release-2511-x64-openhcl.bin", X64),
            /// OpenHCL IGVM (standard, with VTL2 dev kernel)
            LATEST_STANDARD_DEV_KERNEL_X64("openhcl-x64-devkern.bin", X64),
            /// OpenHCL IGVM (for CVM)
            LATEST_CVM_X64("openhcl-x64-cvm.bin", X64),
            /// OpenHCL IGVM (using a linux direct-boot test image instead of UEFI)
            LATEST_LINUX_DIRECT_TEST_X64("openhcl-x64-test-linux-direct.bin", X64),
            /// OpenHCL IGVM last release (using a linux direct-boot test image instead of UEFI)
            LATEST_RELEASE_LINUX_DIRECT_X64("release-2511-x64-direct-openhcl.bin", X64),
            /// OpenHCL IGVM (standard AARCH64)
            LATEST_STANDARD_AARCH64("openhcl-aarch64.bin", AARCH64),
            /// OpenHCL IGVM last release (standard AARCH64)
            LATEST_RELEASE_STANDARD_AARCH64("release-2511-aarch64-openhcl.bin", AARCH64),
            /// OpenHCL IGVM (standard AARCH64, with VTL2 dev kernel)
            LATEST_STANDARD_DEV_KERNEL_AARCH64("openhcl-aarch64-devkern.bin", AARCH64),
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
                LATEST_LINUX_DIRECT_TEST_X64("openvmm_hcl_msft", X64)
            }
        }

        /// OpenHCL debugging symbols for the usermode binary
        pub mod um_dbg {
            use petri_artifacts_core::declare_artifacts;

            declare_artifacts! {
                /// Usermode symbols for Linux direct
                LATEST_LINUX_DIRECT_TEST_X64("openvmm_hcl_msft.dbg", X64)
            }
        }
    }

    /// Azure storage account where test VHDs, ISOs, and VMGS files are stored
    pub const STORAGE_ACCOUNT: &str = "hvlitetestvhds";
    /// Azure container where test VHDs, ISOs, and VMGS files are stored
    pub const CONTAINER: &str = "vhds";
    /// URL of the Azure container where test VHDs, ISOs, and VMGS files are stored
    pub fn blob_storage_url() -> String {
        format!("https://{STORAGE_ACCOUNT}.blob.core.windows.net/{CONTAINER}/*")
    }
    /// Options to pass into `declare_artifacts_with_filename_and_target`
    pub mod blob_disk {
        use crate::artifacts::CONTAINER;
        use crate::artifacts::STORAGE_ACCOUNT;
        use petri_artifacts_core::ArtifactBlobStorage;

        /// The artifact supports being backed by blob disk
        pub const SUPPORTS_BLOB_DISK: Option<ArtifactBlobStorage> = Some(ArtifactBlobStorage {
            storage_account: STORAGE_ACCOUNT,
            container: CONTAINER,
        });
        pub use petri_artifacts_core::DOES_NOT_SUPPORT_BLOB_DISK;
    }

    /// Test VHD artifacts
    pub mod test_vhd {
        use petri_artifacts_common::tags::GuestQuirks;
        use petri_artifacts_common::tags::GuestQuirksInner;
        use petri_artifacts_common::tags::InitialRebootCondition;
        use petri_artifacts_common::tags::IsTestVhd;
        use petri_artifacts_common::tags::MachineArch;
        use petri_artifacts_common::tags::OsFlavor;
        use petri_artifacts_core::declare_artifacts;

        declare_artifacts! {
            /// guest_test_uefi.img, built for x86_64 from the in-tree `guest_test_uefi` codebase.
            GUEST_TEST_UEFI_X64("guest_test_uefi.img", X64),
            /// guest_test_uefi.img, built for aarch64 from the in-tree `guest_test_uefi` codebase.
            GUEST_TEST_UEFI_AARCH64("guest_test_uefi.img", AARCH64),
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

        declare_vmm_test_images! {
            /// Generation 1 windows test image
            GEN1_WINDOWS_DATA_CENTER_CORE2022_X64(
                "WindowsServer-2022-datacenter-core-smalldisk-20348.1906.230803.vhd",
                32214352384,
                "Gen1WindowsDataCenterCore2022X64Vhd",
                SUPPORTS_BLOB_DISK,
            )
        }

        impl IsTestVhd for GEN1_WINDOWS_DATA_CENTER_CORE2022_X64 {
            const OS_FLAVOR: OsFlavor = OsFlavor::Windows;
            const ARCH: MachineArch = MachineArch::X86_64;
        }

        declare_vmm_test_images! {
            /// Generation 2 windows test image
            GEN2_WINDOWS_DATA_CENTER_CORE2022_X64(
                "WindowsServer-2022-datacenter-core-smalldisk-g2-20348.1906.230803.vhd",
                32214352384,
                "Gen2WindowsDataCenterCore2022X64Vhd",
                SUPPORTS_BLOB_DISK,
            )
        }

        impl IsTestVhd for GEN2_WINDOWS_DATA_CENTER_CORE2022_X64 {
            const OS_FLAVOR: OsFlavor = OsFlavor::Windows;
            const ARCH: MachineArch = MachineArch::X86_64;
        }

        declare_vmm_test_images! {
            /// Generation 2 windows test image
            GEN2_WINDOWS_DATA_CENTER_CORE2025_X64(
                "WindowsServer-2025-datacenter-core-smalldisk-g2-26100.3476.250306.vhd",
                32214352384,
                "Gen2WindowsDataCenterCore2025X64Vhd",
                SUPPORTS_BLOB_DISK,
            )
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

        declare_vmm_test_images! {
            /// FreeBSD 13.2
            FREE_BSD_13_2_X64(
                "FreeBSD-13.2-RELEASE-amd64.vhd",
                6477005312,
                "FreeBsd13_2X64Vhd",
                SUPPORTS_BLOB_DISK,
            )
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

        declare_vmm_test_images! {
            /// Ubuntu 24.04 Server X64
            UBUNTU_2404_SERVER_X64(
                "ubuntu-24.04-server-cloudimg-amd64.vhd",
                3758211584,
                "Ubuntu2404ServerX64Vhd",
                SUPPORTS_BLOB_DISK,
            )
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

        declare_vmm_test_images! {
            /// Ubuntu 25.04 Server X64
            UBUNTU_2504_SERVER_X64(
                "ubuntu-25.04-server-cloudimg-amd64.vhd",
                3758211584,
                "Ubuntu2504ServerX64Vhd",
                SUPPORTS_BLOB_DISK,
            )
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

        declare_vmm_test_images! {
            /// Alpine Linux 3.23.2 x64 UEFI nocloud cloud-init
            ALPINE_3_23_X64(
                "nocloud_alpine-3.23.2-x86_64-uefi-cloudinit-r0.vhd",
                224494080,
                "Alpine323X64Vhd",
                SUPPORTS_BLOB_DISK,
            )
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

        declare_vmm_test_images! {
            /// Alpine Linux 3.23.2 aarch64 UEFI nocloud cloud-init
            ALPINE_3_23_AARCH64(
                "nocloud_alpine-3.23.2-aarch64-uefi-cloudinit-r0.vhd",
                258015744,
                "Alpine323Aarch64Vhd",
                SUPPORTS_BLOB_DISK,
            )
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

        declare_vmm_test_images! {
            /// Ubuntu 24.04 Server Aarch64
            UBUNTU_2404_SERVER_AARCH64(
                "ubuntu-24.04-server-cloudimg-arm64.vhd",
                3758211584,
                "Ubuntu2404ServerAarch64Vhd",
                SUPPORTS_BLOB_DISK,
            )
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

        declare_vmm_test_images! {
            /// Windows 11 Enterprise ARM64 24H2
            WINDOWS_11_ENTERPRISE_AARCH64(
                "windows11preview-arm64-win11-24h2-ent-26100.3775.250406-1.vhdx",
                24398266368,
                "Windows11EnterpriseAarch64Vhdx",
                // blob disk does not support VHDX files
                DOES_NOT_SUPPORT_BLOB_DISK,
            )
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

        // VHDs that are created by pre-preparation automation

        declare_prepped_vmm_test_images! {
            /// Generation 2 windows test image
            GEN2_WINDOWS_DATA_CENTER_CORE2025_X64_PREPPED(
                "WindowsServer-2025-datacenter-core-smalldisk-g2-26100.3476.250306-prepped.vhd"
            )
        }

        impl IsTestVhd for GEN2_WINDOWS_DATA_CENTER_CORE2025_X64_PREPPED {
            const OS_FLAVOR: OsFlavor = GEN2_WINDOWS_DATA_CENTER_CORE2025_X64::OS_FLAVOR;
            const ARCH: MachineArch = GEN2_WINDOWS_DATA_CENTER_CORE2025_X64::ARCH;

            fn quirks() -> GuestQuirks {
                GEN2_WINDOWS_DATA_CENTER_CORE2025_X64::quirks()
            }
        }

        declare_prepped_vmm_test_images! {
            /// Generation 2 windows test image
            GEN2_WINDOWS_DATA_CENTER_CORE2022_X64_NO_VMBUS_PREPPED(
                "WindowsServer-2022-datacenter-core-smalldisk-g2-20348.1906.230803-no-vmbus-prepped.vhd"
            )
        }

        impl IsTestVhd for GEN2_WINDOWS_DATA_CENTER_CORE2022_X64_NO_VMBUS_PREPPED {
            const OS_FLAVOR: OsFlavor = GEN2_WINDOWS_DATA_CENTER_CORE2022_X64::OS_FLAVOR;
            const ARCH: MachineArch = GEN2_WINDOWS_DATA_CENTER_CORE2022_X64::ARCH;
        }
    }

    /// Test ISO artifacts
    pub mod test_iso {
        use petri_artifacts_common::tags::GuestQuirks;
        use petri_artifacts_common::tags::GuestQuirksInner;
        use petri_artifacts_common::tags::IsTestIso;
        use petri_artifacts_common::tags::MachineArch;
        use petri_artifacts_common::tags::OsFlavor;

        declare_vmm_test_images! {
            /// FreeBSD 13.2
            FREE_BSD_13_2_X64(
                "FreeBSD-13.2-RELEASE-amd64-dvd1.iso",
                4245487616,
                "FreeBsd13_2X64Iso",
                SUPPORTS_BLOB_DISK,
            )
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
    }

    /// Test VMGS artifacts
    pub mod test_vmgs {
        use petri_artifacts_common::tags::IsTestVmgs;

        // These could support blob disk in some cases, but Petri doesn't support
        // remote VMGS files and they are small, so just disable it for now.
        declare_vmm_test_images! {
            /// VMGS file containing a UEFI boot entry
            ///
            /// The file was generated by booting an arbitrary Windows VHD
            /// (different from the ones used for testing in CI) in OpenVMM
            /// with a persistent VMGS file enabled. This is useful for testing
            /// whether default_boot_always_attempt works to boot other VHDs.
            VMGS_WITH_BOOT_ENTRY(
                "sample-vmgs.vhd",
                4194816,
                "VmgsWithBootEntry",
                DOES_NOT_SUPPORT_BLOB_DISK,
            ),
            /// VMGS file containing a 16k vTPM blob
            ///
            /// This file was created by creating a 16k vTPM blob and loading
            /// it into file index 3 of a blank VMGS file.
            VMGS_WITH_16K_TPM(
                "tpm-16k-vmgs.vhd",
                4194816,
                "VmgsWith16kTpm",
                DOES_NOT_SUPPORT_BLOB_DISK,
            ),
        }

        impl IsTestVmgs for VMGS_WITH_BOOT_ENTRY {}

        impl IsTestVmgs for VMGS_WITH_16K_TPM {}
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

        tmk_native!(TMK_VMM_WINDOWS_X64, "windows", "x86_64");
        tmk_native!(TMK_VMM_LINUX_X64_MUSL, "linux", "x86_64");
        tmk_native!(TMK_VMM_WINDOWS_AARCH64, "windows", "aarch64");
        tmk_native!(TMK_VMM_LINUX_AARCH64_MUSL, "linux", "aarch64");
        tmk_native!(TMK_VMM_MACOS_AARCH64, "macos", "aarch64");

        declare_artifacts! {
            /// TMK VMM for Windows x86_64.
            TMK_VMM_WINDOWS_X64("tmk_vmm.exe", WINDOWS_X64),
            /// TMK VMM for Windows aarch64.
            TMK_VMM_WINDOWS_AARCH64("tmk_vmm.exe", WINDOWS_AARCH64),
            /// TMK VMM for macOS aarch64.
            TMK_VMM_MACOS_AARCH64("tmk_vmm", MACOS_AARCH64),
            /// TMK VMM for Linux musl x86_64.
            TMK_VMM_LINUX_X64_MUSL("tmk_vmm", LINUX_X64_MUSL),
            /// TMK VMM for Linux musl aarch64.
            TMK_VMM_LINUX_AARCH64_MUSL("tmk_vmm", LINUX_AARCH64_MUSL),
            /// TMK binary for x86_64.
            SIMPLE_TMK_X64("simple_tmk", X64),
            /// TMK binary for aarch64.
            SIMPLE_TMK_AARCH64("simple_tmk", AARCH64),
        }
    }

    /// VmgsTool artifacts
    pub mod vmgstool {
        use petri_artifacts_common::tags::IsVmgsTool;
        use petri_artifacts_core::declare_artifacts;

        macro_rules! vmgstool_native {
            ($id_ty:ty, $os:literal, $arch:literal) => {
                /// vmgstool "native" executable (i.e:
                /// [`VMGSTOOL_WINDOWS_X64`](const@VMGSTOOL_WINDOWS_X64) when compiled on windows x86_64,
                /// [`VMGSTOOL_LINUX_AARCH64`](const@VMGSTOOL_LINUX_AARCH64) when compiled on linux aarch64,
                /// etc...)
                // xtask-fmt allow-target-arch oneoff-petri-native-test-deps
                #[cfg(all(target_os = $os, target_arch = $arch))]
                pub const VMGSTOOL_NATIVE: petri_artifacts_core::ArtifactHandle<$id_ty> =
                    petri_artifacts_core::ArtifactHandle::new();
            };
        }

        vmgstool_native!(VMGSTOOL_WINDOWS_X64, "windows", "x86_64");
        vmgstool_native!(VMGSTOOL_LINUX_X64, "linux", "x86_64");
        vmgstool_native!(VMGSTOOL_WINDOWS_AARCH64, "windows", "aarch64");
        vmgstool_native!(VMGSTOOL_LINUX_AARCH64, "linux", "aarch64");
        vmgstool_native!(VMGSTOOL_MACOS_AARCH64, "macos", "aarch64");

        macro_rules! vmgstool_dev_native {
            ($id_ty:ty, $os:literal, $arch:literal) => {
                /// vmgstool-dev "native" executable (i.e:
                /// [`VMGSTOOL_DEV_WINDOWS_X64`](const@VMGSTOOL_DEV_WINDOWS_X64) when compiled on windows x86_64,
                /// [`VMGSTOOL_DEV_LINUX_AARCH64`](const@VMGSTOOL_DEV_LINUX_AARCH64) when compiled on linux aarch64,
                /// etc...)
                // xtask-fmt allow-target-arch oneoff-petri-native-test-deps
                #[cfg(all(target_os = $os, target_arch = $arch))]
                pub const VMGSTOOL_DEV_NATIVE: petri_artifacts_core::ArtifactHandle<$id_ty> =
                    petri_artifacts_core::ArtifactHandle::new();
            };
        }

        vmgstool_dev_native!(VMGSTOOL_DEV_WINDOWS_X64, "windows", "x86_64");
        vmgstool_dev_native!(VMGSTOOL_DEV_LINUX_X64, "linux", "x86_64");
        vmgstool_dev_native!(VMGSTOOL_DEV_WINDOWS_AARCH64, "windows", "aarch64");
        vmgstool_dev_native!(VMGSTOOL_DEV_LINUX_AARCH64, "linux", "aarch64");
        vmgstool_dev_native!(VMGSTOOL_DEV_MACOS_AARCH64, "macos", "aarch64");

        declare_artifacts! {
            /// vmgstool windows x86_64 executable
            VMGSTOOL_WINDOWS_X64("vmgstool.exe", WINDOWS_X64),
            /// vmgstool linux x86_64 executable
            VMGSTOOL_LINUX_X64("vmgstool", LINUX_X64),
            /// vmgstool windows aarch64 executable
            VMGSTOOL_WINDOWS_AARCH64("vmgstool.exe", WINDOWS_AARCH64),
            /// vmgstool linux aarch64 executable
            VMGSTOOL_LINUX_AARCH64("vmgstool", LINUX_AARCH64),
            /// vmgstool macos aarch64 executable
            VMGSTOOL_MACOS_AARCH64("vmgstool", MACOS_AARCH64),
            /// vmgstool-dev windows x86_64 executable
            VMGSTOOL_DEV_WINDOWS_X64("vmgstool-dev.exe", WINDOWS_X64),
            /// vmgstool-dev linux x86_64 executable
            VMGSTOOL_DEV_LINUX_X64("vmgstool-dev", LINUX_X64),
            /// vmgstool-dev windows aarch64 executable
            VMGSTOOL_DEV_WINDOWS_AARCH64("vmgstool-dev.exe", WINDOWS_AARCH64),
            /// vmgstool-dev linux aarch64 executable
            VMGSTOOL_DEV_LINUX_AARCH64("vmgstool-dev", LINUX_AARCH64),
            /// vmgstool-dev macos aarch64 executable
            VMGSTOOL_DEV_MACOS_AARCH64("vmgstool-dev", MACOS_AARCH64),
        }

        impl IsVmgsTool for VMGSTOOL_WINDOWS_X64 {}
        impl IsVmgsTool for VMGSTOOL_LINUX_X64 {}
        impl IsVmgsTool for VMGSTOOL_WINDOWS_AARCH64 {}
        impl IsVmgsTool for VMGSTOOL_LINUX_AARCH64 {}
        impl IsVmgsTool for VMGSTOOL_MACOS_AARCH64 {}
        impl IsVmgsTool for VMGSTOOL_DEV_WINDOWS_X64 {}
        impl IsVmgsTool for VMGSTOOL_DEV_LINUX_X64 {}
        impl IsVmgsTool for VMGSTOOL_DEV_WINDOWS_AARCH64 {}
        impl IsVmgsTool for VMGSTOOL_DEV_LINUX_AARCH64 {}
        impl IsVmgsTool for VMGSTOOL_DEV_MACOS_AARCH64 {}
    }
}

/// Artifact tag trait declarations
pub mod tags {
    use petri_artifacts_core::ArtifactId;

    /// Artifact is associated with a file hosted in HvLite's microsoft-internal
    /// Azure Blob Store.
    pub trait IsHostedOnHvliteAzureBlobStore: ArtifactId {
        /// Size of the file in bytes
        const SIZE: u64;
        /// CLI name for `cargo xtask guest-test download-image --artifacts <name>`
        const DOWNLOAD_NAME: &'static str;
    }
}

#[doc(hidden)]
pub mod vmm_test_images_macro_support {
    use crate::ErasedVmmTestImage;
    pub use linkme;

    #[linkme::distributed_slice]
    pub static VMM_TEST_IMAGES: [ErasedVmmTestImage];
}
