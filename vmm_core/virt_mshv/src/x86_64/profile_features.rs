// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! MSHV processor features derived from a CPU profile, and the CPU surface
//! that MSHV supports on this host.
//!
//! A partition presents a CPU feature only when its processor feature bank
//! bit (or XSAVE feature bit) is set, and some bits also decide which MSRs the
//! guest may use. [`time_abi_features`] starts from the features the host
//! partition offers, and [`hv_banks::profile_features`] makes every bit that
//! the profile decides ([`hv_banks::mapped_mask`]) follow the profile. The
//! other bits, such as the nested-paging and VMX details, keep OpenVMM's
//! legacy policy: what the host offers of OpenVMM's supported lists. The
//! CPUID intercept results then present core's time ABI CPUID verbatim (the
//! profile's complete effective CPUID), including the descriptive leaves and
//! the time policy bits a host lacks. Only the banks and the surface read the
//! profile itself.
//!
//! [`supported_cpu_surface`] reports what MSHV supports on this host without a
//! probe partition: the host partition's processor features, the host's CPUID
//! restricted to them ([`hv_banks::restrict_cpuid_to_features`]), and the
//! guest physical address width.
//!
//! The ignored hardware tests (`profile_features::hw`) check the shared
//! feature table, the surface, and the derived features against MSHV.

use crate::KernelError;
use cpu_profile::CpuProfile;
use cpu_profile::cpuid::CpuidEntry;
use cpu_profile::cpuid::EXTENDED_LEAF_BASE;
use cpu_profile::cpuid::HYPERVISOR_LEAF_BASE;
use cpu_profile::hv_banks;
use cpu_profile::hv_banks::HvFeatureWord;
use cpu_profile::hv_banks::HvFeatures;
use hvdef::HvPartitionPropertyCode;
use hvdef::HvX64PartitionProcessorFeatures1 as Bank1;
use mshv_ioctls::Mshv;
use virt::CpuidLeaf;
use virt::time_abi::TimeAbiCode;
use virt::time_abi::TimeAbiError;
use virt::time_abi::surface::SupportedCpuSurface;
use virt::time_abi::surface::SupportedMsrValue;

/// Returns the features of OpenVMM's supported lists, which every MSHV
/// partition without the time ABI enables.
pub(crate) fn legacy_features() -> HvFeatures {
    HvFeatures {
        banks: [
            super::supported_processor_features().into_bits(),
            super::supported_processor_features1().into_bits(),
        ],
        xsave: super::supported_xsave_features().into_bits(),
    }
}

/// Reads the features that the host partition offers its child partitions.
/// Fails with `E_PROFILE_UNSUPPORTED`.
pub(crate) fn host_features(mshv: &Mshv) -> Result<HvFeatures, TimeAbiError> {
    let property = |code: HvPartitionPropertyCode| {
        mshv.get_host_partition_property(code.0).map_err(|error| {
            TimeAbiError::new(
                TimeAbiCode::ProfileUnsupported,
                format!(
                    "cannot read the host partition's {code:?}: {}",
                    super::time_abi::error_chain(&KernelError::from(error))
                ),
            )
        })
    };
    Ok(HvFeatures {
        banks: [
            property(HvPartitionPropertyCode::ProcessorFeatures0)?,
            property(HvPartitionPropertyCode::ProcessorFeatures1)?,
        ],
        xsave: property(HvPartitionPropertyCode::ProcessorXsaveFeatures)?,
    })
}

/// Returns the processor features of a time ABI partition with CPU profile
/// `profile` on a host that offers `host`.
///
/// The bits the profile decides start from what the host offers; the other
/// bits keep OpenVMM's legacy policy. On top, the time ABI's policy keeps the
/// invariant TSC and hides the TSC-deadline timer, `IA32_TSC_ADJUST`, and
/// APERF/MPERF, whatever the profile says. Fails with `E_PROFILE_UNSUPPORTED`.
pub(crate) fn time_abi_features(
    profile: &CpuProfile,
    host: HvFeatures,
) -> Result<HvFeatures, TimeAbiError> {
    let legacy = legacy_features();
    let mut start = host;
    for word in HvFeatureWord::ALL {
        *start.word_mut(word) &= legacy.word(word) | hv_banks::mapped_mask(word);
    }
    let mut features = hv_banks::profile_features(profile, start)
        .map_err(|error| TimeAbiError::new(TimeAbiCode::ProfileUnsupported, error.message))?;
    features.banks[1] =
        super::time_abi::processor_features1(Bank1::from_bits(features.banks[1])).into_bits();
    Ok(features)
}

