// Copyright (C) Microsoft Corporation. All rights reserved.

#![cfg(target_arch = "x86_64")]

//! x86_64 architecture-specific implementations.

pub mod enlightened_panic;
pub mod hypercall;
pub mod intrinsics;
pub mod msr;
pub mod reftime;
pub mod serial;
