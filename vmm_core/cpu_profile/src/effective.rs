// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! The effective guest CPUID of a VM: its profile's pinned values, the fields
//! its topology and APIC mode define, and the hypervisor identity leaves.

use crate::canonical;
use crate::error::ProfileError;
use crate::error::ProfileErrorCode;
use crate::profile::CpuProfile;
use crate::profile::CpuidLeafValue;
use crate::profile::VM_OWNED_LEAVES;
use crate::profile::describe_leaf;
use crate::profile::find;
use crate::profile::vm_owned_bits;
use serde::Deserialize;
use serde::Serialize;
use std::cmp::Ordering;
use std::collections::BTreeMap;

/// The schema of the effective-CPUID records this crate reads and writes.
pub const EFFECTIVE_CPUID_SCHEMA: &str = "openvmm-effective-cpuid/v1";

/// The leaves a hypervisor may use, which the time ABI's identity owns.
const HYPERVISOR_LEAVES: std::ops::RangeInclusive<u32> = 0x4000_0000..=0x4fff_ffff;

/// The register names, for messages.
const REGISTERS: [&str; 4] = ["EAX", "EBX", "ECX", "EDX"];

/// One CPUID result and the mask of the bits it defines.
///
/// The fields match `virt::CpuidLeaf` one for one, so converting between the
/// two is field for field. `index` is `None` for a result that applies to
/// every subleaf.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CpuidResult {
    /// The leaf, the input value of `EAX`.
    pub function: u32,
    /// The subleaf, the input value of `ECX`, or `None` for every subleaf.
    pub index: Option<u32>,
    /// The output values of `EAX`, `EBX`, `ECX`, and `EDX`.
    pub result: [u32; 4],
    /// The bits of each output register that `result` defines.
    pub mask: [u32; 4],
}

impl CpuidResult {
    /// Returns a result for every subleaf of `function` that defines every
    /// bit.
    pub const fn new(function: u32, result: [u32; 4]) -> Self {
        Self {
            function,
            index: None,
            result,
            mask: [!0; 4],
        }
    }

    /// Returns the result for subleaf `index` only.
    pub const fn indexed(self, index: u32) -> Self {
        Self {
            index: Some(index),
            ..self
        }
    }

    /// Returns the result defining only the bits in `mask`.
    pub const fn masked(self, mask: [u32; 4]) -> Self {
        Self { mask, ..self }
    }
}

/// Returns the CPUID result that reports the x2APIC as the VM's APIC mode
/// decides: `CPUID.1:ECX[21]`, set unless the APIC is xAPIC-only.
pub const fn x2apic_cpuid(x2apic: bool) -> CpuidResult {
    const X2APIC: u32 = 1 << 21;
    CpuidResult::new(0x1, [0, 0, if x2apic { X2APIC } else { 0 }, 0]).masked([0, 0, X2APIC, 0])
}

/// The effective guest CPUID of a VM.
///
/// It defines every bit of every leaf and subleaf the guest can observe,
/// except the runtime state that the processor maintains (OSXSAVE, OSPKE, and
/// the XSAVE sizes of the enabled features). Leaves it does not list read as
/// zero. The per-VP fields carry the BSP's APIC identity; backends set each
/// VP's own.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EffectiveCpuid {
    entries: Vec<CpuidLeafValue>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct EffectiveCpuidDocument {
    schema: String,
    cpuid: Vec<CpuidLeafValue>,
}

