// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Mapping from petri artifact IDs to flowey build/download selections.
//!
//! This module defines a lookup table that maps the string representation of
//! petri `ArtifactHandle` IDs (as output by `--list-required-artifacts`) to
//! their corresponding build selections and download artifacts.

use crate::_jobs::local_build_and_run_nextest_vmm_tests::BuildSelections;
use std::collections::BTreeSet;
use vmm_test_images::KnownTestArtifacts;

/// Result of resolving artifact requirements to build/download selections.
#[derive(Debug, Default)]
pub struct ResolvedArtifactSelections {
    /// What to build
    pub build: BuildSelections,
    /// What to download
    pub downloads: BTreeSet<KnownTestArtifacts>,
    /// Any unknown artifacts that couldn't be mapped
    pub unknown: Vec<String>,
    /// Target triple from the artifacts file (if present)
    pub target_from_file: Option<String>,
}

impl ResolvedArtifactSelections {
    /// Parse the JSON output from `--list-required-artifacts` and resolve to
    /// build/download selections.
    ///
    /// The `target_arch` and `target_os` parameters specify the target to
    /// validate against. If the JSON contains a `target` field, it will be
    /// checked to ensure it matches.
    pub fn from_artifact_list_json(
        json: &str,
        target_arch: target_lexicon::Architecture,
        target_os: target_lexicon::OperatingSystem,
    ) -> anyhow::Result<Self> {
        let parsed: ArtifactListOutput = serde_json::from_str(json)?;

        // Validate target if present in the JSON
        if let Some(ref file_target) = parsed.target {
            let expected_target = format!(
                "{}-{}",
                match target_arch {
                    target_lexicon::Architecture::X86_64 => "x86_64",
                    target_lexicon::Architecture::Aarch64(_) => "aarch64",
                    _ => "unknown",
                },
                match target_os {
                    target_lexicon::OperatingSystem::Windows => "pc-windows-msvc",
                    target_lexicon::OperatingSystem::Linux => "unknown-linux-gnu",
                    _ => "unknown",
                }
            );

            // Check if the target in the file is compatible with what we're building for
            if !file_target.contains(expected_target.split('-').next().unwrap_or(""))
                || (target_os == target_lexicon::OperatingSystem::Windows
                    && !file_target.contains("windows"))
                || (target_os == target_lexicon::OperatingSystem::Linux
                    && !file_target.contains("linux"))
            {
                anyhow::bail!(
                    "Target mismatch: artifacts file was generated for '{}', but building for '{}'",
                    file_target,
                    expected_target
                );
            }
        }

        let mut result = Self {
            target_from_file: parsed.target,
            ..Default::default()
        };

        // Process both required and optional artifacts
        for artifact in parsed.required.iter().chain(parsed.optional.iter()) {
            if !result.resolve_artifact(artifact, target_arch, target_os) {
                result.unknown.push(artifact.clone());
            }
        }

        Ok(result)
    }

