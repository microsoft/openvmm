// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Centralized list of constants enumerating available GitHub build pools.

use flowey::pipeline::prelude::*;

pub fn windows_amd_1es() -> GhRunner {
    GhRunner::SelfHosted(vec![
        "self-hosted".to_string(),
        "1ES.Pool=openvmm-gh-amd-westus3".to_string(),
        "1ES.ImageOverride=win-amd64".to_string(),
    ])
}

pub fn windows_intel_1es() -> GhRunner {
    GhRunner::SelfHosted(vec![
        "self-hosted".to_string(),
        "1ES.Pool=openvmm-gh-intel-westus3".to_string(),
        "1ES.ImageOverride=win-amd64".to_string(),
    ])
}

pub fn windows_arm_1es() -> GhRunner {
    GhRunner::SelfHosted(vec![
        "self-hosted".to_string(),
        "1ES.Pool=openvmm-gh-arm-westus2".to_string(),
        "1ES.ImageOverride=win-arm64".to_string(),
    ])
}

pub fn linux_arm_1es() -> GhRunner {
    GhRunner::SelfHosted(vec![
        "self-hosted".to_string(),
        "1ES.Pool=openvmm-gh-arm-westus2".to_string(),
        "1ES.ImageOverride=ubuntu2404-arm64".to_string(),
    ])
}

pub fn linux_1es() -> GhRunner {
    GhRunner::SelfHosted(vec![
        "self-hosted".to_string(),
        "1ES.Pool=openvmm-gh-amd-westus3".to_string(),
        "1ES.ImageOverride=ubuntu2404-amd64-256gb".to_string(),
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
