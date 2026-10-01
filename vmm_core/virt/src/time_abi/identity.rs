// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! The hypervisor identity CPUID leaves and the CPU time bits.

use super::TimeAbiCode;
use super::TimeAbiError;
use crate::CpuidLeaf;
use crate::CpuidLeafSet;
use std::ops::RangeInclusive;

/// The identity leaves the time ABI defines. Every other leaf in this range
/// reads as zero or as the vendor's out-of-range result.
pub const IDENTITY_CPUID_RANGE: RangeInclusive<u32> = 0x4000_0000..=0x4000_00ff;
/// Every leaf a hypervisor may use, including the bases at `0x400xxx00`
/// that carry KVM, Xen, or VMware signatures.
pub const HYPERVISOR_CPUID_RANGE: RangeInclusive<u32> = 0x4000_0000..=0x4fff_ffff;
/// The highest identity leaf.
pub const IDENTITY_MAX_LEAF: u32 = 0x4000_0005;
/// Leaves beyond the identity that every backend programs as zero, because
/// Linux and Hyper-V-aware software probe them and KVM cannot list the whole
/// range.
pub const IDENTITY_ZERO_LEAVES: [RangeInclusive<u32>; 2] =
    [0x4000_0006..=0x4000_000f, 0x4000_0080..=0x4000_0082];
/// `"Microsoft Hv"` in EBX, ECX, and EDX of leaf `0x40000000`.
pub const VENDOR_SIGNATURE: [u32; 3] = [0x7263_694d, 0x666f_736f, 0x7648_2074];
/// `"Hv#1"` in EAX of leaf `0x40000001`.
pub const INTERFACE_SIGNATURE: u32 = 0x3123_7648;
/// `"NVX"` in EAX of leaf `0x40000002`.
pub const NVX_BUILD_SIGNATURE: u32 = 0x0058_564e;
/// Time ABI 1.0 in EBX of leaf `0x40000002`.
pub const TIME_ABI_VERSION_EBX: u32 = 0x0001_0000;
/// `AccessHypercallMsrs`, `AccessVpIndex`, `AccessFrequencyRegs`, and
/// `AccessTscInvariantControls` in EAX of leaf `0x40000003`.
pub const PRIVILEGES_EAX: u32 = 0x0000_8860;
/// `FrequencyRegsAvailable` in EDX of leaf `0x40000003`.
pub const FEATURES_EDX: u32 = 0x0000_0100;
/// "Never notify" spinlock retries in EBX of leaf `0x40000004`.
pub const SPINLOCK_RETRIES_EBX: u32 = 0xffff_ffff;
/// The invariant-TSC bit in EDX of leaf `0x80000007`.
pub const INVARIANT_TSC_EDX: u32 = 1 << 8;

const LEAF_VERSION_AND_FEATURES: u32 = 0x1;
const LEAF_POWER_MANAGEMENT: u32 = 0x6;
const LEAF_EXTENDED_FEATURES: u32 = 0x7;
const LEAF_PERFORMANCE_MONITORING: u32 = 0xa;
const LEAF_CORE_CRYSTAL_CLOCK: u32 = 0x15;
const LEAF_PROCESSOR_FREQUENCY: u32 = 0x16;
const LEAF_EXTENDED_MAX: u32 = 0x8000_0000;
const LEAF_EXTENDED_FEATURES_1: u32 = 0x8000_0001;
const LEAF_ADVANCED_POWER_MANAGEMENT: u32 = 0x8000_0007;

const ECX1_PDCM: u32 = 1 << 15;
const ECX1_TSC_DEADLINE: u32 = 1 << 24;
const ECX1_HYPERVISOR: u32 = 1 << 31;
const EDX1_TSC: u32 = 1 << 4;
const EBX7_TSC_ADJUST: u32 = 1 << 1;
const EAX6_ARAT: u32 = 1 << 2;
const EDX80000001_RDTSCP: u32 = 1 << 27;

/// Returns the six identity leaves `0x40000000..=0x40000005` for a VM with
/// `vp_count` VPs. Every VP sees the same values.
pub fn identity_cpuid_leaves(vp_count: u32) -> [CpuidLeaf; 6] {
    let [ebx, ecx, edx] = VENDOR_SIGNATURE;
    [
        CpuidLeaf::new(0x4000_0000, [IDENTITY_MAX_LEAF, ebx, ecx, edx]),
        CpuidLeaf::new(0x4000_0001, [INTERFACE_SIGNATURE, 0, 0, 0]),
        CpuidLeaf::new(
            0x4000_0002,
            [NVX_BUILD_SIGNATURE, TIME_ABI_VERSION_EBX, 0, 0],
        ),
        CpuidLeaf::new(0x4000_0003, [PRIVILEGES_EAX, 0, 0, FEATURES_EDX]),
        CpuidLeaf::new(0x4000_0004, [0, SPINLOCK_RETRIES_EBX, 0, 0]),
        CpuidLeaf::new(0x4000_0005, [vp_count, vp_count, 0, 0]),
    ]
}

