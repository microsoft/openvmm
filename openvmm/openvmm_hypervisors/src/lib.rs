// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Hypervisor backend implementations for OpenVMM.
//!
//! Each submodule provides a [`HypervisorProbe`](hypervisor_resources::HypervisorProbe)
//! implementation and a resource resolver for the corresponding handle type.
//!
//! Registration (via `register_hypervisors!`) is done in `openvmm_resources`,
//! not here.

#![forbid(unsafe_code)]

pub mod hvf;
pub mod kvm;
pub mod mshv;
pub mod whp;
