// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Type definitions for loading guest firmware, available as no_std if no
//! features are defined. The `manifest` feature pulls in `std` so the
//! build-side manifest types can use `std::path::PathBuf` / `std::fs` 
//! for path-based inputs.

#![cfg_attr(not(feature = "manifest"), no_std)]
#![forbid(unsafe_code)]

pub mod linux;
pub mod paravisor;
pub mod shim;