/// Returns the CPUID result of the host processor for `leaf` and `subleaf`.
// The host CPU is identified with the CPUID instruction of the host, which
// exists only on x86-64 hosts.
// xtask-fmt allow-target-arch cpu-intrinsic
#[cfg(target_arch = "x86_64")]
fn host_cpuid(leaf: u32, subleaf: u32) -> [u32; 4] {
    let result = std::arch::x86_64::__cpuid_count(leaf, subleaf);
    [result.eax, result.ebx, result.ecx, result.edx]
}

// xtask-fmt allow-target-arch cpu-intrinsic
#[cfg(not(target_arch = "x86_64"))]
fn host_cpuid(_leaf: u32, _subleaf: u32) -> [u32; 4] {
    [0; 4]
}

/// Returns the host's CPUID table, as the CPUID instruction enumerates it,
/// without the hypervisor range: that describes the host's hypervisor rather
/// than the processor, so neither the supported CPU surface nor the unlisted
/// candidates use it, and every CPUID in the root partition exits to the
/// hypervisor.
pub(crate) fn host_cpuid_table() -> Vec<CpuidEntry> {
    cpu_profile::cpuid::enumerate_basic_and_extended(|leaf, subleaf| {
        Ok::<_, std::convert::Infallible>(host_cpuid(leaf, subleaf))
    })
    .unwrap_or_else(|never| match never {})
}

/// Returns the CPU surface that MSHV supports on this host, given the host's
/// CPUID table ([`host_cpuid_table`]), the features the host partition
/// offers, and the guest physical address width.
///
/// This is what a partition that enables every offered feature presents,
/// approximated without creating one: the host's CPUID without the mapped
/// features `host` lacks. `IA32_ARCH_CAPABILITIES` follows the banks.
pub(crate) fn supported_cpu_surface(
    host_cpuid: Vec<CpuidEntry>,
    host: HvFeatures,
    physical_address_width: u8,
) -> SupportedCpuSurface {
    surface_from_host_cpuid(host_cpuid, host, physical_address_width)
}

