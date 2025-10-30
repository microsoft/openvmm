// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Root binary crate for builds of OpenVMM-HCL.

#![forbid(unsafe_code)]

// Link resources.
#[cfg(target_os = "linux")]
use openvmm_hcl_resources as _;

#[cfg(not(target_os = "linux"))]
fn main() {
    unimplemented!("openvmm_hcl only runs on Linux");
}

#[cfg(target_os = "linux")]
use underhill_entry::underhill_main as main;
