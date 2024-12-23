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
#![warn(missing_docs)]

use petri_artifacts_vmm_test::tags::IsHostedOnHvliteAzureBlobStore;

/// The VHDs currently stored in Azure Blob Storage.
#[cfg_attr(feature = "clap", derive(clap::ValueEnum))]
#[cfg_attr(feature = "clap", clap(rename_all = "verbatim"))]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
#[allow(missing_docs)] // Self-describing names
pub enum KnownVhd {
    Gen1WindowsDataCenterCore2022,
    Gen2WindowsDataCenterCore2022,
    FreeBsd13_2,
    Ubuntu2204Server,
    Ubuntu2404ServerAarch64,
}

struct KnownVhdMeta {
    variant: KnownVhd,
    filename: &'static str,
    size: u64,
}

impl KnownVhdMeta {
    const fn new(variant: KnownVhd, filename: &'static str, size: u64) -> Self {
        Self {
            variant,
            filename,
            size,
        }
    }
}

// linear scan to find entries is OK, given how few entries there are
const KNOWN_VHD_METADATA: &[KnownVhdMeta] = &[
    KnownVhdMeta::new(
        KnownVhd::Gen1WindowsDataCenterCore2022,
        petri_artifacts_vmm_test::artifacts::test_vhd::GEN1_WINDOWS_DATA_CENTER_CORE2022_X64::FILENAME,
        petri_artifacts_vmm_test::artifacts::test_vhd::GEN1_WINDOWS_DATA_CENTER_CORE2022_X64::SIZE,
    ),
    KnownVhdMeta::new(
        KnownVhd::Gen2WindowsDataCenterCore2022,
        petri_artifacts_vmm_test::artifacts::test_vhd::GEN2_WINDOWS_DATA_CENTER_CORE2022_X64::FILENAME,
        petri_artifacts_vmm_test::artifacts::test_vhd::GEN2_WINDOWS_DATA_CENTER_CORE2022_X64::SIZE,
    ),
    KnownVhdMeta::new(
        KnownVhd::FreeBsd13_2,
        petri_artifacts_vmm_test::artifacts::test_vhd::FREE_BSD_13_2_X64::FILENAME,
        petri_artifacts_vmm_test::artifacts::test_vhd::FREE_BSD_13_2_X64::SIZE,
    ),
    KnownVhdMeta::new(
        KnownVhd::Ubuntu2204Server,
        petri_artifacts_vmm_test::artifacts::test_vhd::UBUNTU_2204_SERVER_X64::FILENAME,
        petri_artifacts_vmm_test::artifacts::test_vhd::UBUNTU_2204_SERVER_X64::SIZE,
    ),
    KnownVhdMeta::new(
        KnownVhd::Ubuntu2404ServerAarch64,
        petri_artifacts_vmm_test::artifacts::test_vhd::UBUNTU_2404_SERVER_AARCH64::FILENAME,
        petri_artifacts_vmm_test::artifacts::test_vhd::UBUNTU_2404_SERVER_AARCH64::SIZE,
    ),
];

impl KnownVhd {
    /// Get the name of the image.
    pub fn name(self) -> String {
        format!("{:?}", self)
    }

    /// Get the filename of the image.
    pub fn filename(self) -> &'static str {
        KNOWN_VHD_METADATA
            .iter()
            .find(|KnownVhdMeta { variant, .. }| *variant == self)
            .unwrap()
            .filename
    }

    /// Get the image from its filename.
    pub fn from_filename(filename: &str) -> Option<Self> {
        Some(
            KNOWN_VHD_METADATA
                .iter()
                .find(|KnownVhdMeta { filename: s, .. }| *s == filename)?
                .variant,
        )
    }

    /// Get the expected file size of the image.
    pub fn file_size(self) -> u64 {
        KNOWN_VHD_METADATA
            .iter()
            .find(|KnownVhdMeta { variant, .. }| *variant == self)
            .unwrap()
            .size
    }
}

/// The ISOs currently stored in Azure Blob Storage.
#[cfg_attr(feature = "clap", derive(clap::ValueEnum))]
#[cfg_attr(feature = "clap", clap(rename_all = "verbatim"))]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
#[allow(missing_docs)] // Self-describing names
pub enum KnownIso {
    FreeBsd13_2,
}

struct KnownIsoMeta {
    variant: KnownIso,
    filename: &'static str,
    size: u64,
}

impl KnownIsoMeta {
    const fn new(variant: KnownIso, filename: &'static str, size: u64) -> Self {
        Self {
            variant,
            filename,
            size,
        }
    }
}

// linear scan to find entries is OK, given how few entries there are
const KNOWN_ISO_METADATA: &[KnownIsoMeta] = &[KnownIsoMeta::new(
    KnownIso::FreeBsd13_2,
    petri_artifacts_vmm_test::artifacts::test_iso::FREE_BSD_13_2_X64::FILENAME,
    petri_artifacts_vmm_test::artifacts::test_iso::FREE_BSD_13_2_X64::SIZE,
)];

impl KnownIso {
    /// Get the name of the image.
    pub fn name(self) -> String {
        format!("{:?}", self)
    }

    /// Get the filename of the image.
    pub fn filename(self) -> &'static str {
        KNOWN_ISO_METADATA
            .iter()
            .find(|KnownIsoMeta { variant, .. }| *variant == self)
            .unwrap()
            .filename
    }

    /// Get the image from its filename.
    pub fn from_filename(filename: &str) -> Option<Self> {
        Some(
            KNOWN_ISO_METADATA
                .iter()
                .find(|KnownIsoMeta { filename: s, .. }| *s == filename)?
                .variant,
        )
    }

    /// Get the expected file size of the image.
    pub fn file_size(self) -> u64 {
        KNOWN_ISO_METADATA
            .iter()
            .find(|KnownIsoMeta { variant, .. }| *variant == self)
            .unwrap()
            .size
    }
}
