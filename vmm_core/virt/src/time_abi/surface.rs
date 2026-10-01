// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! The effective CPU surface record of a snapshot: the canonical CPUID
//! encoding and its comparison, and the interim CPU profile used until CPU
//! profiles land.

use super::TimeAbiCode;
use super::TimeAbiError;
use crate::CpuidLeaf;

/// The length of one leaf in the canonical CPUID encoding.
pub const ENCODED_CPUID_LEAF_LEN: usize = 24;
/// The most differing leaves an `E_CPU_SURFACE` failure names.
const MAX_REPORTED_CPUID_DIFFERENCES: usize = 16;

/// Returns the canonical encoding of an effective CPUID table, as recorded in
/// a snapshot's CPU profile record.
///
/// Leaves are sorted by function and subleaf, a leaf without a subleaf sorts
/// first, and duplicates keep the last entry. Each leaf is 24 bytes, all
/// little-endian `u32`s: the function, the subleaf (`0xffffffff` for any),
/// and EAX, EBX, ECX, and EDX. The values are the effective results, so the
/// masks are not encoded.
pub fn encode_cpuid(leaves: &[CpuidLeaf]) -> Vec<u8> {
    let mut entries: Vec<(u32, Option<u32>, [u32; 4])> = leaves
        .iter()
        .map(|leaf| (leaf.function, leaf.index, leaf.result))
        .collect();
    entries.sort_by_key(|&(function, index, _)| (function, index));
    let mut deduplicated: Vec<(u32, Option<u32>, [u32; 4])> = Vec::with_capacity(entries.len());
    for entry in entries {
        match deduplicated.last_mut() {
            Some(last) if (last.0, last.1) == (entry.0, entry.1) => *last = entry,
            _ => deduplicated.push(entry),
        }
    }
    let mut bytes = Vec::with_capacity(deduplicated.len() * ENCODED_CPUID_LEAF_LEN);
    for (function, index, result) in deduplicated {
        bytes.extend_from_slice(&function.to_le_bytes());
        bytes.extend_from_slice(&index.unwrap_or(u32::MAX).to_le_bytes());
        for register in result {
            bytes.extend_from_slice(&register.to_le_bytes());
        }
    }
    bytes
}

/// Decodes a canonical CPUID encoding into `((function, subleaf), result)`
/// pairs, or returns `None` if its length is not a multiple of a leaf.
fn decode_cpuid(bytes: &[u8]) -> Option<Vec<((u32, u32), [u32; 4])>> {
    let (leaves, rest) = bytes.as_chunks::<ENCODED_CPUID_LEAF_LEN>();
    if !rest.is_empty() {
        return None;
    }
    let word = |leaf: &[u8; ENCODED_CPUID_LEAF_LEN], n: usize| {
        u32::from_le_bytes(leaf[n * 4..n * 4 + 4].try_into().unwrap())
    };
    Some(
        leaves
            .iter()
            .map(|leaf| {
                (
                    (word(leaf, 0), word(leaf, 1)),
                    [word(leaf, 2), word(leaf, 3), word(leaf, 4), word(leaf, 5)],
                )
            })
            .collect(),
    )
}

/// Compares the effective CPUID a backend programmed with the canonical
/// encoding a snapshot recorded (`E_CPU_SURFACE`), naming the differing
/// leaves and registers.
pub fn check_effective_cpuid(effective: &[CpuidLeaf], recorded: &[u8]) -> Result<(), TimeAbiError> {
    let encoded = encode_cpuid(effective);
    if encoded == recorded {
        return Ok(());
    }
    let recorded = decode_cpuid(recorded).ok_or_else(|| {
        TimeAbiError::new(
            TimeAbiCode::CpuSurface,
            "the snapshot's effective CPUID record is malformed",
        )
    })?;
    let recorded: std::collections::BTreeMap<_, _> = recorded.into_iter().collect();
    let effective: std::collections::BTreeMap<_, _> = decode_cpuid(&encoded)
        .expect("canonical encoding is whole leaves")
        .into_iter()
        .collect();
    let mut differences = Vec::new();
    for key in recorded
        .keys()
        .chain(effective.keys().filter(|key| !recorded.contains_key(key)))
    {
        let (old, new) = (recorded.get(key), effective.get(key));
        if old == new {
            continue;
        }
        let leaf = if key.1 == u32::MAX {
            format!("{:#010x}", key.0)
        } else {
            format!("{:#010x}/{}", key.0, key.1)
        };
        differences.push(match (old, new) {
            (Some(old), Some(new)) => {
                let registers: Vec<String> = ["eax", "ebx", "ecx", "edx"]
                    .iter()
                    .zip(old.iter().zip(new))
                    .filter(|(_, (old, new))| old != new)
                    .map(|(name, (old, new))| {
                        format!(
                            "{name} {old:#010x} -> {new:#010x} (bits {:#010x})",
                            old ^ new
                        )
                    })
                    .collect();
                format!("{leaf} {}", registers.join(", "))
            }
            (Some(_), None) => format!("{leaf} missing"),
            (None, _) => format!("{leaf} added"),
        });
    }
    let count = differences.len();
    differences.truncate(MAX_REPORTED_CPUID_DIFFERENCES);
    Err(TimeAbiError::new(
        TimeAbiCode::CpuSurface,
        format!(
            "the effective CPUID differs from the snapshot's in {count} leaves (snapshot -> destination): {}",
            differences.join("; ")
        ),
    ))
}

