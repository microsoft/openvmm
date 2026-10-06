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
//! 2. Only the leaves of [`DATA_LEAVES`] keep data, and in AMD profiles also
//!    those of [`AMD_DATA_LEAVES`], AMD's cache, topology, and speculation
//!    leaves; every other leaf in range is zero. The time ABI's CPU time bits
//!    are applied, and features a VM must not see (virtualization, power
//!    management, performance monitoring, debug, SGX, RDT, PT, CET, key
//!    locker) are cleared, in AMD profiles also where AMD enumerates them
//!    (SVM, IBS, LWP, the performance counter extensions, MONITORX, RDPRU,
//!    PPIN, CPPC, and the like). AMD's speculation controls and immunities
//!    stay, those of `0x80000021` included, but for what KVM cannot present:
//!    `BTC_NO` goes, and PSFD stays only beside a `SPEC_CTRL` control
//!    ([`has_spec_ctrl_control`]).
//! 3. The XSAVE features are the intersection, within [`ALLOWED_XCR0`], and
//!    the XSAVE leaf is rebuilt from them; every host must agree on the
//!    layout. A feature whose XSAVE state is not enabled is cleared.
//! 4. The brand string is generic per generation, such as
//!    `Intel(R) Xeon(R) Processor (<display name>)`,
//!    `Intel(R) Core(TM) Processor (<display name>)`, or
//!    `AMD EPYC Processor (<display name>)`, without a frequency
//!    ([`KnownGeneration::brand`]), so every host of a generation presents
//!    it whatever its SKU, and the hosts' brands are not compared.
//! 5. `IA32_ARCH_CAPABILITIES` is pinned in [`ARCH_CAPABILITIES_PINNED_MASK`]
//!    to what every backend can present: KVM's value, and for MSHV and WHP
//!    the bits their processor feature banks derive. `ITS_NO` is pinned
//!    clear because the Hyper-V backends cannot present it.
//! 6. The bits OpenVMM sets per VM ([`vm_owned_bits`](crate::vm_owned_bits))
//!    are zero and unmasked.
//!
//! The AMD rules apply only to AMD profiles, so the policy derives every
//! Intel profile as it did before AMD profiles existed.
//!
//! [`derive_host_profile`] applies the same policy to one fingerprint of the
//! host that OpenVMM runs on, for `--cpu-profile host`: a development profile
//! that serves only that host's CPU model and stepping, and that no catalog
//! pins.

use crate::Hex32;
use crate::Hex64;
use crate::cpuid;
use crate::cpuid::EXTENDED_LEAF_BASE;
use crate::cpuid::XsaveComponent;
use crate::error::ProfileError;
use crate::error::ProfileErrorCode;
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
use crate::signature::decode_signature;
use crate::surface::RegisterClass;
use crate::surface::field_value;
use crate::surface::register_class;
use crate::vendor::CpuVendor;
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

