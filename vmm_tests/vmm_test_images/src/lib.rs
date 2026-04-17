// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! A crate containing the list of images stored in Azure Blob Storage for
//! in-tree VMM tests.
//!
//! NOTE: with the introduction of
//! [`petri_artifacts_vmm_test::artifacts::test_vhd`], this crate no longer
//! contains any interesting metadata about any VHDs, and only serves as a
//! bridge between the new petri artifact types in `test_vhd`, and existing code
//! that uses these types in flowey / xtask.
//!
//! FUTURE: this crate should be removed entirely, and flowey / xtask should be
//! updated to use the underlying artifact types themselves.

#![forbid(unsafe_code)]

use petri_artifacts_core::AsArtifactHandle;
use petri_artifacts_core::ErasedArtifactHandle;
use petri_artifacts_vmm_test::tags::IsHostedOnHvliteAzureBlobStore;

/// The VHDs currently stored in Azure Blob Storage.
#[cfg_attr(feature = "clap", derive(clap::ValueEnum))]
#[cfg_attr(feature = "clap", clap(rename_all = "verbatim"))]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
#[expect(missing_docs)] // Self-describing names
pub enum KnownTestArtifacts {
    Alpine323X64Vhd,
    Alpine323Aarch64Vhd,
    Gen1WindowsDataCenterCore2022X64Vhd,
    Gen2WindowsDataCenterCore2022X64Vhd,
    Gen2WindowsDataCenterCore2025X64Vhd,
    FreeBsd13_2X64Vhd,
    FreeBsd13_2X64Iso,
    Ubuntu2404ServerX64Vhd,
    Ubuntu2504ServerX64Vhd,
    Ubuntu2404ServerAarch64Vhd,
    Windows11EnterpriseAarch64Vhdx,
    VmgsWithBootEntry,
    VmgsWith16kTpm,
}

struct KnownTestArtifactMeta {
    variant: KnownTestArtifacts,
    handle_fn: fn() -> ErasedArtifactHandle,
    filename: &'static str,
    size: u64,
    download_name: &'static str,
}

const KNOWN_TEST_ARTIFACT_METADATA: &[KnownTestArtifactMeta] = {
    use petri_artifacts_vmm_test::artifacts::*;

    &[
        meta::<test_vhd::ALPINE_3_23_X64>(KnownTestArtifacts::Alpine323X64Vhd),
        meta::<test_vhd::ALPINE_3_23_AARCH64>(KnownTestArtifacts::Alpine323Aarch64Vhd),
        meta::<test_vhd::GEN1_WINDOWS_DATA_CENTER_CORE2022_X64>(
            KnownTestArtifacts::Gen1WindowsDataCenterCore2022X64Vhd,
        ),
        meta::<test_vhd::GEN2_WINDOWS_DATA_CENTER_CORE2022_X64>(
            KnownTestArtifacts::Gen2WindowsDataCenterCore2022X64Vhd,
        ),
        meta::<test_vhd::GEN2_WINDOWS_DATA_CENTER_CORE2025_X64>(
            KnownTestArtifacts::Gen2WindowsDataCenterCore2025X64Vhd,
        ),
        meta::<test_vhd::FREE_BSD_13_2_X64>(KnownTestArtifacts::FreeBsd13_2X64Vhd),
        meta::<test_iso::FREE_BSD_13_2_X64>(KnownTestArtifacts::FreeBsd13_2X64Iso),
        meta::<test_vhd::UBUNTU_2404_SERVER_X64>(KnownTestArtifacts::Ubuntu2404ServerX64Vhd),
        meta::<test_vhd::UBUNTU_2504_SERVER_X64>(KnownTestArtifacts::Ubuntu2504ServerX64Vhd),
        meta::<test_vhd::UBUNTU_2404_SERVER_AARCH64>(
            KnownTestArtifacts::Ubuntu2404ServerAarch64Vhd,
        ),
        meta::<test_vhd::WINDOWS_11_ENTERPRISE_AARCH64>(
            KnownTestArtifacts::Windows11EnterpriseAarch64Vhdx,
        ),
        meta::<test_vmgs::VMGS_WITH_BOOT_ENTRY>(KnownTestArtifacts::VmgsWithBootEntry),
        meta::<test_vmgs::VMGS_WITH_16K_TPM>(KnownTestArtifacts::VmgsWith16kTpm),
    ]
};

const fn meta<T: IsHostedOnHvliteAzureBlobStore>(
    variant: KnownTestArtifacts,
) -> KnownTestArtifactMeta {
    KnownTestArtifactMeta {
        variant,
        handle_fn: || petri_artifacts_core::ArtifactHandle::<T>::new().erase(),
        filename: T::FILENAME,
        size: T::SIZE,
        download_name: T::DOWNLOAD_NAME,
    }
}

impl KnownTestArtifacts {
    fn meta(self) -> &'static KnownTestArtifactMeta {
        KNOWN_TEST_ARTIFACT_METADATA
            .iter()
            .find(|KnownTestArtifactMeta { variant, .. }| *variant == self)
            .unwrap()
    }

    /// Get the name of the image.
    pub fn name(self) -> &'static str {
        self.meta().download_name
    }

    /// Get the filename of the image.
    pub fn filename(self) -> &'static str {
        self.meta().filename
    }

    /// Get the expected file size of the image.
    pub fn file_size(self) -> u64 {
        self.meta().size
    }

    /// Get the erased artifact handle for this image.
    pub fn artifact_handle(self) -> ErasedArtifactHandle {
        (self.meta().handle_fn)()
    }

    /// Get the image from its filename.
    pub fn from_filename(filename: &str) -> Option<Self> {
        Some(
            KNOWN_TEST_ARTIFACT_METADATA
                .iter()
                .find(|KnownTestArtifactMeta { filename: s, .. }| *s == filename)?
                .variant,
        )
    }

    /// Look up a known test artifact by its erased artifact handle.
    pub fn from_handle(id: ErasedArtifactHandle) -> Option<Self> {
        KNOWN_TEST_ARTIFACT_METADATA
            .iter()
            .find(|m| (m.handle_fn)() == id)
            .map(|m| m.variant)
    }
}
