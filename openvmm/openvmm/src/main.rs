// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Root binary crate for OpenVMM.

// Ensure openvmm_resources gets linked.
extern crate openvmm_resources as _;

fn main() {
    openvmm_resources::ensure_linked_on_macos();
    openvmm_entry::hvlite_main()
}