/// Returns the CPU profile ID recorded until CPU profiles land: the backend's
/// own CPUID with the time ABI applied. Such a snapshot restores only on the
/// same backend and host CPU signature, with an identical effective CPUID.
pub fn interim_cpu_profile_id(hypervisor: &str) -> String {
    format!("interim.host.{hypervisor}.v1")
}

/// Resolves a requested CPU profile (`auto` or a profile ID) on `hypervisor`
/// to the profile ID a VM uses (`E_PROFILE_UNKNOWN`). Until CPU profiles land,
/// only the interim profile exists.
pub fn resolve_cpu_profile(requested: &str, hypervisor: &str) -> Result<String, TimeAbiError> {
    let interim = interim_cpu_profile_id(hypervisor);
    if requested == "auto" || requested == interim {
        Ok(interim)
    } else {
        Err(TimeAbiError::new(
            TimeAbiCode::ProfileUnknown,
            format!(
                "CPU profile '{requested}' is not pinned in this OpenVMM; the {hypervisor} backend offers '{interim}'"
            ),
        ))
    }
}

/// Returns the host's display signature, CPUID.1:EAX, as the VMM's host OS
/// sees it, or `None` on a host that is not x86-64.
pub fn host_cpu_signature() -> Option<u32> {
    // xtask-fmt allow-target-arch cpu-intrinsic
    #[cfg(target_arch = "x86_64")]
    {
        Some(safe_intrinsics::cpuid(1, 0).eax)
    }
    // xtask-fmt allow-target-arch cpu-intrinsic
    #[cfg(not(target_arch = "x86_64"))]
    {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cpuid_encoding_is_canonical() {
        let leaves = [
            CpuidLeaf::new(7, [1, 2, 3, 4]).indexed(1),
            CpuidLeaf::new(1, [5, 6, 7, 8]),
            CpuidLeaf::new(7, [9, 10, 11, 12]).indexed(0),
            CpuidLeaf::new(1, [13, 14, 15, 16]),
        ];
        let bytes = encode_cpuid(&leaves);
        assert_eq!(bytes.len(), 3 * 24);
        let words: Vec<u32> = bytes
            .chunks(4)
            .map(|word| u32::from_le_bytes(word.try_into().unwrap()))
            .collect();
        assert_eq!(
            words,
            [
                1,
                u32::MAX,
                13,
                14,
                15,
                16,
                7,
                0,
                9,
                10,
                11,
                12,
                7,
                1,
                1,
                2,
                3,
                4,
            ]
        );
        let mut reversed = leaves;
        reversed.swap(0, 2);
        reversed.swap(1, 3);
        assert_ne!(encode_cpuid(&reversed), bytes);
        assert_eq!(encode_cpuid(&[]), Vec::<u8>::new());
    }

    #[test]
    fn effective_cpuid_differences_are_named() {
        let recorded = encode_cpuid(&[
            CpuidLeaf::new(1, [1, 2, 3, 4]),
            CpuidLeaf::new(7, [0; 4]).indexed(0),
        ]);
        check_effective_cpuid(
            &[
                CpuidLeaf::new(7, [0; 4]).indexed(0),
                CpuidLeaf::new(1, [1, 2, 3, 4]),
            ],
            &recorded,
        )
        .unwrap();

        let err = check_effective_cpuid(
            &[
                CpuidLeaf::new(1, [1, 2, 7, 4]),
                CpuidLeaf::new(0xd, [1; 4]).indexed(1),
            ],
            &recorded,
        )
        .unwrap_err();
        assert_eq!(err.code, TimeAbiCode::CpuSurface);
        for expected in [
            "in 3 leaves",
            "0x00000001 ecx 0x00000003 -> 0x00000007 (bits 0x00000004)",
            "0x00000007/0 missing",
            "0x0000000d/1 added",
        ] {
            assert!(err.message.contains(expected), "{err}");
        }

        let malformed = check_effective_cpuid(&[], &[0; 23]).unwrap_err();
        assert_eq!(malformed.code, TimeAbiCode::CpuSurface);
        assert!(malformed.message.contains("malformed"), "{malformed}");
    }

    #[test]
    fn interim_cpu_profile_resolution() {
        assert_eq!(
            resolve_cpu_profile("auto", "kvm").unwrap(),
            "interim.host.kvm.v1"
        );
        assert_eq!(
            resolve_cpu_profile("interim.host.mshv.v1", "mshv").unwrap(),
            "interim.host.mshv.v1"
        );
        for (requested, hypervisor) in [
            ("interim.host.kvm.v1", "mshv"),
            ("intel.icelake-sp.kvm.v1", "kvm"),
        ] {
            assert_eq!(
                resolve_cpu_profile(requested, hypervisor).unwrap_err().code,
                TimeAbiCode::ProfileUnknown
            );
        }
        assert_ne!(host_cpu_signature(), Some(0));
    }
}