impl EffectiveCpuid {
    /// Returns the results, sorted by leaf and subleaf.
    pub fn results(&self) -> impl Iterator<Item = CpuidResult> + '_ {
        self.entries.iter().map(|entry| CpuidResult {
            function: entry.leaf.0,
            index: entry.subleaf.map(|subleaf| subleaf.0),
            result: entry.values(),
            mask: entry.masks(),
        })
    }

    /// Returns the output of CPUID for `leaf` and `subleaf`, with zeros in
    /// the runtime-owned bits, or zeros for a leaf the table does not list.
    pub fn lookup(&self, leaf: u32, subleaf: u32) -> [u32; 4] {
        find(&self.entries, leaf, subleaf).map_or([0; 4], CpuidLeafValue::values)
    }

    /// Returns the canonical encoding: compact canonical JSON.
    pub fn encode(&self) -> Vec<u8> {
        let document = EffectiveCpuidDocument {
            schema: EFFECTIVE_CPUID_SCHEMA.to_owned(),
            cpuid: self.entries.clone(),
        };
        let value =
            serde_json::to_value(document).expect("effective CPUID serialization is infallible");
        canonical::to_compact(&value).into_bytes()
    }

    /// Returns the SHA-256 of [`Self::encode`].
    pub fn digest(&self) -> [u8; 32] {
        canonical::sha256(&self.encode())
    }

    /// Decodes an effective CPUID from its canonical encoding.
    ///
    /// Bytes that are not exactly a canonical encoding fail with
    /// `E_PROFILE_DIGEST`.
    pub fn decode(bytes: &[u8]) -> Result<Self, ProfileError> {
        let invalid = |message: &str| {
            ProfileError::new(
                ProfileErrorCode::ProfileDigest,
                format!("invalid effective CPUID record: {message}"),
            )
        };
        let document: EffectiveCpuidDocument = serde_json::from_slice(bytes)
            .map_err(|error| invalid(&format!("malformed: {error}")))?;
        if document.schema != EFFECTIVE_CPUID_SCHEMA {
            return Err(invalid(&format!(
                "unsupported schema {:?}",
                document.schema
            )));
        }
        if document
            .cpuid
            .windows(2)
            .any(|pair| pair[0].key() >= pair[1].key())
        {
            return Err(invalid("entries are not strictly sorted"));
        }
        if document.cpuid.iter().any(|entry| {
            entry
                .values()
                .iter()
                .zip(entry.masks())
                .any(|(value, mask)| value & !mask != 0)
        }) {
            return Err(invalid("an entry sets bits outside its mask"));
        }
        let this = Self {
            entries: document.cpuid,
        };
        if this.encode() != bytes {
            return Err(invalid("not in canonical form"));
        }
        Ok(this)
    }

    /// Decodes a recorded effective CPUID and verifies it against its
    /// recorded SHA-256 (`E_PROFILE_DIGEST`).
    pub fn decode_verified(bytes: &[u8], sha256: &[u8]) -> Result<Self, ProfileError> {
        if canonical::sha256(bytes).as_slice() != sha256 {
            return Err(ProfileError::new(
                ProfileErrorCode::ProfileDigest,
                "the effective CPUID record does not match its digest",
            ));
        }
        Self::decode(bytes)
    }

    /// Checks that this effective CPUID, recomputed on the destination,
    /// equals the `recorded` one (`E_CPU_SURFACE`, naming every difference).
    pub fn check_matches(&self, recorded: &Self) -> Result<(), ProfileError> {
        let differences = self.differences(recorded);
        if differences.is_empty() {
            return Ok(());
        }
        Err(ProfileError::new(
            ProfileErrorCode::CpuSurface,
            format!(
                "the effective CPUID differs from the snapshot's: {}",
                differences.join("; ")
            ),
        ))
    }

    /// Lists every entry that differs between this effective CPUID and the
    /// `recorded` one; both are sorted by leaf and subleaf.
    fn differences(&self, recorded: &Self) -> Vec<String> {
        let (ours, theirs) = (&self.entries, &recorded.entries);
        let (mut i, mut j) = (0, 0);
        let mut differences = Vec::new();
        loop {
            let order = match (ours.get(i), theirs.get(j)) {
                (None, None) => return differences,
                (Some(_), None) => Ordering::Less,
                (None, Some(_)) => Ordering::Greater,
                (Some(a), Some(b)) => a.key().cmp(&b.key()),
            };
            match order {
                Ordering::Less => {
                    differences.push(format!(
                        "{} exists only on the destination",
                        describe(&ours[i])
                    ));
                    i += 1;
                }
                Ordering::Greater => {
                    differences.push(format!(
                        "{} exists only in the snapshot",
                        describe(&theirs[j])
                    ));
                    j += 1;
                }
                Ordering::Equal => {
                    let (a, b) = (&ours[i], &theirs[j]);
                    if a != b {
                        differences.push(format!(
                            "{} is {:#010x?} under mask {:#010x?} on the destination, but {:#010x?} under mask {:#010x?} in the snapshot",
                            describe(a),
                            a.values(),
                            a.masks(),
                            b.values(),
                            b.masks()
                        ));
                    }
                    i += 1;
                    j += 1;
                }
            }
        }
    }
}

fn describe(entry: &CpuidLeafValue) -> String {
    describe_leaf(entry.leaf.0, entry.subleaf.map(|subleaf| subleaf.0))
}