fn surface_from_host_cpuid(
    mut cpuid: Vec<CpuidEntry>,
    host: HvFeatures,
    physical_address_width: u8,
) -> SupportedCpuSurface {
    // The hypervisor range is the host's own Hyper-V interface, not the
    // guest's, whose identity the time ABI defines.
    cpuid.retain(|entry| !(HYPERVISOR_LEAF_BASE..EXTENDED_LEAF_BASE).contains(&entry.leaf.0));
    hv_banks::restrict_cpuid_to_features(&mut cpuid, host);
    let msr = hv_banks::arch_capabilities_msr(host.banks);
    SupportedCpuSurface {
        cpuid: cpuid
            .iter()
            .map(|entry| {
                let leaf = CpuidLeaf::new(entry.leaf.0, entry.registers());
                match entry.subleaf {
                    Some(subleaf) => leaf.indexed(subleaf.0),
                    None => leaf,
                }
            })
            .collect(),
        physical_address_width,
        msrs: vec![SupportedMsrValue {
            index: msr.index,
            supported: msr.supported,
            controllable: msr.controllable,
        }],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cpu_profile::hv_banks::HV_FEATURES;
    use cpu_profile::hv_banks::Pinned;
    use hvdef::HvX64PartitionProcessorFeatures as Bank0;
    use hvdef::HvX64PartitionProcessorXsaveFeatures as XsaveBank;

    /// `IA32_ARCH_CAPABILITIES`.
    const MSR_ARCH_CAPABILITIES: u32 = 0x10a;

    /// Features that the host partition offered on the fleet's MSHV hosts
    /// (CPU fingerprints): the bare-metal Skylake-SP host and the 8573C
    /// runners (Emerald Rapids, nested).
    const SKYLAKE_SP_MSHV_HOST: HvFeatures = HvFeatures {
        banks: [0x1005_f9ff_fff7_859f, 0x0000_008f_1086_0063],
        xsave: 0x3fff,
    };
    const AZURE_8573C: HvFeatures = HvFeatures {
        banks: [0x0e2a_8bff_fff7_859f, 0x0000_000c_0000_0051],
        xsave: 0x7f_ffdf,
    };

    const HOSTS: [(&str, HvFeatures); 2] = [
        ("intel.skylake-sp.v1", SKYLAKE_SP_MSHV_HOST),
        ("intel.emeraldrapids.v1", AZURE_8573C),
    ];

    fn profile(id: &str) -> &'static CpuProfile {
        cpu_profile::pinned(id).unwrap_or_else(|| panic!("profile {id} is not pinned"))
    }

    fn has(features: &HvFeatures, word: HvFeatureWord, mask: u64) -> bool {
        features.word(word) & mask != 0
    }

    fn bank1(f: fn(Bank1) -> Bank1) -> u64 {
        f(Bank1::new()).into_bits()
    }

    #[test]
    fn pinned_profiles_derive_on_their_hosts() {
        let legacy = legacy_features();
        let invariant = bank1(|b| b.with_tsc_invariant_support(true));
        for (id, host) in HOSTS {
            let profile = profile(id);
            let features = time_abi_features(profile, host).unwrap();
            for word in HvFeatureWord::ALL {
                // Only the forced invariant TSC may exceed what the host offers.
                let allowed = if word == HvFeatureWord::Bank1 {
                    invariant
                } else {
                    0
                };
                let gained = features.word(word) & !host.word(word);
                assert_eq!(gained & !allowed, 0, "{id}: {} gained bits", word.name());
                // The bits the profile does not decide keep the legacy policy.
                let unmapped = !hv_banks::mapped_mask(word) & !allowed;
                assert_eq!(
                    features.word(word) & unmapped,
                    host.word(word) & legacy.word(word) & unmapped,
                    "{id}: {} unmapped bits",
                    word.name()
                );
            }
            // Every mapped bit follows the profile, on its vendor's bits.
            for feature in HV_FEATURES
                .iter()
                .filter(|feature| !feature.is_time_policy())
            {
                let offered = has(&host, feature.word, feature.mask);
                let kept = has(&features, feature.word, feature.mask);
                match feature.pinned_by(profile) {
                    Pinned::Clear => assert!(!kept, "{id}: {}", feature.name()),
                    Pinned::Set => assert!(kept, "{id}: {}", feature.name()),
                    Pinned::Unpinned => assert_eq!(kept, offered, "{id}: {}", feature.name()),
                }
            }
            // The time policy.
            assert!(has(&features, HvFeatureWord::Bank1, invariant), "{id}");
            for mask in [
                bank1(|b| b.with_tsc_deadline_tmr_support(true)),
                bank1(|b| b.with_tsc_adjust_support(true)),
                bank1(|b| b.with_a_count_m_count_support(true)),
            ] {
                assert!(
                    !has(&features, HvFeatureWord::Bank1, mask),
                    "{id}: {mask:#x}"
                );
            }
            let (value, mask) = profile.msr(MSR_ARCH_CAPABILITIES).unwrap();
            assert_eq!(
                hv_banks::arch_capabilities_from_banks(features.banks) & mask,
                value & mask,
                "{id}"
            );
        }
    }

    #[test]
    fn bits_the_profile_does_not_decide_keep_the_legacy_policy() {
        let features =
            time_abi_features(profile("intel.skylake-sp.v1"), SKYLAKE_SP_MSHV_HOST).unwrap();
        // The bare-metal host offers the deprecated HLE and RTM bits, which no CPUID
        // feature maps and OpenVMM never listed: they stay off.
        for mask in [
            Bank0::new().with_hle_support_deprecated(true).into_bits(),
            Bank0::new().with_rtm_support_deprecated(true).into_bits(),
        ] {
            assert!(
                has(&SKYLAKE_SP_MSHV_HOST, HvFeatureWord::Bank0, mask),
                "{mask:#x}"
            );
            assert!(!has(&features, HvFeatureWord::Bank0, mask), "{mask:#x}");
        }
        // Unrestricted guest support is listed and offered: it stays on.
        let unrestricted = Bank0::new()
            .with_unrestricted_guest_support(true)
            .into_bits();
        assert!(has(&features, HvFeatureWord::Bank0, unrestricted));
    }

    #[test]
    fn skylake_presents_no_arch_capabilities() {
        // The Skylake-SP profile pins every bank-derived bit clear, including
        // RDCL_NO, IBRS_ALL, and TSX_CTRL, which OpenVMM's lists enable.
        let features =
            time_abi_features(profile("intel.skylake-sp.v1"), SKYLAKE_SP_MSHV_HOST).unwrap();
        assert_eq!(hv_banks::arch_capabilities_from_banks(features.banks), 0);
    }

    #[test]
    fn a_missing_feature_is_unsupported() {
        let mut host = SKYLAKE_SP_MSHV_HOST;
        host.xsave &= !XsaveBank::new().with_avx512_support(true).into_bits();
        host.banks[0] &= !Bank0::new().with_ibrs_support(true).into_bits();
        let error = time_abi_features(profile("intel.skylake-sp.v1"), host).unwrap_err();
        assert_eq!(error.code, TimeAbiCode::ProfileUnsupported);
        assert!(error.message.contains("avx512_support"), "{error}");
        assert!(error.message.contains("ibrs_support"), "{error}");
        let error =
            time_abi_features(profile("intel.emeraldrapids.v1"), SKYLAKE_SP_MSHV_HOST).unwrap_err();
        assert_eq!(error.code, TimeAbiCode::ProfileUnsupported);
    }

    #[test]
    fn the_time_policy_bits_need_no_host_support() {
        // Azure's MSHV offers no invariant TSC; the profile pins it, the CPUID
        // results supply it, and the bank bit is forced as before.
        let invariant = bank1(|b| b.with_tsc_invariant_support(true));
        assert!(!has(&AZURE_8573C, HvFeatureWord::Bank1, invariant));
        let features = time_abi_features(profile("intel.emeraldrapids.v1"), AZURE_8573C).unwrap();
        assert!(has(&features, HvFeatureWord::Bank1, invariant));
    }

    #[test]
    fn the_surface_follows_the_banks() {
        let stibp = Bank0::new().with_stibp_support(true).into_bits();
        let host = HvFeatures {
            banks: [stibp, 0],
            xsave: 0,
        };
        // The host lacks L1D_FLUSH and IBRS, so their bits go, and STIBP stays.
        // Bit 0, which no feature controls, keeps the host's value, and the
        // hypervisor range is dropped.
        let cpuid = vec![
            CpuidEntry::new(0, None, [7, 0, 0, 0]),
            CpuidEntry::new(7, Some(0), [0, 0, 0, (1 << 28) | (1 << 27) | (1 << 26) | 1]),
            CpuidEntry::new(
                HYPERVISOR_LEAF_BASE,
                None,
                [HYPERVISOR_LEAF_BASE + 5, 0, 0, 0],
            ),
        ];
        let surface = surface_from_host_cpuid(cpuid, host, 46);
        let leaves: Vec<_> = surface
            .cpuid
            .iter()
            .map(|leaf| (leaf.function, leaf.index, leaf.result))
            .collect();
        assert_eq!(
            leaves,
            [
                (0, None, [7, 0, 0, 0]),
                (7, Some(0), [0, 0, 0, (1 << 27) | 1]),
            ]
        );
        assert_eq!(surface.physical_address_width, 46);
        let [msr] = surface.msrs.as_slice() else {
            panic!("{:?}", surface.msrs)
        };
        assert_eq!(msr.index, MSR_ARCH_CAPABILITIES);
        assert_eq!(msr.supported, 0);
        assert_eq!(msr.controllable, hv_banks::ARCH_CAPABILITIES_BANK_MASK);
    }

    #[test]
    fn the_surface_supports_each_host_profile() {
        // On a host whose CPUID already matches its profile, the surface of the
        // fingerprinted banks supports the profile.
        for (id, host) in HOSTS {
            let profile = profile(id);
            let cpuid = profile
                .cpuid()
                .iter()
                .map(|entry| {
                    let (leaf, subleaf) = entry.key();
                    CpuidEntry::new(leaf, subleaf, entry.values())
                })
                .collect();
            let surface = surface_from_host_cpuid(cpuid, host, 52);
            let host_surface = cpu_profile::HostCpuSurface {
                cpuid: surface
                    .cpuid
                    .iter()
                    .map(|leaf| CpuidEntry::new(leaf.function, leaf.index, leaf.result))
                    .collect(),
                presentation: cpu_profile::CpuidPresentation::PassThroughHostView,
                physical_address_width: surface.physical_address_width,
                msrs: surface
                    .msrs
                    .iter()
                    .map(|msr| cpu_profile::SupportedMsr {
                        index: msr.index,
                        supported: msr.supported,
                        controllable: msr.controllable,
                    })
                    .collect(),
            };
            let violations = cpu_profile::support_violations(profile, &host_surface);
            assert!(violations.is_empty(), "{id}: {violations:#?}");
        }
    }

    /// Returns the XCR0 state components that MSHV enables with `features`:
    /// x87 and SSE with XSAVE itself, then AVX, MPX's bound registers,
    /// AVX-512's opmask and upper ZMM states, and AMX's tile configuration and
    /// data. No MSHV feature controls PKRU.
    fn xcr0_components(features: &HvFeatures) -> u64 {
        let xsave = XsaveBank::from_bits(features.xsave);
        [
            (xsave.xsave_support(), 0x3),
            (xsave.avx_support(), 1 << 2),
            (xsave.mpx_support(), 0x3 << 3),
            (xsave.avx512_support(), 0x7 << 5),
            (xsave.amx_tile_support(), 0x3 << 17),
        ]
        .into_iter()
        .filter(|&(enabled, _)| enabled)
        .fold(0, |xcr0, (_, components)| xcr0 | components)
    }

    #[test]
    fn the_xsave_features_enable_the_profile_xcr0() {
        // The features derive from the profile's CPUID bits; the components
        // they enable are the profile's XCR0, with MPX on Skylake-SP only.
        for (id, host) in HOSTS {
            let profile = profile(id);
            let features = time_abi_features(profile, host).unwrap();
            assert_eq!(xcr0_components(&features), profile.xcr0(), "{id}");
        }
    }
}

