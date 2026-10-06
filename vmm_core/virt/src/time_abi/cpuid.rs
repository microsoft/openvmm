// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! The effective CPUID of a time ABI partition: its CPU profile completed with
//! OpenVMM's topology leaves, the APIC mode, and the time ABI's identity
//! leaves. Core, the backends, and their tests all build it here.

use super::TimeAbiCode;
use super::TimeAbiError;
use crate::CpuidLeaf;
use crate::CpuidLeafSet;
use cpu_profile::CpuProfile;
use cpu_profile::CpuidResult;
use cpu_profile::EffectiveCpuid;
use vm_topology::processor::ProcessorTopology;
use vm_topology::processor::x86::ApicMode;
use vm_topology::processor::x86::X86Topology;

impl From<&CpuidLeaf> for CpuidResult {
    fn from(leaf: &CpuidLeaf) -> Self {
        Self {
            function: leaf.function,
            index: leaf.index,
            result: leaf.result,
            mask: leaf.mask,
        }
    }
}

/// Builds the effective CPUID of a partition with `profile` and `topology`:
/// the profile completed with OpenVMM's topology leaves, each extended
/// topology leaf's terminating subleaf, the APIC mode, and the time ABI's
/// identity and explicit zero leaves. Fails with `E_CPU_SURFACE` if they do
/// not complete the profile exactly.
pub fn effective_cpuid(
    profile: &CpuProfile,
    topology: &ProcessorTopology<X86Topology>,
) -> Result<EffectiveCpuid, TimeAbiError> {
    let mut topology_leaves = Vec::new();
    crate::x86::topology::topology_cpuid(
        topology,
        &|leaf, subleaf| profile.lookup(leaf, subleaf),
        &mut topology_leaves,
    )
    .map_err(|err| {
        TimeAbiError::new(
            TimeAbiCode::CpuSurface,
            format!(
                "cannot build the topology CPUID of CPU profile {}: {err}",
                profile.id()
            ),
        )
    })?;
    crate::x86::topology::terminate_extended_topology(topology, &mut topology_leaves);
    let mut vm: Vec<_> = topology_leaves.iter().map(CpuidResult::from).collect();
    vm.push(cpu_profile::x2apic_cpuid(!matches!(
        topology.apic_mode(),
        ApicMode::XApic
    )));
    let identity: Vec<_> = super::identity::identity_cpuid_leaves(topology.vp_count())
        .iter()
        .map(CpuidResult::from)
        .chain(super::identity::identity_zero_cpuid_leaves().map(|leaf| CpuidResult::from(&leaf)))
        .collect();
    Ok(profile.effective_cpuid(&vm, &identity)?)
}

