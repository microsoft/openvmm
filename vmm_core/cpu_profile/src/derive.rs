// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Deriving a CPU profile from host fingerprints.
//!
//! A profile serves one CPU generation on every backend, so it is the
//! intersection of what every backend supports on every fingerprinted host
//! of the generation, under a fixed policy:
//!
//! 1. Within each backend, and then across backends, each CPUID register is
//!    combined by its class: feature flags by intersection, numeric limits
//!    by minimum, and descriptive values (signature, cache, TLB) by majority,
//!    one vote per host within a backend and one per backend across them. A
//!    tie is an error.
//! 2. Only the leaves of [`DATA_LEAVES`] keep data; every other leaf in range
//!    is zero. The time ABI's CPU time bits are applied, and features a VM
//!    must not see (virtualization, power management, performance
//!    monitoring, debug, SGX, RDT, PT, CET, key locker) are cleared.
//! 3. The XSAVE features are the intersection, within [`ALLOWED_XCR0`], and
//!    the XSAVE leaf is rebuilt from them; every host must agree on the
//!    layout. A feature whose XSAVE state is not enabled is cleared.
//! 4. The brand string is generic per generation,
//!    `Intel(R) Xeon(R) Processor (<display name>)` without a frequency
//!    ([`KnownGeneration::brand`]), so every host of a generation presents
//!    it whatever its SKU, and the hosts' brands are not compared.
//! 5. `IA32_ARCH_CAPABILITIES` is pinned in [`ARCH_CAPABILITIES_PINNED_MASK`]
//!    to what every backend can present: KVM's value, and for MSHV and WHP
//!    the bits their processor feature banks derive. `ITS_NO` is pinned
//!    clear because the Hyper-V backends cannot present it.
//! 6. The bits OpenVMM sets per VM are zero and unmasked.

use crate::Hex32;
use crate::Hex64;
use crate::cpuid;
use crate::cpuid::EXTENDED_LEAF_BASE;
use crate::cpuid::XsaveComponent;
use crate::fingerprint::CpuFingerprint;
use crate::fingerprint::IA32_ARCH_CAPABILITIES;
use crate::hv_banks;
use crate::profile::CpuProfile;
use crate::profile::CpuidLeafValue;
use crate::profile::Generation;
use crate::profile::GenerationCpu;
use crate::profile::PinnedMsr;
use crate::profile::Provenance;
use crate::profile::ProvenanceSource;
use crate::profile::VM_OWNED_LEAVES;
use crate::profile::describe_leaf;
use crate::profile::pinned_mask;
use crate::signature::HostCpuSignature;
use crate::surface::RegisterClass;
use crate::surface::field_value;
use crate::surface::register_class;
use std::collections::BTreeMap;
use std::collections::BTreeSet;
use thiserror::Error;

/// The version of the derivation policy, recorded in each profile's
/// provenance.
pub const POLICY: &str = "openvmm-cpu-profile-policy/v1";

/// The leaves that keep data; every other leaf in range is zero.
pub const DATA_LEAVES: [u32; 16] = [
    0x0,
    0x1,
    0x2,
    0x4,
    0x6,
    0x7,
    0xd,
    0x18,
    EXTENDED_LEAF_BASE,
    EXTENDED_LEAF_BASE + 1,
    EXTENDED_LEAF_BASE + 2,
    EXTENDED_LEAF_BASE + 3,
    EXTENDED_LEAF_BASE + 4,
    EXTENDED_LEAF_BASE + 6,
    EXTENDED_LEAF_BASE + 7,
    EXTENDED_LEAF_BASE + 8,
];

/// The XCR0 bits a profile may enable: x87, SSE, AVX, MPX, AVX-512, and
/// PKRU. AMX needs dynamic XSAVE permissions that no backend grants yet.
pub const ALLOWED_XCR0: u64 = 0x2ff;

/// The IA32_XSS bits a profile may enable: none yet.
pub const ALLOWED_XSS: u64 = 0;

/// The XSAVE extensions of `CPUID.(0xd,1):EAX` a profile may enable:
/// XSAVEOPT, XSAVEC, XGETBV with ECX=1, and XSAVES. XFD only matters with
/// AMX.
pub const ALLOWED_XSAVE_EXTENSIONS: u32 = 0xf;

/// `IA32_ARCH_CAPABILITIES.ITS_NO`.
pub const ARCH_CAPABILITIES_ITS_NO: u64 = 1 << 62;