/// Hardware tests: `cargo test -p virt_mshv -- --ignored --nocapture
/// --test-threads=1 profile_features::hw`.
#[cfg(test)]
mod hw {
    use super::*;
    use cpu_profile::HostCpuSignature;
    use cpu_profile::hv_banks::CpuidBit;
    use cpu_profile::hv_banks::HV_FEATURES;
    use cpu_profile::hv_banks::HvFeature;
    use mshv_ioctls::VcpuFd;
    use mshv_ioctls::VmFd;

    /// A transient partition with one VP that never runs.
    struct Probe {
        vmfd: VmFd,
        vp: VcpuFd,
    }

    fn probe_partition(mshv: &Mshv, features: HvFeatures) -> Result<Probe, String> {
        let args = super::super::time_abi::with_features(
            super::super::partition_create_args(&virt::ProtoPartitionIsolation::None, false, false)
                .unwrap(),
            features,
        );
        let vmfd = crate::create_vm_with_retry(mshv, &args).map_err(|e| e.to_string())?;
        vmfd.initialize().map_err(|e| e.to_string())?;
        let vp = vmfd.create_vcpu(0).map_err(|e| e.to_string())?;
        Ok(Probe { vmfd, vp })
    }

    /// The vendor of this host's CPU, whose CPUID bits the features control.
    fn host_vendor() -> cpu_profile::CpuVendor {
        cpu_profile::CpuVendor::from_cpuid_vendor(&HostCpuSignature::current().vendor())
            .unwrap_or(cpu_profile::CpuVendor::Intel)
    }

