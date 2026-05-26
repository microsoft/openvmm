// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Type definitions for loading guest firmware. The crate is `no_std`;
//! the `manifest` feature pulls in `serde` + `base64` (both `alloc`)
//! for build-side manifest deserialization.

#![no_std]
#![forbid(unsafe_code)]

pub mod linux;
pub mod paravisor;
pub mod shim;
