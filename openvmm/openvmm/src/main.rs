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
    openvmm_entry::openvmm_main()
}