    /// The leaves and subleaves of the mapped features on `vendor`'s CPUs.
    fn feature_leaves(vendor: cpu_profile::CpuVendor) -> Vec<(u32, u32)> {
        let mut leaves: Vec<_> = HV_FEATURES
            .iter()
            .filter_map(|feature| feature.cpuid_for(vendor))
            .map(|cpuid| (cpuid.leaf, cpuid.subleaf))
            .collect();
        leaves.sort_unstable();
        leaves.dedup();
        leaves
    }

    fn feature_cpuid(probe: &Probe, leaves: &[(u32, u32)]) -> Vec<[u32; 4]> {
        leaves
            .iter()
            .map(|&(leaf, subleaf)| probe.vp.get_cpuid_values(leaf, subleaf, 0, 0).unwrap())
            .collect()
    }

    /// Returns the CPUID bits set in `base` and clear in `probe`.
    fn removed(leaves: &[(u32, u32)], base: &[[u32; 4]], probe: &[[u32; 4]]) -> Vec<CpuidBit> {
        let mut bits = Vec::new();
        for (index, &(leaf, subleaf)) in leaves.iter().enumerate() {
            for register in 0..4 {
                let gone = base[index][register] & !probe[index][register];
                for bit in (0..32).filter(|bit| gone & (1 << bit) != 0) {
                    bits.push(CpuidBit::new(leaf, subleaf, register, bit));
                }
            }
        }
        bits
    }

    fn list(bits: &[CpuidBit]) -> String {
        if bits.is_empty() {
            return "nothing".to_owned();
        }
        bits.iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join(" ")
    }

    /// Whether another feature bit controls the same CPUID bit on `vendor`'s
    /// CPUs, so that clearing one of them alone may leave it set.
    fn shared(feature: &HvFeature, vendor: cpu_profile::CpuVendor) -> bool {
        feature.cpuid_for(vendor).is_some_and(|bit| {
            HV_FEATURES
                .iter()
                .filter(|other| other.cpuid_for(vendor) == Some(bit))
                .count()
                > 1
        })
    }

