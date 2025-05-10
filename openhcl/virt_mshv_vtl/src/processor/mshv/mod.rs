// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Processor support for Microsoft hypervisor-backed partitions.

pub mod arm64;
mod tlb_lock;
pub mod x64;

#[derive(Default, inspect::Inspect)]
pub(crate) struct VbsIsolatedVtl1State {
    #[inspect(hex, with = "|flags| flags.map(u32::from)")]
    default_vtl_protections: Option<hvdef::HvMapGpaFlags>,
    enable_vtl_protection: bool,
}
