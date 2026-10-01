// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! CPUID adjustments for snapshots: hiding CET-SS, whose state KVM cannot
//! save.

use virt::CpuidLeaf;
use x86defs::cpuid::CpuidFunction;

/// Returns the leaf that hides CET-SS from the guest.
///
/// KVM does not expose an API to save or restore the shadow-stack
/// pointer. Do not advertise CET-SS, or a snapshot could silently lose
/// architecturally active state that the CPU contract claimed to cover.
pub(super) fn hide_cet_ss() -> CpuidLeaf {
    let cet_ss_mask: u32 = x86defs::cpuid::ExtendedFeatureSubleaf0Ecx::new()
        .with_cet_ss(true)
        .into();
    CpuidLeaf::new(CpuidFunction::ExtendedFeatures.0, [0; 4])
        .indexed(0)
        .masked([0, 0, cet_ss_mask, 0])
}
