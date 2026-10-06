// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! WHP processor features derived from a CPU profile, and the CPU surface
//! that WHP supports.
//!
//! A WHP partition presents a CPU feature only when the feature's processor
//! feature bank bit (or XSAVE feature bit) is set, and some of those bits also
//! decide which MSRs the guest may use (for example `IA32_SPEC_CTRL`).
//! [`profile_features`] derives the partition's features from its CPU profile
//! with the Hyper-V feature table that MSHV shares
//! ([`hv_banks::HV_FEATURES`]): it starts from the features WHP offers, clears
//! every bit whose CPUID feature the profile clears, and restricts the
//! `IA32_ARCH_CAPABILITIES` bits that the banks derive to the profile's pinned
//! value. Features that no bit controls (for example PKU), the descriptive
//! leaves (caches, the brand string), and the time policy bits a host lacks
//! (ARAT and invariant TSC on Azure) reach the guest through the time ABI's
//! CPUID results: the partition's complete effective CPUID, which
//! `configure` programs with `CpuidResultList2`.
//!
//! [`supported_surface`] reports what WHP supports without a probe partition.
//! The hardware tests check the table and the surface against what WHP
//! presents on a host.

use cpu_profile::CpuProfile;
use cpu_profile::cpuid::CpuidEntry;
use cpu_profile::hv_banks;
use cpu_profile::hv_banks::HvFeatures;
use hvdef::HvX64PartitionProcessorFeatures1 as Bank1;
use hvdef::HvX64PartitionProcessorXsaveFeatures as XsaveBank;
use virt::time_abi::TimeAbiCode;
use virt::time_abi::TimeAbiError;
use virt::time_abi::surface::SupportedCpuSurface;
use virt::time_abi::surface::SupportedMsrValue;

/// `IA32_ARCH_CAPABILITIES`.
#[cfg(test)]
const MSR_ARCH_CAPABILITIES: u32 = 0x10a;

const EAX: usize = 0;
#[cfg(test)]
const EBX: usize = 1;
const ECX: usize = 2;
const EDX: usize = 3;

/// The extended feature leaf.
#[cfg(test)]
const X1: u32 = 0x8000_0001;
/// The advanced power management leaf.
#[cfg(test)]
const X7: u32 = 0x8000_0007;
/// The AMD extended feature identifiers leaf.
const X8: u32 = 0x8000_0008;

/// WHP processor features: feature banks 0 and 1, and the XSAVE features.
pub(crate) type WhpFeatures = HvFeatures;

/// Returns the WHP features of a partition that presents `profile`, given
/// the features WHP offers ([`hv_banks::profile_features`]). Fails with
/// `E_PROFILE_UNSUPPORTED` when WHP cannot present the profile's features
/// or its `IA32_ARCH_CAPABILITIES`.
pub(crate) fn profile_features(
    profile: &CpuProfile,
    available: WhpFeatures,
) -> Result<WhpFeatures, TimeAbiError> {
    hv_banks::profile_features(profile, available)
        .map_err(|err| TimeAbiError::new(TimeAbiCode::ProfileUnsupported, err.message))
}

/// Returns whether a partition with `available` features supports XSAVE
/// state component `component` (an XCR0 or `IA32_XSS` bit). PKRU has no WHP
/// feature: like PKU, it follows the host.
fn xsave_component_enabled(component: u32, available: WhpFeatures) -> bool {
    let xsave = XsaveBank::from(available.xsave);
    let bank1 = Bank1::from(available.banks[1]);
    if !xsave.xsave_support() {
        return false;
    }
    match component {
        // x87, SSE, and PKRU.
        0 | 1 | 9 => true,
        2 => xsave.avx_support(),
        3 | 4 => xsave.mpx_support(),
        5..=7 => xsave.avx512_support(),
        // CET user and supervisor state.
        11 | 12 => {
            xsave.xsave_supervisor_support() && (bank1.cet_ss_support() || bank1.cet_ibt_support())
        }
        17 | 18 => xsave.amx_tile_support(),
        _ => false,
    }
}

