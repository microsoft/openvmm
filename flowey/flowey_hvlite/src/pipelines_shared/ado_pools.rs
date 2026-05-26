// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Centralized list of constants enumerating available ADO build pools.

use flowey::pipeline::prelude::*;

use super::gh_pools::LINUX_IMAGE_AMD64;
use super::gh_pools::WINDOWS_IMAGE_AMD64;

pub const AMD_POOL_1ES: &str = "openvmm-ado-amd-westus2";
pub const INTEL_POOL_1ES: &str = "openvmm-ado-intel-centralus";

fn ado_pool_with_image_1es(pool: &str, image: &str) -> AdoPool {
    AdoPool {
        name: pool.into(),
        demands: vec![format!("ImageOverride -equals {image}")],
    }
}

pub fn windows_amd_1es() -> AdoPool {
    ado_pool_with_image_1es(AMD_POOL_1ES, WINDOWS_IMAGE_AMD64)
}

pub fn windows_intel_1es() -> AdoPool {
    ado_pool_with_image_1es(INTEL_POOL_1ES, WINDOWS_IMAGE_AMD64)
}

pub fn linux_amd_1es() -> AdoPool {
    ado_pool_with_image_1es(AMD_POOL_1ES, LINUX_IMAGE_AMD64)
}

pub fn default_windows() -> AdoPool {
    windows_amd_1es()
}

pub fn default_linux() -> AdoPool {
    linux_amd_1es()
}
