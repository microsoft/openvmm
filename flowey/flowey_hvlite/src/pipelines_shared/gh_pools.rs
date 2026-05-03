// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Centralized list of constants enumerating available GitHub build pools.

use flowey::pipeline::prelude::*;

pub const AMD_POOL_1ES: &str = "openvmm-gh-amd";
pub const INTEL_POOL_1ES: &str = "openvmm-gh-intel";
pub const ARM_POOL_1ES: &str = "openvmm-gh-arm";

pub const WINDOWS_IMAGE_AMD64: &str = "win-amd64-v2";
pub const WINDOWS_IMAGE_ARM64: &str = "win-arm64-v2";
pub const LINUX_IMAGE_AMD64: &str = "ubuntu2404-amd64-v2";
pub const LINUX_IMAGE_ARM64: &str = "ubuntu2404-arm64-v2";
pub const MSHV_IMAGE_AMD64: &str = "azurelinux3-amd64-dom0-v2";

pub const WINDOWS_WORK_FOLDER: &str = "E:\\_work";
pub const LINUX_WORK_FOLDER: &str = "/mnt/azure_nvme_temp/_work";

fn gh_pool_with_image_1es(pool: &str, image: &str, work_folder: &str) -> GhRunner {
    GhRunner::SelfHosted(vec![
        "self-hosted".to_string(),
        format!("1ES.Pool={pool}"),
        format!("1ES.ImageOverride={image}"),
        format!("1ES.WorkFolder={work_folder}"),
    ])
}

pub fn windows_amd_1es() -> GhRunner {
    gh_pool_with_image_1es(AMD_POOL_1ES, WINDOWS_IMAGE_AMD64, WINDOWS_WORK_FOLDER)
}

pub fn windows_intel_1es() -> GhRunner {
    gh_pool_with_image_1es(INTEL_POOL_1ES, WINDOWS_IMAGE_AMD64, WINDOWS_WORK_FOLDER)
}

pub fn windows_arm_1es() -> GhRunner {
    gh_pool_with_image_1es(ARM_POOL_1ES, WINDOWS_IMAGE_ARM64, WINDOWS_WORK_FOLDER)
}

pub fn linux_arm_1es() -> GhRunner {
    gh_pool_with_image_1es(ARM_POOL_1ES, LINUX_IMAGE_ARM64, LINUX_WORK_FOLDER)
}

pub fn linux_amd_1es() -> GhRunner {
    gh_pool_with_image_1es(AMD_POOL_1ES, LINUX_IMAGE_AMD64, LINUX_WORK_FOLDER)
}

pub fn linux_mshv_1es() -> GhRunner {
    gh_pool_with_image_1es(INTEL_POOL_1ES, MSHV_IMAGE_AMD64, LINUX_WORK_FOLDER)
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
    windows_amd_1es()
}

pub fn default_linux() -> GhRunner {
    linux_amd_1es()
}
