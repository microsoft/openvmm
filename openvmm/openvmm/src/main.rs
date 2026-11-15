// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Host-side executable for the OpenVMM virtual machine monitor.
//!
//! This crate links the platform hypervisors and device resource resolvers,
//! then delegates startup and command-line processing to
//! [`openvmm_entry::openvmm_main`]. It can be launched from the repository
//! with `cargo run -p openvmm -- <options>`.

#![forbid(unsafe_code)]

// Ensure openvmm_resources and openvmm_hypervisors get linked.
extern crate openvmm_hypervisors as _;
extern crate openvmm_resources as _;

#[cfg(not(test))]
crypto::ensure_single_backend!();

fn main() {
    openvmm_entry::openvmm_main()
}
