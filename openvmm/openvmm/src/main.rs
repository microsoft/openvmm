// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Root binary crate for OpenVMM.

#![forbid(unsafe_code)]

// Ensure openvmm_resources and openvmm_hypervisors get linked.
extern crate openvmm_hypervisors as _;
extern crate openvmm_resources as _;

#[cfg(not(test))]
crypto::ensure_single_backend!();

fn main() {
    let mut args = std::env::args_os().skip(1);
    let version_requested = args
        .next()
        .is_some_and(|arg| arg == "--version" || arg == "-V")
        && args.next().is_none();

    if version_requested {
        println!("openvmm {}", openvmm_build_info::get().version());
        return;
    }

    openvmm_entry::openvmm_main()
}