    /// Clears each feature bit the host offers, one at a time, and checks that
    /// every mapped one removes its CPUID bit on this host's vendor.
    #[test]
    #[ignore = "requires /dev/mshv"]
    fn features_control_the_mapped_cpuid_bits() {
        let mshv = Mshv::new().unwrap();
        let host = host_features(&mshv).unwrap();
        let vendor = host_vendor();
        println!(
            "{vendor}: host: bank0 {:#x} bank1 {:#x} xsave {:#x}",
            host.banks[0], host.banks[1], host.xsave
        );
        let leaves = feature_leaves(vendor);
        let base = feature_cpuid(&probe_partition(&mshv, host).unwrap(), &leaves);
        let mut failures = Vec::new();
        for word in HvFeatureWord::ALL {
            for bit in (0..64).filter(|bit| host.word(word) & (1 << bit) != 0) {
                let mask = 1u64 << bit;
                let feature = HV_FEATURES
                    .iter()
                    .find(|feature| feature.word == word && feature.mask == mask);
                let name = feature.map_or("(unmapped)", |feature| feature.name());
                let mut features = host;
                *features.word_mut(word) &= !mask;
                let probe = match probe_partition(&mshv, features) {
                    Ok(probe) => feature_cpuid(&probe, &leaves),
                    Err(error) => {
                        println!("{} bit {bit} {name}: rejected: {error}", word.name());
                        continue;
                    }
                };
                let removed = removed(&leaves, &base, &probe);
                let mut verdict = "";
                if let Some((feature, cpuid)) =
                    feature.and_then(|feature| Some((feature, feature.cpuid_for(vendor)?)))
                {
                    let index = leaves
                        .iter()
                        .position(|&key| key == (cpuid.leaf, cpuid.subleaf))
                        .unwrap();
                    let offered = base[index][cpuid.register] & (1 << cpuid.bit) != 0;
                    if offered && !shared(feature, vendor) && !removed.contains(&cpuid) {
                        verdict = " MISMATCH";
                        failures.push(format!(
                            "{} bit {bit} {name}: expected {} removed {}",
                            word.name(),
                            cpuid,
                            list(&removed)
                        ));
                    }
                }
                println!(
                    "{} bit {bit} {name}: removes {}{verdict}",
                    word.name(),
                    list(&removed)
                );
            }
        }
        assert!(failures.is_empty(), "{failures:#?}");
    }

    /// Converts `surface` to the profile crate's form. `presentation` says
    /// whose view its CPUID is: the root's for the cheap surface, a guest's
    /// for a probe partition.
    fn host_surface(
        surface: &SupportedCpuSurface,
        presentation: cpu_profile::CpuidPresentation,
    ) -> cpu_profile::HostCpuSurface {
        let mut cpuid = surface
            .cpuid
            .iter()
            .map(|leaf| CpuidEntry::new(leaf.function, leaf.index, leaf.result))
            .collect();
        cpu_profile::cpuid::normalize(&mut cpuid);
        cpu_profile::HostCpuSurface {
            cpuid,
            presentation,
            physical_address_width: surface.physical_address_width,
            msrs: surface
                .msrs
                .iter()
                .map(|msr| cpu_profile::SupportedMsr {
                    index: msr.index,
                    supported: msr.supported,
                    controllable: msr.controllable,
                })
                .collect(),
        }
    }

    /// Compares the cheap surface (host CPUID and banks) with the surface of a
    /// probe partition that enables every offered feature, which is how the
    /// CPU fingerprint measures it: every pinned profile must get the same
    /// support verdict from both. The host's own profile must also pass the
    /// full check on the probe's guest view, which requires the entries
    /// outside its tables to read zero, as `--cpu-fingerprint` checks them.
    #[test]
    #[ignore = "requires /dev/mshv"]
    fn the_cheap_surface_gives_the_probe_verdicts() {
        let mshv = Mshv::new().unwrap();
        let host = host_features(&mshv).unwrap();
        let probe = probe_partition(&mshv, host).unwrap();
        let width = probe
            .vmfd
            .get_partition_property(HvPartitionPropertyCode::PhysicalAddressWidth.0)
            .unwrap() as u8;
        let probe_cpuid = cpu_profile::cpuid::enumerate(|leaf, subleaf| {
            probe.vp.get_cpuid_values(leaf, subleaf, 0, 0)
        })
        .unwrap();
        // The probe partition's own CPUID already presents its features.
        let msr = hv_banks::arch_capabilities_msr(host.banks);
        let probe_surface = SupportedCpuSurface {
            cpuid: probe_cpuid
                .iter()
                .filter(|entry| !(HYPERVISOR_LEAF_BASE..EXTENDED_LEAF_BASE).contains(&entry.leaf.0))
                .map(|entry| {
                    let leaf = CpuidLeaf::new(entry.leaf.0, entry.registers());
                    match entry.subleaf {
                        Some(subleaf) => leaf.indexed(subleaf.0),
                        None => leaf,
                    }
                })
                .collect(),
            physical_address_width: width,
            msrs: vec![SupportedMsrValue {
                index: msr.index,
                supported: msr.supported,
                controllable: msr.controllable,
            }],
        };

        let mut timings = Vec::new();
        let mut cheap = None;
        for _ in 0..10 {
            let started = std::time::Instant::now();
            cheap = Some(supported_cpu_surface(host_cpuid_table(), host, width));
            timings.push(started.elapsed().as_micros());
        }
        timings.sort_unstable();
        let cheap = cheap.unwrap();
        println!(
            "host {}: cheap surface {} leaves in {} us (p50 of 10; min {} max {}), probe surface {} leaves",
            HostCpuSignature::current(),
            cheap.cpuid.len(),
            timings[5],
            timings[0],
            timings[9],
            probe_surface.cpuid.len()
        );

        // Report the bits that differ, for the record. The probe's bits that the
        // cheap surface lacks would make it under-report.
        let cheap_host = host_surface(&cheap, cpu_profile::CpuidPresentation::PassThroughHostView);
        let probe_host = host_surface(
            &probe_surface,
            cpu_profile::CpuidPresentation::PassThroughGuestView,
        );
        let mut under = 0;
        for entry in &probe_host.cpuid {
            let (leaf, subleaf) = entry.key();
            let theirs = entry.registers();
            let ours = cpu_profile::cpuid::lookup(&cheap_host.cpuid, leaf, subleaf.unwrap_or(0))
                .unwrap_or_default();
            for register in 0..4 {
                if theirs[register] != ours[register] {
                    under += (theirs[register] & !ours[register]).count_ones();
                    println!(
                        "  {leaf:#x}.{}:{} probe {:#010x} cheap {:#010x} (only probe {:#x}, only cheap {:#x})",
                        subleaf.unwrap_or(0),
                        ["eax", "ebx", "ecx", "edx"][register],
                        theirs[register],
                        ours[register],
                        theirs[register] & !ours[register],
                        ours[register] & !theirs[register]
                    );
                }
            }
        }
        println!("bits only the probe has: {under}");

        let mut failures = Vec::new();
        for profile in cpu_profile::pinned_profiles() {
            let theirs = cpu_profile::support_violations(profile, &probe_host);
            let ours = cpu_profile::support_violations(profile, &cheap_host);
            println!(
                "{}: probe {} violations, cheap {} violations",
                profile.id(),
                theirs.len(),
                ours.len()
            );
            if theirs != ours {
                failures.push(format!(
                    "{}: probe {theirs:#?} cheap {ours:#?}",
                    profile.id()
                ));
            }
        }
        let own = cpu_profile::select_auto(&HostCpuSignature::current()).unwrap();
        assert!(
            cpu_profile::support_violations(own, &cheap_host).is_empty(),
            "{}",
            own.id()
        );
        // The probe partition is a guest's view, as `--cpu-fingerprint`
        // records it, so the full check also requires every entry outside the
        // profile's tables to read zero there (`E_CPU_UNLISTED`).
        cpu_profile::verify_support(own, &probe_host)
            .unwrap_or_else(|error| panic!("probe partition: {error}"));
        assert!(failures.is_empty(), "{failures:#?}");
    }

