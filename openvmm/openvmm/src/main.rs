// Copyright (C) Microsoft Corporation. All rights reserved.

//! Root binary crate for OpenVMM.

// Ensure openvmm_resources gets linked.
extern crate openvmm_resources as _;

// Use Win10+ PRNG APIs to run on smaller Windows SKUs.
//
// Just override advapi32 for now; bcrypt is used by vmgs encryption.
#[cfg(windows)]
win_prng_support::use_win10_prng_apis!(advapi32);

fn main() {
    openvmm_resources::ensure_linked_on_macos();
    hvlite_entry::hvlite_main()
}
