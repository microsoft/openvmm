// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Root binary crate for OpenVMM.

#![forbid(unsafe_code)]

// Ensure openvmm_resources gets linked.
extern crate openvmm_resources as _;

fn main() {
    openvmm_entry::openvmm_main()
}