/// The `IA32_ARCH_CAPABILITIES` bits that profiles pin: those the Hyper-V
/// processor feature banks derive, and `ITS_NO`.
pub const ARCH_CAPABILITIES_PINNED_MASK: u64 =
    hv_banks::ARCH_CAPABILITIES_BANK_MASK | ARCH_CAPABILITIES_ITS_NO;

/// A CPU generation that profiles are derived for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct KnownGeneration {
    /// The CPUID vendor string.
    pub vendor: &'static str,
    /// The vendor as profile IDs spell it.
    pub vendor_short: &'static str,
    /// The generation name, as logs and reports spell it.
    pub name: &'static str,
    /// The CPUs of the generation: family, model, and inclusive stepping
    /// range.
    pub cpus: &'static [(u32, u32, [u32; 2])],
    /// A human-readable name.
    pub description: &'static str,
    /// The brand string of every profile of the generation: generic, with
    /// the generation's display name and without a SKU or frequency.
    pub brand: &'static str,
}

impl KnownGeneration {
    /// Returns the generation as profiles record it.
    pub(crate) fn generation(&self) -> Generation {
        Generation {
            name: self.name.to_owned(),
            cpus: self
                .cpus
                .iter()
                .map(|&(family, model, steppings)| GenerationCpu {
                    family,
                    model,
                    steppings,
                })
                .collect(),
        }
    }
}

/// The generations profiles exist for.
pub const KNOWN_GENERATIONS: [KnownGeneration; 3] = [
    KnownGeneration {
        vendor: "GenuineIntel",
        vendor_short: "intel",
        name: "skylake-sp",
        // Steppings 5 to 7 are Cascade Lake, and 10 and 11 Cooper Lake.
        cpus: &[(6, 85, [0, 4])],
        description: "Intel Xeon Scalable, first generation (Skylake-SP)",
        brand: "Intel(R) Xeon(R) Processor (Skylake-SP)",
    },
    KnownGeneration {
        vendor: "GenuineIntel",
        vendor_short: "intel",
        name: "icelake-sp",
        cpus: &[(6, 106, [0, 15])],
        description: "Intel Xeon Scalable, third generation (Ice Lake-SP)",
        brand: "Intel(R) Xeon(R) Processor (Ice Lake-SP)",
    },
    KnownGeneration {
        vendor: "GenuineIntel",
        vendor_short: "intel",
        name: "emeraldrapids",
        cpus: &[(6, 207, [0, 15])],
        description: "Intel Xeon Scalable, fifth generation (Emerald Rapids)",
        brand: "Intel(R) Xeon(R) Processor (Emerald Rapids)",
    },
];

/// Returns the known generation named `name`.
pub fn known_generation(name: &str) -> Option<&'static KnownGeneration> {
    KNOWN_GENERATIONS
        .iter()
        .find(|generation| generation.name == name)
}

