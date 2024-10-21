// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Crate to interact with the Windows vmswitch-based virtual networking
//! capabilities, including the vmswitch and the HCN/HNS service.

#![cfg(windows)]
// UNSAFETY: Calling Win32 VMS and HCN APIs.
#![allow(unsafe_code)]
#![allow(clippy::undocumented_unsafe_blocks)]

pub mod dio;
pub mod hcn;
pub mod kernel;
mod vmsif;
