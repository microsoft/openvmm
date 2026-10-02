// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! CPUID entries outside a profile's tables.
//!
//! No CPUID entry outside a VM's effective CPUID carries host data. KVM
//! installs the effective CPUID as the guest's whole CPUID table and answers
//! every other entry from it: zero, or a value the architecture's rules derive
//! from the table (Intel's out-of-range result, the highest basic leaf's,
//! above the maximum basic leaf, and an invalid level with the x2APIC ID past
//! the last topology level). MSHV and WHP program the effective CPUID over the
//! hypervisor's own guest view instead, so an entry it does not list reads
//! that view unless the backend registers a result there. The time ABI
//! reserves the entries past a profile's advertised maxima, and verification
//! requires that a pass-through backend presents zero at every entry outside
//! the profile's tables (`E_CPU_UNLISTED`), so no host data reaches the guest
//! there.

use crate::cpuid::CpuidEntry;
use crate::cpuid::HYPERVISOR_LEAF_BASE;
use crate::error::ProfileError;
use crate::error::ProfileErrorCode;
use crate::profile::CpuProfile;
use crate::profile::VM_OWNED_LEAVES;
use crate::profile::describe_leaf;
use std::ops::RangeInclusive;

/// The leaves reserved for hypervisors, where the time ABI's identity leaves
/// are.
const IDENTITY_RANGE: RangeInclusive<u32> = HYPERVISOR_LEAF_BASE..=0x4fff_ffff;

/// Returns whether the entry for `leaf` and `subleaf` is outside `profile`'s
/// tables and outside the leaves that the time ABI owns: the identity range
/// `0x40000000` to `0x4fffffff` and the VM-owned topology leaves. A VM's
/// effective CPUID adds those, and other checks cover them.
///
/// A subleaf-independent entry, in the profile or presented, covers every
/// subleaf of its leaf.
fn is_unlisted(profile: &CpuProfile, leaf: u32, subleaf: Option<u32>) -> bool {
    let owned = IDENTITY_RANGE.contains(&leaf) || VM_OWNED_LEAVES.contains(&leaf);
    // The table is sorted by leaf, and core checks every boot's report.
    let table = profile.cpuid();
    let start = table.partition_point(|entry| entry.leaf.0 < leaf);
    !owned
        && !table[start..]
            .iter()
            .take_while(|entry| entry.leaf.0 == leaf)
            .any(|entry| match (entry.subleaf, subleaf) {
                (Some(listed), Some(subleaf)) => listed.0 == subleaf,
                (None, _) | (_, None) => true,
            })
}

/// Returns every non-zero entry of `presented`, in order, that is outside
/// `profile`'s tables, other than the identity range and the VM-owned
/// topology leaves, as [`check_unlisted_cpuid`] reports it.
pub fn unlisted_cpuid_violations(profile: &CpuProfile, presented: &[CpuidEntry]) -> Vec<String> {
    presented
        .iter()
        .filter(|entry| {
            let (leaf, subleaf) = entry.key();
            entry.registers() != [0; 4] && is_unlisted(profile, leaf, subleaf)
        })
        .map(|entry| {
            let (leaf, subleaf) = entry.key();
            let [eax, ebx, ecx, edx] = entry.registers();
            format!(
                "{} is outside the profile and reads EAX {eax:#x}, EBX {ebx:#x}, ECX {ecx:#x}, EDX {edx:#x}",
                describe_leaf(leaf, subleaf)
            )
        })
        .collect()
}

/// Checks that a guest of a pass-through backend reads zero at every CPUID
/// entry of `presented` that is outside `profile`'s tables, other than the
/// identity range and the VM-owned topology leaves.
///
/// `presented` is what a guest reads: a probe partition's architectural
/// enumeration, as `--cpu-fingerprint` records it, or VP 0's view at the
/// host's [`unlisted_cpuid_candidates`]. A table backend, KVM, passes by
/// construction.
///
/// Fails with `E_CPU_UNLISTED`, naming every such entry and its
/// values at once.
pub fn check_unlisted_cpuid(
    profile: &CpuProfile,
    presented: &[CpuidEntry],
) -> Result<(), ProfileError> {
    let violations = unlisted_cpuid_violations(profile, presented);
    if violations.is_empty() {
        return Ok(());
    }
    Err(unlisted_cpuid_error(profile, &violations))
}