impl CpuProfile {
    /// Builds the effective guest CPUID of a VM that uses this profile.
    ///
    /// `vm` holds the results that the VM configuration defines: the topology
    /// leaves and fields (`virt::x86::topology::topology_cpuid`, with the
    /// profile's [`CpuProfile::lookup`] as its CPUID source) and the APIC
    /// mode ([`x2apic_cpuid`]). They may set only the bits that
    /// [`vm_owned_bits`] assigns to the VM, and together they must set all of
    /// them. A result for every subleaf (`index` of `None`) applies to each of
    /// the profile's subleaves that has VM-owned bits. `identity` holds the
    /// time ABI's hypervisor identity leaves, which must lie in
    /// `0x40000000..=0x4fffffff`. Later results override earlier ones.
    ///
    /// Fails with `E_CPU_SURFACE` when the inputs do not complete the profile
    /// exactly.
    pub fn effective_cpuid(
        &self,
        vm: &[CpuidResult],
        identity: &[CpuidResult],
    ) -> Result<EffectiveCpuid, ProfileError> {
        let error = |message: String| {
            ProfileError::new(
                ProfileErrorCode::CpuSurface,
                format!(
                    "cannot build the effective CPUID of profile {}: {message}",
                    self.id()
                ),
            )
        };
        let mut entries = self
            .cpuid()
            .iter()
            .map(|entry| (entry.key(), (entry.values(), entry.masks())))
            .collect::<BTreeMap<_, _>>();
        let max_basic = self.lookup(0, 0)[0];

        for leaf in vm.iter().filter(|leaf| leaf.mask != [0; 4]) {
            let what = describe_leaf(leaf.function, leaf.index);
            if VM_OWNED_LEAVES.contains(&leaf.function) {
                if leaf.function > max_basic {
                    return Err(error(format!("{what} is beyond the maximum basic leaf")));
                }
                if leaf.index.is_none() {
                    return Err(error(format!("{what} has no subleaf")));
                }
                merge(
                    entries
                        .entry((leaf.function, leaf.index))
                        .or_insert(([0; 4], [0; 4])),
                    leaf,
                );
                continue;
            }
            let mut applied = false;
            for (&(function, index), slot) in entries.range_mut((leaf.function, None)..) {
                if function != leaf.function {
                    break;
                }
                if leaf.index.is_some() && index.is_some() && index != leaf.index {
                    continue;
                }
                let owned = vm_owned_bits(function, slot.0);
                if leaf.index.is_none() && owned == [0; 4] {
                    // A result for every subleaf skips subleaves without VM
                    // fields, such as the null cache type that ends leaf 4.
                    continue;
                }
                for (register, (bits, owned)) in leaf.mask.iter().zip(owned).enumerate() {
                    if bits & !owned != 0 {
                        return Err(error(format!(
                            "{what} sets {} bits {:#x}, which the profile pins",
                            REGISTERS[register],
                            bits & !owned
                        )));
                    }
                }
                merge(slot, leaf);
                applied = true;
            }
            if !applied {
                return Err(error(format!(
                    "{what} matches no profile leaf with VM fields"
                )));
            }
        }

        for leaf in identity {
            let what = describe_leaf(leaf.function, leaf.index);
            if !HYPERVISOR_LEAVES.contains(&leaf.function) {
                return Err(error(format!(
                    "identity {what} is outside the hypervisor range"
                )));
            }
            merge(
                entries
                    .entry((leaf.function, leaf.index))
                    .or_insert(([0; 4], [0; 4])),
                leaf,
            );
        }

        for &leaf in &VM_OWNED_LEAVES {
            if leaf <= max_basic && !entries.contains_key(&(leaf, Some(0))) {
                return Err(error(format!(
                    "the VM did not define {}",
                    describe_leaf(leaf, Some(0))
                )));
            }
        }
        for (&(function, index), (value, mask)) in &entries {
            if HYPERVISOR_LEAVES.contains(&function) {
                continue;
            }
            let owned = vm_owned_bits(function, *value);
            for (register, (owned, mask)) in owned.iter().zip(mask).enumerate() {
                if owned & !mask != 0 {
                    return Err(error(format!(
                        "the VM did not define {} {} bits {:#x}",
                        describe_leaf(function, index),
                        REGISTERS[register],
                        owned & !mask
                    )));
                }
            }
        }

        Ok(EffectiveCpuid {
            entries: entries
                .into_iter()
                .map(|((function, index), (value, mask))| {
                    CpuidLeafValue::new(function, index, value, mask)
                })
                .collect(),
        })
    }
}