    /// Characterizes how MSHV composes CPUID results, using leaf 4, whose
    /// subleaves differ natively:
    ///
    /// - A subleaf result applies over the hypervisor's own value, so the bits
    ///   it leaves unmasked stay native (the per-VP APIC IDs and the runtime
    ///   XSAVE sizes rely on this), and the subleaves it does not name stay
    ///   native.
    /// - A whole-leaf result answers every subleaf.
    /// - The hypervisor refuses a whole-leaf result and a subleaf result for
    ///   the same leaf, whichever comes second. So unlisted subleaves cannot
    ///   read zero through one whole-leaf result under the listed ones.
    #[test]
    #[ignore = "requires /dev/mshv"]
    fn cpuid_results_compose_over_native_values() {
        let mshv = Mshv::new().unwrap();
        let host = host_features(&mshv).unwrap();
        let read = |probe: &Probe| -> Vec<[u32; 4]> {
            (0..4)
                .map(|subleaf| probe.vp.get_cpuid_values(4, subleaf, 0, 0).unwrap())
                .collect()
        };
        let native = read(&probe_partition(&mshv, host).unwrap());
        println!("native leaf 4: {native:#010x?}");
        assert_ne!(native[1], [0; 4], "leaf 4 has no native subleaf 1");
        let whole = CpuidLeaf::new(4, [0; 4]);
        let pinned = [0x1111_1111, 0x2222_2222, 0x3333_3333];
        // Subleaf 0 pins EBX through EDX and leaves EAX to the hypervisor.
        let listed = CpuidLeaf::new(4, [0, pinned[0], pinned[1], pinned[2]])
            .indexed(0)
            .masked([0, !0, !0, !0]);

        let probe = probe_partition(&mshv, host).unwrap();
        super::super::register_cpuid_result(&probe.vmfd, &listed).unwrap();
        let refused = super::super::register_cpuid_result(&probe.vmfd, &whole).unwrap_err();
        let leaf4 = read(&probe);
        println!("subleaf first: {leaf4:#010x?}; whole leaf refused: {refused}");
        assert_eq!(leaf4[0][1..], pinned);
        assert_eq!(leaf4[0][0], native[0][0], "unmasked bits stay native");
        assert_eq!(leaf4[1..], native[1..], "unnamed subleaves stay native");

        let probe = probe_partition(&mshv, host).unwrap();
        super::super::register_cpuid_result(&probe.vmfd, &whole).unwrap();
        let refused = super::super::register_cpuid_result(&probe.vmfd, &listed).unwrap_err();
        let leaf4 = read(&probe);
        println!("whole leaf first: {leaf4:#010x?}; subleaf refused: {refused}");
        assert!(leaf4.iter().all(|subleaf| *subleaf == [0; 4]));
    }

