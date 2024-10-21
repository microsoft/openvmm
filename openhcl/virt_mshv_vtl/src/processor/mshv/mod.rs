// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Processor support for Microsoft hypervisor-backed partitions.

pub mod apic;
pub mod arm64;
mod tlb_lock;
pub mod x64;