/// A failure to derive a profile.
#[derive(Debug, Error)]
pub enum DeriveError {
    /// No fingerprint was given.
    #[error("no fingerprints")]
    NoFingerprints,
    /// A fingerprint's host is not in the generation.
    #[error("the {backend} fingerprint {digest} is of {host}, not generation {generation}")]
    WrongGeneration {
        /// The fingerprint's backend.
        backend: String,
        /// The fingerprint's digest.
        digest: String,
        /// The host CPU.
        host: String,
        /// The generation.
        generation: String,
    },
    /// The values of a descriptive register tie.
    #[error("no majority for {what}: {values}")]
    Tie {
        /// The register.
        what: String,
        /// The tied values and their votes.
        values: String,
    },
    /// The hosts disagree on the layout of an XSAVE component.
    #[error("the hosts disagree on the layout of XSAVE component {0}")]
    XsaveLayout(u32),
    /// A feature the time ABI requires is missing.
    #[error("not every backend supports {0}, which the time ABI requires")]
    TimeFeature(&'static str),
    /// The derived profile is invalid.
    #[error("the derived profile is invalid: {0}")]
    Invalid(String),
}

/// A CPUID table: the four registers of each leaf and subleaf.
type Table = BTreeMap<(u32, Option<u32>), [u32; 4]>;

/// Derives revision `revision` of the profile of `generation` from the
/// `fingerprints` of its hosts, on any backends.
pub fn derive_profile(
    generation: &KnownGeneration,
    revision: u32,
    fingerprints: &[CpuFingerprint],
) -> Result<CpuProfile, DeriveError> {
    if fingerprints.is_empty() {
        return Err(DeriveError::NoFingerprints);
    }
    let profile_generation = generation.generation();
    for fingerprint in fingerprints {
        let cpu = &fingerprint.host.cpu;
        let mut vendor = [0; 12];
        let host_vendor = cpu.vendor.as_bytes();
        if host_vendor.len() == vendor.len() {
            vendor.copy_from_slice(host_vendor);
        }
        let host = HostCpuSignature::new(vendor, cpu.signature.0);
        if !profile_generation.contains(generation.vendor, &host) {
            return Err(DeriveError::WrongGeneration {
                backend: fingerprint.backend.name.clone(),
                digest: fingerprint.digest.clone(),
                host: host.to_string(),
                generation: generation.name.to_owned(),
            });
        }
    }

    let mut backends = BTreeMap::<&str, Vec<&CpuFingerprint>>::new();
    for fingerprint in fingerprints {
        backends
            .entry(&fingerprint.backend.name)
            .or_default()
            .push(fingerprint);
    }
    let backend_tables = backends
        .iter()
        .map(|(backend, members)| {
            let tables = members
                .iter()
                .map(|member| table(&member.backend.cpuid))
                .collect::<Vec<_>>();
            combine(&tables, &format!("{backend} hosts"))
        })
        .collect::<Result<Vec<_>, _>>()?;
    let combined = combine(&backend_tables, "backends")?;

    let mut leaves = apply_policy(&combined)?;

    // The brand string: generic per generation.
    for (leaf, value) in (EXTENDED_LEAF_BASE + 2..).zip(brand_leaves(generation.brand)) {
        if let Some(entry) = leaves.get_mut(&(leaf, None)) {
            *entry = value;
        }
    }

    // XSAVE.
    let xcr0 = fingerprints.iter().fold(ALLOWED_XCR0, |xcr0, fingerprint| {
        xcr0 & fingerprint.backend.xsave.xcr0_supported.0
    });
    let xss = fingerprints.iter().fold(ALLOWED_XSS, |xss, fingerprint| {
        xss & fingerprint.backend.xsave.xss_supported.0
    });
    let mut components = Vec::<XsaveComponent>::new();
    for index in (2..63).filter(|index| (xcr0 | xss) & (1 << index) != 0) {
        let mut layouts = fingerprints.iter().map(|fingerprint| {
            fingerprint
                .backend
                .xsave
                .components
                .iter()
                .find(|component| component.index == index)
        });
        let first = layouts.next().flatten();
        match first {
            Some(component) if layouts.all(|layout| layout == Some(component)) => {
                components.push(component.clone());
            }
            _ => return Err(DeriveError::XsaveLayout(index)),
        }
    }
    let extensions = fingerprints
        .iter()
        .fold(ALLOWED_XSAVE_EXTENSIONS, |extensions, fingerprint| {
            extensions & fingerprint.backend.xsave.extensions.0
        });
    leaves.retain(|&(leaf, _), _| leaf != 0xd);
    leaves.insert(
        (0xd, Some(0)),
        [
            xcr0 as u32,
            0,
            cpuid::xsave_standard_size(&components, xcr0),
            (xcr0 >> 32) as u32,
        ],
    );
    leaves.insert(
        (0xd, Some(1)),
        [extensions, 0, xss as u32, (xss >> 32) as u32],
    );
    for component in &components {
        let flags = u32::from(component.supervisor)
            | u32::from(component.align64) << 1
            | u32::from(component.xfd) << 2;
        leaves.insert(
            (0xd, Some(component.index)),
            [component.size, component.offset, flags, 0],
        );
    }
    clear_stateless_features(&mut leaves, xcr0);

    // IA32_ARCH_CAPABILITIES.
    let mut msrs = Vec::new();
    if leaves.get(&(0x7, Some(0))).copied().unwrap_or_default()[3] & 1 << 29 != 0 {
        let value = fingerprints
            .iter()
            .map(|fingerprint| {
                let backend = &fingerprint.backend;
                match backend.msrs.arch_capabilities {
                    Some(value) => value.0,
                    None => hv_banks::fingerprint_banks(backend)
                        .map_or(0, hv_banks::arch_capabilities_from_banks),
                }
            })
            .fold(ARCH_CAPABILITIES_PINNED_MASK, |value, presentable| {
                value & presentable
            })
            & !ARCH_CAPABILITIES_ITS_NO;
        msrs.push(PinnedMsr {
            index: Hex32(IA32_ARCH_CAPABILITIES),
            value: Hex64(value),
            mask: Hex64(ARCH_CAPABILITIES_PINNED_MASK),
        });
    }

    // Unmask the bits OpenVMM sets per VM.
    let leaves = leaves
        .into_iter()
        .map(|((leaf, subleaf), value)| {
            let mask = pinned_mask(leaf, subleaf, value);
            let value = [0, 1, 2, 3].map(|register| value[register] & mask[register]);
            CpuidLeafValue::new(leaf, subleaf, value, mask)
        })
        .collect::<Vec<_>>();
    let physical_address_width = combined
        .get(&(EXTENDED_LEAF_BASE + 8, None))
        .copied()
        .unwrap_or_default()[0] as u8;

    let provenance = Provenance {
        method: format!(
            "derived by cpu_profile::derive under {POLICY}: per-register consensus within each \
             backend, then across backends, of the hosts' supported CPU surfaces"
        ),
        sources: backends
            .iter()
            .map(|(backend, members)| ProvenanceSource {
                backend: (*backend).to_owned(),
                hosts: members.len() as u32,
                surface_digests: members
                    .iter()
                    .map(|member| member.surface_digest.clone())
                    .collect::<BTreeSet<_>>()
                    .into_iter()
                    .collect(),
            })
            .collect(),
    };
    CpuProfile::from_parts(
        format!(
            "{}.{}.v{revision}",
            generation.vendor_short, generation.name
        ),
        format!(
            "{}, on every backend; NVX time ABI v1",
            generation.description
        ),
        generation.vendor.to_owned(),
        profile_generation,
        leaves,
        xcr0,
        xss,
        physical_address_width,
        msrs,
        provenance,
    )
    .map_err(DeriveError::Invalid)
}

/// Returns the table of a fingerprint's CPUID for combining: without the
/// hypervisor range, the XSAVE leaf (rebuilt from the fingerprints' XSAVE
/// information), and the brand string (generic per generation), and with
/// the bits OpenVMM sets per VM cleared, so hosts are compared only on what a
/// profile pins.
fn table(entries: &[cpuid::CpuidEntry]) -> Table {
    entries
        .iter()
        .filter(|entry| {
            let leaf = entry.leaf.0;
            !(cpuid::HYPERVISOR_LEAF_BASE..EXTENDED_LEAF_BASE).contains(&leaf)
                && leaf != 0xd
                && !(EXTENDED_LEAF_BASE + 2..=EXTENDED_LEAF_BASE + 4).contains(&leaf)
        })
        .map(|entry| {
            let (leaf, subleaf) = entry.key();
            let value = entry.registers();
            let mask = pinned_mask(leaf, subleaf, value);
            (
                (leaf, subleaf),
                [0, 1, 2, 3].map(|register| value[register] & mask[register]),
            )
        })
        .collect()
}

/// Combines `tables` register by register; see the module documentation.
fn combine(tables: &[Table], voters: &str) -> Result<Table, DeriveError> {
    let keys = tables
        .iter()
        .flat_map(|table| table.keys().copied())
        .collect::<BTreeSet<_>>();
    let mut combined = Table::new();
    for (leaf, subleaf) in keys {
        let mut registers = [0; 4];
        for (register, out) in registers.iter_mut().enumerate() {
            let values = tables
                .iter()
                .map(|table| table.get(&(leaf, subleaf)).copied().unwrap_or_default()[register])
                .collect::<Vec<_>>();
            *out = match register_class(leaf, subleaf.unwrap_or(0), register) {
                RegisterClass::Features => values.iter().fold(!0, |bits, value| bits & value),
                RegisterClass::Limits(fields) => fields.iter().fold(0, |out, &(shift, width)| {
                    let minimum = values
                        .iter()
                        .map(|&value| field_value(value, shift, width))
                        .min()
                        .unwrap_or(0);
                    out | minimum << shift
                }),
                RegisterClass::Informational => majority(&values).ok_or_else(|| {
                    let mut votes = BTreeMap::<u32, usize>::new();
                    for &value in &values {
                        *votes.entry(value).or_default() += 1;
                    }
                    DeriveError::Tie {
                        what: format!(
                            "{} {} among the {voters}",
                            describe_leaf(leaf, subleaf),
                            ["EAX", "EBX", "ECX", "EDX"][register]
                        ),
                        values: votes
                            .iter()
                            .map(|(value, count)| format!("{value:#x} ({count})"))
                            .collect::<Vec<_>>()
                            .join(", "),
                    }
                })?,
            };
        }
        combined.insert((leaf, subleaf), registers);
    }
    Ok(combined)
}

/// Returns the value with the most votes, or `None` on a tie.
fn majority(values: &[u32]) -> Option<u32> {
    let mut votes = BTreeMap::<u32, usize>::new();
    for &value in values {
        *votes.entry(value).or_default() += 1;
    }
    let best = votes.values().copied().max()?;
    let mut winners = votes.iter().filter(|&(_, &count)| count == best);
    let (&winner, _) = winners.next()?;
    winners.next().is_none().then_some(winner)
}

/// Lays out the profile's leaves from the combined table: the dense leaf and
/// subleaf set, the [`DATA_LEAVES`] allowlist, the CPU time bits, and the
/// features a VM must not see. The XSAVE leaf and the brand are filled in
/// later.
fn apply_policy(combined: &Table) -> Result<Table, DeriveError> {
    let get = |leaf: u32, subleaf: Option<u32>| {
        combined.get(&(leaf, subleaf)).copied().unwrap_or_default()
    };
    let max_basic = get(0, None)[0];
    let max_extended = get(EXTENDED_LEAF_BASE, None)[0];
    let mut leaves = Table::new();
    let all_leaves = (0..=max_basic).chain(EXTENDED_LEAF_BASE..=max_extended);
    for leaf in all_leaves.filter(|leaf| !VM_OWNED_LEAVES.contains(leaf)) {
        let data = DATA_LEAVES.contains(&leaf);
        if !cpuid::is_indexed_leaf(leaf) {
            leaves.insert((leaf, None), if data { get(leaf, None) } else { [0; 4] });
            continue;
        }
        let last = match leaf {
            _ if !data => 0,
            0x4 => (0..0x40)
                .find(|&subleaf| get(leaf, Some(subleaf))[0] & 0x1f == 0)
                .unwrap_or(0x3f),
            // Rebuilt from the XSAVE features later.
            0xd => continue,
            _ => get(leaf, Some(0))[0].min(0x3f),
        };
        for subleaf in 0..=last {
            let value = if data {
                get(leaf, Some(subleaf))
            } else {
                [0; 4]
            };
            leaves.insert((leaf, Some(subleaf)), value);
        }
    }

    let edit =
        |leaves: &mut Table, key: (u32, Option<u32>), register: usize, set: u32, clear: u32| {
            if let Some(value) = leaves.get_mut(&key) {
                value[register] = (value[register] | set) & !clear;
            }
        };
    let require =
        |leaves: &Table, key: (u32, Option<u32>), register: usize, bit: u32, what: &'static str| {
            if leaves
                .get(&key)
                .is_none_or(|value| value[register] & bit == 0)
            {
                Err(DeriveError::TimeFeature(what))
            } else {
                Ok(())
            }
        };
    // CPUID.1: the hypervisor bit (time ABI); no VMX, SMX, EST, TM2, DTES64,
    // MONITOR, DS-CPL, SDBG, xTPR, PDCM (time ABI, no PMU), DCA, or
    // TSC-deadline (time ABI); no DS, ACPI, TM, or PBE.
    edit(
        &mut leaves,
        (0x1, None),
        2,
        1 << 31,
        1 << 2
            | 1 << 3
            | 1 << 4
            | 1 << 5
            | 1 << 6
            | 1 << 7
            | 1 << 8
            | 1 << 11
            | 1 << 14
            | 1 << 15
            | 1 << 18
            | 1 << 24,
    );
    edit(
        &mut leaves,
        (0x1, None),
        3,
        0,
        1 << 21 | 1 << 22 | 1 << 29 | 1 << 31,
    );
    require(&leaves, (0x1, None), 3, 1 << 4, "the TSC")?;
    // CPUID.6: ARAT only (time ABI).
    leaves.insert((0x6, None), [1 << 2, 0, 0, 0]);
    // CPUID.(7,0): no TSC_ADJUST (time ABI), SGX, RDT, or Intel PT; no CET,
    // key locker, or SGX launch control; no hybrid, PCONFIG, or arch LBR.
    edit(
        &mut leaves,
        (0x7, Some(0)),
        1,
        0,
        1 << 1 | 1 << 2 | 1 << 12 | 1 << 15 | 1 << 25,
    );
    edit(
        &mut leaves,
        (0x7, Some(0)),
        2,
        0,
        1 << 7 | 1 << 23 | 1 << 30,
    );
    edit(
        &mut leaves,
        (0x7, Some(0)),
        3,
        0,
        1 << 15 | 1 << 18 | 1 << 19 | 1 << 20,
    );
    // CPUID.0x80000001: RDTSCP (time ABI).
    require(
        &leaves,
        (EXTENDED_LEAF_BASE + 1, None),
        3,
        1 << 27,
        "RDTSCP",
    )?;
    // CPUID.0x80000007: the invariant TSC only (time ABI).
    leaves.insert((EXTENDED_LEAF_BASE + 7, None), [0, 0, 0, 1 << 8]);
    // CPUID.0x80000008: the address widths only; no GuestPhysBits, which only
    // newer KVM reports, and no AMD core count.
    edit(
        &mut leaves,
        (EXTENDED_LEAF_BASE + 8, None),
        0,
        0,
        0xffff_0000,
    );
    edit(&mut leaves, (EXTENDED_LEAF_BASE + 8, None), 2, 0, !0);
    edit(&mut leaves, (EXTENDED_LEAF_BASE + 8, None), 3, 0, !0);
    Ok(leaves)
}