/// Returns the CPU surface that WHP supports for a partition with every
/// feature in `available`, without a probe partition: `host`, the host's
/// CPUID as the root partition sees it, with
///
/// - every CPUID feature bit that a WHP feature controls cleared where
///   `available` lacks the feature ([`hv_banks::restrict_cpuid_to_features`]);
/// - only the XSAVE state components that `available` enables, in leaf
///   `0xD` (subleaves 0 and 1, and the component subleaves, whose layout is
///   the host's);
/// - no hypervisor-range leaf, since the partition has no synthetic
///   processor features.
///
/// The guest physical address width, in the surface and in CPUID
/// `0x80000008` EAX[7:0], is `physical_address_width`, the partition's: WHP's
/// limit can differ from the host's. The other bits, the maximum leaves, and
/// the linear address width are the host's. `IA32_ARCH_CAPABILITIES` is what
/// the banks derive.
pub(crate) fn supported_surface(
    host: &[CpuidEntry],
    available: WhpFeatures,
    physical_address_width: u8,
) -> SupportedCpuSurface {
    let components = (0..64)
        .filter(|&component| xsave_component_enabled(component, available))
        .fold(0u64, |mask, component| mask | 1 << component);
    let mut host: Vec<CpuidEntry> = host
        .iter()
        .filter(|entry| !(0x4000_0000..=0x4fff_ffff).contains(&entry.leaf.0))
        .cloned()
        .collect();
    hv_banks::restrict_cpuid_to_features(&mut host, available);

    let cpuid = host
        .iter()
        .filter_map(|entry| {
            let (leaf, subleaf) = entry.key();
            let index = subleaf.unwrap_or(0);
            if leaf == 0xd && (2..64).contains(&index) && components & (1 << index) == 0 {
                return None;
            }
            let mut registers = entry.registers();
            match (leaf, index) {
                (0xd, 0) => {
                    registers[EAX] &= components as u32;
                    registers[EDX] &= (components >> 32) as u32;
                }
                (0xd, 1) => {
                    registers[ECX] &= components as u32;
                    registers[EDX] &= (components >> 32) as u32;
                }
                (X8, _) => {
                    registers[EAX] = (registers[EAX] & !0xff) | u32::from(physical_address_width);
                }
                _ => {}
            }
            let result = virt::CpuidLeaf::new(leaf, registers);
            Some(match subleaf {
                Some(subleaf) => result.indexed(subleaf),
                None => result,
            })
        })
        .collect();

    let arch_capabilities = hv_banks::arch_capabilities_msr(available.banks);
    SupportedCpuSurface {
        cpuid,
        physical_address_width,
        msrs: vec![SupportedMsrValue {
            index: arch_capabilities.index,
            supported: arch_capabilities.supported,
            controllable: arch_capabilities.controllable,
        }],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hvdef::HvX64PartitionProcessorFeatures as Bank0;

    /// Feature capabilities that WHP reported on the fleet's hosts (CPU
    /// fingerprints): the bare-metal Skylake-SP host and the 8370C
    /// runners (Ice Lake, nested).
    const SKYLAKE_SP_WHP_HOST: WhpFeatures = WhpFeatures {
        banks: [0x1001_f9ff_e7f7_859f, 0x0000_000f_1086_0063],
        xsave: 0x3fff,
    };
    const AZURE_8370C: WhpFeatures = WhpFeatures {
        banks: [0x2e0a_8bff_e7f7_859f, 0x0001_000e_0000_00f1],
        xsave: 0x7f_ffdf,
    };

    #[test]
    fn profiles_derive_and_shortfalls_are_unsupported() {
        let skylake = cpu_profile::pinned("intel.skylake-sp.v1").unwrap();
        profile_features(skylake, SKYLAKE_SP_WHP_HOST).unwrap();
        let emerald = cpu_profile::pinned("intel.emeraldrapids.v1").unwrap();
        let error = profile_features(emerald, SKYLAKE_SP_WHP_HOST).unwrap_err();
        assert_eq!(error.code, TimeAbiCode::ProfileUnsupported);
        assert!(error.message.contains("avx512"), "{error}");
    }

    #[test]
    fn the_supported_surface_follows_the_capability() {
        // A host that reports every feature, AVX-512 and AMX state, and a
        // hypervisor leaf.
        let mut host = vec![
            CpuidEntry::new(0, None, [0xd, 0x756e_6547, 0x6c65_746e, 0x4965_6e69]),
            CpuidEntry::new(1, None, [0x0005_0654, 0, !0, !0]),
            CpuidEntry::new(7, Some(0), [0, !0, !0, !0]),
            CpuidEntry::new(0xd, Some(0), [0x6_02e7, 0x2b00, 0x2b00, 0]),
            CpuidEntry::new(0xd, Some(1), [0xf, 0, 0x1800, 0]),
            CpuidEntry::new(0x4000_0000, None, [0x4000_000b, 1, 2, 3]),
            CpuidEntry::new(X8, None, [0x3_302e, 0, 0, 0]),
        ];
        for component in [2, 5, 6, 7, 9, 17, 18] {
            host.push(CpuidEntry::new(0xd, Some(component), [8, 64, 0, 0]));
        }
        host.sort_by_key(CpuidEntry::key);
        let mut available = AZURE_8370C;
        available.xsave &= !XsaveBank::new().with_avx512_support(true).into_bits();
        available.banks[0] &= !Bank0::new().with_smep_support(true).into_bits();
        let surface = supported_surface(&host, available, 0x2e);
        let lookup = |leaf: u32, subleaf: u32| {
            surface
                .cpuid
                .iter()
                .find(|result| result.matches(leaf, subleaf))
                .map(|result| result.result)
        };
        // A feature WHP does not offer is cleared; one it offers stays.
        assert_eq!(lookup(7, 0).unwrap()[EBX] & (1 << 7), 0, "SMEP");
        assert_ne!(lookup(7, 0).unwrap()[EBX] & 1, 0, "FSGSBASE");
        assert_eq!(lookup(7, 0).unwrap()[EBX] & (1 << 16), 0, "AVX512F");
        // Bits no WHP feature controls keep the host's value.
        assert_ne!(lookup(1, 0).unwrap()[EDX] & 1, 0, "FPU");
        // Only the XSAVE components the features enable remain: no AVX-512
        // and no AMX (the 8370C offers no AMX tiles).
        assert_eq!(lookup(0xd, 0).unwrap()[EAX], 0x207);
        assert!(lookup(0xd, 5).is_none() && lookup(0xd, 17).is_none());
        assert_eq!(lookup(0xd, 2).unwrap(), [8, 64, 0, 0]);
        // No hypervisor leaf; the physical address width is the partition's.
        assert!(lookup(0x4000_0000, 0).is_none());
        assert_eq!(surface.physical_address_width, 0x2e);
        let narrow = supported_surface(&host, available, 0x27);
        let x8 = narrow
            .cpuid
            .iter()
            .find(|result| result.function == X8)
            .unwrap();
        assert_eq!(x8.result[EAX], 0x3_3027);
        assert_eq!(surface.msrs.len(), 1);
        assert_eq!(surface.msrs[0].index, MSR_ARCH_CAPABILITIES);
    }
}

#[cfg(test)]
mod whp_tests {
    //! Tests that need WHP. Run them on a host with
    //! `cargo test -p virt_whp -- --ignored`.

    use super::*;
    use cpu_profile::CpuVendor;
    use cpu_profile::HostCpuSignature;
    use cpu_profile::hv_banks::CpuidBit;
    use cpu_profile::hv_banks::HV_FEATURES;
    use cpu_profile::hv_banks::HvFeature;
    use cpu_profile::hv_banks::HvFeatureWord;
    use whp::abi::WHV_CPUID_OUTPUT;
    use whp::abi::WHV_PROCESSOR_FEATURES;
    use whp::abi::WHV_PROCESSOR_FEATURES1;
    use whp::abi::WHV_PROCESSOR_XSAVE_FEATURES;
    use whp::abi::WHV_X64_CPUID_RESULT2;
    use whp::abi::WHV_X64_CPUID_RESULT2_FLAGS;
    use whp::abi::WHvX64CpuidResult2FlagSubleafSpecific;

    /// The leaves whose feature bits the WHP features control.
    const FEATURE_LEAVES: [(u32, u32); 14] = [
        (1, 0),
        (6, 0),
        (7, 0),
        (7, 1),
        (7, 2),
        (0xd, 0),
        (0xd, 1),
        (0x14, 0),
        (0x19, 0),
        (0x24, 0),
        (X1, 0),
        (X7, 0),
        (X8, 0),
        (0x8000_0021, 0),
    ];

    fn available() -> WhpFeatures {
        let banks = whp::capabilities::processor_features().unwrap();
        let xsave = whp::capabilities::processor_xsave_features().unwrap();
        WhpFeatures {
            banks: [banks.bank0.0, banks.bank1.0],
            xsave: xsave.0,
        }
    }

    /// Creates a partition with `vp_count` VPs, `features`, and the CPUID
    /// `results`.
    fn probe_partition_with(
        features: WhpFeatures,
        vp_count: u32,
        results: &[WHV_X64_CPUID_RESULT2],
    ) -> Result<whp::Partition, whp::WHvError> {
        let mut config = whp::PartitionConfig::new()?;
        config.set_property(whp::PartitionProperty::ProcessorCount(vp_count))?;
        config.set_property(whp::PartitionProperty::LocalApicEmulationMode(
            whp::abi::WHvX64LocalApicEmulationModeXApic,
        ))?;
        let mut banks = whp::capabilities::processor_features()?;
        banks.bank0 = WHV_PROCESSOR_FEATURES(features.banks[0]);
        banks.bank1 = WHV_PROCESSOR_FEATURES1(features.banks[1]);
        config.set_property(whp::PartitionProperty::ProcessorFeaturesBanks(banks))?;
        config.set_property(whp::PartitionProperty::ProcessorXsaveFeatures(
            WHV_PROCESSOR_XSAVE_FEATURES(features.xsave),
        ))?;
        if !results.is_empty() {
            config.set_property(whp::PartitionProperty::CpuidResultList2(results))?;
        }
        let partition = config.create()?;
        for vp in 0..vp_count {
            partition.create_vp(vp).create()?;
        }
        Ok(partition)
    }

    /// Creates a one-VP partition with `features`.
    fn probe_partition(features: WhpFeatures) -> Result<whp::Partition, whp::WHvError> {
        probe_partition_with(features, 1, &[])
    }

    fn read_vp(partition: &whp::Partition, vp: u32, leaf: u32, subleaf: u32) -> [u32; 4] {
        let output = partition.vp(vp).get_cpuid_output(leaf, subleaf).unwrap();
        [output.Eax, output.Ebx, output.Ecx, output.Edx]
    }

    fn read(partition: &whp::Partition, leaf: u32, subleaf: u32) -> [u32; 4] {
        read_vp(partition, 0, leaf, subleaf)
    }

    fn feature_cpuid(partition: &whp::Partition) -> Vec<[u32; 4]> {
        FEATURE_LEAVES
            .iter()
            .map(|&(leaf, subleaf)| read(partition, leaf, subleaf))
            .collect()
    }

    /// The CPUID bits set in `base` and clear in `probe`.
    fn removed(base: &[[u32; 4]], probe: &[[u32; 4]]) -> Vec<CpuidBit> {
        let mut bits = Vec::new();
        for (index, &(leaf, subleaf)) in FEATURE_LEAVES.iter().enumerate() {
            for register in 0..4 {
                // Leaf 0xD.0 reports XSAVE sizes and components, and leaf 7.0
                // EAX the last subleaf; neither holds features.
                if (leaf, subleaf) == (0xd, 0) || (leaf, subleaf, register) == (7, 0, EAX) {
                    continue;
                }
                let lost = base[index][register] & !probe[index][register];
                for bit in 0..32 {
                    if lost & (1 << bit) != 0 {
                        bits.push(CpuidBit::new(leaf, subleaf, register, bit));
                    }
                }
            }
        }
        bits
    }

    /// The vendor of this host's CPU, whose CPUID bits the features control.
    fn host_vendor() -> CpuVendor {
        CpuVendor::from_cpuid_vendor(&HostCpuSignature::current().vendor())
            .unwrap_or(CpuVendor::Intel)
    }

    /// Whether another feature maps the same CPUID bit on `vendor`'s CPUs
    /// (as IBRS and IBPB do on Intel's): WHP may derive the bit from either.
    fn shared(feature: &HvFeature, vendor: CpuVendor) -> bool {
        feature.cpuid_for(vendor).is_some_and(|bit| {
            HV_FEATURES
                .iter()
                .any(|other| !std::ptr::eq(other, feature) && other.cpuid_for(vendor) == Some(bit))
        })
    }

    fn list(bits: &[CpuidBit]) -> String {
        if bits.is_empty() {
            return "none".to_owned();
        }
        bits.iter()
            .map(|bit| bit.to_string())
            .collect::<Vec<_>>()
            .join(" ")
    }

    /// Clears each feature bit WHP offers, one at a time, and checks that the
    /// CPUID bit the table maps to it on this host's vendor disappears.
    /// Prints what every bit controls on this host.
    #[test]
    #[ignore = "requires WHP"]
    fn features_control_the_mapped_cpuid_bits() {
        let vendor = host_vendor();
        let available = available();
        let base = feature_cpuid(&probe_partition(available).unwrap());
        println!(
            "{vendor}: available: bank0 {:#x} bank1 {:#x} xsave {:#x}",
            available.banks[0], available.banks[1], available.xsave
        );
        let mut failures = Vec::new();
        for bank in HvFeatureWord::ALL {
            let word = available.word(bank);
            for bit in (0..64).filter(|bit| word & (1 << bit) != 0) {
                let mask = 1u64 << bit;
                let feature = HV_FEATURES
                    .iter()
                    .find(|feature| feature.word == bank && feature.mask == mask);
                let name = feature.map_or("(unmapped)", |feature| feature.name());
                let mut features = available;
                *features.word_mut(bank) &= !mask;
                let probe = match probe_partition(features) {
                    Ok(partition) => feature_cpuid(&partition),
                    Err(err) => {
                        println!("{} bit {bit} {name}: rejected: {err}", bank.name());
                        continue;
                    }
                };
                let removed = removed(&base, &probe);
                let mut verdict = "";
                if let Some((feature, cpuid)) =
                    feature.and_then(|feature| Some((feature, feature.cpuid_for(vendor)?)))
                {
                    let index = FEATURE_LEAVES
                        .iter()
                        .position(|&key| key == (cpuid.leaf, cpuid.subleaf))
                        .unwrap();
                    let offered = base[index][cpuid.register] & (1 << cpuid.bit) != 0;
                    if offered && !shared(feature, vendor) && !removed.contains(&cpuid) {
                        verdict = " MISMATCH";
                        failures.push(format!(
                            "{} bit {bit} {name}: expected {} removed {}",
                            bank.name(),
                            cpuid,
                            list(&removed)
                        ));
                    }
                }
                println!(
                    "{} bit {bit} {name}: removes {}{verdict}",
                    bank.name(),
                    list(&removed)
                );
            }
        }
        assert!(failures.is_empty(), "{failures:#?}");
    }

    /// Derives the features for this host's profile and reports the profile
    /// bits that the features alone do not present (the CPUID results must
    /// supply them). Fails if a mapped feature bit disagrees.
    #[test]
    #[ignore = "requires WHP"]
    fn host_profile_features_present_the_profile() {
        let profile = match cpu_profile::select_auto(&HostCpuSignature::current()) {
            Ok(profile) => profile,
            Err(err) => {
                println!("skipped: no profile for this host: {err}");
                return;
            }
        };
        let available = available();
        let features = profile_features(profile, available).unwrap();
        println!(
            "{}: bank0 {:#x} -> {:#x}, bank1 {:#x} -> {:#x}, xsave {:#x} -> {:#x}",
            profile.id(),
            available.banks[0],
            features.banks[0],
            available.banks[1],
            features.banks[1],
            available.xsave,
            features.xsave
        );
        let partition = probe_partition(features).unwrap();
        let mut failures = Vec::new();
        for entry in profile.cpuid() {
            let (leaf, subleaf) = entry.key();
            let subleaf = subleaf.unwrap_or(0);
            let actual = read(&partition, leaf, subleaf);
            for register in 0..4 {
                let mask = entry.masks()[register];
                let differs = (actual[register] ^ entry.values()[register]) & mask;
                if differs == 0 {
                    continue;
                }
                let bits: Vec<_> = (0..32)
                    .filter(|bit| differs & (1 << bit) != 0)
                    .map(|bit| CpuidBit::new(leaf, subleaf, register, bit))
                    .collect();
                let mapped: Vec<_> = bits
                    .iter()
                    .filter(|&&bit| {
                        HV_FEATURES.iter().any(|feature| {
                            feature.cpuid_for(profile.cpu_vendor()) == Some(bit)
                                && !feature.is_time_policy()
                        })
                    })
                    .copied()
                    .collect();
                println!(
                    "{leaf:#x}.{subleaf} {}: actual {:#010x} profile {:#010x} mask {mask:#010x} ({})",
                    ["eax", "ebx", "ecx", "edx"][register],
                    actual[register],
                    entry.values()[register],
                    list(&bits)
                );
                if !mapped.is_empty() {
                    failures.push(list(&mapped));
                }
            }
        }
        assert!(failures.is_empty(), "mapped bits differ: {failures:?}");
    }

    fn output(registers: [u32; 4]) -> WHV_CPUID_OUTPUT {
        WHV_CPUID_OUTPUT {
            Eax: registers[0],
            Ebx: registers[1],
            Ecx: registers[2],
            Edx: registers[3],
        }
    }

    fn result2(
        leaf: u32,
        subleaf: Option<u32>,
        vp: Option<u32>,
        value: [u32; 4],
        mask: [u32; 4],
    ) -> WHV_X64_CPUID_RESULT2 {
        let mut flags = WHV_X64_CPUID_RESULT2_FLAGS(0);
        if subleaf.is_some() {
            flags |= WHvX64CpuidResult2FlagSubleafSpecific;
        }
        if vp.is_some() {
            flags |= whp::abi::WHvX64CpuidResult2FlagVpSpecific;
        }
        WHV_X64_CPUID_RESULT2 {
            Function: leaf,
            Index: subleaf.unwrap_or(0),
            VpIndex: vp.unwrap_or(0),
            Flags: flags,
            Output: output(value),
            Mask: output(mask),
        }
    }

    /// Programs CPUID results with `CpuidResultList2` (a whole descriptive
    /// leaf, a masked feature bit the banks set, a masked bit, and a subleaf
    /// before setup; per-VP values after the VPs exist) and checks what
    /// `WHvGetVirtualProcessorCpuidOutput` returns.
    #[test]
    #[ignore = "requires WHP"]
    fn cpuid_result_list2_reaches_the_cpuid_output() {
        let available = available();
        let native = probe_partition(available).unwrap();
        let native_7 = read(&native, 7, 0);
        let native_x7 = read(&native, X7, 0);
        let native_xsave1 = read(&native, 0xd, 1);
        drop(native);
        assert_ne!(native_7[EBX] & 1, 0, "the host lacks FSGSBASE");

        let descriptors = [0x7603_6301, 0x00f0_b5ff, 0, 0x00c3_0000];
        let results = [
            result2(2, None, None, descriptors, [!0; 4]),
            result2(7, Some(0), None, [0; 4], [0, 1, 0, 0]),
            result2(X7, None, None, [0, 0, 0, 1 << 8], [0, 0, 0, 1 << 8]),
            result2(0xd, Some(1), None, [0x5, 0, 0, 0], [0xf, 0, 0, 0]),
        ];
        let partition = match probe_partition_with(available, 2, &results) {
            Ok(partition) => partition,
            Err(err) => panic!("WHP rejects CpuidResultList2: {err}"),
        };
        let mut expected_7 = native_7;
        expected_7[EBX] &= !1;
        let mut expected_x7 = native_x7;
        expected_x7[EDX] |= 1 << 8;
        let mut expected_xsave1 = native_xsave1;
        expected_xsave1[EAX] = (native_xsave1[EAX] & !0xf) | 0x5;
        let mut checks = vec![
            ("leaf 2 (whole leaf)", read(&partition, 2, 0), descriptors),
            (
                "leaf 7.0 (FSGSBASE masked clear)",
                read(&partition, 7, 0),
                expected_7,
            ),
            (
                "leaf 0x80000007 (invariant TSC masked set)",
                read(&partition, X7, 0),
                expected_x7,
            ),
            (
                "leaf 0xd.1 (subleaf, masked)",
                read(&partition, 0xd, 1),
                expected_xsave1,
            ),
            ("leaf 2 on VP 1", read_vp(&partition, 1, 2, 0), descriptors),
        ];
        let per_vp = [
            result2(0x16, None, Some(0), [100, 200, 300, 0], [!0; 4]),
            result2(0x16, None, Some(1), [101, 201, 301, 1], [!0; 4]),
        ];
        let mut all = results.to_vec();
        all.extend_from_slice(&per_vp);
        match partition.set_property(whp::PartitionProperty::CpuidResultList2(&all)) {
            Ok(()) => {
                checks.push((
                    "leaf 0x16 on VP 0",
                    read_vp(&partition, 0, 0x16, 0),
                    [100, 200, 300, 0],
                ));
                checks.push((
                    "leaf 0x16 on VP 1",
                    read_vp(&partition, 1, 0x16, 0),
                    [101, 201, 301, 1],
                ));
            }
            Err(err) => println!("per-VP results after setup: rejected: {err}"),
        }
        let mut failures = Vec::new();
        for (name, actual, expected) in checks {
            let verdict = if actual == expected { "ok" } else { "DIFFERS" };
            println!("{name}: {actual:08x?} expected {expected:08x?} {verdict}");
            if actual != expected {
                failures.push(name);
            }
        }
        assert!(failures.is_empty(), "{failures:?}");
    }

    /// Compares the cheap supported surface (host CPUID and WHP's
    /// capabilities) with the probe partition's (`--cpu-fingerprint`): every
    /// pinned profile gets the same `verify_support` verdict from both, and
    /// the host's own profile passes.
    #[test]
    #[ignore = "requires WHP"]
    fn cheap_surface_matches_the_probe_surface() {
        let fingerprint = crate::fingerprint::cpu_fingerprint().unwrap();
        let probe = cpu_profile::HostCpuSurface::from_fingerprint(&fingerprint);
        let started = std::time::Instant::now();
        let cheap = supported_surface(
            crate::time_abi::host_cpuid(),
            available(),
            probe.physical_address_width,
        );
        let elapsed = started.elapsed();
        let mut cpuid: Vec<CpuidEntry> = cheap
            .cpuid
            .iter()
            .map(|leaf| CpuidEntry::new(leaf.function, leaf.index, leaf.result))
            .collect();
        cpu_profile::cpuid::normalize(&mut cpuid);
        let cheap = cpu_profile::HostCpuSurface {
            cpuid,
            presentation: cpu_profile::CpuidPresentation::PassThroughHostView,
            physical_address_width: cheap.physical_address_width,
            msrs: cheap
                .msrs
                .iter()
                .map(|msr| cpu_profile::SupportedMsr {
                    index: msr.index,
                    supported: msr.supported,
                    controllable: msr.controllable,
                })
                .collect(),
        };
        println!(
            "cheap surface: {} entries in {} us; probe surface: {} entries; widths {} and {}",
            cheap.cpuid.len(),
            elapsed.as_micros(),
            probe.cpuid.len(),
            cheap.physical_address_width,
            probe.physical_address_width
        );
        let host = cpu_profile::select_auto(&HostCpuSignature::current()).ok();
        let mut mismatches = Vec::new();
        for profile in cpu_profile::pinned_profiles() {
            // Where the two surfaces differ on a leaf the profile lists.
            for entry in profile.cpuid() {
                let (leaf, subleaf) = entry.key();
                let subleaf = subleaf.unwrap_or(0);
                let ours =
                    cpu_profile::cpuid::lookup(&cheap.cpuid, leaf, subleaf).unwrap_or_default();
                let theirs =
                    cpu_profile::cpuid::lookup(&probe.cpuid, leaf, subleaf).unwrap_or_default();
                for register in 0..4 {
                    let differs = ours[register] ^ theirs[register];
                    if differs != 0 {
                        println!(
                            "{}: {leaf:#x}.{subleaf} register {register}: cheap {:#010x} probe {:#010x} pinned mask {:#010x}",
                            profile.id(),
                            ours[register],
                            theirs[register],
                            entry.masks()[register]
                        );
                    }
                }
            }
            let ours = cpu_profile::support_violations(profile, &cheap);
            let theirs = cpu_profile::support_violations(profile, &probe);
            println!("{}: cheap {ours:?}; probe {theirs:?}", profile.id());
            if ours.is_empty() != theirs.is_empty() {
                mismatches.push(profile.id());
            }
            if host.is_some_and(|host| host.id() == profile.id()) {
                assert!(ours.is_empty(), "{}: {ours:?}", profile.id());
            }
        }
        assert!(mismatches.is_empty(), "{mismatches:?}");
    }

    /// Times what a WHP `supported_cpu_surface()` costs with a probe
    /// partition: creating a one-VP partition with every feature WHP offers,
    /// reading its whole CPUID, and tearing it down.
    #[test]
    #[ignore = "requires WHP"]
    fn supported_surface_probe_cost() {
        for round in 0..5 {
            let start = std::time::Instant::now();
            let partition = probe_partition(available()).unwrap();
            let created = start.elapsed();
            let mut reads = 0;
            let entries = cpu_profile::cpuid::enumerate(|leaf, subleaf| {
                reads += 1;
                partition
                    .vp(0)
                    .get_cpuid_output(leaf, subleaf)
                    .map(|output| [output.Eax, output.Ebx, output.Ecx, output.Edx])
            })
            .unwrap();
            let enumerated = start.elapsed();
            drop(partition);
            let total = start.elapsed();
            println!(
                "round {round}: create {created:?}, {reads} reads ({} entries) {:?}, teardown {:?}, total {total:?}",
                entries.len(),
                enumerated - created,
                total - enumerated
            );
        }
    }

    /// A zeroed 4 KiB page that a test maps into a partition, freed on drop.
    /// Declare it before the partition, so that it outlives the partition's
    /// mapping of it, also when the test panics.
    struct TestPage {
        ptr: *mut u8,
        layout: std::alloc::Layout,
    }

    impl TestPage {
        fn new() -> Self {
            let layout = std::alloc::Layout::from_size_align(4096, 4096).unwrap();
            // SAFETY: the layout has a nonzero size.
            let ptr = unsafe { std::alloc::alloc_zeroed(layout) };
            assert!(!ptr.is_null());
            Self { ptr, layout }
        }
    }

    impl Drop for TestPage {
        fn drop(&mut self) {
            // SAFETY: `ptr` was allocated with `layout`.
            unsafe { std::alloc::dealloc(self.ptr, self.layout) };
        }
    }

    /// Runs `cpuid; hlt` in real mode at the reset vector and checks how
    /// `CpuidResultList2` reaches the guest: a leaf that also exits reports
    /// the programmed result as the exit's default result, which OpenVMM's
    /// exit handler starts from, and a leaf that does not exit returns it
    /// directly. The time ABI exits some profile leaves (1, 4, 6, 7, ...), so
    /// their profile values depend on the former.
    #[test]
    #[ignore = "requires WHP"]
    fn cpuid_result_list2_feeds_exit_defaults() {
        const EXIT_LEAF: u32 = 0x16;
        const PLAIN_LEAF: u32 = 0x8000_0002;
        let exit_value = [0x1111_0001, 0x2222_0002, 0x3333_0003, 0x4444_0004];
        let plain_value = [0x4141_4141, 0x4242_4242, 0x4343_4343, 0x4444_4444];
        let results = [
            result2(EXIT_LEAF, None, None, exit_value, [!0; 4]),
            result2(PLAIN_LEAF, None, None, plain_value, [!0; 4]),
        ];
        // At 0xffff0: cpuid; hlt; jmp back to the cpuid. Declared before the
        // partition, so that it is freed after the partition is dropped.
        let page = TestPage::new();
        let code = [0x0f, 0xa2, 0xf4, 0xeb, 0xfb];
        // SAFETY: the code fits in the page at offset 0xff0.
        unsafe { std::ptr::copy_nonoverlapping(code.as_ptr(), page.ptr.add(0xff0), code.len()) };
        let mut config = whp::PartitionConfig::new().unwrap();
        config
            .set_property(whp::PartitionProperty::ProcessorCount(1))
            .unwrap();
        config
            .set_property(whp::PartitionProperty::ExtendedVmExits(
                whp::abi::WHV_EXTENDED_VM_EXITS::X64CpuidExit,
            ))
            .unwrap();
        config
            .set_property(whp::PartitionProperty::CpuidExitList(&[EXIT_LEAF]))
            .unwrap();
        config
            .set_property(whp::PartitionProperty::CpuidResultList2(&results))
            .unwrap();
        let partition = config.create().unwrap();
        partition.create_vp(0).create().unwrap();

        let rwx = whp::abi::WHV_MAP_GPA_RANGE_FLAGS(
            whp::abi::WHvMapGpaRangeFlagRead.0
                | whp::abi::WHvMapGpaRangeFlagWrite.0
                | whp::abi::WHvMapGpaRangeFlagExecute.0,
        );
        // SAFETY: `page` is dropped after the partition, so its memory is not
        // reused while the partition maps it.
        unsafe { partition.map_range(None, page.ptr, 4096, 0xff000, rwx) }.unwrap();

        let vp = partition.vp(0);
        let registers = |vp: &whp::Processor<'_>| {
            [
                whp::Register64::Rax,
                whp::Register64::Rbx,
                whp::Register64::Rcx,
                whp::Register64::Rdx,
            ]
            .map(|register| vp.get_register(register).unwrap() as u32)
        };
        vp.set_register(whp::Register64::Rax, EXIT_LEAF.into())
            .unwrap();
        vp.set_register(whp::Register64::Rcx, 0).unwrap();
        let mut runner = vp.runner();

        // The exiting leaf: its default result is the programmed one.
        let (default, next_rip) = match runner.run().unwrap() {
            whp::Exit {
                vp_context,
                reason: whp::ExitReason::Cpuid(info),
            } => (
                [
                    info.DefaultResultRax as u32,
                    info.DefaultResultRbx as u32,
                    info.DefaultResultRcx as u32,
                    info.DefaultResultRdx as u32,
                ],
                vp_context.Rip + u64::from(vp_context.InstructionLength()),
            ),
            exit => panic!("expected a CPUID exit: {exit:#x?}"),
        };
        println!("exit leaf {EXIT_LEAF:#x}: default result {default:08x?}");
        // Complete the CPUID with the default result and continue to the HLT.
        for (register, value) in [
            whp::Register64::Rax,
            whp::Register64::Rbx,
            whp::Register64::Rcx,
            whp::Register64::Rdx,
        ]
        .into_iter()
        .zip(default)
        {
            vp.set_register(register, value.into()).unwrap();
        }
        vp.set_register(whp::Register64::Rip, next_rip).unwrap();
        match runner.run().unwrap().reason {
            whp::ExitReason::Halt => {}
            reason => panic!("expected a halt: {reason:#x?}"),
        }

        // The plain leaf: the guest gets the programmed result directly.
        vp.set_register(whp::Register64::Rax, PLAIN_LEAF.into())
            .unwrap();
        vp.set_register(whp::Register64::Rcx, 0).unwrap();
        match runner.run().unwrap().reason {
            whp::ExitReason::Halt => {}
            reason => panic!("expected a halt: {reason:#x?}"),
        }
        let plain = registers(&vp);
        println!("plain leaf {PLAIN_LEAF:#x}: guest result {plain:08x?}");

        assert_eq!(default, exit_value, "the exit's default result");
        assert_eq!(plain, plain_value, "the guest's result");
    }
}
