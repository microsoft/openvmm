// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Fixtures shared by the unit tests.

use crate::CpuProfile;
use crate::Hex32;
use crate::Hex64;
use crate::cpuid::CpuidEntry;
use crate::fingerprint::BackendFingerprint;
use crate::fingerprint::CpuFingerprint;
use crate::fingerprint::IA32_ARCH_CAPABILITIES;
use crate::fingerprint::ToolIdentity;
use crate::host::HostCpu;
use crate::host::HostIdentity;
use crate::host::HostOs;
use crate::signature::decode_signature;

/// Returns the pinned profile `id`.
pub(crate) fn profile(id: &str) -> &'static CpuProfile {
    crate::pinned(id).unwrap_or_else(|| panic!("profile {id} is not pinned"))
}

/// Returns a profile's CPUID values as fingerprint entries.
pub(crate) fn profile_entries(profile: &CpuProfile) -> Vec<CpuidEntry> {
    profile
        .cpuid()
        .iter()
        .map(|entry| CpuidEntry::new(entry.leaf.0, entry.subleaf.map(|s| s.0), entry.values()))
        .collect()
}

/// Returns the fingerprint of a host whose `backend` supports exactly
/// `profile`: its CPUID values and, as a KVM host would report it, its pinned
/// `IA32_ARCH_CAPABILITIES` value.
pub(crate) fn fingerprint(profile: &CpuProfile, backend: &str) -> CpuFingerprint {
    fingerprint_with(profile, backend, profile_entries(profile))
}

/// Returns the fingerprint of a host whose `backend` reports `cpuid`, of
/// `profile`'s CPU.
pub(crate) fn fingerprint_with(
    profile: &CpuProfile,
    backend: &str,
    cpuid: Vec<CpuidEntry>,
) -> CpuFingerprint {
    let signature = profile.lookup(1, 0)[0];
    let (family, model, stepping) = decode_signature(profile.vendor().as_bytes(), signature);
    let brand = (0x8000_0002..=0x8000_0004)
        .flat_map(|leaf| profile.lookup(leaf, 0))
        .flat_map(u32::to_le_bytes)
        .collect::<Vec<_>>();
    let host = HostIdentity {
        cpu: HostCpu {
            vendor: profile.vendor().to_owned(),
            signature: Hex32(signature),
            family,
            model,
            stepping,
            brand: String::from_utf8_lossy(&brand)
                .trim_end_matches('\0')
                .to_owned(),
            microcode: Vec::new(),
            invariant_tsc: true,
            tsc_deadline: true,
            tsc_adjust: true,
        },
        os: HostOs {
            kind: "linux".to_owned(),
            release: None,
            version: None,
            cpu_flags: Vec::new(),
            clocksource: None,
            available_clocksources: Vec::new(),
        },
        hypervisor: None,
    };
    let mut backend = BackendFingerprint::new(backend, "test", cpuid);
    backend.msrs.arch_capabilities = profile
        .msr(IA32_ARCH_CAPABILITIES)
        .map(|(value, _)| Hex64(value));
    CpuFingerprint::new(
        ToolIdentity {
            name: "test".to_owned(),
            version: "0".to_owned(),
        },
        host,
        backend,
    )
}
