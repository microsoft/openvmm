// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Processor support for Microsoft hypervisor-backed partitions.

pub mod arm64;
mod tlb_lock;
pub mod x64;

#[derive(Default, inspect::Inspect)]
pub(crate) struct VbsIsolatedVtl1State {
    #[inspect(with = "|flags| flags.map(|f| inspect::AsHex(u32::from(f)))")]
    default_vtl_protections: Option<hvdef::HvMapGpaFlags>,
    enable_vtl_protection: bool,
}