/// Returns the explicit zero leaves of [`IDENTITY_ZERO_LEAVES`].
pub fn identity_zero_cpuid_leaves() -> impl Iterator<Item = CpuidLeaf> {
    IDENTITY_ZERO_LEAVES
        .into_iter()
        .flatten()
        .map(|leaf| CpuidLeaf::new(leaf, [0; 4]))
}

/// Returns CPUID results that enforce the CPU time bits over another CPUID
/// source. Bits outside the time bits keep that source's values.
///
/// `invariant_tsc` sets CPUID `0x80000007` EDX to the invariant-TSC bit, or to
/// zero when the backend must not expose it (TBD(whp)).
pub fn time_bits_cpuid_leaves(invariant_tsc: bool) -> Vec<CpuidLeaf> {
    vec![
        CpuidLeaf::new(LEAF_VERSION_AND_FEATURES, [0, 0, ECX1_HYPERVISOR, EDX1_TSC]).masked([
            0,
            0,
            ECX1_HYPERVISOR | ECX1_TSC_DEADLINE | ECX1_PDCM,
            EDX1_TSC,
        ]),
        CpuidLeaf::new(LEAF_POWER_MANAGEMENT, [EAX6_ARAT, 0, 0, 0]),
        CpuidLeaf::new(LEAF_EXTENDED_FEATURES, [0; 4])
            .indexed(0)
            .masked([0, EBX7_TSC_ADJUST, 0, 0]),
        CpuidLeaf::new(LEAF_PERFORMANCE_MONITORING, [0; 4]),
        CpuidLeaf::new(LEAF_CORE_CRYSTAL_CLOCK, [0; 4]),
        CpuidLeaf::new(LEAF_PROCESSOR_FREQUENCY, [0; 4]),
        CpuidLeaf::new(LEAF_EXTENDED_FEATURES_1, [0, 0, 0, EDX80000001_RDTSCP]).masked([
            0,
            0,
            0,
            EDX80000001_RDTSCP,
        ]),
        CpuidLeaf::new(
            LEAF_ADVANCED_POWER_MANAGEMENT,
            [0, 0, 0, if invariant_tsc { INVARIANT_TSC_EDX } else { 0 }],
        ),
    ]
}

/// Returns the time ABI CPUID results a backend applies over its own CPUID
/// until CPU profiles land: the identity leaves, the explicit zero leaves,
/// and the CPU time bits.
///
/// The caller must also ensure that no other leaf in
/// [`HYPERVISOR_CPUID_RANGE`] is exposed.
pub fn time_abi_cpuid(vp_count: u32, invariant_tsc: bool) -> CpuidLeafSet {
    let mut leaves = time_bits_cpuid_leaves(invariant_tsc);
    leaves.extend(identity_cpuid_leaves(vp_count));
    leaves.extend(identity_zero_cpuid_leaves());
    CpuidLeafSet::new(leaves)
}

/// Wraps a CPUID lookup so that every leaf in [`HYPERVISOR_CPUID_RANGE`]
/// reads as zero.
///
/// Backends pass this view to
/// [`X86PartitionCapabilities::from_cpuid`](crate::x86::X86PartitionCapabilities::from_cpuid),
/// so the identity leaves never make `hv1` true: the time ABI does not expose
/// the synthetic MSR, SynIC, and reference-time state that `hv1` implies.
pub fn capabilities_cpuid<'a>(
    cpuid: &'a mut dyn FnMut(u32, u32) -> [u32; 4],
) -> impl FnMut(u32, u32) -> [u32; 4] + 'a {
    move |leaf, subleaf| {
        if HYPERVISOR_CPUID_RANGE.contains(&leaf) {
            [0; 4]
        } else {
            cpuid(leaf, subleaf)
        }
    }
}

