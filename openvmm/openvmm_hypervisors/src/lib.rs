// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Hypervisor backend implementations for OpenVMM.
//!
//! Each submodule provides a [`HypervisorProbe`](hypervisor_resources::HypervisorProbe)
//! implementation and a resource resolver for the corresponding handle type.
//!
//! Probes are registered here via `register_hypervisor_probes!`. Resource
//! resolvers are registered separately in `openvmm_resources`.

#![forbid(unsafe_code)]

pub mod hvf;
pub mod kvm;
pub mod mshv;
pub mod whp;

// Register probes for auto-detection (checked in this order).
openvmm_core::register_hypervisor_probes! {
    #[cfg(all(target_os = "linux", feature = "virt_mshv", guest_is_native, guest_arch = "x86_64"))]
    mshv::MshvProbe,

    #[cfg(all(target_os = "linux", feature = "virt_kvm", guest_is_native))]
    kvm::KvmProbe,

    #[cfg(all(target_os = "windows", feature = "virt_whp", guest_is_native))]
    whp::WhpProbe,

    #[cfg(all(target_os = "macos", guest_arch = "aarch64", guest_is_native, feature = "virt_hvf"))]
    hvf::HvfProbe,
}
