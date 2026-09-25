// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! CPUID adjustments for the CPU compatibility contract: KVM paravirtual clock
//! exposure for versioned machine profiles, and hiding CET-SS, whose state KVM
//! cannot save.

use kvm::KVM_CPUID_FLAG_SIGNIFCANT_INDEX;
use virt::CpuidLeaf;
use x86defs::cpuid::CpuidFunction;

/// Returns the leaf to expose for a KVM hypervisor CPUID entry
/// (`0x4000_0000..=0x4fff_ffff`), if any.
///
/// KVM's entries are filtered out unless the versioned CPU contract exposes
/// the KVM clock. Then the feature leaf is reduced to the clock features and
/// the other entries pass through unchanged.
pub(super) fn hypervisor_leaf(
    entry: &kvm::kvm_cpuid_entry2,
    expose_kvm_clock: bool,
) -> Option<CpuidLeaf> {
    if !expose_kvm_clock {
        return None;
    }
    if entry.function == 0x4000_0001 {
        const KVM_FEATURE_CLOCKSOURCE2: u32 = 1 << 3;
        const KVM_FEATURE_CLOCKSOURCE_STABLE_BIT: u32 = 1 << 24;
        return Some(CpuidLeaf::new(
            entry.function,
            [
                entry.eax & (KVM_FEATURE_CLOCKSOURCE2 | KVM_FEATURE_CLOCKSOURCE_STABLE_BIT),
                0,
                0,
                0,
            ],
        ));
    }
    let mut leaf = CpuidLeaf::new(entry.function, [entry.eax, entry.ebx, entry.ecx, entry.edx]);
    if entry.flags & KVM_CPUID_FLAG_SIGNIFCANT_INDEX != 0 {
        leaf = leaf.indexed(entry.index);
    }

    Some(leaf)
}

/// Returns the leaf that sets the hypervisor-present bit when the KVM clock is
/// exposed.
pub(super) fn hypervisor_bit(expose_kvm_clock: bool) -> Option<CpuidLeaf> {
    expose_kvm_clock.then(|| {
        CpuidLeaf::new(CpuidFunction::VersionAndFeatures.0, [0, 0, 1 << 31, 0]).masked([
            0,
            0,
            1 << 31,
            0,
        ])
    })
}

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