/// Checks the identity leaves of an effective CPUID: the six leaves exactly,
/// the explicit zero leaves, and no hypervisor signature at any base
/// `0x40000100..=0x4000ff00`. `cpuid` returns zeros for leaves absent from
/// the CPUID table being checked.
pub fn check_identity(
    cpuid: &mut dyn FnMut(u32, u32) -> [u32; 4],
    vp_count: u32,
) -> Result<(), TimeAbiError> {
    let expected = identity_cpuid_leaves(vp_count)
        .into_iter()
        .chain(identity_zero_cpuid_leaves());
    for leaf in expected {
        let actual = cpuid(leaf.function, 0);
        if actual != leaf.result {
            return Err(TimeAbiError::new(
                TimeAbiCode::IdentityRouting,
                format!(
                    "CPUID {:#x} is {actual:#x?}, expected {:#x?}",
                    leaf.function, leaf.result
                ),
            ));
        }
    }
    for base in (0x4000_0100..=0x4000_ff00).step_by(0x100) {
        let [_, ebx, ecx, edx] = cpuid(base, 0);
        if [ebx, ecx, edx] != [0; 3] {
            return Err(TimeAbiError::new(
                TimeAbiCode::IdentityRouting,
                format!("CPUID {base:#x} carries a hypervisor signature"),
            ));
        }
    }
    Ok(())
}

