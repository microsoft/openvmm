// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Hypervisor backend resource resolvers.
//!
//! Each submodule provides a resolver that converts a hypervisor handle
//! (from [`hypervisor_resources`]) into a [`ResolvedHypervisorBackend`]
//! (from [`openvmm_core`]).

pub mod hvf;
pub mod kvm;
pub mod mshv;
pub mod whp;
