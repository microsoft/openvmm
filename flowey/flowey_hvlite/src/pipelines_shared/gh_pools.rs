// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Centralized list of constants enumerating available GitHub build pools.

use flowey::pipeline::prelude::*;

// Constants in this file may be added, renamed or deleted, but the values
// of these constants should not be changed to avoid changing the image to one
// that may not be present in all of the pools where the constant is used
// (in both GitHub and ADO).
pub const WINDOWS_IMAGE_AMD64_V2: &str = "win-amd64-v2";
pub const WINDOWS_IMAGE_ARM64_V2: &str = "win-arm64-v2";
pub const LINUX_IMAGE_AMD64: &str = "ubuntu2404-amd64";
pub const LINUX_IMAGE_ARM64: &str = "ubuntu2404-arm64";
pub const MSHV_IMAGE_AMD64: &str = "azurelinux3-amd64-dom0";

pub const AMD_V6_POOL_1ES: &str = "openvmm-gh-amd-westus3-v6";
pub const AMD_V7_POOL_1ES: &str = "openvmm-gh-amd-westus3-v7";
pub const INTEL_V5_POOL_1ES: &str = "openvmm-gh-intel-westus3";
pub const INTEL_V6_POOL_1ES: &str = "openvmm-gh-intel-westus3-v6";
pub const ARM_V5_POOL_1ES: &str = "openvmm-gh-arm-westus2";
pub const ARM_V6_POOL_1ES: &str = "openvmm-gh-arm-westus3";

fn gh_pool_with_image_1es(pool: &str, image: &str) -> GhRunner {
    GhRunner::SelfHosted(vec![
        "self-hosted".to_string(),
        format!("1ES.Pool={pool}"),
        format!("1ES.ImageOverride={image}"),
    ])
}

pub fn windows_arm_v6_1es() -> GhRunner {
    gh_pool_with_image_1es(ARM_V6_POOL_1ES, WINDOWS_IMAGE_ARM64_V2)
}

pub fn windows_amd_v6_1es() -> GhRunner {
    gh_pool_with_image_1es(AMD_V6_POOL_1ES, WINDOWS_IMAGE_AMD64_V2)
}

pub fn windows_intel_v6_1es() -> GhRunner {
    gh_pool_with_image_1es(INTEL_V6_POOL_1ES, WINDOWS_IMAGE_AMD64_V2)
}

pub fn linux_arm_v5_1es() -> GhRunner {
    gh_pool_with_image_1es(ARM_V5_POOL_1ES, LINUX_IMAGE_ARM64)
}

pub fn linux_intel_v6_1es() -> GhRunner {
    gh_pool_with_image_1es(INTEL_V6_POOL_1ES, LINUX_IMAGE_AMD64)
}

pub fn linux_amd_v7_1es() -> GhRunner {
    gh_pool_with_image_1es(AMD_V7_POOL_1ES, LINUX_IMAGE_AMD64)
}

pub fn linux_mshv_intel_v5_1es() -> GhRunner {
    gh_pool_with_image_1es(INTEL_V5_POOL_1ES, MSHV_IMAGE_AMD64)
}

pub fn windows_x64_gh() -> GhRunner {
    GhRunner::GhHosted(GhRunnerOsLabel::WindowsLatest)
}

pub fn linux_x64_gh() -> GhRunner {
    GhRunner::GhHosted(GhRunnerOsLabel::UbuntuLatest)
}

pub fn windows_arm_gh() -> GhRunner {
    GhRunner::GhHosted(GhRunnerOsLabel::Windows11Arm)
}

pub fn linux_arm_gh() -> GhRunner {
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

pub fn default_windows() -> GhRunner {
    windows_intel_v6_1es()
}

pub fn default_linux() -> GhRunner {
    linux_amd_v7_1es()
}