    /// Resolve a single artifact ID and update selections. Returns true if the
    /// artifact was recognized.
    fn resolve_artifact(
        &mut self,
        artifact_id: &str,
        target_arch: target_lexicon::Architecture,
        target_os: target_lexicon::OperatingSystem,
    ) -> bool {
        // Artifact IDs are in the format:
        // "petri_artifacts_vmm_test::artifacts::ARTIFACT_NAME"
        // or nested like:
        // "petri_artifacts_vmm_test::artifacts::test_vhd::ARTIFACT_NAME"

        // Common artifacts
        let is_windows = matches!(target_os, target_lexicon::OperatingSystem::Windows);
        let _is_linux = matches!(target_os, target_lexicon::OperatingSystem::Linux);
        let is_x64 = matches!(target_arch, target_lexicon::Architecture::X86_64);
        let _is_aarch64 = matches!(target_arch, target_lexicon::Architecture::Aarch64(_));

        match artifact_id {
            // OpenVMM binary
            "petri_artifacts_vmm_test::artifacts::OPENVMM_WIN_X64"
            | "petri_artifacts_vmm_test::artifacts::OPENVMM_LINUX_X64"
            | "petri_artifacts_vmm_test::artifacts::OPENVMM_WIN_AARCH64"
            | "petri_artifacts_vmm_test::artifacts::OPENVMM_LINUX_AARCH64"
            | "petri_artifacts_vmm_test::artifacts::OPENVMM_MACOS_AARCH64" => {
                self.build.openvmm = true;
                true
            }

            // OpenHCL IGVM files
            "petri_artifacts_vmm_test::artifacts::openhcl_igvm::LATEST_STANDARD_X64"
            | "petri_artifacts_vmm_test::artifacts::openhcl_igvm::LATEST_STANDARD_DEV_KERNEL_X64"
            | "petri_artifacts_vmm_test::artifacts::openhcl_igvm::LATEST_CVM_X64"
            | "petri_artifacts_vmm_test::artifacts::openhcl_igvm::LATEST_LINUX_DIRECT_TEST_X64"
            | "petri_artifacts_vmm_test::artifacts::openhcl_igvm::LATEST_STANDARD_AARCH64"
            | "petri_artifacts_vmm_test::artifacts::openhcl_igvm::LATEST_STANDARD_DEV_KERNEL_AARCH64" =>
            {
                self.build.openhcl = true;
                true
            }

            // Release IGVM files (downloaded, not built)
            "petri_artifacts_vmm_test::artifacts::openhcl_igvm::LATEST_RELEASE_STANDARD_X64"
            | "petri_artifacts_vmm_test::artifacts::openhcl_igvm::LATEST_RELEASE_LINUX_DIRECT_X64"
            | "petri_artifacts_vmm_test::artifacts::openhcl_igvm::LATEST_RELEASE_STANDARD_AARCH64" =>
            {
                // These are downloaded from GitHub releases, not built
                // The download is handled separately
                true
            }

            // Guest test UEFI
            "petri_artifacts_vmm_test::artifacts::test_vhd::GUEST_TEST_UEFI_X64"
            | "petri_artifacts_vmm_test::artifacts::test_vhd::GUEST_TEST_UEFI_AARCH64" => {
                self.build.guest_test_uefi = true;
                true
            }

            // TMKs
            "petri_artifacts_vmm_test::artifacts::tmks::SIMPLE_TMK_X64"
            | "petri_artifacts_vmm_test::artifacts::tmks::SIMPLE_TMK_AARCH64" => {
                self.build.tmks = true;
                true
            }

            // TMK VMM
            "petri_artifacts_vmm_test::artifacts::tmks::TMK_VMM_WIN_X64"
            | "petri_artifacts_vmm_test::artifacts::tmks::TMK_VMM_WIN_AARCH64" => {
                self.build.tmk_vmm_windows = true;
                true
            }
            "petri_artifacts_vmm_test::artifacts::tmks::TMK_VMM_LINUX_X64"
            | "petri_artifacts_vmm_test::artifacts::tmks::TMK_VMM_LINUX_AARCH64"
            | "petri_artifacts_vmm_test::artifacts::tmks::TMK_VMM_LINUX_X64_MUSL"
            | "petri_artifacts_vmm_test::artifacts::tmks::TMK_VMM_LINUX_AARCH64_MUSL"
            | "petri_artifacts_vmm_test::artifacts::tmks::TMK_VMM_MACOS_AARCH64" => {
                self.build.tmk_vmm_linux = true;
                true
            }

            // VMGSTool
            "petri_artifacts_vmm_test::artifacts::VMGSTOOL_WIN_X64"
            | "petri_artifacts_vmm_test::artifacts::VMGSTOOL_WIN_AARCH64" => {
                self.build.vmgstool = true;
                true
            }
            "petri_artifacts_vmm_test::artifacts::VMGSTOOL_LINUX_X64"
            | "petri_artifacts_vmm_test::artifacts::VMGSTOOL_LINUX_AARCH64"
            | "petri_artifacts_vmm_test::artifacts::VMGSTOOL_MACOS_AARCH64" => {
                self.build.vmgstool = true;
                true
            }

            // TPM guest tests
            "petri_artifacts_vmm_test::artifacts::guest_tools::TPM_GUEST_TESTS_WINDOWS_X64" => {
                self.build.tpm_guest_tests_windows = true;
                true
            }
            "petri_artifacts_vmm_test::artifacts::guest_tools::TPM_GUEST_TESTS_LINUX_X64" => {
                self.build.tpm_guest_tests_linux = true;
                true
            }

            // Loadable firmware artifacts (these come from deps, not built)
            "petri_artifacts_vmm_test::artifacts::loadable::LINUX_DIRECT_TEST_KERNEL_X64"
            | "petri_artifacts_vmm_test::artifacts::loadable::LINUX_DIRECT_TEST_INITRD_X64"
            | "petri_artifacts_vmm_test::artifacts::loadable::LINUX_DIRECT_TEST_KERNEL_AARCH64"
            | "petri_artifacts_vmm_test::artifacts::loadable::LINUX_DIRECT_TEST_INITRD_AARCH64"
            | "petri_artifacts_vmm_test::artifacts::loadable::PCAT_FIRMWARE_X64"
            | "petri_artifacts_vmm_test::artifacts::loadable::SVGA_FIRMWARE_X64"
            | "petri_artifacts_vmm_test::artifacts::loadable::UEFI_FIRMWARE_X64"
            | "petri_artifacts_vmm_test::artifacts::loadable::UEFI_FIRMWARE_AARCH64" => {
                // These are resolved from OpenVMM deps, always available
                true
            }

            // Test VHDs (downloaded)
            "petri_artifacts_vmm_test::artifacts::test_vhd::GEN1_WINDOWS_DATA_CENTER_CORE2022_X64" =>
            {
                self.downloads
                    .insert(KnownTestArtifacts::Gen1WindowsDataCenterCore2022X64Vhd);
                // Requires pipette for Windows guests
                if is_windows {
                    self.build.pipette_windows = true;
                }
                true
            }
            "petri_artifacts_vmm_test::artifacts::test_vhd::GEN2_WINDOWS_DATA_CENTER_CORE2022_X64" =>
            {
                self.downloads
                    .insert(KnownTestArtifacts::Gen2WindowsDataCenterCore2022X64Vhd);
                if is_windows {
                    self.build.pipette_windows = true;
                }
                true
            }
            "petri_artifacts_vmm_test::artifacts::test_vhd::GEN2_WINDOWS_DATA_CENTER_CORE2025_X64" =>
            {
                self.downloads
                    .insert(KnownTestArtifacts::Gen2WindowsDataCenterCore2025X64Vhd);
                if is_windows {
                    self.build.pipette_windows = true;
                }
                // Requires prep_steps for CVM tests
                self.build.prep_steps = is_windows && is_x64;
                true
            }
            "petri_artifacts_vmm_test::artifacts::test_vhd::GEN2_WINDOWS_DATA_CENTER_CORE2025_X64_PREPPED" =>
            {
                // This is created by prep_steps, not downloaded
                self.build.prep_steps = is_windows && is_x64;
                if is_windows {
                    self.build.pipette_windows = true;
                }
                true
            }
            "petri_artifacts_vmm_test::artifacts::test_vhd::FREE_BSD_13_2_X64" => {
                self.downloads.insert(KnownTestArtifacts::FreeBsd13_2X64Vhd);
                true
            }
            "petri_artifacts_vmm_test::artifacts::test_vhd::UBUNTU_2404_SERVER_X64" => {
                self.downloads
                    .insert(KnownTestArtifacts::Ubuntu2404ServerX64Vhd);
                self.build.pipette_linux = true;
                true
            }
            "petri_artifacts_vmm_test::artifacts::test_vhd::UBUNTU_2504_SERVER_X64" => {
                self.downloads
                    .insert(KnownTestArtifacts::Ubuntu2504ServerX64Vhd);
                self.build.pipette_linux = true;
                true
            }
            "petri_artifacts_vmm_test::artifacts::test_vhd::UBUNTU_2404_SERVER_AARCH64" => {
                self.downloads
                    .insert(KnownTestArtifacts::Ubuntu2404ServerAarch64Vhd);
                self.build.pipette_linux = true;
                true
            }
            "petri_artifacts_vmm_test::artifacts::test_vhd::WINDOWS_11_ENTERPRISE_AARCH64" => {
                self.downloads
                    .insert(KnownTestArtifacts::Windows11EnterpriseAarch64Vhdx);
                if is_windows {
                    self.build.pipette_windows = true;
                }
                true
            }

            // Test ISOs (downloaded)
            "petri_artifacts_vmm_test::artifacts::test_iso::FREE_BSD_13_2_X64" => {
                self.downloads.insert(KnownTestArtifacts::FreeBsd13_2X64Iso);
                true
            }

            // Test VMGS files (downloaded)
            "petri_artifacts_vmm_test::artifacts::test_vmgs::VMGS_WITH_BOOT_ENTRY" => {
                self.downloads.insert(KnownTestArtifacts::VmgsWithBootEntry);
                true
            }

            // OpenHCL usermode binaries (built as part of IGVM)
            "petri_artifacts_vmm_test::artifacts::openhcl_igvm::um_bin::LATEST_LINUX_DIRECT_TEST_X64"
            | "petri_artifacts_vmm_test::artifacts::openhcl_igvm::um_dbg::LATEST_LINUX_DIRECT_TEST_X64" =>
            {
                self.build.openhcl = true;
                true
            }

            // Common artifacts (always available, no build needed)
            "petri_artifacts_common::artifacts::TEST_LOG_DIRECTORY" => true,

            // Pipette binaries (from petri_artifacts_common)
            "petri_artifacts_common::artifacts::PIPETTE_LINUX_X64"
            | "petri_artifacts_common::artifacts::PIPETTE_LINUX_AARCH64" => {
                self.build.pipette_linux = true;
                true
            }
            "petri_artifacts_common::artifacts::PIPETTE_WINDOWS_X64"
            | "petri_artifacts_common::artifacts::PIPETTE_WINDOWS_AARCH64" => {
                self.build.pipette_windows = true;
                true
            }

            _ => false,
        }
    }
}

