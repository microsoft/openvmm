// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![cfg(target_arch = "aarch64")]

//! aarch64 specifics.

pub mod enlightened_panic;
pub mod hypercall;
pub mod intrinsics;
pub mod reftime;
pub mod serial;
