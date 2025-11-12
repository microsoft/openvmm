// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Centralized list of constants enumerating available GitHub build pools.

use flowey::pipeline::prelude::*;

/// This overrides the default image with a larger disk image for use with
/// jobs that require more than the default disk space (e.g. to ensure vmm_tests
/// have enough space to download test VHDs)
pub fn windows_amd_self_hosted_largedisk() -> GhRunner {
    GhRunner::SelfHosted(vec![
        "self-hosted".to_string(),
        "1ES.Pool=OpenVMM-GitHub-Win-Pool-WestUS3".to_string(),
        "1ES.ImageOverride=HvLite-CI-Win-Ge-Image-256GB".to_string(),
    ])
}

/// This overrides the default image with a larger disk image for use with
/// jobs that require more than the default disk space (e.g. to ensure vmm_tests
/// have enough space to download test VHDs)
pub fn windows_intel_self_hosted_largedisk() -> GhRunner {
    GhRunner::SelfHosted(vec![
        "self-hosted".to_string(),
        "1ES.Pool=OpenVMM-GitHub-Win-Pool-Intel-WestUS3".to_string(),
        "1ES.ImageOverride=HvLite-CI-Win-Ge-Image-256GB".to_string(),
    ])
}

pub fn windows_arm_self_hosted() -> GhRunner {
    GhRunner::SelfHosted(vec![
        "self-hosted".to_string(),
        "1ES.Pool=OpenVMM-GitHub-ARM64-Pool-WestUS2".to_string(),
        "1ES.ImageOverride=OpenVMM-CI-Windows-ARM64".to_string(),
    ])
}

pub fn linux_arm_self_hosted() -> GhRunner {
    GhRunner::SelfHosted(vec![
        "self-hosted".to_string(),
        "1ES.Pool=OpenVMM-GitHub-ARM64-Pool-WestUS2".to_string(),
        "1ES.ImageOverride=OpenVMM-CI-Ubuntu-ARM64".to_string(),
    ])
}

pub fn linux_self_hosted_largedisk() -> GhRunner {
    GhRunner::SelfHosted(vec![
        "self-hosted".to_string(),
        "1ES.Pool=OpenVMM-GitHub-Linux-Pool-WestUS3".to_string(),
        "1ES.ImageOverride=MMSUbuntu22.04-256GB".to_string(),
    ])
}

pub fn gh_hosted_x64_windows() -> GhRunner {
    GhRunner::GhHosted(GhRunnerOsLabel::WindowsLatest)
}

pub fn gh_hosted_x64_linux() -> GhRunner {
    GhRunner::GhHosted(GhRunnerOsLabel::UbuntuLatest)
}

pub fn gh_hosted_arm_windows() -> GhRunner {
    GhRunner::GhHosted(GhRunnerOsLabel::Windows11Arm)
}

pub fn gh_hosted_arm_linux() -> GhRunner {
    GhRunner::GhHosted(GhRunnerOsLabel::Ubuntu2404Arm)
}

pub fn windows_arm_self_hosted_baremetal() -> GhRunner {
    GhRunner::SelfHosted(vec![
        "self-hosted".to_string(),
        "Windows".to_string(),
        "ARM64".to_string(),
        "Baremetal".to_string(),
    ])
}

pub fn windows_tdx_self_hosted_baremetal() -> GhRunner {
    GhRunner::SelfHosted(vec![
        "self-hosted".to_string(),
        "Windows".to_string(),
        "X64".to_string(),
        "TDX".to_string(),
        "Baremetal".to_string(),
    ])
}

pub fn windows_snp_self_hosted_baremetal() -> GhRunner {
    GhRunner::SelfHosted(vec![
        "self-hosted".to_string(),
        "Windows".to_string(),
        "X64".to_string(),
        "SNP".to_string(),
        "Baremetal".to_string(),
    ])
}