/// The leaves that also keep data in AMD profiles: the L1 cache and TLB leaf
/// `0x80000005`, the cache topology leaf `0x8000001D`, the processor topology
/// leaf `0x8000001E`, whose fields OpenVMM sets per VM, and `0x80000021`,
/// which enumerates AutoIBRS, `LFENCE` serialization, SBPB, `SRSO_NO`, and
/// the TSA immunities. Intel CPUs leave them zero or out of range.
pub const AMD_DATA_LEAVES: [u32; 4] = [
    EXTENDED_LEAF_BASE + 5,
    EXTENDED_LEAF_BASE + 0x1d,
    EXTENDED_LEAF_BASE + 0x1e,
    EXTENDED_LEAF_BASE + 0x21,
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

/// `CPUID.0x80000008:EBX[28]`, PSFD: predictive store forwarding can be
/// disabled, through `SPEC_CTRL`.
pub const AMD_PSFD: u32 = 1 << 28;

/// `CPUID.0x80000008:EBX[29]`, `BTC_NO`: the CPU is not affected by branch
/// type confusion.
pub const AMD_BTC_NO: u32 = 1 << 29;

/// Returns whether a CPUID whose `CPUID.(7,0):EDX` is `leaf7_edx` and whose
/// `CPUID.0x80000008:EBX` is `ext8_ebx` enumerates a `SPEC_CTRL` control, as
/// KVM's `guest_has_spec_ctrl_msr()` decides whether a guest may access
/// `SPEC_CTRL`: Intel's IBRS (`EDX[26]`), or AMD's IBRS, STIBP, or SSBD
/// (`EBX` bits 14, 15, and 24). AMD profiles keep [`AMD_PSFD`] only where it
/// holds, and OpenVMM's KVM backend applies it to KVM's supported CPUID.
pub fn has_spec_ctrl_control(leaf7_edx: u32, ext8_ebx: u32) -> bool {
    leaf7_edx & 1 << 26 != 0 || ext8_ebx & (1 << 14 | 1 << 15 | 1 << 24) != 0
}

/// A CPU generation that profiles are derived for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct KnownGeneration {
    /// The vendor of the generation's CPUs.
    pub vendor: CpuVendor,
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
pub const KNOWN_GENERATIONS: [KnownGeneration; 5] = [
    KnownGeneration {
        vendor: CpuVendor::Intel,
        name: "skylake-sp",
        // Steppings 5 to 7 are Cascade Lake, and 10 and 11 Cooper Lake.
        cpus: &[(6, 85, [0, 4])],
        description: "Intel Xeon Scalable, first generation (Skylake-SP)",
        brand: "Intel(R) Xeon(R) Processor (Skylake-SP)",
    },
    KnownGeneration {
        vendor: CpuVendor::Intel,
        name: "icelake-sp",
        cpus: &[(6, 106, [0, 15])],
        description: "Intel Xeon Scalable, third generation (Ice Lake-SP)",
        brand: "Intel(R) Xeon(R) Processor (Ice Lake-SP)",
    },
    KnownGeneration {
        vendor: CpuVendor::Intel,
        name: "emeraldrapids",
        cpus: &[(6, 207, [0, 15])],
        description: "Intel Xeon Scalable, fifth generation (Emerald Rapids)",
        brand: "Intel(R) Xeon(R) Processor (Emerald Rapids)",
    },
    KnownGeneration {
        vendor: CpuVendor::Intel,
        name: "alderlake",
        // Alder Lake-S (151) and Alder Lake-P and -H (154).
        cpus: &[(6, 151, [0, 15]), (6, 154, [0, 15])],
        description: "Intel Core, twelfth generation (Alder Lake)",
        brand: "Intel(R) Core(TM) Processor (Alder Lake)",
    },
    KnownGeneration {
        vendor: CpuVendor::Amd,
        name: "milan",
        // Every stepping of the Milan die, Milan-X's included (stepping 2):
        // no other generation uses family 25 model 1. Genoa is model 17.
        cpus: &[(25, 1, [0, 15])],
        description: "AMD EPYC, third generation (Milan)",
        brand: "AMD EPYC Processor (Milan)",
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

/// The generation name of every host profile ([`derive_host_profile`]). No
/// known generation uses it.
pub const HOST_GENERATION: &str = "host";

/// Returns the brand string of every host profile of `vendor`'s CPUs.
fn host_brand(vendor: CpuVendor) -> &'static str {
    match vendor {
        CpuVendor::Intel => "Intel(R) Processor (host profile)",
        CpuVendor::Amd => "AMD Processor (host profile)",
    }
}

/// What a derivation derives a profile for: a known generation, or one host.
struct Target<'a> {
    /// The vendor of the profile's CPUs.
    vendor: CpuVendor,
    /// The generation that the profile records.
    generation: Generation,
    /// The profile's description.
    description: String,
    /// The profile's brand string.
    brand: &'a str,
}

/// Derives revision `revision` of the profile of `generation` from the
/// `fingerprints` of its hosts, on any backends.
pub fn derive_profile(
    generation: &KnownGeneration,
    revision: u32,
    fingerprints: &[CpuFingerprint],
) -> Result<CpuProfile, DeriveError> {
    derive(
        &Target {
            vendor: generation.vendor,
            generation: generation.generation(),
            description: format!(
                "{}, on every backend; NVX time ABI v1",
                generation.description
            ),
            brand: generation.brand,
        },
        revision,
        fingerprints,
    )
}

/// Returns whether host profiles serve the CPU `host`, so that
/// `--cpu-profile host` can boot on it: [`derive_host_profile`] derives them
/// for the CPUs of every vendor that profiles serve, Intel's and AMD's.
pub fn supports_host_profiles(host: &HostCpuSignature) -> bool {
    CpuVendor::from_cpuid_vendor(&host.vendor()).is_some()
}

/// Derives the host profile of the host and backend that `fingerprint`
/// describes, for `--cpu-profile host`: the profile that the derivation
/// policy gives for this one fingerprint, with the ID
/// `<vendor>.host.v1`, the generation [`HOST_GENERATION`] limited to the
/// host's family, model, and stepping, and a generic brand string of the
/// vendor.
///
/// Unlike a pinned profile, a host profile is neither reviewed nor
/// immutable: a microcode, firmware, or hypervisor update can change it.
///
/// Fails with `E_PROFILE_HOST_UNKNOWN` for a CPU of a vendor that profiles do
/// not serve ([`supports_host_profiles`]), and with `E_PROFILE_UNSUPPORTED`
/// when the backend lacks a feature that the time ABI requires.
pub fn derive_host_profile(fingerprint: &CpuFingerprint) -> Result<CpuProfile, ProfileError> {
    let host = host_signature(fingerprint);
    let cpu = &fingerprint.host.cpu;
    let Some(vendor) = CpuVendor::from_cpuid_vendor(cpu.vendor.as_bytes()) else {
        return Err(ProfileError::new(
            ProfileErrorCode::ProfileHostUnknown,
            format!(
                "host CPU profiles support only Intel and AMD CPUs, and the host CPU is {host}"
            ),
        ));
    };
    let (family, model, stepping) = decode_signature(cpu.vendor.as_bytes(), cpu.signature.0);
    derive(
        &Target {
            vendor,
            generation: Generation {
                name: HOST_GENERATION.to_owned(),
                cpus: vec![GenerationCpu {
                    family,
                    model,
                    steppings: [stepping, stepping],
                }],
            },
            description: format!(
                "Host profile of {host}, derived from one {} CPU fingerprint for development; \
                 not pinned; NVX time ABI v1",
                fingerprint.backend.name
            ),
            brand: host_brand(vendor),
        },
        1,
        std::slice::from_ref(fingerprint),
    )
    .map_err(|error| {
        let code = match error {
            DeriveError::TimeFeature(_) => ProfileErrorCode::ProfileUnsupported,
            _ => ProfileErrorCode::ProfileHostUnknown,
        };
        ProfileError::new(
            code,
            format!("cannot derive a host CPU profile for {host}: {error}"),
        )
    })
}

/// Returns the vendor and signature of the host that `fingerprint`
/// describes.
fn host_signature(fingerprint: &CpuFingerprint) -> HostCpuSignature {
    let cpu = &fingerprint.host.cpu;
    let mut vendor = [0; 12];
    let host_vendor = cpu.vendor.as_bytes();
    if host_vendor.len() == vendor.len() {
        vendor.copy_from_slice(host_vendor);
    }
    HostCpuSignature::new(vendor, cpu.signature.0)
}

/// Derives revision `revision` of the profile of `target` from
/// `fingerprints`; see the module documentation.
fn derive(
    target: &Target<'_>,
    revision: u32,
    fingerprints: &[CpuFingerprint],
) -> Result<CpuProfile, DeriveError> {
    if fingerprints.is_empty() {
        return Err(DeriveError::NoFingerprints);
    }
    let profile_generation = &target.generation;
    let vendor = target.vendor;
    for fingerprint in fingerprints {
        let host = host_signature(fingerprint);
        if !profile_generation.contains(vendor.cpuid_vendor(), &host) {
            return Err(DeriveError::WrongGeneration {
                backend: fingerprint.backend.name.clone(),
                digest: fingerprint.digest.clone(),
                host: host.to_string(),
                generation: profile_generation.name.clone(),
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
                .map(|member| table(vendor, &member.backend.cpuid))
                .collect::<Vec<_>>();
            combine(&tables, &format!("{backend} hosts"))
        })
        .collect::<Result<Vec<_>, _>>()?;
    let combined = combine(&backend_tables, "backends")?;

    let mut leaves = apply_policy(vendor, &combined)?;

    // The brand string: generic per generation.
    for (leaf, value) in (EXTENDED_LEAF_BASE + 2..).zip(brand_leaves(target.brand)) {
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
            let mask = pinned_mask(vendor, leaf, subleaf, value);
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
        format!("{}.{}.v{revision}", vendor.id(), profile_generation.name),
        target.description.clone(),
        vendor.cpuid_vendor().to_owned(),
        profile_generation.clone(),
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
/// the bits OpenVMM sets per VM in a profile of `vendor`'s CPUs cleared, so
/// hosts are compared only on what a profile pins.
fn table(vendor: CpuVendor, entries: &[cpuid::CpuidEntry]) -> Table {
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
            let mask = pinned_mask(vendor, leaf, subleaf, value);
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
/// subleaf set, the [`DATA_LEAVES`] allowlist (and [`AMD_DATA_LEAVES`] when
/// `vendor` is AMD), the CPU time bits, and the features a VM must not see.
/// The XSAVE leaf and the brand are filled in later.
fn apply_policy(vendor: CpuVendor, combined: &Table) -> Result<Table, DeriveError> {
    let get = |leaf: u32, subleaf: Option<u32>| {
        combined.get(&(leaf, subleaf)).copied().unwrap_or_default()
    };
    let max_basic = get(0, None)[0];
    let max_extended = get(EXTENDED_LEAF_BASE, None)[0];
    let mut leaves = Table::new();
    let all_leaves = (0..=max_basic).chain(EXTENDED_LEAF_BASE..=max_extended);
    for leaf in all_leaves.filter(|leaf| !VM_OWNED_LEAVES.contains(leaf)) {
        let data = DATA_LEAVES.contains(&leaf)
            || (vendor == CpuVendor::Amd && AMD_DATA_LEAVES.contains(&leaf));
        if !cpuid::is_indexed_leaf(leaf) {
            leaves.insert((leaf, None), if data { get(leaf, None) } else { [0; 4] });
            continue;
        }
        let last = match leaf {
            _ if !data => 0,
            // Up to and including the first null cache type.
            0x4 | 0x8000_001d => (0..0x40)
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
    if vendor == CpuVendor::Amd {
        // CPUID.0x80000001 EBX: no brand ID or package type, which describe
        // the host's SKU and socket rather than the CPU's features.
        edit(&mut leaves, (EXTENDED_LEAF_BASE + 1, None), 1, 0, !0);
        // CPUID.0x80000001 ECX: no SVM, IBS, SKINIT, watchdog timer, LWP, or
        // node ID MSR; no core, northbridge, or LLC performance counter
        // extensions, performance TSC, data breakpoint or address mask
        // extensions, or MONITORX. No 3DNowPrefetch either: the Hyper-V root
        // of a nested Azure host does not see it, so that host's cold-boot
        // support check, which reads the root's CPUID, would fail a profile
        // that pins it, although WHP's guests see it; Linux infers PREFETCHW
        // from long mode whatever the bit says.
        edit(
            &mut leaves,
            (EXTENDED_LEAF_BASE + 1, None),
            2,
            0,
            1 << 2
                | 1 << 8
                | 1 << 10
                | 1 << 12
                | 1 << 13
                | 1 << 15
                | 1 << 19
                | 1 << 23
                | 1 << 24
                | 1 << 26
                | 1 << 27
                | 1 << 28
                | 1 << 29
                | 1 << 30,
        );
        // CPUID.0x80000008 EBX: no instructions-retired counter, INVLPGB
        // (nested pages included), RDPRU, which reads APERF and MPERF, memory
        // bandwidth enforcement, PPIN, CPPC, or branch sampling. No BTC_NO
        // either: KVM never enumerates it in KVM_GET_SUPPORTED_CPUID, so every
        // KVM host would fail a profile that set it, and a guest loses
        // nothing without it, because Linux marks no CPU of family 0x19 or
        // later as affected by RETBLEED, which is all the bit decides. The
        // speculation controls and the other immunities stay.
        edit(
            &mut leaves,
            (EXTENDED_LEAF_BASE + 8, None),
            1,
            0,
            1 << 1 | 1 << 3 | 1 << 4 | 1 << 6 | 1 << 21 | 1 << 23 | 1 << 27 | AMD_BTC_NO | 1 << 31,
        );
        // PSFD is a bit of SPEC_CTRL, so it means nothing to a guest that
        // cannot access SPEC_CTRL, and KVM lets a guest access it only if the
        // guest's CPUID has a SPEC_CTRL control. The rule reads the derived
        // table, the CPUID that a guest of the profile has: applied to each
        // fingerprint instead, it would keep PSFD where every backend offers
        // it beside a different control, which the intersection then drops.
        let register = |leaves: &Table, key, register: usize| {
            leaves
                .get(&key)
                .map_or(0, |value: &[u32; 4]| value[register])
        };
        if !has_spec_ctrl_control(
            register(&leaves, (0x7, Some(0)), 3),
            register(&leaves, (EXTENDED_LEAF_BASE + 8, None), 1),
        ) {
            edit(&mut leaves, (EXTENDED_LEAF_BASE + 8, None), 1, 0, AMD_PSFD);
        }
        // CPUID.0x80000021: AutoIBRS, LFENCE serialization, SBPB, SRSO_NO,
        // and the TSA immunities stay; no CPUID faulting, which writes HWCR,
        // or workload classification; nothing in EBX (the microcode patch and
        // return address predictor sizes) or EDX.
        edit(
            &mut leaves,
            (EXTENDED_LEAF_BASE + 0x21, None),
            0,
            0,
            1 << 17 | 1 << 22,
        );
        edit(&mut leaves, (EXTENDED_LEAF_BASE + 0x21, None), 1, 0, !0);
        edit(&mut leaves, (EXTENDED_LEAF_BASE + 0x21, None), 3, 0, !0);
    }
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
    use crate::test_support::host_fingerprint;
    use crate::test_support::milan_whp_entries;
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

    #[test]
    fn a_host_profile_applies_the_policy_to_one_host() {
        let pinned = profile("intel.alderlake.v1");
        let host = derive_host_profile(&fingerprint(pinned, "whp")).unwrap();
        assert_eq!(host.id(), "intel.host.v1");
        assert_eq!(host.vendor(), "GenuineIntel");
        assert_eq!(
            host.generation(),
            &Generation {
                name: HOST_GENERATION.to_owned(),
                cpus: vec![GenerationCpu {
                    family: 6,
                    model: 154,
                    steppings: [3, 3],
                }],
            }
        );
        assert!(
            host.description()
                .starts_with("Host profile of GenuineIntel family 6 model 154 stepping 3"),
            "{}",
            host.description()
        );
        assert!(host.description().contains("one whp CPU fingerprint"));
        assert_eq!(brand(&host), "Intel(R) Processor (host profile)");
        // Everything else is what the catalog's derivation gives for the
        // same fingerprint.
        let brand_leaves = 0x8000_0002..=0x8000_0004;
        let without_brand = |profile: &CpuProfile| {
            profile
                .cpuid()
                .iter()
                .filter(|entry| !brand_leaves.contains(&entry.leaf.0))
                .cloned()
                .collect::<Vec<_>>()
        };
        assert_eq!(without_brand(&host), without_brand(pinned));
        assert_eq!(host.xcr0(), pinned.xcr0());
        assert_eq!(host.xss(), pinned.xss());
        assert_eq!(host.xsave_components(), pinned.xsave_components());
        assert_eq!(host.msrs(), pinned.msrs());
        assert_eq!(
            host.physical_address_width(),
            pinned.physical_address_width()
        );
        // The derivation is deterministic, and the profile is canonical.
        let again = derive_host_profile(&fingerprint(pinned, "whp")).unwrap();
        assert_eq!(again.digest(), host.digest());
        assert_eq!(CpuProfile::decode(&host.encode()).unwrap(), host);
    }

    #[test]
    fn host_profiles_fail_with_the_time_abi_codes() {
        let pinned = profile("intel.alderlake.v1");
        let mut hygon = host_fingerprint("kvm", milan_whp_entries());
        hygon.host.cpu.vendor = "HygonGenuine".to_owned();
        let error = derive_host_profile(&hygon).unwrap_err();
        assert_eq!(error.code, ProfileErrorCode::ProfileHostUnknown);
        assert!(error.message.contains("only Intel and AMD CPUs"), "{error}");

        let no_rdtscp = edited(pinned, "whp", |entries| {
            set(entries, 0x8000_0001, None, 3, |edx| edx & !(1 << 27));
        });
        let error = derive_host_profile(&no_rdtscp).unwrap_err();
        assert_eq!(error.code, ProfileErrorCode::ProfileUnsupported);
        assert!(
            error.message.starts_with(
                "cannot derive a host CPU profile for GenuineIntel family 6 model 154 stepping 3"
            ),
            "{error}"
        );
        assert!(
            error
                .message
                .ends_with("RDTSCP, which the time ABI requires")
        );
    }

    #[test]
    fn no_known_generation_is_the_host_generation() {
        assert!(known_generation(HOST_GENERATION).is_none());
    }

    /// Host profiles serve the CPUs that [`derive_host_profile`] accepts:
    /// Intel's and AMD's.
    #[test]
    fn host_profiles_serve_intel_and_amd_cpus() {
        let intel = fingerprint(profile("intel.alderlake.v1"), "whp");
        let amd = host_fingerprint("whp", milan_whp_entries());
        let mut hygon = amd.clone();
        hygon.host.cpu.vendor = "HygonGenuine".to_owned();
        for (fingerprint, served) in [(intel, true), (amd, true), (hygon, false)] {
            let host = host_signature(&fingerprint);
            assert_eq!(supports_host_profiles(&host), served, "{host}");
            assert_eq!(derive_host_profile(&fingerprint).is_ok(), served, "{host}");
        }
    }

    /// Returns the profile of AMD's Milan generation that `fingerprints`
    /// derive, as the catalog pins it.
    fn derive_milan(fingerprints: &[CpuFingerprint]) -> Result<CpuProfile, DeriveError> {
        derive_profile(generation("milan"), 1, fingerprints)
    }

    /// The pinned Milan profile is the policy's profile of the one host it was
    /// derived from: an AMD EPYC 7763 that WHP serves on an Azure host, whose
    /// CPUID `test_support::MILAN_WHP_CPUID` records.
    #[test]
    fn the_milan_profile_is_its_hosts_derivation() {
        let pinned = profile("amd.milan.v1");
        let derived = derive_milan(&[host_fingerprint("whp", milan_whp_entries())]).unwrap();
        assert_eq!(derived.id(), pinned.id());
        assert_eq!(derived.description(), pinned.description());
        assert_eq!(derived.generation(), pinned.generation());
        assert_eq!(derived.cpuid(), pinned.cpuid());
        assert_eq!(derived.xcr0(), pinned.xcr0());
        assert_eq!(derived.xss(), pinned.xss());
        assert_eq!(derived.xsave_components(), pinned.xsave_components());
        assert_eq!(derived.msrs(), pinned.msrs());
        assert_eq!(brand(pinned), "AMD EPYC Processor (Milan)");
    }

    /// A KVM host of the same CPU supports the Milan profile: KVM never
    /// enumerates `BTC_NO`, and OpenVMM's KVM backend withholds PSFD without
    /// a `SPEC_CTRL` control, which that host's KVM, nested on Azure, would
    /// not offer either; the profile has neither.
    #[test]
    fn a_kvm_host_of_its_cpu_supports_the_milan_profile() {
        let mut entries = milan_whp_entries();
        set(&mut entries, 0x8000_0008, None, 1, |ebx| {
            ebx & !(AMD_PSFD | AMD_BTC_NO)
        });
        let surface =
            crate::HostCpuSurface::from_fingerprint(&host_fingerprint("kvm", entries).backend);
        assert_eq!(
            crate::support_violations(profile("amd.milan.v1"), &surface),
            Vec::<String>::new()
        );
    }

    /// The CPUID entry of `profile` at `leaf` and `subleaf`.
    fn entry(profile: &CpuProfile, leaf: u32, subleaf: Option<u32>) -> &CpuidLeafValue {
        profile
            .cpuid()
            .iter()
            .find(|entry| entry.key() == (leaf, subleaf))
            .unwrap_or_else(|| panic!("{} lacks {}", profile.id(), describe_leaf(leaf, subleaf)))
    }

    #[test]
    fn an_amd_host_profile_leaves_amd_topology_to_the_vm() {
        let host = derive_host_profile(&host_fingerprint("whp", milan_whp_entries())).unwrap();
        assert_eq!(host.id(), "amd.host.v1");
        assert_eq!(host.vendor(), "AuthenticAMD");
        assert_eq!(host.cpu_vendor(), CpuVendor::Amd);
        assert_eq!(
            host.generation(),
            &Generation {
                name: HOST_GENERATION.to_owned(),
                cpus: vec![GenerationCpu {
                    family: 0x19,
                    model: 1,
                    steppings: [1, 1],
                }],
            }
        );
        assert!(
            host.description()
                .starts_with("Host profile of AuthenticAMD family 25 model 1 stepping 1"),
            "{}",
            host.description()
        );
        assert_eq!(brand(&host), "AMD Processor (host profile)");

        // The cache topology lists every cache up to the null cache type, as
        // leaf 4 does on Intel, with the sharing count left to the VM.
        let caches = host
            .cpuid()
            .iter()
            .filter(|entry| entry.leaf.0 == 0x8000_001d)
            .map(|entry| entry.subleaf.map(|subleaf| subleaf.0))
            .collect::<Vec<_>>();
        assert_eq!(caches, [Some(0), Some(1), Some(2), Some(3), Some(4)]);
        let l3 = entry(&host, 0x8000_001d, Some(3));
        assert_eq!(l3.values(), [0x163, 0x03c0_003f, 0x7fff, 0x1]);
        assert_eq!(l3.masks(), [0xfc00_3fff, !0, !0, !0]);
        assert_eq!(entry(&host, 0x8000_001d, Some(4)).masks(), [!0; 4]);
        // The core count and APIC ID size, and the processor topology leaf,
        // are the VM's.
        assert_eq!(entry(&host, 0x8000_0008, None).masks()[2], !0xf0ff);
        assert_eq!(
            entry(&host, 0x8000_001e, None).masks(),
            [0, 0xffff_0000, 0xffff_f800, !0]
        );
        // The L1 cache and TLB leaf keeps its data; leaf 4, which AMD CPUs
        // leave zero, is pinned zero.
        assert_eq!(
            host.lookup(0x8000_0005, 0),
            [0xff40_ff40, 0xff40_ff40, 0x2008_0140, 0x2008_0140]
        );
        assert_eq!(entry(&host, 0x4, Some(0)).masks(), [!0; 4]);
        // Topology extensions stay: the cache and processor topology leaves
        // depend on them.
        assert_ne!(host.lookup(0x8000_0001, 0)[2] & 1 << 22, 0);
        // The policy's time bits apply as on Intel.
        assert_eq!(host.lookup(0x8000_0007, 0), [0, 0, 0, 1 << 8]);
        assert_eq!(host.lookup(6, 0), [1 << 2, 0, 0, 0]);
        // The derivation is deterministic, and the profile is canonical.
        let again = derive_host_profile(&host_fingerprint("whp", milan_whp_entries())).unwrap();
        assert_eq!(again.digest(), host.digest());
        assert_eq!(CpuProfile::decode(&host.encode()).unwrap(), host);
    }

    #[test]
    fn the_amd_policy_keeps_the_speculation_controls_and_clears_what_a_vm_must_not_see() {
        // A Milan host whose backend offers SVM, IBS, LWP, the performance
        // counter extensions, MONITORX, the speculation controls and
        // immunities, RDPRU, PPIN, CPPC, and 0x80000021's features, and
        // reports its own core count, cache sharing, and topology, as KVM
        // can on bare metal, and BTC_NO, as WHP does.
        let mut entries = milan_whp_entries();
        set(&mut entries, 0x8000_0001, None, 2, |ecx| {
            ecx | 1 << 2 | 1 << 10 | 1 << 15 | 1 << 23 | 1 << 24 | 1 << 28 | 1 << 29
        });
        set(&mut entries, 0x8000_0008, None, 1, |ebx| {
            ebx | 1 << 1 | 1 << 12 | 1 << 14 | 1 << 15 | 1 << 23 | 1 << 24 | 1 << 27
        });
        set(&mut entries, 0x8000_0008, None, 2, |_| 0x0000_707f);
        set(&mut entries, 0x8000_001d, Some(3), 0, |eax| eax | 15 << 14);
        set(&mut entries, 0x8000_001e, None, 1, |_| 0x0000_0103);
        set(&mut entries, 0x8000_0021, None, 0, |_| {
            1 << 2 | 1 << 6 | 1 << 8 | 1 << 17 | 1 << 22 | 1 << 27 | 1 << 29
        });
        set(&mut entries, 0x8000_0021, None, 1, |_| 0x10);
        set(&mut entries, 0x8000_0021, None, 2, |_| 1 << 1 | 1 << 2);
        let derived = derive_milan(&[host_fingerprint("kvm", entries)]).unwrap();
        assert_eq!(derived.id(), "amd.milan.v1");
        assert_eq!(brand(&derived), "AMD EPYC Processor (Milan)");

        let x1 = derived.lookup(0x8000_0001, 0)[2];
        for (bit, name) in [(0, "LAHF"), (5, "ABM"), (6, "SSE4A"), (22, "TOPOEXT")] {
            assert_ne!(x1 & 1 << bit, 0, "{name}");
        }
        for (bit, name) in [
            (2, "SVM"),
            (8, "3DNowPrefetch"),
            (10, "IBS"),
            (15, "LWP"),
            (23, "PERFCTR_CORE"),
            (24, "PERFCTR_NB"),
            (28, "PERFCTR_LLC"),
            (29, "MONITORX"),
        ] {
            assert_eq!(x1 & 1 << bit, 0, "{name}");
        }
        let x8 = derived.lookup(0x8000_0008, 0)[1];
        // PSFD stays beside IBRS, STIBP, and SSBD, the SPEC_CTRL controls.
        for (bit, name) in [
            (0, "CLZERO"),
            (12, "IBPB"),
            (14, "IBRS"),
            (15, "STIBP"),
            (24, "SSBD"),
            (28, "PSFD"),
        ] {
            assert_ne!(x8 & 1 << bit, 0, "{name}");
        }
        for (bit, name) in [
            (1, "IRPERF"),
            (4, "RDPRU"),
            (23, "PPIN"),
            (27, "CPPC"),
            (29, "BTC_NO"),
        ] {
            assert_eq!(x8 & 1 << bit, 0, "{name}");
        }
        // AutoIBRS, LFENCE serialization, the null selector rule, SBPB,
        // SRSO_NO, and the TSA immunities stay; CPUID faulting and workload
        // classification go, and so does EBX.
        assert_eq!(
            derived.lookup(0x8000_0021, 0),
            [
                1 << 2 | 1 << 6 | 1 << 8 | 1 << 27 | 1 << 29,
                0,
                1 << 1 | 1 << 2,
                0
            ]
        );
        // The host's core count, cache sharing, and topology are not the
        // profile's, and neither are its brand ID and package type.
        assert_eq!(derived.lookup(0x8000_0008, 0)[2], 0);
        assert_eq!(derived.lookup(0x8000_001d, 3)[0], 0x163);
        assert_eq!(derived.lookup(0x8000_001e, 0), [0; 4]);
        assert_eq!(derived.lookup(0x8000_0001, 0)[1], 0);
    }

    /// PSFD is a bit of `SPEC_CTRL`, so an AMD profile keeps it only beside a
    /// `SPEC_CTRL` control, as KVM's `guest_has_spec_ctrl_msr()` requires of
    /// a guest's CPUID and OpenVMM's KVM backend of KVM's supported CPUID:
    /// Intel's IBRS, or AMD's IBRS, STIBP, or SSBD. IBPB controls another
    /// MSR. The rule reads the derived profile, so controls that the
    /// backends do not share do not keep PSFD. `BTC_NO`, which KVM never
    /// offers, always goes.
    #[test]
    fn an_amd_profile_keeps_psfd_only_beside_a_spec_ctrl_control() {
        let with = |leaf: u32, register: usize, bits: u32| {
            let mut entries = milan_whp_entries();
            let subleaf = (leaf == 7).then_some(0);
            set(&mut entries, leaf, subleaf, register, |value| value | bits);
            entries
        };
        let spec =
            |profile: &CpuProfile| profile.lookup(0x8000_0008, 0)[1] & (AMD_PSFD | AMD_BTC_NO);
        // The Milan WHP host offers PSFD and BTC_NO, and no SPEC_CTRL control.
        let whp = derive_milan(&[host_fingerprint("whp", milan_whp_entries())]).unwrap();
        assert_eq!(spec(&whp), 0);
        let ibpb = derive_milan(&[host_fingerprint("kvm", with(0x8000_0008, 1, 1 << 12))]).unwrap();
        assert_eq!(spec(&ibpb), 0);
        for (leaf, register, bit, name) in [
            (0x8000_0008, 1, 14, "AMD IBRS"),
            (0x8000_0008, 1, 15, "AMD STIBP"),
            (0x8000_0008, 1, 24, "AMD SSBD"),
            (0x7, 3, 26, "Intel IBRS"),
        ] {
            let derived =
                derive_milan(&[host_fingerprint("kvm", with(leaf, register, 1 << bit))]).unwrap();
            assert_eq!(spec(&derived), AMD_PSFD, "{name}");
            assert!(
                has_spec_ctrl_control(derived.lookup(7, 0)[3], derived.lookup(0x8000_0008, 0)[1]),
                "{name}"
            );
        }
        // Each backend offers PSFD with another control, so the profile has
        // neither control and no PSFD.
        let derived = derive_milan(&[
            host_fingerprint("kvm", with(0x8000_0008, 1, 1 << 14)),
            host_fingerprint("whp", with(0x8000_0008, 1, 1 << 24)),
        ])
        .unwrap();
        assert_eq!(
            derived.lookup(0x8000_0008, 0)[1] & (1 << 14 | 1 << 24 | AMD_PSFD | AMD_BTC_NO),
            0
        );
    }

    /// The AMD policy leaves nothing pinned that differs between the hosts of
    /// a generation or between a Hyper-V root's view and its guests': AMD
    /// CPUs repeat their signature in `0x80000001` EAX, which describes the
    /// CPU as leaf 1's does, so a host of another stepping, such as
    /// Milan-X's 2, supports the profile, and so does a host whose root does
    /// not see 3DNowPrefetch.
    #[test]
    fn other_hosts_of_the_generation_support_an_amd_profile() {
        let derived = derive_milan(&[host_fingerprint("whp", milan_whp_entries())]).unwrap();
        assert_eq!(derived.lookup(0x8000_0001, 0)[0], 0x00a0_0f11);
        let mut stepping2 = milan_whp_entries();
        set(&mut stepping2, 1, None, 0, |_| 0x00a0_0f12);
        set(&mut stepping2, 0x8000_0001, None, 0, |_| 0x00a0_0f12);
        // Its backend reports another package type, too, and, as the
        // Hyper-V root of a nested Azure host sees it, no 3DNowPrefetch.
        set(&mut stepping2, 0x8000_0001, None, 1, |_| 0);
        set(&mut stepping2, 0x8000_0001, None, 2, |ecx| ecx & !(1 << 8));
        let surface =
            crate::HostCpuSurface::from_fingerprint(&host_fingerprint("whp", stepping2).backend);
        assert_eq!(
            crate::support_violations(&derived, &surface),
            Vec::<String>::new()
        );
    }

    #[test]
    fn amd_cache_topology_is_descriptive() {
        // Two backends report the reference host's L3, and a third another
        // SKU's: the majority's stays, where intersecting would mix them.
        let mut other_sku = milan_whp_entries();
        set(&mut other_sku, 0x8000_001d, Some(3), 2, |_| 0x3fff);
        let fingerprints = [
            host_fingerprint("kvm", other_sku.clone()),
            host_fingerprint("mshv", milan_whp_entries()),
            host_fingerprint("whp", milan_whp_entries()),
        ];
        let derived = derive_milan(&fingerprints).unwrap();
        assert_eq!(derived.lookup(0x8000_001d, 3)[2], 0x7fff);
        // Two backends that disagree have no majority.
        let tie = derive_milan(&fingerprints[..2]).unwrap_err();
        assert!(
            matches!(&tie, DeriveError::Tie { what, .. } if what == "CPUID 0x8000001d.3 ECX among the backends"),
            "{tie}"
        );
    }

    /// The AMD rules do not reach Intel profiles: an Intel host whose
    /// backend reports values in AMD's leaves and fields derives the
    /// profile that the Intel rules alone give.
    #[test]
    fn the_amd_rules_leave_intel_profiles_unchanged() {
        let pinned = profile("intel.alderlake.v1");
        let kvm = edited(pinned, "kvm", |entries| {
            // MONITORX and PPIN, which AMD profiles clear; AMD's L1 cache
            // leaf, which AMD profiles keep; and an AMD core count, which AMD
            // profiles leave to the VM.
            set(entries, 0x8000_0001, None, 2, |ecx| ecx | 1 << 29);
            set(entries, 0x8000_0005, None, 2, |_| 0x2008_0140);
            set(entries, 0x8000_0008, None, 1, |ebx| ebx | 1 << 23);
            set(entries, 0x8000_0008, None, 2, |_| 0x0000_701f);
        });
        let derived = derive_profile(generation("alderlake"), 1, &[kvm]).unwrap();
        assert_eq!(
            derived.lookup(0x8000_0001, 0)[2],
            pinned.lookup(0x8000_0001, 0)[2] | 1 << 29
        );
        assert_eq!(
            derived.lookup(0x8000_0008, 0)[1],
            pinned.lookup(0x8000_0008, 0)[1] | 1 << 23
        );
        assert_eq!(derived.lookup(0x8000_0005, 0), [0; 4]);
        assert_eq!(derived.lookup(0x8000_0008, 0)[2], 0);
        assert_eq!(entry(&derived, 0x8000_0008, None).masks(), [!0; 4]);

        // PSFD without a SPEC_CTRL control, and BTC_NO, which AMD profiles
        // clear, stay too: the Ice Lake-SP profile has no SPEC_CTRL control,
        // which Azure withholds from its hosts.
        let pinned = profile("intel.icelake-sp.v1");
        assert!(!has_spec_ctrl_control(
            pinned.lookup(7, 0)[3],
            pinned.lookup(0x8000_0008, 0)[1]
        ));
        let kvm = edited(pinned, "kvm", |entries| {
            set(entries, 0x8000_0008, None, 1, |ebx| {
                ebx | AMD_PSFD | AMD_BTC_NO
            });
        });
        let derived = derive_profile(generation("icelake-sp"), 1, &[kvm]).unwrap();
        assert_eq!(
            derived.lookup(0x8000_0008, 0)[1],
            pinned.lookup(0x8000_0008, 0)[1] | AMD_PSFD | AMD_BTC_NO
        );
    }
}