/// Clears the features whose XSAVE state `xcr0` does not enable.
fn clear_stateless_features(leaves: &mut Table, xcr0: u64) {
    const AVX: u64 = 1 << 2;
    const MPX: u64 = 3 << 3;
    const AVX512: u64 = 7 << 5;
    const PKRU: u64 = 1 << 9;
    let enabled = |state: u64| xcr0 & state == state;
    if let Some(value) = leaves.get_mut(&(0x1, None)) {
        if !enabled(AVX) {
            // AVX, FMA, and F16C.
            value[2] &= !(1 << 28 | 1 << 12 | 1 << 29);
        }
    }
    if let Some(value) = leaves.get_mut(&(0x7, Some(0))) {
        if !enabled(AVX) {
            // AVX2, VAES, and VPCLMULQDQ.
            value[1] &= !(1 << 5);
            value[2] &= !(1 << 9 | 1 << 10);
        }
        if !enabled(MPX) {
            value[1] &= !(1 << 14);
        }
        if !enabled(AVX512) {
            // AVX512F, DQ, IFMA, PF, ER, CD, BW, and VL; VBMI, VBMI2, VNNI,
            // BITALG, and VPOPCNTDQ; 4VNNIW, 4FMAPS, VP2INTERSECT, and FP16.
            value[1] &=
                !(1 << 16 | 1 << 17 | 1 << 21 | 1 << 26 | 1 << 27 | 1 << 28 | 1 << 30 | 1 << 31);
            value[2] &= !(1 << 1 | 1 << 6 | 1 << 11 | 1 << 12 | 1 << 14);
            value[3] &= !(1 << 2 | 1 << 3 | 1 << 8 | 1 << 23);
        }
        if !enabled(PKRU) {
            // PKU.
            value[2] &= !(1 << 3);
        }
    }
    if let Some(value) = leaves.get_mut(&(0x7, Some(1))) {
        if !enabled(AVX) {
            // AVX-VNNI.
            value[0] &= !(1 << 4);
        }
        if !enabled(AVX512) {
            // AVX512-BF16.
            value[0] &= !(1 << 5);
        }
    }
}