/// Returns the CPUID results a backend programs for `effective`
/// ([`TimeAbiConfig::cpuid`](super::TimeAbiConfig::cpuid)): every result of
/// the effective CPUID, with the per-VP APIC identity bits unmasked, so that
/// the backend sets each VP's own.
pub fn backend_cpuid(effective: &EffectiveCpuid) -> CpuidLeafSet {
    CpuidLeafSet::new(
        effective
            .results()
            .map(|result| {
                let per_vp = crate::x86::topology::per_vp_cpuid_bits(result.function);
                CpuidLeaf {
                    function: result.function,
                    index: result.index,
                    result: result.result,
                    mask: [0, 1, 2, 3].map(|register| result.mask[register] & !per_vp[register]),
                }
            })
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::time_abi::identity;
    use cpu_profile::CpuVendor;
    use test_with_tracing::test;
    use vm_topology::processor::TopologyBuilder;
    use vm_topology::processor::x86::X2ApicState;
    use x86defs::cpuid::CacheParametersEax;

    fn topology(vp_count: u32, x2apic: X2ApicState) -> ProcessorTopology {
        TopologyBuilder::new_x86()
            .vps_per_socket(vp_count)
            .x2apic(x2apic)
            .build(vp_count)
            .unwrap()
    }

    /// The contract between OpenVMM and the profiles: OpenVMM's topology
    /// leaves and the time ABI's identity leaves complete every pinned
    /// profile into an effective CPUID that satisfies the CPU time bits and
    /// the identity, in both APIC modes.
    #[test]
    fn pinned_profiles_satisfy_the_time_abi_with_openvmm_topologies() {
        for profile in cpu_profile::pinned_profiles() {
            for vp_count in [1, 2, 8] {
                for x2apic in [X2ApicState::Unsupported, X2ApicState::Supported] {
                    let effective = effective_cpuid(profile, &topology(vp_count, x2apic))
                        .unwrap_or_else(|error| panic!("{}: {error}", profile.id()));
                    identity::check_time_bits(
                        &mut |leaf, subleaf| effective.lookup(leaf, subleaf),
                        true,
                    )
                    .unwrap_or_else(|error| panic!("{}: {error}", profile.id()));
                    identity::check_identity(
                        &mut |leaf, subleaf| effective.lookup(leaf, subleaf),
                        vp_count,
                    )
                    .unwrap_or_else(|error| panic!("{}: {error}", profile.id()));
                    assert_eq!(
                        EffectiveCpuid::decode(&effective.encode()).unwrap(),
                        effective
                    );
                }
            }
        }
    }

    /// Every vCPU of a time ABI partition has its own L1 and L2 caches, and
    /// all of them share the L3 cache, as in one socket of the profile's
    /// generation: in leaf 4 on Intel, and on AMD in leaf 8000001Dh, which
    /// has leaf 4's layout without the core count.
    #[test]
    fn pinned_profiles_share_the_l3_cache_across_the_socket() {
        for profile in cpu_profile::pinned_profiles() {
            let (leaf, has_core_count) = match profile.cpu_vendor() {
                CpuVendor::Intel => (4, true),
                CpuVendor::Amd => (0x8000_001d, false),
            };
            for vp_count in [1, 2, 8] {
                let topology = topology(vp_count, X2ApicState::Supported);
                let socket = topology.reserved_vps_per_socket();
                let effective = effective_cpuid(profile, &topology).unwrap();
                let mut levels = Vec::new();
                for subleaf in 0..16 {
                    let eax = CacheParametersEax::from(effective.lookup(leaf, subleaf)[0]);
                    if eax.cache_type() == 0 {
                        break;
                    }
                    let sharing = if eax.cache_level() == 3 {
                        socket - 1
                    } else {
                        0
                    };
                    assert_eq!(
                        eax.threads_sharing_cache_minus_one(),
                        sharing,
                        "{}, {vp_count} VPs, subleaf {subleaf}",
                        profile.id()
                    );
                    assert_eq!(
                        eax.cores_per_socket_minus_one(),
                        if has_core_count { socket - 1 } else { 0 },
                        "{}, {vp_count} VPs, subleaf {subleaf}",
                        profile.id()
                    );
                    levels.push(eax.cache_level());
                }
                assert_eq!(levels.last(), Some(&3), "{}", profile.id());
            }
        }
    }

    #[test]
    fn backend_cpuid_unmasks_only_the_per_vp_apic_bits() {
        for profile in cpu_profile::pinned_profiles() {
            let effective = effective_cpuid(profile, &topology(2, X2ApicState::Supported)).unwrap();
            let table = backend_cpuid(&effective);
            assert_eq!(
                table.leaves().len(),
                effective.results().count(),
                "{}",
                profile.id()
            );
            for result in effective.results() {
                let leaf = table
                    .leaves()
                    .iter()
                    .find(|leaf| leaf.function == result.function && leaf.index == result.index)
                    .unwrap_or_else(|| panic!("{}: {result:?} is missing", profile.id()));
                let per_vp = crate::x86::topology::per_vp_cpuid_bits(result.function);
                assert_eq!(leaf.result, result.result);
                for ((mask, effective_mask), per_vp) in
                    leaf.mask.iter().zip(result.mask).zip(per_vp)
                {
                    assert_eq!(*mask, effective_mask & !per_vp);
                }
            }
        }
    }
}
