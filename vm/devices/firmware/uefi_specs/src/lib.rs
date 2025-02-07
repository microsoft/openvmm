// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Types and constants related to the UEFI spec.
//!
//! This crate is divided into 3 submodules:
//!
//! - `uefi`: types directly lifted from the official UEFI spec
//! - `hyperv`: types specific to the Hyper-V UEFI implementation
//! - `linux`: types specific to UEFI on Linux

#![no_std]

// TODO: find a nice way to create const `Ucs2LeSlice` instances, and use proper
// `const`ants instead of runtime methods...
macro_rules! defn_nvram_var {
    ($varname:ident = ($guid:expr, $name:literal)) => {
        #[allow(non_snake_case)]
        pub fn $varname() -> (Guid, &'static ucs2::Ucs2LeSlice) {
            use ucs2::Ucs2LeSlice;
            use zerocopy::IntoBytes;

            (
                $guid,
                Ucs2LeSlice::from_slice_with_nul(wchar::wchz!(u16, $name).as_bytes()).unwrap(),
            )
        }
    };
}

pub mod hyperv;
pub mod linux;
pub mod uefi;