/// Checks the CPU time bits of an effective CPUID.
///
/// With `invariant_tsc`, CPUID `0x80000007` EDX must be exactly the
/// invariant-TSC bit; without it, EDX may also be zero (TBD(whp)).
pub fn check_time_bits(
    cpuid: &mut dyn FnMut(u32, u32) -> [u32; 4],
    invariant_tsc: bool,
) -> Result<(), TimeAbiError> {
    fn violation(what: &str) -> TimeAbiError {
        TimeAbiError::new(
            TimeAbiCode::ProfileTimeBits,
            format!("CPU time bits violated: {what}"),
        )
    }

    let max_basic = cpuid(0, 0)[0];
    if max_basic < LEAF_VERSION_AND_FEATURES {
        return Err(violation("CPUID leaf 0x1 is missing"));
    }
    let [_, _, ecx, edx] = cpuid(LEAF_VERSION_AND_FEATURES, 0);
    if ecx & ECX1_HYPERVISOR == 0 {
        return Err(violation("CPUID.1:ECX[31] hypervisor present is clear"));
    }
    if ecx & ECX1_TSC_DEADLINE != 0 {
        return Err(violation("CPUID.1:ECX[24] TSC-deadline timer is set"));
    }
    if ecx & ECX1_PDCM != 0 {
        return Err(violation("CPUID.1:ECX[15] PDCM is set"));
    }
    if edx & EDX1_TSC == 0 {
        return Err(violation("CPUID.1:EDX[4] TSC is clear"));
    }
    if max_basic >= LEAF_POWER_MANAGEMENT && cpuid(LEAF_POWER_MANAGEMENT, 0) != [EAX6_ARAT, 0, 0, 0]
    {
        return Err(violation("CPUID.6 is not ARAT only"));
    }
    if max_basic >= LEAF_EXTENDED_FEATURES
        && cpuid(LEAF_EXTENDED_FEATURES, 0)[1] & EBX7_TSC_ADJUST != 0
    {
        return Err(violation("CPUID.7.0:EBX[1] IA32_TSC_ADJUST is set"));
    }
    for leaf in [
        LEAF_PERFORMANCE_MONITORING,
        LEAF_CORE_CRYSTAL_CLOCK,
        LEAF_PROCESSOR_FREQUENCY,
    ] {
        if max_basic >= leaf && cpuid(leaf, 0) != [0; 4] {
            return Err(violation(&format!("CPUID {leaf:#x} is not zero")));
        }
    }
    let max_extended = cpuid(LEAF_EXTENDED_MAX, 0)[0];
    if max_extended < LEAF_ADVANCED_POWER_MANAGEMENT {
        return Err(violation("CPUID leaf 0x80000007 is missing"));
    }
    if cpuid(LEAF_EXTENDED_FEATURES_1, 0)[3] & EDX80000001_RDTSCP == 0 {
        return Err(violation("CPUID.80000001:EDX[27] RDTSCP is clear"));
    }
    let apm = cpuid(LEAF_ADVANCED_POWER_MANAGEMENT, 0);
    let apm_valid = apm == [0, 0, 0, INVARIANT_TSC_EDX] || (!invariant_tsc && apm == [0; 4]);
    if !apm_valid {
        return Err(violation("CPUID.80000007 is not invariant TSC only"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::x86::X86PartitionCapabilities;
    use vm_topology::processor::TopologyBuilder;
    use vm_topology::processor::x86::ApicMode;

    /// A host CPUID with every time bit wrong and a KVM signature.
    fn host_cpuid(leaf: u32, subleaf: u32) -> [u32; 4] {
        match (leaf, subleaf) {
            (0, _) => [0x1f, 0x756e_6547, 0x6c65_746e, 0x4965_6e69],
            (1, _) => [0x0005_0657, 0, 0x7ffa_fbff | ECX1_TSC_DEADLINE, 0xbfeb_fbff],
            (6, _) => [0x77, 2, 9, 0],
            (7, 0) => [0, 0xd19f_4fbb, 0, 0],
            (0xa, _) => [0x0740_4f04, 0, 0, 0x603],
            (0x15, _) => [2, 0xd4, 0x017d_7840, 0],
            (0x16, _) => [0x0bb8, 0x0e74, 0x64, 0],
            (0x4000_0000, _) => [0x4000_0001, 0x4b4d_564b, 0x564b_4d56, 0x4d],
            (0x4000_0001, _) => [0x0100_8efb, 0, 0, 0],
            (0x8000_0000, _) => [0x8000_0008, 0, 0, 0],
            (0x8000_0001, _) => [0, 0, 0x121, 0x2c10_0800],
            (0x8000_0007, _) => [0, 0, 0, 0x100],
            _ => [0; 4],
        }
    }

    fn effective(leaf: u32, subleaf: u32, overlay: &CpuidLeafSet) -> [u32; 4] {
        let base = if HYPERVISOR_CPUID_RANGE.contains(&leaf) {
            [0; 4]
        } else {
            host_cpuid(leaf, subleaf)
        };
        overlay.result(leaf, subleaf, &base)
    }

    #[test]
    fn identity_leaves_match_the_specification() {
        let leaves = identity_cpuid_leaves(4);
        let results: Vec<_> = leaves
            .iter()
            .map(|leaf| (leaf.function, leaf.result))
            .collect();
        assert_eq!(
            results,
            [
                (
                    0x4000_0000,
                    [0x4000_0005, 0x7263_694d, 0x666f_736f, 0x7648_2074]
                ),
                (0x4000_0001, [0x3123_7648, 0, 0, 0]),
                (0x4000_0002, [0x0058_564e, 0x0001_0000, 0, 0]),
                (0x4000_0003, [0x0000_8860, 0, 0, 0x0000_0100]),
                (0x4000_0004, [0, 0xffff_ffff, 0, 0]),
                (0x4000_0005, [4, 4, 0, 0]),
            ]
        );
        let vendor: Vec<u8> = VENDOR_SIGNATURE
            .iter()
            .flat_map(|word| word.to_le_bytes())
            .collect();
        assert_eq!(vendor, b"Microsoft Hv");
        assert_eq!(&INTERFACE_SIGNATURE.to_le_bytes(), b"Hv#1");
        assert_eq!(&NVX_BUILD_SIGNATURE.to_le_bytes(), b"NVX\0");
    }

    #[test]
    fn overlay_enforces_identity_and_time_bits() {
        let overlay = time_abi_cpuid(8, true);
        let mut cpuid = |leaf, subleaf| effective(leaf, subleaf, &overlay);
        check_time_bits(&mut cpuid, true).unwrap();
        check_identity(&mut cpuid, 8).unwrap();
        // Bits outside the time bits keep the host's values.
        assert_eq!(
            cpuid(1, 0)[2],
            (0x7ffa_fbff & !(ECX1_TSC_DEADLINE | ECX1_PDCM)) | ECX1_HYPERVISOR
        );
        assert_eq!(cpuid(7, 0)[1], 0xd19f_4fbb & !EBX7_TSC_ADJUST);
        assert_eq!(cpuid(0x8000_0001, 0)[3], 0x2c10_0800 | EDX80000001_RDTSCP);
        assert_eq!(cpuid(0x15, 0), [0; 4]);
    }

    #[test]
    fn invariant_tsc_may_be_clear_only_when_allowed() {
        let overlay = time_abi_cpuid(1, false);
        let mut cpuid = |leaf, subleaf| effective(leaf, subleaf, &overlay);
        check_time_bits(&mut cpuid, false).unwrap();
        assert_eq!(
            check_time_bits(&mut cpuid, true).unwrap_err().code,
            TimeAbiCode::ProfileTimeBits
        );
    }

    #[test]
    fn time_bit_violations_are_rejected() {
        let mut host = host_cpuid;
        assert_eq!(
            check_time_bits(&mut host, true).unwrap_err().code,
            TimeAbiCode::ProfileTimeBits
        );
        let overlay = time_abi_cpuid(1, true);
        for (leaf, subleaf, register, bit) in [
            (1, 0, 2, ECX1_TSC_DEADLINE),
            (1, 0, 2, ECX1_PDCM),
            (6, 0, 1, 1),
            (7, 0, 1, EBX7_TSC_ADJUST),
            (0xa, 0, 0, 1),
            (0x15, 0, 2, 1),
            (0x8000_0007, 0, 3, 1),
        ] {
            let mut cpuid = |l, s| {
                let mut result = effective(l, s, &overlay);
                if (l, s) == (leaf, subleaf) {
                    result[register] |= bit;
                }
                result
            };
            assert_eq!(
                check_time_bits(&mut cpuid, true).unwrap_err().code,
                TimeAbiCode::ProfileTimeBits,
                "{leaf:#x}"
            );
        }
        for (leaf, register, bit) in [
            (1, 2, ECX1_HYPERVISOR),
            (1, 3, EDX1_TSC),
            (0x8000_0001, 3, EDX80000001_RDTSCP),
        ] {
            let mut cpuid = |l, s| {
                let mut result = effective(l, s, &overlay);
                if l == leaf {
                    result[register] &= !bit;
                }
                result
            };
            assert_eq!(
                check_time_bits(&mut cpuid, true).unwrap_err().code,
                TimeAbiCode::ProfileTimeBits,
                "{leaf:#x}"
            );
        }
    }

    #[test]
    fn identity_check_rejects_other_signatures() {
        let overlay = time_abi_cpuid(2, true);
        let mut cpuid = |leaf, subleaf| {
            if leaf == 0x4000_0100 {
                [0x4000_0101, 0x4b4d_564b, 0x564b_4d56, 0x4d]
            } else {
                effective(leaf, subleaf, &overlay)
            }
        };
        assert_eq!(
            check_identity(&mut cpuid, 2).unwrap_err().code,
            TimeAbiCode::IdentityRouting
        );
        let mut cpuid = |leaf, subleaf| effective(leaf, subleaf, &overlay);
        assert_eq!(
            check_identity(&mut cpuid, 3).unwrap_err().code,
            TimeAbiCode::IdentityRouting
        );
        // A "VS#1" interface signature, which the explicit zero leaves rule
        // out.
        let mut cpuid = |leaf, subleaf| {
            if leaf == 0x4000_0081 {
                [0x3123_5356, 0, 0, 0]
            } else {
                effective(leaf, subleaf, &overlay)
            }
        };
        assert_eq!(
            check_identity(&mut cpuid, 2).unwrap_err().code,
            TimeAbiCode::IdentityRouting
        );
    }

    #[test]
    fn overlay_programs_explicit_zero_leaves() {
        let overlay = time_abi_cpuid(1, true);
        let listed: Vec<u32> = overlay
            .leaves()
            .iter()
            .map(|leaf| leaf.function)
            .filter(|function| HYPERVISOR_CPUID_RANGE.contains(function))
            .collect();
        let mut expected: Vec<u32> = (0x4000_0000..=0x4000_000f).collect();
        expected.extend(0x4000_0080..=0x4000_0082);
        assert_eq!(listed, expected);
        for leaf in overlay.leaves() {
            if (0x4000_0006..=0x4000_0082).contains(&leaf.function) {
                assert_eq!((leaf.result, leaf.mask), ([0; 4], [!0; 4]));
            }
        }
    }

    #[test]
    fn capabilities_ignore_the_identity() {
        let topology = TopologyBuilder::new_x86().build(1).unwrap();
        let x2apic = topology.apic_mode() != ApicMode::XApic;
        let overlay = time_abi_cpuid(1, true);
        let mut cpuid = |leaf, subleaf| {
            let mut result = effective(leaf, subleaf, &overlay);
            if leaf == 1 {
                // Match the topology's APIC mode and drop XSAVE, which the
                // fake host does not describe.
                result[2] &= !((1 << 21) | (1 << 26));
                if x2apic {
                    result[2] |= 1 << 21;
                }
            }
            result
        };
        let unmasked = X86PartitionCapabilities::from_cpuid(&topology, &mut cpuid).unwrap();
        assert!(unmasked.hv1);
        let masked =
            X86PartitionCapabilities::from_cpuid(&topology, &mut capabilities_cpuid(&mut cpuid))
                .unwrap();
        assert!(!masked.hv1);
        assert!(!masked.tsc_deadline);
    }
}
