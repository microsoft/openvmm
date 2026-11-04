// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! User-mode VMM process that runs inside OpenHCL's Linux VTL2 environment.
//!
//! This binary links the OpenHCL resource resolvers and delegates to
//! `underhill_entry`, which starts the workers that manage the VTL0 guest and
//! paravisor services. It is a Linux component of the OpenHCL firmware image,
//! not the host-side `openvmm` CLI.
//!
//! The supported build path is `cargo xflowey build-igvm <recipe>`, which
//! selects features, builds this executable for the OpenHCL userspace, and
//! packages it with the boot loader, kernel, initrd, and other measured
//! resources. Non-Linux builds contain only an unsupported-platform stub.

#![forbid(unsafe_code)]

// Link resources.
#[cfg(target_os = "linux")]
use openvmm_hcl_resources as _;

// OpenVMM-HCL only needs libcrypto from openssl, not libssl.
#[cfg(target_os = "linux")]
openssl_crypto_only::openssl_crypto_only!();

#[cfg(all(not(test), target_os = "linux"))]
crypto::ensure_single_backend!();

#[cfg(not(target_os = "linux"))]
fn main() {
    unimplemented!("openvmm_hcl only runs on Linux");
}

#[cfg(target_os = "linux")]
use underhill_entry::underhill_main as main;