fn merge(slot: &mut ([u32; 4], [u32; 4]), leaf: &CpuidResult) {
    let (value, mask) = slot;
    for register in 0..4 {
        value[register] = (value[register] & !leaf.mask[register])
            | (leaf.result[register] & leaf.mask[register]);
        mask[register] |= leaf.mask[register];
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::profile;
    use test_with_tracing::test;

    const ICELAKE: &str = "intel.icelake-sp.v1";

    /// Returns hand-built VM leaves for `profile`: the fields OpenVMM's
    /// topology code sets for one socket of `vp_count` VPs, and an xAPIC.
    fn vm_leaves(profile: &CpuProfile, vp_count: u32) -> Vec<CpuidResult> {
        let mut leaves = vec![
            CpuidResult::new(1, [0, vp_count << 16, 0, 0]).masked([0, 0xffff_0000, 0, 0]),
            CpuidResult::new(4, [(vp_count - 1) << 26, 0, 0, 0]).masked([0xffff_c000, 0, 0, 0]),
            x2apic_cpuid(false),
        ];
        let max_basic = profile.lookup(0, 0)[0];
        for leaf in VM_OWNED_LEAVES
            .into_iter()
            .filter(|&leaf| leaf <= max_basic)
        {
            leaves.push(CpuidResult::new(leaf, [0, 1, 0x100, 0]).indexed(0));
            leaves.push(CpuidResult::new(leaf, [0, vp_count, 0x201, 0]).indexed(1));
        }
        leaves
    }

    fn identity() -> Vec<CpuidResult> {
        vec![CpuidResult::new(
            0x4000_0000,
            [0x4000_0005, 0x7263_694d, 0x666f_736f, 0x7648_2074],
        )]
    }

    fn error_message(result: Result<EffectiveCpuid, ProfileError>) -> String {
        let error = result.unwrap_err();
        assert_eq!(error.code, ProfileErrorCode::CpuSurface, "{error}");
        error.message
    }

    #[test]
    fn completes_the_profile_with_vm_fields_and_identity() {
        let profile = profile(ICELAKE);
        let effective = profile
            .effective_cpuid(&vm_leaves(profile, 4), &identity())
            .unwrap();
        // The profile's pinned bits and the VM's fields.
        let leaf1 = effective.lookup(1, 0);
        assert_eq!(leaf1[0], profile.lookup(1, 0)[0]);
        assert_eq!(leaf1[1] >> 16, 4);
        assert_eq!(leaf1[2] & (1 << 21), 0);
        // Every subleaf of leaf 4 that describes a cache, but not the null
        // cache type that ends it.
        assert_eq!(effective.lookup(4, 0)[0] >> 26, 3);
        assert_eq!(effective.lookup(4, 4), [0; 4]);
        assert_eq!(effective.lookup(0xb, 1)[1], 4);
        assert_eq!(effective.lookup(0x4000_0000, 0)[0], 0x4000_0005);
        // Only the runtime state is left undefined.
        let undefined = effective
            .results()
            .filter(|result| result.mask != [!0; 4])
            .map(|result| (result.function, result.index, result.mask))
            .collect::<Vec<_>>();
        assert_eq!(
            undefined,
            [
                (1, None, [!0, !0, !(1 << 27), !0]),
                (7, Some(0), [!0, !0, !(1 << 4), !0]),
                (0xd, Some(0), [!0, 0, !0, !0]),
                (0xd, Some(1), [!0, 0, !0, !0]),
            ]
        );
    }

    #[test]
    fn rejects_inputs_that_do_not_complete_the_profile_exactly() {
        let profile = profile(ICELAKE);
        let mut leaves = vm_leaves(profile, 2);
        leaves.push(CpuidResult::new(1, [0, 0, 1, 0]).masked([0, 0, 1, 0]));
        assert!(
            error_message(profile.effective_cpuid(&leaves, &[]))
                .contains("CPUID 0x1 sets ECX bits 0x1, which the profile pins")
        );

        let mut leaves = vm_leaves(profile, 2);
        leaves.retain(|leaf| *leaf != x2apic_cpuid(false));
        assert!(
            error_message(profile.effective_cpuid(&leaves, &[]))
                .contains("the VM did not define CPUID 0x1 ECX bits 0x200000")
        );

        let leaves = vm_leaves(profile, 2)
            .into_iter()
            .filter(|leaf| leaf.function != 0xb)
            .collect::<Vec<_>>();
        assert!(
            error_message(profile.effective_cpuid(&leaves, &[]))
                .contains("the VM did not define CPUID 0xb.0")
        );

        let leaves = vm_leaves(profile, 2);
        assert!(
            error_message(profile.effective_cpuid(&leaves, &[CpuidResult::new(3, [0; 4])]))
                .contains("outside the hypervisor range")
        );
        let mut leaves = vm_leaves(profile, 2);
        leaves.push(CpuidResult::new(0x1f, [0; 4]).indexed(0));
        assert!(
            error_message(profile.effective_cpuid(&leaves, &[]))
                .contains("beyond the maximum basic leaf")
        );
    }

    #[test]
    fn records_round_trip_and_compare() {
        let profile = profile(ICELAKE);
        let effective = profile
            .effective_cpuid(&vm_leaves(profile, 2), &identity())
            .unwrap();
        let bytes = effective.encode();
        assert_eq!(EffectiveCpuid::decode(&bytes).unwrap(), effective);
        assert_eq!(
            EffectiveCpuid::decode_verified(&bytes, &effective.digest()).unwrap(),
            effective
        );
        assert_eq!(
            EffectiveCpuid::decode_verified(&bytes, &[0; 32])
                .unwrap_err()
                .code,
            ProfileErrorCode::ProfileDigest
        );
        let mut spaced = bytes.clone();
        spaced.insert(1, b' ');
        assert_eq!(
            EffectiveCpuid::decode(&spaced).unwrap_err().code,
            ProfileErrorCode::ProfileDigest
        );

        effective.check_matches(&effective).unwrap();
        let other = profile
            .effective_cpuid(&vm_leaves(profile, 4), &identity())
            .unwrap();
        let error = other.check_matches(&effective).unwrap_err();
        assert_eq!(error.code, ProfileErrorCode::CpuSurface);
        // Every difference is named: the logical processor counts of leaves
        // 1, 4, and 0xB.
        for difference in ["CPUID 0x1 is", "CPUID 0x4.0 is", "CPUID 0xb.1 is"] {
            assert!(error.message.contains(difference), "{error}");
        }
        let without_identity = profile
            .effective_cpuid(&vm_leaves(profile, 2), &[])
            .unwrap();
        let error = without_identity.check_matches(&effective).unwrap_err();
        assert!(
            error
                .message
                .contains("CPUID 0x40000000 exists only in the snapshot"),
            "{error}"
        );
        let error = effective.check_matches(&without_identity).unwrap_err();
        assert!(
            error
                .message
                .ends_with(": CPUID 0x40000000 exists only on the destination"),
            "{error}"
        );
    }

    /// The contract with core: OpenVMM's topology leaves and the time ABI's
    /// identity leaves complete every pinned profile into an effective CPUID
    /// that satisfies the CPU time bits and the identity.
    #[cfg(guest_arch = "x86_64")]
    #[test]
    fn pinned_profiles_satisfy_the_time_abi_with_openvmm_topologies() {
        use virt::time_abi::identity;
        use vm_topology::processor::TopologyBuilder;
        use vm_topology::processor::x86::X2ApicState;

        let result = |leaf: &virt::CpuidLeaf| CpuidResult {
            function: leaf.function,
            index: leaf.index,
            result: leaf.result,
            mask: leaf.mask,
        };
        for profile in crate::pinned_profiles() {
            for vp_count in [1, 2, 8] {
                let topology = TopologyBuilder::new_x86()
                    .vps_per_socket(vp_count)
                    .x2apic(X2ApicState::Unsupported)
                    .build(vp_count)
                    .unwrap();
                let mut topology_leaves = Vec::new();
                virt::x86::topology::topology_cpuid(
                    &topology,
                    &|leaf, subleaf| profile.lookup(leaf, subleaf),
                    &mut topology_leaves,
                )
                .unwrap();
                let mut vm = topology_leaves.iter().map(result).collect::<Vec<_>>();
                vm.push(x2apic_cpuid(false));
                let identity_leaves = identity::identity_cpuid_leaves(vp_count)
                    .iter()
                    .map(result)
                    .chain(identity::identity_zero_cpuid_leaves().map(|leaf| result(&leaf)))
                    .collect::<Vec<_>>();
                let effective = profile.effective_cpuid(&vm, &identity_leaves).unwrap();

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
