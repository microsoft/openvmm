// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Centralized list of constants enumerating available ADO build pools.

use flowey::node::prelude::FlowPlatformLinuxDistro;
use flowey::pipeline::prelude::*;

pub const WINDOWS_INTEL: &str = "HvLite-CI-Win-Pool";
pub const WINDOWS_AMD: &str = "HvLite-CI-Win-Pool-WestUS2";
pub const LINUX: &str = "HvLite-CI-Linux-Pool-CentralUS";

pub fn default_x86_pool(platform: FlowPlatform) -> &'static str {
    match platform {
        FlowPlatform::Windows => WINDOWS_INTEL,
        FlowPlatform::Linux(FlowPlatformLinuxDistro::Ubuntu) => LINUX,
        platform => panic!("unsupported platform {platform}"),
    }
}