/// Returns the CPUID brand string leaves `0x80000002..=0x80000004` for
/// `brand`, padded with zeros.
fn brand_leaves(brand: &str) -> [[u32; 4]; 3] {
    let mut bytes = [0u8; 48];
    let brand = brand.as_bytes();
    let len = brand.len().min(47);
    bytes[..len].copy_from_slice(&brand[..len]);
    let mut leaves = [[0; 4]; 3];
    for (i, word) in bytes.as_chunks::<4>().0.iter().enumerate() {
        leaves[i / 4][i % 4] = u32::from_le_bytes(*word);
    }
    leaves
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cpuid::CpuidEntry;
    use crate::test_support::fingerprint;
    use crate::test_support::fingerprint_with;
    use crate::test_support::profile;
    use crate::test_support::profile_entries;
    use test_with_tracing::test;

    const ICELAKE: &str = "intel.icelake-sp.v1";

    fn generation(name: &str) -> &'static KnownGeneration {
        known_generation(name).unwrap()
    }

    /// Returns the fingerprint of a `backend` host of `profile`'s CPU whose
    /// CPUID is the profile's, changed by `edit`.
    fn edited(
        profile: &CpuProfile,
        backend: &str,
        edit: impl FnOnce(&mut Vec<CpuidEntry>),
    ) -> CpuFingerprint {
        let mut entries = profile_entries(profile);
        edit(&mut entries);
        fingerprint_with(profile, backend, entries)
    }

    /// Changes register `register` of `leaf` and `subleaf`.
    fn set(
        entries: &mut [CpuidEntry],
        leaf: u32,
        subleaf: Option<u32>,
        register: usize,
        f: impl FnOnce(u32) -> u32,
    ) {
        let entry = entries
            .iter_mut()
            .find(|entry| entry.key() == (leaf, subleaf))
            .unwrap();
        let mut registers = entry.registers();
        registers[register] = f(registers[register]);
        *entry = CpuidEntry::new(leaf, subleaf, registers);
    }

    #[test]
    fn rederiving_a_pinned_profile_reproduces_it() {
        for pinned in crate::pinned_profiles() {
            let known = generation(&pinned.generation().name);
            let derived = derive_profile(
                known,
                1,
                &[fingerprint(pinned, "kvm"), fingerprint(pinned, "whp")],
            )
            .unwrap();
            assert_eq!(derived.id(), pinned.id());
            assert_eq!(derived.description(), pinned.description());
            assert_eq!(derived.generation(), pinned.generation());
            assert_eq!(derived.cpuid(), pinned.cpuid());
            assert_eq!(derived.xcr0(), pinned.xcr0());
            assert_eq!(derived.xss(), pinned.xss());
            assert_eq!(derived.xsave_components(), pinned.xsave_components());
            assert_eq!(
                derived.physical_address_width(),
                pinned.physical_address_width()
            );
            assert_eq!(derived.msrs(), pinned.msrs());
            assert_eq!(
                derived
                    .provenance()
                    .sources
                    .iter()
                    .map(|source| (source.backend.as_str(), source.hosts))
                    .collect::<Vec<_>>(),
                [("kvm", 1), ("whp", 1)]
            );
        }
    }

    #[test]
    fn votes_only_on_pinned_bits() {
        // KVM reports the host's topology fields and no brand string; two
        // backends must still agree.
        let pinned = profile(ICELAKE);
        let kvm = edited(pinned, "kvm", |entries| {
            set(entries, 1, None, 1, |ebx| ebx | 8 << 16);
            set(entries, 4, Some(0), 0, |eax| eax | 0xc000_0000);
            for leaf in 0x8000_0002..=0x8000_0004 {
                set(entries, leaf, None, 0, |_| 0);
            }
        });
        let derived = derive_profile(
            generation("icelake-sp"),
            1,
            &[kvm, fingerprint(pinned, "whp")],
        )
        .unwrap();
        assert_eq!(derived.cpuid(), pinned.cpuid());
    }

    /// Returns the brand string of `profile`.
    fn brand(profile: &CpuProfile) -> String {
        let bytes = (0x8000_0002..=0x8000_0004)
            .flat_map(|leaf| profile.lookup(leaf, 0))
            .flat_map(u32::to_le_bytes)
            .collect::<Vec<_>>();
        let end = bytes.iter().position(|&byte| byte == 0).unwrap();
        String::from_utf8(bytes[..end].to_vec()).unwrap()
    }

    #[test]
    fn the_brand_is_generic_per_generation() {
        for known in &KNOWN_GENERATIONS {
            // The brand string holds 47 bytes and a terminator.
            assert!(known.brand.len() < 48, "{}", known.brand);
        }
        for pinned in crate::pinned_profiles() {
            assert_eq!(brand(pinned), generation(&pinned.generation().name).brand);
        }

        // A host of another SKU derives the same brand.
        let pinned = profile(ICELAKE);
        let mut other_sku = edited(pinned, "whp", |entries| {
            for leaf in 0x8000_0002..=0x8000_0004 {
                set(entries, leaf, None, 0, |eax| eax ^ 0x0101_0101);
            }
        });
        other_sku.host.cpu.brand = "Intel(R) Xeon(R) Gold 6338 CPU @ 2.00GHz".to_owned();
        let derived = derive_profile(
            generation("icelake-sp"),
            1,
            &[fingerprint(pinned, "kvm"), other_sku],
        )
        .unwrap();
        assert_eq!(brand(&derived), "Intel(R) Xeon(R) Processor (Ice Lake-SP)");
        assert_eq!(derived.cpuid(), pinned.cpuid());
    }

    #[test]
    fn intersects_features_and_votes_on_descriptions() {
        let pinned = profile(ICELAKE);
        let fingerprints = [
            // PKU, and another TLB descriptor.
            edited(pinned, "kvm", |entries| {
                set(entries, 7, Some(0), 2, |ecx| ecx | 1 << 3);
                set(entries, 2, None, 1, |ebx| ebx + 1);
            }),
            // No AVX512-VBMI.
            edited(pinned, "mshv", |entries| {
                set(entries, 7, Some(0), 2, |ecx| ecx & !(1 << 1));
            }),
            fingerprint(pinned, "whp"),
        ];
        let derived = derive_profile(generation("icelake-sp"), 2, &fingerprints).unwrap();
        assert_eq!(derived.id(), "intel.icelake-sp.v2");
        assert_eq!(derived.lookup(7, 0)[2], pinned.lookup(7, 0)[2] & !(1 << 1));
        assert_eq!(derived.lookup(2, 0), pinned.lookup(2, 0));

        // Two backends that disagree have no majority.
        let tie = derive_profile(generation("icelake-sp"), 2, &fingerprints[..2]).unwrap_err();
        assert!(
            matches!(&tie, DeriveError::Tie { what, .. } if what == "CPUID 0x2 EBX among the backends"),
            "{tie}"
        );
    }

    #[test]
    fn pins_the_arch_capabilities_every_backend_can_present() {
        let pinned = profile(ICELAKE);
        let mut kvm = fingerprint(pinned, "kvm");
        kvm.backend.msrs.arch_capabilities = Some(Hex64(0x4000_0000_0c00_016d));
        let mut whp = fingerprint(pinned, "whp");
        whp.backend.msrs.arch_capabilities = None;
        whp.backend.set_feature_bank(
            "whp.capability.ProcessorFeaturesBanks.bank0",
            0x2e0a_8bff_e7f7_859f,
        );
        whp.backend.set_feature_bank(
            "whp.capability.ProcessorFeaturesBanks.bank1",
            0x0001_000e_0000_00f1,
        );
        let derived = derive_profile(generation("icelake-sp"), 1, &[kvm.clone(), whp]).unwrap();
        assert_eq!(
            derived.msr(IA32_ARCH_CAPABILITIES),
            Some((0x0800_0121, ARCH_CAPABILITIES_PINNED_MASK))
        );
        // ITS_NO stays clear even where every backend presents it.
        let derived = derive_profile(generation("icelake-sp"), 1, &[kvm]).unwrap();
        assert_eq!(
            derived.msr(IA32_ARCH_CAPABILITIES),
            Some((0x0c00_0129, ARCH_CAPABILITIES_PINNED_MASK))
        );
    }

    #[test]
    fn clears_features_whose_xsave_state_is_disabled() {
        let pinned = profile(ICELAKE);
        // No AVX-512 state.
        let fingerprint = edited(pinned, "kvm", |entries| {
            set(entries, 0xd, Some(0), 0, |eax| eax & !0xe0);
            entries.retain(|entry| !matches!(entry.key(), (0xd, Some(5..=7))));
        });
        let derived = derive_profile(generation("icelake-sp"), 1, &[fingerprint]).unwrap();
        assert_eq!(derived.xcr0(), 0x7);
        assert_eq!(derived.lookup(7, 0)[1] & (1 << 16 | 1 << 31), 0);
        assert_eq!(derived.lookup(7, 0)[2] & (1 << 1 | 1 << 14), 0);
        assert_eq!(derived.lookup(0xd, 0)[2], 832);
        assert_eq!(derived.xsave_components().len(), 1);
    }

    #[test]
    fn rejects_inconsistent_inputs() {
        let pinned = profile(ICELAKE);
        assert!(matches!(
            derive_profile(generation("icelake-sp"), 1, &[]),
            Err(DeriveError::NoFingerprints)
        ));
        assert!(matches!(
            derive_profile(generation("skylake-sp"), 1, &[fingerprint(pinned, "kvm")]),
            Err(DeriveError::WrongGeneration { .. })
        ));

        let moved = edited(pinned, "whp", |entries| {
            set(entries, 0xd, Some(7), 1, |offset| offset + 64);
        });
        assert!(matches!(
            derive_profile(
                generation("icelake-sp"),
                1,
                &[fingerprint(pinned, "kvm"), moved]
            ),
            Err(DeriveError::XsaveLayout(7))
        ));

        let no_rdtscp = edited(pinned, "whp", |entries| {
            set(entries, 0x8000_0001, None, 3, |edx| edx & !(1 << 27));
        });
        assert!(matches!(
            derive_profile(generation("icelake-sp"), 1, &[no_rdtscp]),
            Err(DeriveError::TimeFeature("RDTSCP"))
        ));
    }
}