    /// Creates a partition with this host's profile features and no CPUID
    /// results, and checks that the banks alone present every mapped feature
    /// as the profile pins it. The CPUID results supply the rest.
    #[test]
    #[ignore = "requires /dev/mshv"]
    fn host_profile_features_present_the_profile() {
        let profile = cpu_profile::select_auto(&HostCpuSignature::current()).unwrap();
        let mshv = Mshv::new().unwrap();
        let host = host_features(&mshv).unwrap();
        let features = time_abi_features(profile, host).unwrap();
        println!(
            "{}: bank0 {:#x} bank1 {:#x} xsave {:#x} (host {:#x} {:#x} {:#x})",
            profile.id(),
            features.banks[0],
            features.banks[1],
            features.xsave,
            host.banks[0],
            host.banks[1],
            host.xsave
        );
        let probe = probe_partition(&mshv, features).unwrap();
        let mut failures = Vec::new();
        let mut unmapped = 0;
        for entry in profile.cpuid() {
            let (leaf, subleaf) = entry.key();
            let presented = probe
                .vp
                .get_cpuid_values(leaf, subleaf.unwrap_or(0), 0, 0)
                .unwrap();
            for (register, &value) in presented.iter().enumerate() {
                let mask = entry.masks()[register];
                let differs = (value ^ entry.values()[register]) & mask;
                for bit in (0..32).filter(|bit| differs & (1 << bit) != 0) {
                    let at = CpuidBit::new(leaf, subleaf.unwrap_or(0), register, bit);
                    let feature = HV_FEATURES
                        .iter()
                        .find(|feature| feature.cpuid_for(profile.cpu_vendor()) == Some(at));
                    match feature {
                        Some(feature) if !feature.is_time_policy() => failures.push(format!(
                            "{at} ({}): presented {}, pinned {}",
                            feature.name(),
                            value >> bit & 1,
                            entry.values()[register] >> bit & 1
                        )),
                        _ => unmapped += 1,
                    }
                }
            }
        }
        println!(
            "{}: {unmapped} pinned bits that no feature maps differ from the hypervisor's own CPUID",
            profile.id()
        );
        assert!(failures.is_empty(), "{failures:#?}");
    }

    /// A partition with the derived features supports exactly the profile's
    /// XSAVE state components: its own `CPUID.(0xd,0)` (XCR0) and
    /// `CPUID.(0xd,1)` (IA32_XSS), before any CPUID result applies. A PKRU
    /// state the profile lacks is reported rather than failed, because no MSHV
    /// feature controls it.
    #[test]
    #[ignore = "requires /dev/mshv"]
    fn host_profile_features_enable_the_profile_xsave_components() {
        const XCR0_PKRU: u64 = 1 << 9;
        let profile = cpu_profile::select_auto(&HostCpuSignature::current()).unwrap();
        let mshv = Mshv::new().unwrap();
        let host = host_features(&mshv).unwrap();
        let features = time_abi_features(profile, host).unwrap();
        let probe = probe_partition(&mshv, features).unwrap();
        let partition = |subleaf, low: usize| {
            let registers = probe.vp.get_cpuid_values(0xd, subleaf, 0, 0).unwrap();
            u64::from(registers[low]) | u64::from(registers[3]) << 32
        };
        let host_cpuid = host_cpuid_table();
        let root = |subleaf, low: usize| {
            cpu_profile::cpuid::lookup(&host_cpuid, 0xd, subleaf).map_or(0, |registers| {
                u64::from(registers[low]) | u64::from(registers[3]) << 32
            })
        };
        let (xcr0, xss) = (partition(0, 0), partition(1, 2));
        println!(
            "{}: xsave features {:#x}: XCR0 {xcr0:#x} (profile {:#x}, root {:#x}), \
             IA32_XSS {xss:#x} (profile {:#x}, root {:#x})",
            profile.id(),
            features.xsave,
            profile.xcr0(),
            root(0, 0),
            profile.xss(),
            root(1, 2)
        );
        let unpinned_pkru = XCR0_PKRU & !profile.xcr0();
        if xcr0 & unpinned_pkru != 0 {
            println!(
                "{}: the partition supports PKRU, which the profile lacks",
                profile.id()
            );
        }
        assert_eq!(xcr0 & !unpinned_pkru, profile.xcr0(), "XCR0 components");
        assert_eq!(xss, profile.xss(), "IA32_XSS components");
    }
}