/// Returns the `E_CPU_UNLISTED` failure for `violations`.
pub(crate) fn unlisted_cpuid_error(profile: &CpuProfile, violations: &[String]) -> ProfileError {
    ProfileError::new(
        ProfileErrorCode::CpuUnlisted,
        format!(
            "the backend presents CPUID entries outside CPU profile {}: {}",
            profile.id(),
            violations.join("; ")
        ),
    )
}

/// Returns the keys of the entries of `host`, in order, that are outside
/// `profile`'s tables, other than the identity range and the VM-owned
/// topology leaves.
///
/// `host` is the host's own CPUID, as [`cpuid::enumerate`](crate::cpuid::enumerate)
/// walks it. It reaches every entry the processor implements, including those
/// past the profile's maxima and those that only the root partition sees, so
/// these are the entries where a pass-through backend's guest may read the
/// hypervisor's own guest view. Because the identity range is exempt,
/// [`cpuid::enumerate_basic_and_extended`](crate::cpuid::enumerate_basic_and_extended)
/// gives the same candidates with fewer queries. A backend reads VP 0's view
/// there, at subleaf 0 for a subleaf-independent key, for
/// [`check_unlisted_cpuid`].
pub fn unlisted_cpuid_candidates(
    profile: &CpuProfile,
    host: &[CpuidEntry],
) -> Vec<(u32, Option<u32>)> {
    host.iter()
        .map(CpuidEntry::key)
        .filter(|&(leaf, subleaf)| is_unlisted(profile, leaf, subleaf))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::profile;
    use crate::test_support::profile_entries;
    use test_with_tracing::test;

    const SKYLAKE: &str = "intel.skylake-sp.v1";

    /// Returns `profile`'s entries with `extra` added, sorted.
    fn with(profile: &CpuProfile, extra: &[CpuidEntry]) -> Vec<CpuidEntry> {
        let mut entries = profile_entries(profile);
        entries.extend_from_slice(extra);
        entries.sort_by_key(CpuidEntry::key);
        entries
    }

    #[test]
    fn every_pinned_profile_lists_its_own_entries() {
        for profile in crate::pinned_profiles() {
            let entries = profile_entries(profile);
            assert!(unlisted_cpuid_candidates(profile, &entries).is_empty());
            check_unlisted_cpuid(profile, &entries).unwrap();
        }
    }

    #[test]
    fn names_every_non_zero_entry_outside_the_tables() {
        let profile = profile(SKYLAKE);
        let presented = with(
            profile,
            &[
                // RDT monitoring and allocation, Intel PT (as the root
                // partition of a Skylake-SP host reads it), and the PKRU XSAVE
                // component, which the profile does not enable.
                CpuidEntry::new(0xf, Some(1), [0, 0xb, 0x6f, 0x7]),
                CpuidEntry::new(0x10, Some(1), [0xa, 0x600, 0x4, 0xf]),
                CpuidEntry::new(0x14, Some(1), [0x0249_0002, 0x003f_3fff, 0, 0]),
                CpuidEntry::new(0xd, Some(9), [8, 0xa80, 0, 0]),
                // A leaf past the profile's maximum basic leaf, 0x16.
                CpuidEntry::new(0x17, Some(0), [0, 0, 0, 1]),
            ],
        );
        let error = check_unlisted_cpuid(profile, &presented).unwrap_err();
        assert_eq!(error.code, ProfileErrorCode::CpuUnlisted);
        assert_eq!(
            error.to_string(),
            "[E_CPU_UNLISTED] the backend presents CPUID entries outside CPU profile \
             intel.skylake-sp.v1: \
             CPUID 0xd.9 is outside the profile and reads EAX 0x8, EBX 0xa80, ECX 0x0, EDX 0x0; \
             CPUID 0xf.1 is outside the profile and reads EAX 0x0, EBX 0xb, ECX 0x6f, EDX 0x7; \
             CPUID 0x10.1 is outside the profile and reads EAX 0xa, EBX 0x600, ECX 0x4, EDX 0xf; \
             CPUID 0x14.1 is outside the profile and reads EAX 0x2490002, EBX 0x3f3fff, ECX 0x0, EDX 0x0; \
             CPUID 0x17.0 is outside the profile and reads EAX 0x0, EBX 0x0, ECX 0x0, EDX 0x1"
        );
    }

    #[test]
    fn accepts_zeros_and_the_leaves_the_time_abi_owns() {
        let profile = profile(SKYLAKE);
        let presented = with(
            profile,
            &[
                // Subleaves that read zero, as a guest enumerates them.
                CpuidEntry::new(0xf, Some(1), [0; 4]),
                CpuidEntry::new(0x10, Some(3), [0; 4]),
                CpuidEntry::new(0x12, Some(2), [0; 4]),
                // The hypervisor range, where the identity leaves are.
                CpuidEntry::new(0x4000_0000, None, [0x4000_000b, 0x7263_694d, 0, 0]),
                CpuidEntry::new(0x4000_0003, None, [0x3fff, 0x2bfe, 0x2, 0xbed7b2]),
                // The topology leaves, which the VM's effective CPUID adds.
                CpuidEntry::new(0xb, Some(0), [1, 2, 0x100, 0]),
                CpuidEntry::new(0xb, Some(1), [5, 20, 0x201, 0]),
                CpuidEntry::new(0x1f, Some(1), [5, 20, 0x201, 0]),
            ],
        );
        check_unlisted_cpuid(profile, &presented).unwrap();
        assert_eq!(
            unlisted_cpuid_candidates(profile, &presented),
            [(0xf, Some(1)), (0x10, Some(3)), (0x12, Some(2))]
        );
    }

    #[test]
    fn a_subleaf_independent_entry_covers_every_subleaf() {
        let profile = profile(SKYLAKE);
        // Leaf 6 is subleaf-independent in the profile, and a backend may
        // report an indexed leaf without its subleaf.
        let presented = [
            CpuidEntry::new(0x6, Some(3), [0x4, 0, 0, 0]),
            CpuidEntry::new(0x7, None, [0, 0xd19f_4fbb, 0x8, 0x8400_0000]),
        ];
        check_unlisted_cpuid(profile, &presented).unwrap();
        assert!(unlisted_cpuid_candidates(profile, &presented).is_empty());
    }

    #[test]
    fn the_identity_range_ends_at_0x4fffffff() {
        let profile = profile(SKYLAKE);
        let presented = [
            CpuidEntry::new(0x4fff_ffff, None, [1, 0, 0, 0]),
            CpuidEntry::new(0x5000_0000, None, [1, 0, 0, 0]),
        ];
        assert_eq!(
            unlisted_cpuid_violations(profile, &presented),
            [
                "CPUID 0x50000000 is outside the profile and reads EAX 0x1, EBX 0x0, ECX 0x0, EDX 0x0"
            ]
        );
    }

    #[test]
    fn candidates_are_the_host_entries_outside_the_tables() {
        let profile = profile(SKYLAKE);
        let host = with(
            profile,
            &[
                CpuidEntry::new(0xb, Some(0), [1, 2, 0x100, 0]),
                CpuidEntry::new(0xf, Some(1), [0, 0xb, 0x6f, 0x7]),
                CpuidEntry::new(0x14, Some(1), [0x0249_0002, 0x003f_3fff, 0, 0]),
                CpuidEntry::new(0x4000_0000, None, [0x4000_000b, 0, 0, 0]),
                CpuidEntry::new(0x8000_0009, None, [0; 4]),
            ],
        );
        assert_eq!(
            unlisted_cpuid_candidates(profile, &host),
            [(0xf, Some(1)), (0x14, Some(1)), (0x8000_0009, None)]
        );
    }
}
