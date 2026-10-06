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
    let host = host_identity(profile.vendor().as_bytes(), &|leaf| profile.lookup(leaf, 0));
    let mut backend = BackendFingerprint::new(backend, "test", cpuid);
    backend.msrs.arch_capabilities = profile
        .msr(IA32_ARCH_CAPABILITIES)
        .map(|(value, _)| Hex64(value));
    CpuFingerprint::new(tool(), host, backend)
}

/// Returns the fingerprint of a host whose `backend` reports `cpuid`, of the
/// CPU whose vendor, signature, and brand `cpuid` reports.
pub(crate) fn host_fingerprint(backend: &str, cpuid: Vec<CpuidEntry>) -> CpuFingerprint {
    let leaf = |leaf| crate::cpuid::lookup(&cpuid, leaf, 0).unwrap_or_default();
    let [_, ebx, ecx, edx] = leaf(0);
    let host = host_identity(&crate::signature::vendor_bytes(ebx, edx, ecx), &leaf);
    CpuFingerprint::new(
        tool(),
        host,
        BackendFingerprint::new(backend, "test", cpuid),
    )
}

fn tool() -> ToolIdentity {
    ToolIdentity {
        name: "test".to_owned(),
        version: "0".to_owned(),
    }
}

/// Returns the identity of a host of `vendor`'s CPU whose signature and
/// brand string `cpuid(leaf)` reports.
fn host_identity(vendor: &[u8], cpuid: &dyn Fn(u32) -> [u32; 4]) -> HostIdentity {
    let signature = cpuid(1)[0];
    let (family, model, stepping) = decode_signature(vendor, signature);
    let brand = (0x8000_0002..=0x8000_0004)
        .flat_map(cpuid)
        .flat_map(u32::to_le_bytes)
        .collect::<Vec<_>>();
    HostIdentity {
        cpu: HostCpu {
            vendor: String::from_utf8_lossy(vendor).into_owned(),
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
    }
}

/// The CPUID of an AMD EPYC 7763 (Milan: family 0x19, model 1, stepping 1)
/// as a WHP probe partition presents it on an Azure host, which
/// `--cpu-fingerprint` recorded: each leaf, subleaf, and the registers. WHP
/// offers that host neither the speculation controls of `0x80000008` EBX nor
/// anything in `0x80000021`, and reports no cache sharing in `0x8000001D`.
pub(crate) const MILAN_WHP_CPUID: [(u32, Option<u32>, [u32; 4]); 57] = [
    (0x0, None, [0xd, 0x6874_7541, 0x444d_4163, 0x6974_6e65]),
    (0x1, None, [0x00a0_0f11, 0x800, 0x76fa_3203, 0x078b_fbff]),
    (0x2, None, [0; 4]),
    (0x3, None, [0; 4]),
    (0x4, Some(0), [0; 4]),
    (0x5, None, [0; 4]),
    (0x6, None, [0, 0, 1, 0]),
    (0x7, Some(0), [0, 0x219c_07a9, 0x0040_0684, 0x10]),
    (0x8, None, [0; 4]),
    (0x9, None, [0; 4]),
    (0xa, None, [0; 4]),
    (0xb, Some(0), [0; 4]),
    (0xc, None, [0; 4]),
    (0xd, Some(0), [0x7, 0x340, 0x340, 0]),
    (0xd, Some(1), [0xf, 0x368, 0x1800, 0]),
    (0xd, Some(2), [0x100, 0x240, 0, 0]),
    (0xd, Some(0xb), [0x10, 0, 1, 0]),
    (0xd, Some(0xc), [0x18, 0, 1, 0]),
    (0x4000_0000, None, [0; 4]),
    (
        0x8000_0000,
        None,
        [0x8000_0021, 0x6874_7541, 0x444d_4163, 0x6974_6e65],
    ),
    (
        0x8000_0001,
        None,
        [0x00a0_0f11, 0x4000_0000, 0x0040_03f3, 0x2fd3_fbff],
    ),
    (
        0x8000_0002,
        None,
        [0x2044_4d41, 0x4359_5045, 0x3637_3720, 0x3436_2033],
    ),
    (
        0x8000_0003,
        None,
        [0x726f_432d, 0x7250_2065, 0x7365_636f, 0x2072_6f73],
    ),
    (
        0x8000_0004,
        None,
        [0x2020_2020, 0x2020_2020, 0x2020_2020, 0x0020_2020],
    ),
    (
        0x8000_0005,
        None,
        [0xff40_ff40, 0xff40_ff40, 0x2008_0140, 0x2008_0140],
    ),
    (
        0x8000_0006,
        None,
        [0x4800_2200, 0x6800_4200, 0x0200_6140, 0x0800_9140],
    ),
    (0x8000_0007, None, [0; 4]),
    (0x8000_0008, None, [0x3030, 0x3000_0015, 0, 0x0001_0000]),
    (0x8000_0009, None, [0; 4]),
    (0x8000_000a, None, [0; 4]),
    (0x8000_000b, None, [0; 4]),
    (0x8000_000c, None, [0; 4]),
    (0x8000_000d, None, [0; 4]),
    (0x8000_000e, None, [0; 4]),
    (0x8000_000f, None, [0; 4]),
    (0x8000_0010, None, [0; 4]),
    (0x8000_0011, None, [0; 4]),
    (0x8000_0012, None, [0; 4]),
    (0x8000_0013, None, [0; 4]),
    (0x8000_0014, None, [0; 4]),
    (0x8000_0015, None, [0; 4]),
    (0x8000_0016, None, [0; 4]),
    (0x8000_0017, None, [0; 4]),
    (0x8000_0018, None, [0; 4]),
    (0x8000_0019, None, [0; 4]),
    (0x8000_001a, None, [0x2, 0, 0, 0]),
    (0x8000_001b, None, [0; 4]),
    (0x8000_001c, None, [0; 4]),
    (0x8000_001d, Some(0), [0x121, 0x01c0_003f, 0x3f, 0]),
    (0x8000_001d, Some(1), [0x122, 0x01c0_003f, 0x3f, 0]),
    (0x8000_001d, Some(2), [0x143, 0x01c0_003f, 0x3ff, 0x2]),
    (0x8000_001d, Some(3), [0x163, 0x03c0_003f, 0x7fff, 0x1]),
    (0x8000_001d, Some(4), [0; 4]),
    (0x8000_001e, None, [0; 4]),
    (0x8000_001f, None, [0; 4]),
    (0x8000_0020, None, [0; 4]),
    (0x8000_0021, None, [0; 4]),
];

/// Returns [`MILAN_WHP_CPUID`] as fingerprint entries.
pub(crate) fn milan_whp_entries() -> Vec<CpuidEntry> {
    MILAN_WHP_CPUID
        .iter()
        .map(|&(leaf, subleaf, registers)| CpuidEntry::new(leaf, subleaf, registers))
        .collect()
}
