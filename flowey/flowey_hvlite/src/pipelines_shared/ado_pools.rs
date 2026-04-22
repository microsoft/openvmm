// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Centralized list of constants enumerating available ADO build pools.

use flowey::node::prelude::FlowPlatformLinuxDistro;
use flowey::pipeline::prelude::*;

pub const INTEL_POOL: &str = "openvmm-ado-intel-centralus";

fn intel_pool_with_image(image: &str) -> AdoPool {
    AdoPool {
        name: INTEL_POOL.into(),
        demands: vec![format!("ImageOverride -equals {image}")],
    }
}

pub fn default_x86_pool(platform: FlowPlatform) -> AdoPool {
    match platform {
        FlowPlatform::Windows => intel_pool_with_image("win-amd64"),
        FlowPlatform::Linux(FlowPlatformLinuxDistro::Ubuntu) => {
            intel_pool_with_image("ubuntu2404-amd64-256gb")
        }
        platform => panic!("unsupported platform {platform}"),
    }
}
