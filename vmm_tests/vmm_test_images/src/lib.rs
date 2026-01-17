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

use petri_artifacts_vmm_test::tags::IsHostedOnHvliteAzureBlobStore;

/// The VHDs currently stored in Azure Blob Storage.
#[cfg_attr(feature = "clap", derive(clap::ValueEnum))]
#[cfg_attr(feature = "clap", clap(rename_all = "verbatim"))]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
#[expect(missing_docs)] // Self-describing names
pub enum KnownTestArtifacts {
    Alpine323X64Vhd,
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
}

struct KnownTestArtifactMeta {
    variant: KnownTestArtifacts,
    filename: &'static str,
    size: u64,
}

impl KnownTestArtifactMeta {
    const fn new(variant: KnownTestArtifacts, filename: &'static str, size: u64) -> Self {
        Self {
            variant,
            filename,
            size,
        }
    }
}

// linear scan to find entries is OK, given how few entries there are
const KNOWN_TEST_ARTIFACT_METADATA: &[KnownTestArtifactMeta] = &[
    KnownTestArtifactMeta::new(
        KnownTestArtifacts::Alpine323X64Vhd,
        petri_artifacts_vmm_test::artifacts::test_vhd::ALPINE_3_23_X64::FILENAME,
        petri_artifacts_vmm_test::artifacts::test_vhd::ALPINE_3_23_X64::SIZE,
    ),
    KnownTestArtifactMeta::new(
        KnownTestArtifacts::Gen1WindowsDataCenterCore2022X64Vhd,
        petri_artifacts_vmm_test::artifacts::test_vhd::GEN1_WINDOWS_DATA_CENTER_CORE2022_X64::FILENAME,
        petri_artifacts_vmm_test::artifacts::test_vhd::GEN1_WINDOWS_DATA_CENTER_CORE2022_X64::SIZE,
    ),
    KnownTestArtifactMeta::new(
        KnownTestArtifacts::Gen2WindowsDataCenterCore2022X64Vhd,
        petri_artifacts_vmm_test::artifacts::test_vhd::GEN2_WINDOWS_DATA_CENTER_CORE2022_X64::FILENAME,
        petri_artifacts_vmm_test::artifacts::test_vhd::GEN2_WINDOWS_DATA_CENTER_CORE2022_X64::SIZE,
    ),
    KnownTestArtifactMeta::new(
        KnownTestArtifacts::Gen2WindowsDataCenterCore2025X64Vhd,
        petri_artifacts_vmm_test::artifacts::test_vhd::GEN2_WINDOWS_DATA_CENTER_CORE2025_X64::FILENAME,
        petri_artifacts_vmm_test::artifacts::test_vhd::GEN2_WINDOWS_DATA_CENTER_CORE2025_X64::SIZE,
    ),
    KnownTestArtifactMeta::new(
        KnownTestArtifacts::FreeBsd13_2X64Vhd,
        petri_artifacts_vmm_test::artifacts::test_vhd::FREE_BSD_13_2_X64::FILENAME,
        petri_artifacts_vmm_test::artifacts::test_vhd::FREE_BSD_13_2_X64::SIZE,
    ),
    KnownTestArtifactMeta::new(
        KnownTestArtifacts::FreeBsd13_2X64Iso,
        petri_artifacts_vmm_test::artifacts::test_iso::FREE_BSD_13_2_X64::FILENAME,
        petri_artifacts_vmm_test::artifacts::test_iso::FREE_BSD_13_2_X64::SIZE,
    ),
    KnownTestArtifactMeta::new(
        KnownTestArtifacts::Ubuntu2404ServerX64Vhd,
        petri_artifacts_vmm_test::artifacts::test_vhd::UBUNTU_2404_SERVER_X64::FILENAME,
        petri_artifacts_vmm_test::artifacts::test_vhd::UBUNTU_2404_SERVER_X64::SIZE,
    ),
        KnownTestArtifactMeta::new(
        KnownTestArtifacts::Ubuntu2504ServerX64Vhd,
        petri_artifacts_vmm_test::artifacts::test_vhd::UBUNTU_2504_SERVER_X64::FILENAME,
        petri_artifacts_vmm_test::artifacts::test_vhd::UBUNTU_2504_SERVER_X64::SIZE,
    ),
    KnownTestArtifactMeta::new(
        KnownTestArtifacts::Ubuntu2404ServerAarch64Vhd,
        petri_artifacts_vmm_test::artifacts::test_vhd::UBUNTU_2404_SERVER_AARCH64::FILENAME,
        petri_artifacts_vmm_test::artifacts::test_vhd::UBUNTU_2404_SERVER_AARCH64::SIZE,
    ),
    KnownTestArtifactMeta::new(
        KnownTestArtifacts::Windows11EnterpriseAarch64Vhdx,
        petri_artifacts_vmm_test::artifacts::test_vhd::WINDOWS_11_ENTERPRISE_AARCH64::FILENAME,
        petri_artifacts_vmm_test::artifacts::test_vhd::WINDOWS_11_ENTERPRISE_AARCH64::SIZE
    ),
    KnownTestArtifactMeta::new(
        KnownTestArtifacts::VmgsWithBootEntry,
        petri_artifacts_vmm_test::artifacts::test_vmgs::VMGS_WITH_BOOT_ENTRY::FILENAME,
        petri_artifacts_vmm_test::artifacts::test_vmgs::VMGS_WITH_BOOT_ENTRY::SIZE,
    ),
];

impl KnownTestArtifacts {
    /// Get the name of the image.
    pub fn name(self) -> String {
        format!("{:?}", self)
    }

    /// Get the filename of the image.
    pub fn filename(self) -> &'static str {
        KNOWN_TEST_ARTIFACT_METADATA
            .iter()
            .find(|KnownTestArtifactMeta { variant, .. }| *variant == self)
            .unwrap()
            .filename
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

    /// Get the expected file size of the image.
    pub fn file_size(self) -> u64 {
        KNOWN_TEST_ARTIFACT_METADATA
            .iter()
            .find(|KnownTestArtifactMeta { variant, .. }| *variant == self)
            .unwrap()
            .size
    }
}