/// JSON structure matching the output of `--list-required-artifacts`
#[derive(serde::Deserialize)]
struct ArtifactListOutput {
    /// Target triple the artifacts were discovered for (if present)
    #[serde(default)]
    target: Option<String>,
    required: Vec<String>,
    optional: Vec<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_resolve_openvmm() {
        let json = r#"{"required":["petri_artifacts_vmm_test::artifacts::OPENVMM_WIN_X64"],"optional":[]}"#;
        let result = ResolvedArtifactSelections::from_artifact_list_json(
            json,
            target_lexicon::Architecture::X86_64,
            target_lexicon::OperatingSystem::Windows,
        )
        .unwrap();

        assert!(result.build.openvmm);
        assert!(!result.build.openhcl);
        assert!(result.downloads.is_empty());
        assert!(result.unknown.is_empty());
    }

    #[test]
    fn test_resolve_with_downloads() {
        let json = r#"{"required":["petri_artifacts_vmm_test::artifacts::test_vhd::UBUNTU_2404_SERVER_X64"],"optional":[]}"#;
        let result = ResolvedArtifactSelections::from_artifact_list_json(
            json,
            target_lexicon::Architecture::X86_64,
            target_lexicon::OperatingSystem::Linux,
        )
        .unwrap();

        assert!(result.build.pipette_linux);
        assert!(
            result
                .downloads
                .contains(&KnownTestArtifacts::Ubuntu2404ServerX64Vhd)
        );
    }

    #[test]
    fn test_unknown_artifact() {
        let json = r#"{"required":["some::unknown::artifact"],"optional":[]}"#;
        let result = ResolvedArtifactSelections::from_artifact_list_json(
            json,
            target_lexicon::Architecture::X86_64,
            target_lexicon::OperatingSystem::Linux,
        )
        .unwrap();

        assert_eq!(result.unknown, vec!["some::unknown::artifact"]);
    }
}
