// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Root binary crate for OpenVMM.

#![forbid(unsafe_code)]

use std::ffi::OsStr;
use std::ffi::OsString;

// Ensure openvmm_resources and openvmm_hypervisors get linked.
extern crate openvmm_hypervisors as _;
extern crate openvmm_resources as _;

#[cfg(not(test))]
crypto::ensure_single_backend!();

fn main() {
    if version_requested(std::env::args_os().skip(1)) {
        println!("openvmm {}", openvmm_build_info::get().version());
        return;
    }

    openvmm_entry::openvmm_main()
}

fn version_requested(args: impl IntoIterator<Item = OsString>) -> bool {
    for arg in args {
        let arg = arg.as_os_str();
        if arg == OsStr::new("--") {
            return false;
        }
        if arg == OsStr::new("--version") || arg == OsStr::new("-V") {
            return true;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::version_requested;
    use std::ffi::OsString;

    #[test]
    fn version_flag_is_global() {
        assert!(version_requested(["--version"].map(OsString::from)));
        assert!(version_requested(["--help", "-V"].map(OsString::from)));
        assert!(!version_requested(["--help"].map(OsString::from)));
        assert!(!version_requested(["--", "--version"].map(OsString::from)));
    }
}
