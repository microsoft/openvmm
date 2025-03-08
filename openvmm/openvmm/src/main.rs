// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Root binary crate for OpenVMM.


// Ensure openvmm_resources gets linked.
extern crate openvmm_resources as _;

// Use Win10+ PRNG APIs to run on smaller Windows SKUs.
//
// Just override advapi32 for now; bcrypt is used by vmgs encryption.
#[cfg(windows)]
mod win_prng {
    win_prng_support::use_win10_prng_apis!(advapi32);
}

fn main() {
    openvmm_resources::ensure_linked_on_macos();
    openvmm_entry::hvlite_main()
}
