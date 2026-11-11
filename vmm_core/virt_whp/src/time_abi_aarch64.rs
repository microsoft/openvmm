// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! The ARM adapter of the existing optional CPU/time backend contract.
//!
//! Feature-bank equality verifies partition configuration, not effective
//! guest ID/cache enforcement. Published ARM ID registers have a native
//! partition-wide provider through `WHV_ANY_VP`, not an ordinary VP index.
//! Counter frequency is checked against the declared time contract. Cache
//! operating properties remain native/informational, without normative pins;
//! canonical-profile validation and qualification remain separate.
//! No diagnostic guest is installed.

use crate::Error;
use crate::WhpPartitionInner;
use crate::WhpResultExt;
use aarch64defs::SystemReg;
use hvdef::Vtl;
use inspect::Inspect;
use virt::ProtoPartitionConfig;
use virt::time_abi::BackendPreflight;
use virt::time_abi::TimeAbiCode;
use virt::time_abi::TimeAbiConfig;
use virt::time_abi::TimeAbiError;
use virt::time_abi::aarch64::CounterReading;
use whp::RegisterValue;

const CPU_ID_REGISTERS: [(SystemReg, whp::abi::WHV_REGISTER_NAME); 15] = [
    (
        SystemReg::ID_AA64PFR0_EL1,
        whp::abi::WHvArm64RegisterIdAa64Pfr0El1,
    ),
    (
        SystemReg::ID_AA64PFR1_EL1,
        whp::abi::WHvArm64RegisterIdAa64Pfr1El1,
    ),
    (
        SystemReg::ID_AA64PFR2_EL1,
        whp::abi::WHvArm64RegisterIdAa64Pfr2El1,
    ),
    (
        SystemReg::ID_AA64ISAR0_EL1,
        whp::abi::WHvArm64RegisterIdAa64Isar0El1,
    ),
    (
        SystemReg::ID_AA64ISAR1_EL1,
        whp::abi::WHvArm64RegisterIdAa64Isar1El1,
    ),
    (
        SystemReg::ID_AA64ISAR2_EL1,
        whp::abi::WHvArm64RegisterIdAa64Isar2El1,
    ),
    (
        SystemReg::ID_AA64MMFR0_EL1,
        whp::abi::WHvArm64RegisterIdAa64Mmfr0El1,
    ),
    (
        SystemReg::ID_AA64MMFR1_EL1,
        whp::abi::WHvArm64RegisterIdAa64Mmfr1El1,
    ),
    (
        SystemReg::ID_AA64MMFR2_EL1,
        whp::abi::WHvArm64RegisterIdAa64Mmfr2El1,
    ),
    (
        SystemReg::ID_AA64MMFR3_EL1,
        whp::abi::WHvArm64RegisterIdAa64Mmfr3El1,
    ),
    (
        SystemReg::ID_AA64MMFR4_EL1,
        whp::abi::WHvArm64RegisterIdAa64Mmfr4El1,
    ),
    (
        SystemReg::ID_AA64DFR0_EL1,
        whp::abi::WHvArm64RegisterIdAa64Dfr0El1,
    ),
    (
        SystemReg::ID_AA64DFR1_EL1,
        whp::abi::WHvArm64RegisterIdAa64Dfr1El1,
    ),
    (
        SystemReg::ID_AA64ZFR0_EL1,
        whp::abi::WHvArm64RegisterIdAa64Zfr0El1,
    ),
    (
        SystemReg::ID_AA64SMFR0_EL1,
        whp::abi::WHvArm64RegisterIdAa64Smfr0El1,
    ),
];

fn cpu_id_name(register: SystemReg) -> Result<whp::abi::WHV_REGISTER_NAME, TimeAbiError> {
    CPU_ID_REGISTERS
        .iter()
        .find_map(|&(id, name)| (id == register).then_some(name))
        .ok_or_else(|| {
            TimeAbiError::new(
                TimeAbiCode::ProfileUnsupported,
                format!("no published WHP partition-wide provider for {register:?}"),
            )
        })
}

fn read_cpu_id(partition: &whp::Partition, register: SystemReg) -> Result<u64, TimeAbiError> {
    let name = cpu_id_name(register)?;
    let mut value = [0u64.as_abi()];
    partition
        .vp(whp::abi::WHV_ANY_VP)
        .get_registers(&[name], &mut value)
        .map_err(|error| {
            TimeAbiError::new(
                TimeAbiCode::ProfileUnsupported,
                format!("reading partition-wide {register:?} through WHV_ANY_VP: {error}"),
            )
        })?;
    Ok(u64::from_abi(&value[0]))
}

/// Published native reads only: no instructions, code mappings or product
/// state. Destination presentation is checked again on the actual partition.
pub fn native_fingerprint() -> Result<virt::time_abi::aarch64::Fingerprint, Error> {
    let mut raw = whp::PartitionConfig::new().for_op("create ARM fingerprint configuration")?;
    raw.set_property(whp::PartitionProperty::ProcessorCount(1))
        .for_op("set ARM fingerprint CPU count")?;
    let features =
        whp::capabilities::processor_features().for_op("read ARM fingerprint capabilities")?;
    raw.set_property(whp::PartitionProperty::ProcessorFeaturesBanks(features))
        .for_op("set ARM fingerprint native features")?;
    raw.set_property(whp::PartitionProperty::GicParameters(
        whp::abi::WHV_ARM64_IC_PARAMETERS {
            EmulationMode: whp::abi::WHV_ARM64_IC_EMULATION_MODE::GicV3,
            Reserved: 0,
            GicV3Parameters: whp::abi::WHV_ARM64_IC_GIC_V3_PARAMETERS {
                GicdBaseAddress: 0xffff_0000,
                GitsTranslatorBaseAddress: 0,
                Reserved: 0,
                GicLpiIntIdBits: whp::abi::DEFAULT_GIC_LPI_INT_ID_BITS,
                GicPpiOverflowInterruptFromCntv:
                    whp::abi::DEFAULT_GIC_PPI_OVERFLOW_INTERRUPT_FROM_CNTV,
                GicPpiPerformanceMonitorsInterrupt: crate::WHP_PMU_GSIV,
                Reserved1: [0; 6],
            },
        },
    ))
    .for_op("set required ARM fingerprint GIC configuration")?;
    let partition = raw
        .create()
        .for_op("create ARM native fingerprint partition")?;
    let registers = CPU_ID_REGISTERS
        .iter()
        .map(|(register, _)| {
            Ok(virt::time_abi::aarch64::CpuIdRegister {
                register: *register,
                mask: u64::MAX,
                value: read_cpu_id(&partition, *register)?,
            })
        })
        .collect::<Result<Vec<_>, TimeAbiError>>()?;
    partition
        .create_vp(0)
        .create()
        .for_op("create ARM identity fingerprint VP")?;
    let mut value = [0u64.as_abi()];
    let result = partition
        .vp(0)
        .get_registers(&[whp::abi::WHvArm64RegisterMidrEl1], &mut value)
        .for_op("read native ARM guest model");
    partition.delete_vp(0).for_op("delete ARM fingerprint VP")?;
    result?;
    Ok(virt::time_abi::aarch64::Fingerprint {
        midr: u64::from_abi(&value[0]),
        registers,
        counter_hz: partition
            .tsc_frequency()
            .for_op("read native ARM counter frequency")?,
        backend_usable_gpa_bits: partition
            .physical_address_width()
            .for_op("read native ARM backend usable GPA width")?,
    })
}

fn verify_vp_models(
    partition: &whp::Partition,
    count: u32,
    model: cpu_profile::aarch64::CpuModel,
) -> Result<(), Error> {
    let mut violations = Vec::new();
    for index in 0..count {
        let mut value = [0u64.as_abi()];
        partition
            .vp(index)
            .get_registers(&[whp::abi::WHvArm64RegisterMidrEl1], &mut value)
            .for_op("read retained ARM VP model before any VP release")?;
        let midr = u64::from_abi(&value[0]);
        if !model.matches(midr) {
            violations.push(format!("VP {index} MIDR={midr:#x}"));
        }
    }
    if !violations.is_empty() {
        return Err(TimeAbiError::new(
            TimeAbiCode::ProfileUnsupported,
            format!("ARM model mismatch: {}", violations.join("; ")),
        )
        .into());
    }
    Ok(())
}

fn profile_field(
    profile: &virt::time_abi::aarch64::CpuProfile,
    register: SystemReg,
    shift: u32,
    width: u32,
    known: &[u8],
) -> Result<u8, TimeAbiError> {
    let mask = ((1u64 << width) - 1) << shift;
    let entry = profile
        .id_registers
        .iter()
        .find(|entry| entry.register == register && entry.mask & mask == mask)
        .ok_or_else(|| {
            TimeAbiError::new(
                TimeAbiCode::ProfileUnsupported,
                format!("ARM profile does not govern {register:?} mask={mask:#018x}"),
            )
        })?;
    let value = ((entry.value & mask) >> shift) as u8;
    if !known.contains(&value) {
        return Err(TimeAbiError::new(
            TimeAbiCode::ProfileUnsupported,
            format!(
                "unsupported architectural encoding {register:?} mask={mask:#018x} value={value:#x}"
            ),
        ));
    }
    Ok(value)
}

/// Translates architectural fields using SDK 26100's documented ARM bank
/// semantics. Encoded architectural levels are not bitsets. Missing ownership
/// or unsupported encodings fail instead of selecting host passthrough.
fn profile_features(
    profile: &virt::time_abi::aarch64::CpuProfile,
    native: whp::abi::WHV_PROCESSOR_FEATURES,
) -> Result<whp::ProcessorFeatures, TimeAbiError> {
    use whp::abi::WHV_PROCESSOR_FEATURES as B0;
    use whp::abi::WHV_PROCESSOR_FEATURES1 as B1;

    let mut features = whp::ProcessorFeatures::default();
    macro_rules! field {
        ($reg:ident, $shift:expr, $($known:expr),+) => {
            profile_field(profile, SystemReg::$reg, $shift, 4, &[$($known),+])?
        };
    }
    macro_rules! bank0 {
        ($flag:ident, $enabled:expr) => {
            if $enabled {
                features.bank0 |= B0::$flag;
            }
        };
    }
    macro_rules! bank1 {
        ($flag:ident, $enabled:expr) => {
            if $enabled {
                features.bank1 |= B1::$flag;
            }
        };
    }
    bank0!(Asid16, field!(ID_AA64MMFR0_EL1, 4, 0, 2) == 2);
    bank0!(TGran16, field!(ID_AA64MMFR0_EL1, 20, 0, 1, 2) != 0);
    bank0!(TGran64, field!(ID_AA64MMFR0_EL1, 24, 0, 15) == 0);
    let hafdbs = field!(ID_AA64MMFR1_EL1, 0, 0, 1, 2);
    bank0!(Haf, hafdbs >= 1);
    bank0!(Hdbs, hafdbs >= 2);
    let pan = field!(ID_AA64MMFR1_EL1, 20, 0, 1, 2);
    bank0!(Pan, pan >= 1);
    bank0!(AtS1E1, pan >= 2);
    bank0!(Uao, field!(ID_AA64MMFR2_EL1, 4, 0, 1) == 1);
    bank0!(El0Aarch32, field!(ID_AA64PFR0_EL1, 0, 1, 2) == 2);
    let fp = field!(ID_AA64PFR0_EL1, 16, 0, 1, 15);
    bank0!(Fp, fp != 15);
    bank0!(FpHp, fp == 1);
    let simd = field!(ID_AA64PFR0_EL1, 20, 0, 1, 15);
    bank0!(AdvSimd, simd != 15);
    bank0!(AdvSimdHp, simd == 1);
    let gic = field!(ID_AA64PFR0_EL1, 24, 0, 1, 3);
    bank0!(GicV3V4, gic != 0);
    bank0!(GicV41, gic == 3);
    bank0!(Ras, field!(ID_AA64PFR0_EL1, 28, 0, 1, 2) != 0);
    // This implementation does not yet support profiles with these extended
    // execution/state families; it must not claim to program them.
    field!(ID_AA64PFR0_EL1, 32, 0);
    field!(ID_AA64PFR1_EL1, 8, 0);
    field!(ID_AA64PFR1_EL1, 24, 0);
    field!(ID_AA64DFR0_EL1, 8, 0);
    let apa = field!(ID_AA64ISAR1_EL1, 4, 0, 5);
    bank0!(ApaEp2Fpc, apa == 5);
    let gpa = field!(ID_AA64ISAR1_EL1, 24, 0, 1);
    bank0!(Gpa, gpa == 1);
    if (apa == 0) != (gpa == 0) {
        return Err(TimeAbiError::new(
            TimeAbiCode::ProfileUnsupported,
            "ARM PAC address/generic policy is inconsistent",
        ));
    }
    for shift in [8, 28] {
        profile_field(profile, SystemReg::ID_AA64ISAR1_EL1, shift, 4, &[0])?;
    }
    for shift in [8, 12, 24] {
        profile_field(profile, SystemReg::ID_AA64ISAR2_EL1, shift, 4, &[0])?;
    }
    let aes = field!(ID_AA64ISAR0_EL1, 4, 0, 1, 2);
    bank0!(Aes, aes >= 1);
    bank0!(PolyMul, aes == 2);
    bank0!(Sha1, field!(ID_AA64ISAR0_EL1, 8, 0, 1) == 1);
    let sha2 = field!(ID_AA64ISAR0_EL1, 12, 0, 1, 2);
    bank0!(Sha256, sha2 >= 1);
    bank0!(Sha512, sha2 == 2);
    bank0!(Crc32, field!(ID_AA64ISAR0_EL1, 16, 0, 1) == 1);
    bank0!(Atomic, field!(ID_AA64ISAR0_EL1, 20, 0, 2) == 2);
    bank0!(Rdm, field!(ID_AA64ISAR0_EL1, 28, 0, 1) == 1);
    bank0!(Sha3, field!(ID_AA64ISAR0_EL1, 32, 0, 1) == 1);
    bank0!(Sm3, field!(ID_AA64ISAR0_EL1, 36, 0, 1) == 1);
    bank0!(Sm4, field!(ID_AA64ISAR0_EL1, 40, 0, 1) == 1);
    bank0!(Dp, field!(ID_AA64ISAR0_EL1, 44, 0, 1) == 1);
    bank0!(Fhm, field!(ID_AA64ISAR0_EL1, 48, 0, 1) == 1);
    let dpb = field!(ID_AA64ISAR1_EL1, 0, 0, 1, 2);
    bank0!(DcCvap, dpb >= 1);
    bank0!(DcCvadp, dpb == 2);
    bank0!(Jscvt, field!(ID_AA64ISAR1_EL1, 12, 0, 1) == 1);
    bank0!(Fcma, field!(ID_AA64ISAR1_EL1, 16, 0, 1) == 1);
    let rcpc = field!(ID_AA64ISAR1_EL1, 20, 0, 1, 2);
    bank0!(RcpcV83, rcpc >= 1);
    bank0!(RcpcV84, rcpc == 2);
    // Preserve only explicitly native-owned cache controls, not host ISA
    // passthrough. This is not a fabricated raw cache-register read.
    features.bank0 |= native & (B0::L1ipPipt | B0::DzPermitted | B0::Idc | B0::Dic);
    let ssbs = field!(ID_AA64PFR1_EL1, 4, 0, 1, 2);
    bank0!(Ssbs, ssbs >= 1);
    bank0!(SsbsRw, ssbs == 2);
    bank0!(Csv2, field!(ID_AA64PFR0_EL1, 56, 0, 1, 2, 3) != 0);
    bank0!(Csv3, field!(ID_AA64PFR0_EL1, 60, 0, 1) == 1);
    bank0!(Sb, field!(ID_AA64ISAR1_EL1, 36, 0, 1) == 1);
    let tlb = field!(ID_AA64ISAR0_EL1, 56, 0, 1, 2);
    bank0!(TlbiOs, tlb >= 1);
    bank0!(TlbiOsRange, tlb == 2);
    let flagm = field!(ID_AA64ISAR0_EL1, 52, 0, 1, 2);
    bank0!(FlagsM, flagm >= 1);
    bank0!(FlagsM2, flagm == 2);
    let bf16 = field!(ID_AA64ISAR1_EL1, 44, 0, 1, 2);
    bank0!(Bf16, bf16 >= 1);
    bank0!(Ebf16, bf16 == 2);
    bank1!(I8mm, field!(ID_AA64ISAR1_EL1, 52, 0, 1) == 1);
    bank1!(Frintts, field!(ID_AA64ISAR1_EL1, 32, 0, 1) == 1);
    bank1!(Specres, field!(ID_AA64ISAR1_EL1, 40, 0, 1) == 1);
    bank1!(Rpres, field!(ID_AA64ISAR2_EL1, 4, 0, 1) == 1);
    bank1!(Exs, field!(ID_AA64MMFR0_EL1, 44, 0, 1) == 1);
    bank1!(SpecSei, field!(ID_AA64MMFR1_EL1, 24, 0, 1) == 1);
    bank1!(Ets, field!(ID_AA64MMFR1_EL1, 36, 0, 1) == 1);
    bank1!(Afp, field!(ID_AA64MMFR1_EL1, 44, 0, 1) == 1);
    bank1!(Iesb, field!(ID_AA64MMFR2_EL1, 12, 0, 1) == 1);
    bank1!(Rng, field!(ID_AA64ISAR0_EL1, 60, 0, 1) == 1);
    bank1!(Lse2, field!(ID_AA64MMFR2_EL1, 32, 0, 1) == 1);
    bank1!(Idst, field!(ID_AA64MMFR2_EL1, 36, 0, 1) == 1);
    bank1!(Ccidx, field!(ID_AA64MMFR2_EL1, 20, 0, 1) == 1);
    bank1!(TtCnp, field!(ID_AA64MMFR2_EL1, 0, 0, 1) == 1);
    bank1!(Hpds, field!(ID_AA64MMFR1_EL1, 12, 0, 1) == 1);
    bank1!(SpecFpacc, field!(ID_AA64MMFR3_EL1, 60, 0, 1) == 1);
    Ok(features)
}

fn profile_physical_address_bits(
    profile: &virt::time_abi::aarch64::CpuProfile,
) -> Result<u32, TimeAbiError> {
    let parange = profile_field(
        profile,
        SystemReg::ID_AA64MMFR0_EL1,
        0,
        4,
        &[0, 1, 2, 3, 4, 5, 6, 7],
    )?;
    if cpu_profile::aarch64::physical_address_bits(parange) != Some(profile.physical_address_bits) {
        return Err(TimeAbiError::new(
            TimeAbiCode::ProfileUnsupported,
            "ARM profile PARange disagrees with its physical address requirement",
        ));
    }
    Ok(u32::from(profile.physical_address_bits))
}

fn require_supported_features(
    requested: [u64; 2],
    supported: [u64; 2],
) -> Result<(), TimeAbiError> {
    let missing = [requested[0] & !supported[0], requested[1] & !supported[1]];
    if missing != [0, 0] {
        return Err(TimeAbiError::new(
            TimeAbiCode::ProfileUnsupported,
            format!(
                "WHP lacks ARM profile features: bank0={:#018x}, bank1={:#018x}",
                missing[0], missing[1]
            ),
        ));
    }
    Ok(())
}

fn cpu_id_override(
    requirement: &virt::time_abi::aarch64::CpuIdRegister,
    actual: u64,
) -> Result<Option<u64>, TimeAbiError> {
    if actual & !requirement.mask != 0 {
        return Err(TimeAbiError::new(
            TimeAbiCode::CpuSurface,
            format!(
                "{:?} exposes nonzero reserved fields {:#x}",
                requirement.register,
                actual & !requirement.mask
            ),
        ));
    }
    let changed = (actual ^ requirement.value) & requirement.mask;
    if changed == 0 {
        return Ok(None);
    }
    // Only remove complete EL2/EL3 descriptions after native feature-bank
    // support has been checked. No ISA, mitigation, address, or cache value
    // may be manufactured through an ID override.
    let mut permitted = 0;
    if requirement.register == SystemReg::ID_AA64PFR0_EL1 {
        for mask in [0xf00, 0xf000] {
            if requirement.mask & mask == mask && requirement.value & mask == 0 {
                permitted |= mask;
            }
        }
    }
    if changed & !permitted != 0 {
        return Err(TimeAbiError::new(
            TimeAbiCode::CpuSurface,
            format!(
                "{:?} unsupported native field mismatch: mask={:#018x}, expected={:#018x}, actual={actual:#018x}",
                requirement.register, requirement.mask, requirement.value
            ),
        ));
    }
    Ok(Some(actual & !permitted))
}

#[derive(Inspect)]
pub(crate) struct WhpTimeAbi {
    #[inspect(skip)]
    config: TimeAbiConfig,
    #[inspect(skip)]
    features: [u64; 2],
}

impl WhpTimeAbi {
    pub(crate) fn verify_vp_models(
        &self,
        partition: &whp::Partition,
        count: u32,
    ) -> Result<(), Error> {
        verify_vp_models(partition, count, self.config.cpu_profile.model)
    }

    pub(crate) fn validate_partition(
        config: &ProtoPartitionConfig<'_>,
        vtl: Vtl,
    ) -> Result<(), Error> {
        if config.time_abi.is_some()
            && (vtl != Vtl::Vtl0
                || config.hv_config.is_some()
                || config.isolation.is_isolated()
                || config.nested_virt
                || config.device_assignment_msi_iova_range.is_some())
        {
            return Err(TimeAbiError::new(
                TimeAbiCode::ProfileUnsupported,
                "ARM CPU/time contract requires VTL0, no Hyper-V interface, isolation, nested virtualization, or MSI assignment",
            ).into());
        }
        Ok(())
    }

    pub(crate) fn configure(
        config: &TimeAbiConfig,
        partition: &mut whp::PartitionConfig,
        _exits: &mut whp::abi::WHV_EXTENDED_VM_EXITS,
    ) -> Result<Self, Error> {
        config.validate()?;
        config
            .cpu_profile
            .validate()
            .map_err(|e| TimeAbiError::new(TimeAbiCode::ProfileDigest, e.to_string()))?;
        let supported = whp::capabilities::processor_features_banks()
            .for_op("get ARM processor feature capabilities")?;
        if supported.BanksCount != 2 {
            return Err(TimeAbiError::new(
                TimeAbiCode::ProfileUnsupported,
                format!(
                    "WHP reports {} ARM feature banks, expected 2",
                    supported.BanksCount
                ),
            )
            .into());
        }
        let features = profile_features(
            &config.cpu_profile,
            whp::abi::WHV_PROCESSOR_FEATURES(supported.Banks[0]),
        )?;
        let address_bits = profile_physical_address_bits(&config.cpu_profile)?;
        let requested = [features.bank0.0, features.bank1.0];
        require_supported_features(requested, supported.Banks)?;
        partition
            .set_property(whp::PartitionProperty::ProcessorFeaturesBanks(features))
            .for_op("set ARM CPU-profile feature banks")?;
        partition
            .set_property(whp::PartitionProperty::PhysicalAddressWidth(address_bits))
            .for_op("set approved ARM CPU-profile physical address width")?;
        Ok(Self {
            config: config.clone(),
            features: requested,
        })
    }

    pub(crate) fn verify_configuration(
        &self,
        partition: &whp::Partition,
    ) -> Result<BackendPreflight, Error> {
        let expected_bits = profile_physical_address_bits(&self.config.cpu_profile)?;
        let observed_bits = partition
            .physical_address_width()
            .for_op("read ARM CPU-profile physical address width")?;
        let mmfr0 = read_cpu_id(partition, SystemReg::ID_AA64MMFR0_EL1)?;
        let expected_parange = self
            .config
            .cpu_profile
            .id_registers
            .iter()
            .find(|r| r.register == SystemReg::ID_AA64MMFR0_EL1)
            .ok_or_else(|| {
                TimeAbiError::new(
                    TimeAbiCode::CpuSurface,
                    "ARM profile has no MMFR0 requirement",
                )
            })?
            .value
            & 0xf;
        if observed_bits != expected_bits || mmfr0 & 0xf != expected_parange {
            return Err(TimeAbiError::new(
                TimeAbiCode::CpuSurface,
                format!("ARM address contract mismatch: requested={expected_bits} bits, backend={observed_bits} bits, effective MMFR0={mmfr0:#018x}"),
            ).into());
        }
        let observed = partition
            .processor_features_banks()
            .for_op("read ARM CPU-profile feature-bank configuration")?;
        if observed.BanksCount != 2 || observed.Banks != self.features {
            return Err(TimeAbiError::new(
                TimeAbiCode::CpuSurface,
                format!("ARM feature-bank configuration mismatch: requested={:#x?}, count={}, observed={:#x?}; effective guest IDs not verified",
                    self.features, observed.BanksCount, observed.Banks),
            ).into());
        }
        // This existing getter reads ProcessorClockFrequency on both ISAs.
        let frequency = partition
            .tsc_frequency()
            .for_op("get ARM processor counter frequency")?;
        if frequency != self.config.counter_hz {
            return Err(TimeAbiError::new(
                TimeAbiCode::ProfileTimeBits,
                format!("ARM counter-rate configuration mismatch: requested={} Hz, observed={frequency} Hz", self.config.counter_hz),
            ).into());
        }
        tracing::info!(
            native_backend_usable_gpa_bits = self.config.backend_usable_gpa_bits,
            configured_backend_gpa_bits = observed_bits,
            architectural_pa_bits = expected_bits,
            effective_parange = mmfr0 & 0xf,
            "verified ARM backend and architectural address metadata"
        );
        Ok(BackendPreflight {
            counter_hz: frequency,
            physical_address_bits: observed_bits,
            effective_parange: (mmfr0 & 0xf) as u8,
            feature_banks: observed.Banks,
        })
    }

    pub(crate) fn program_cpu_fields(&self, partition: &whp::Partition) -> Result<(), Error> {
        let mut updates = Vec::new();
        let mut violations = Vec::new();
        for requirement in &self.config.cpu_profile.id_registers {
            match read_cpu_id(partition, requirement.register)
                .and_then(|actual| cpu_id_override(requirement, actual))
            {
                Ok(Some(value)) => updates.push((requirement.register, value)),
                Ok(None) => {}
                Err(error) => violations.push(error.to_string()),
            }
        }
        // Validate every native requirement before changing any ID register.
        if !violations.is_empty() {
            return Err(TimeAbiError::new(
                TimeAbiCode::CpuSurface,
                format!("ARM native CPU support failed:\n{}", violations.join("\n")),
            )
            .into());
        }
        for (register, value) in updates {
            let name = cpu_id_name(register)?;
            partition
                .vp(whp::abi::WHV_ANY_VP)
                .set_registers(&[name], &[value.as_abi()])
                .for_op("set supported ARM partition-wide CPU-field override")?;
            let actual = read_cpu_id(partition, register)?;
            if actual != value {
                return Err(TimeAbiError::new(
                    TimeAbiCode::CpuSurface,
                    format!("ARM {register:?} override readback mismatch: expected={value:#018x}, actual={actual:#018x}"),
                ).into());
            }
        }
        Ok(())
    }
}

impl virt::time_abi::TimeAbiBackend for WhpPartitionInner {
    fn configuration(&self) -> &TimeAbiConfig {
        &self
            .time_abi
            .as_ref()
            .expect("time ABI interface requires configured state")
            .config
    }

    fn preflight(&self) -> Result<BackendPreflight, TimeAbiError> {
        let state = self.time_abi.as_ref().ok_or_else(|| {
            TimeAbiError::new(
                TimeAbiCode::ProfileUnsupported,
                "WHP ARM partition has no CPU/time contract",
            )
        })?;
        state
            .verify_configuration(&self.vtl0.whp)
            .map_err(|error| match error {
                Error::TimeAbi(error) => error,
                error => TimeAbiError::new(
                    TimeAbiCode::ProfileUnsupported,
                    format!("WHP ARM configuration readback failed: {error:?}"),
                ),
            })
    }

    fn read_cpu_id_register(&self, register: SystemReg) -> Result<u64, TimeAbiError> {
        read_cpu_id(&self.vtl0.whp, register)
    }

    fn read_active_counter(&self) -> Result<CounterReading, TimeAbiError> {
        let frozen = self.vtl0.time_frozen.lock();
        if *frozen {
            return Err(TimeAbiError::new(
                TimeAbiCode::ProfileTimeBits,
                "WHP ARM counter sampling requires resumed partition time",
            ));
        }
        let mut value = [0u64.as_abi()];
        self.vtl0
            .whp
            .vp(0)
            .get_registers(&[whp::abi::WHvArm64RegisterCntvctEl0], &mut value)
            .map_err(|error| {
                TimeAbiError::new(
                    TimeAbiCode::ProfileTimeBits,
                    format!("reading active WHP ARM CNTVCT_EL0: {error}"),
                )
            })?;
        Ok(CounterReading {
            ticks: u64::from_abi(&value[0]),
            epoch: self
                .vtl0
                .counter_epoch
                .load(std::sync::atomic::Ordering::Relaxed),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use test_with_tracing::test;
    use whp::abi;

    fn synthetic_profile() -> virt::time_abi::aarch64::CpuProfile {
        use virt::time_abi::aarch64::CpuIdRegister;
        let mut profile = virt::time_abi::aarch64::CpuProfile {
            id: "arm64-synthetic-test".into(),
            sha256: [0x5a; 32],
            model: cpu_profile::aarch64::CpuModel {
                mask: 0xff0f_fff0,
                value: 0x410f_d030,
            },
            physical_address_bits: 36,
            cache_policy: virt::time_abi::aarch64::CachePolicy::NativeInformational,
            id_registers: CPU_ID_REGISTERS
                .iter()
                .map(|&(register, _)| CpuIdRegister {
                    register,
                    mask: u64::MAX,
                    value: 0,
                })
                .collect(),
        };
        set_value(&mut profile, SystemReg::ID_AA64PFR0_EL1, 0xff_0001);
        set_value(&mut profile, SystemReg::ID_AA64MMFR0_EL1, 0xf00_0001);
        profile
    }

    fn set_value(
        profile: &mut virt::time_abi::aarch64::CpuProfile,
        register: SystemReg,
        value: u64,
    ) {
        profile
            .id_registers
            .iter_mut()
            .find(|entry| entry.register == register)
            .unwrap()
            .value = value;
    }

    #[test]
    fn cpu_id_overrides_only_remove_owned_privileged_level_descriptions() {
        use virt::time_abi::aarch64::CpuIdRegister;
        let actual = 0x1100_0000_0111_1111;
        let mut requirement = CpuIdRegister {
            register: SystemReg::ID_AA64PFR0_EL1,
            mask: u64::MAX,
            value: actual & !0xff00,
        };
        assert_eq!(
            cpu_id_override(&requirement, actual).unwrap(),
            Some(actual & !0xff00)
        );
        assert_eq!(
            cpu_id_override(&requirement, actual & !0xff00).unwrap(),
            None
        );
        requirement.value &= !(0xf << 16);
        assert!(cpu_id_override(&requirement, actual).is_err());
        requirement.value = actual ^ 0x300;
        assert!(cpu_id_override(&requirement, actual).is_err());
        requirement.register = SystemReg::ID_AA64MMFR0_EL1;
        requirement.value = 1;
        assert!(cpu_id_override(&requirement, 2).is_err());
    }

    #[test]
    fn address_contract_requires_explicit_36_bit_parange() {
        let mut profile = synthetic_profile();
        assert_eq!(profile_physical_address_bits(&profile).unwrap(), 36);
        for encoding in [0, 2, 7, 15] {
            set_value(&mut profile, SystemReg::ID_AA64MMFR0_EL1, encoding);
            assert_eq!(
                profile_physical_address_bits(&profile).unwrap_err().code,
                TimeAbiCode::ProfileUnsupported
            );
        }
        profile
            .id_registers
            .iter_mut()
            .find(|r| r.register == SystemReg::ID_AA64MMFR0_EL1)
            .unwrap()
            .mask &= !0xf;
        assert_eq!(
            profile_physical_address_bits(&profile).unwrap_err().code,
            TimeAbiCode::ProfileUnsupported
        );
    }

    #[test]
    fn address_translation_uses_the_profile_requirement_not_a_model_constant() {
        let mut profile = synthetic_profile();
        profile.id = "another-model.v1".into();
        profile.physical_address_bits = 40;
        set_value(&mut profile, SystemReg::ID_AA64MMFR0_EL1, 2);
        assert_eq!(profile_physical_address_bits(&profile).unwrap(), 40);
        profile.physical_address_bits = 36;
        assert!(profile_physical_address_bits(&profile).is_err());
    }

    #[test]
    fn feature_banks_are_derived_from_architectural_levels_not_generic_bank_bits() {
        let mut profile = synthetic_profile();
        let features = profile_features(&profile, abi::WHV_PROCESSOR_FEATURES::default()).unwrap();
        assert_eq!([features.bank0.0, features.bank1.0], [0, 0]);
        set_value(
            &mut profile,
            SystemReg::ID_AA64ISAR0_EL1,
            2 << 4 | 2 << 12 | 2 << 20,
        );
        set_value(&mut profile, SystemReg::ID_AA64ISAR1_EL1, 1 << 52);
        let features = profile_features(&profile, abi::WHV_PROCESSOR_FEATURES::default()).unwrap();
        assert_eq!(
            features.bank0,
            abi::WHV_PROCESSOR_FEATURES::Aes
                | abi::WHV_PROCESSOR_FEATURES::PolyMul
                | abi::WHV_PROCESSOR_FEATURES::Sha256
                | abi::WHV_PROCESSOR_FEATURES::Sha512
                | abi::WHV_PROCESSOR_FEATURES::Atomic
        );
        assert_eq!(features.bank1, abi::WHV_PROCESSOR_FEATURES1::I8mm);
        set_value(&mut profile, SystemReg::ID_AA64PFR0_EL1, 0x11_0001);
        let features = profile_features(&profile, abi::WHV_PROCESSOR_FEATURES::default()).unwrap();
        let fp_simd = abi::WHV_PROCESSOR_FEATURES::Fp
            | abi::WHV_PROCESSOR_FEATURES::FpHp
            | abi::WHV_PROCESSOR_FEATURES::AdvSimd
            | abi::WHV_PROCESSOR_FEATURES::AdvSimdHp;
        assert_eq!(features.bank0 & fp_simd, fp_simd);
    }

    #[test]
    fn explicit_native_cache_ownership_does_not_pass_through_host_isa() {
        let mut profile = synthetic_profile();
        let features = profile_features(&profile, abi::WHV_PROCESSOR_FEATURES(u64::MAX)).unwrap();
        assert_eq!(
            features.bank0,
            abi::WHV_PROCESSOR_FEATURES::L1ipPipt
                | abi::WHV_PROCESSOR_FEATURES::DzPermitted
                | abi::WHV_PROCESSOR_FEATURES::Idc
                | abi::WHV_PROCESSOR_FEATURES::Dic
        );
        assert_eq!(features.bank1, abi::WHV_PROCESSOR_FEATURES1::default());
        profile
            .id_registers
            .retain(|entry| entry.register != SystemReg::ID_AA64ISAR0_EL1);
        assert!(profile_features(&profile, abi::WHV_PROCESSOR_FEATURES(u64::MAX)).is_err());
    }

    #[test]
    fn missing_ownership_unknown_levels_and_unimplemented_state_families_fail_closed() {
        for (register, value) in [
            (SystemReg::ID_AA64ISAR0_EL1, 1 << 20),
            (SystemReg::ID_AA64ISAR1_EL1, 1 << 4),
            (SystemReg::ID_AA64PFR0_EL1, 0x1_00ff_0001),
            (SystemReg::ID_AA64DFR0_EL1, 1 << 8),
            (SystemReg::ID_AA64MMFR0_EL1, 2 << 24),
        ] {
            let mut profile = synthetic_profile();
            set_value(&mut profile, register, value);
            assert_eq!(
                profile_features(&profile, abi::WHV_PROCESSOR_FEATURES::default())
                    .unwrap_err()
                    .code,
                TimeAbiCode::ProfileUnsupported
            );
        }
        let mut profile = synthetic_profile();
        profile
            .id_registers
            .retain(|entry| entry.register != SystemReg::ID_AA64PFR1_EL1);
        assert_eq!(
            profile_features(&profile, abi::WHV_PROCESSOR_FEATURES::default())
                .unwrap_err()
                .code,
            TimeAbiCode::ProfileUnsupported
        );
        let mut profile = synthetic_profile();
        profile.id_registers[0].mask &= !1;
        assert_eq!(
            profile_features(&profile, abi::WHV_PROCESSOR_FEATURES::default())
                .unwrap_err()
                .code,
            TimeAbiCode::ProfileUnsupported
        );
    }

    #[test]
    fn cpu_id_provider_is_partition_scoped_and_rejects_other_registers() {
        let mut names = std::collections::BTreeSet::new();
        for (register, name) in CPU_ID_REGISTERS {
            assert!((0x00022000..=0x0002207f).contains(&name.0));
            assert!(names.insert(name.0));
            assert_eq!(cpu_id_name(register).unwrap(), name);
        }
        assert_eq!(names.len(), 15);
        assert_eq!(abi::WHV_ANY_VP, u32::MAX);
        for register in [
            SystemReg::MPIDR_EL1,
            SystemReg::CTR_EL0,
            SystemReg::CNTFRQ_EL0,
        ] {
            assert_eq!(
                cpu_id_name(register).unwrap_err().code,
                TimeAbiCode::ProfileUnsupported
            );
        }
    }

    #[test]
    #[ignore = "requires native ARM64 Windows with WHP enabled; configuration diagnostic only"]
    fn native_arm_feature_configuration_readback() {
        let host = whp::capabilities::processor_features().unwrap();
        let mut reduced = host;
        reduced.bank0 &= !(abi::WHV_PROCESSOR_FEATURES::PmuV3
            | abi::WHV_PROCESSOR_FEATURES::PmuV3ArmV81
            | abi::WHV_PROCESSOR_FEATURES::PmuV3ArmV84
            | abi::WHV_PROCESSOR_FEATURES::PmuV3ArmV85);
        for features in [host, reduced] {
            for count in [1, 2, 4, 8] {
                let expected = [features.bank0.0, features.bank1.0];
                let counter_hz = whp::capabilities::processor_clock_frequency().unwrap();
                let mut raw = whp::PartitionConfig::new().unwrap();
                raw.set_property(whp::PartitionProperty::ProcessorCount(count))
                    .unwrap();
                raw.set_property(whp::PartitionProperty::ProcessorFeaturesBanks(features))
                    .unwrap();
                let gic = abi::WHV_ARM64_IC_PARAMETERS {
                    EmulationMode: abi::WHV_ARM64_IC_EMULATION_MODE::GicV3,
                    Reserved: 0,
                    GicV3Parameters: abi::WHV_ARM64_IC_GIC_V3_PARAMETERS {
                        GicdBaseAddress: 0xffff_0000,
                        GitsTranslatorBaseAddress: 0,
                        Reserved: 0,
                        GicLpiIntIdBits: 1,
                        GicPpiOverflowInterruptFromCntv: 20,
                        GicPpiPerformanceMonitorsInterrupt: 23,
                        Reserved1: [0; 6],
                    },
                };
                raw.set_property(whp::PartitionProperty::GicParameters(gic))
                    .unwrap();
                let partition = raw.create().unwrap();
                let observed = partition.processor_features_banks().unwrap();
                assert_eq!(observed.BanksCount, 2);
                assert_eq!(observed.Banks, expected);
                assert_eq!(partition.tsc_frequency().unwrap(), counter_hz);
                println!(
                    "ARM_PUBLIC_PROPERTY capacity={count} cl_flush_size_raw={:?} physical_address_width={:?}",
                    partition.processor_cl_flush_size(),
                    partition.physical_address_width()
                );
                let mut fixture = synthetic_profile();
                for (register, _) in CPU_ID_REGISTERS {
                    let value = read_cpu_id(&partition, register).unwrap();
                    set_value(&mut fixture, register, value);
                    println!(
                        "ARM_ID_BEFORE_VP_CREATE capacity={count} register={register:?} value={value:#018x}"
                    );
                }
                if features.bank0 & abi::WHV_PROCESSOR_FEATURES::PmuV3
                    == abi::WHV_PROCESSOR_FEATURES::default()
                {
                    // This checks native-ID -> bank translation, not a released
                    // profile. Native cache ownership is not a raw cache read.
                    let translated = profile_features(&fixture, features.bank0).unwrap();
                    assert_eq!([translated.bank0.0, translated.bank1.0], expected);
                    println!(
                        "ARM_TRANSLATION_PARITY capacity={count} explicit_cache_ownership=native_informational admitted=false"
                    );
                    let mmfr0 = fixture
                        .id_registers
                        .iter()
                        .find(|r| r.register == SystemReg::ID_AA64MMFR0_EL1)
                        .unwrap()
                        .value;
                    set_value(
                        &mut fixture,
                        SystemReg::ID_AA64MMFR0_EL1,
                        (mmfr0 & !0xf) | 1,
                    );
                    let pfr0 = fixture
                        .id_registers
                        .iter()
                        .find(|r| r.register == SystemReg::ID_AA64PFR0_EL1)
                        .unwrap()
                        .value;
                    set_value(&mut fixture, SystemReg::ID_AA64PFR0_EL1, pfr0 & !0xff00);
                    let config = TimeAbiConfig {
                        cpu_profile: cpu_profile::aarch64::derive_host(
                            &virt::time_abi::aarch64::Fingerprint {
                                midr: native_fingerprint().unwrap().midr,
                                registers: fixture.id_registers,
                                counter_hz,
                                backend_usable_gpa_bits: partition
                                    .physical_address_width()
                                    .unwrap(),
                            },
                        )
                        .unwrap(),
                        counter_hz,
                        backend_usable_gpa_bits: partition.physical_address_width().unwrap(),
                    };
                    let mut governed = whp::PartitionConfig::new().unwrap();
                    governed
                        .set_property(whp::PartitionProperty::ProcessorCount(count))
                        .unwrap();
                    governed
                        .set_property(whp::PartitionProperty::GicParameters(gic))
                        .unwrap();
                    let contract = WhpTimeAbi::configure(
                        &config,
                        &mut governed,
                        &mut abi::WHV_EXTENDED_VM_EXITS(0),
                    )
                    .unwrap();
                    let governed = governed.create().unwrap();
                    let readback = contract.verify_configuration(&governed).unwrap();
                    assert_eq!(readback.counter_hz, counter_hz);
                    assert_eq!(readback.physical_address_bits, 36);
                    assert_eq!(readback.effective_parange, 1);
                    assert_eq!(readback.feature_banks, expected);
                    let mut unsupported = WhpTimeAbi {
                        config: config.clone(),
                        features: expected,
                    };
                    unsupported
                        .config
                        .cpu_profile
                        .id_registers
                        .iter_mut()
                        .find(|r| r.register == SystemReg::ID_AA64MMFR1_EL1)
                        .unwrap()
                        .value ^= 1;
                    assert!(unsupported.program_cpu_fields(&governed).is_err());
                    assert_eq!(
                        read_cpu_id(&governed, SystemReg::ID_AA64PFR0_EL1).unwrap(),
                        pfr0
                    );
                    contract.program_cpu_fields(&governed).unwrap();
                    for (register, _) in CPU_ID_REGISTERS {
                        let expected = config
                            .cpu_profile
                            .id_registers
                            .iter()
                            .find(|r| r.register == register)
                            .unwrap();
                        assert_eq!(read_cpu_id(&governed, register).unwrap(), expected.value);
                    }
                    println!(
                        "ARM_ADDRESS_CONTRACT capacity={count} configured_bits=36 effective_parange=1 before_vp_create=true full_profile_admitted=false"
                    );
                }
                for vp in 0..count {
                    partition.create_vp(vp).create().unwrap();
                }
                for vp in 0..count {
                    partition.delete_vp(vp).unwrap();
                }
                println!(
                    "ARM_CONFIG_ONLY vps={count} banks={:#x?} counter_hz={} effective_guest_surface_verified=false product_boots=0",
                    expected, counter_hz
                );
            }
        }
    }
    #[test]
    fn native_pac_uses_only_the_measured_architectural_level_and_related_flag() {
        let profile = cpu_profile::aarch64::pinned().unwrap();
        let features = profile_features(&profile, abi::WHV_PROCESSOR_FEATURES::default()).unwrap();
        let pac = abi::WHV_PROCESSOR_FEATURES::ApaBase
            | abi::WHV_PROCESSOR_FEATURES::ApaEp
            | abi::WHV_PROCESSOR_FEATURES::ApaEp2
            | abi::WHV_PROCESSOR_FEATURES::ApaEp2Fp
            | abi::WHV_PROCESSOR_FEATURES::ApaEp2Fpc
            | abi::WHV_PROCESSOR_FEATURES::Gpa;
        assert_eq!(
            features.bank0 & pac,
            abi::WHV_PROCESSOR_FEATURES::ApaEp2Fpc | abi::WHV_PROCESSOR_FEATURES::Gpa
        );
        assert_eq!(
            features.bank1 & abi::WHV_PROCESSOR_FEATURES1::SpecFpacc,
            abi::WHV_PROCESSOR_FEATURES1::SpecFpacc
        );
        require_supported_features([features.bank0.0, features.bank1.0], [u64::MAX, u64::MAX])
            .unwrap();
        let missing_pac = [
            features.bank0.0 & !abi::WHV_PROCESSOR_FEATURES::ApaEp2Fpc.0,
            features.bank1.0,
        ];
        assert_eq!(
            require_supported_features([features.bank0.0, features.bank1.0], missing_pac)
                .unwrap_err()
                .code,
            TimeAbiCode::ProfileUnsupported
        );
        assert!(features.bank0.is_set(abi::WHV_PROCESSOR_FEATURES::Bf16));
        assert!(!features.bank0.is_set(abi::WHV_PROCESSOR_FEATURES::Ebf16));
        let mut enhanced = features;
        enhanced.bank0 |= abi::WHV_PROCESSOR_FEATURES::Ebf16;
        require_supported_features(
            [features.bank0.0, features.bank1.0],
            [enhanced.bank0.0, enhanced.bank1.0],
        )
        .unwrap();
    }

    #[test]
    #[ignore = "requires native ARM64 Windows WHP; no guest entry"]
    fn native_canonical_selection_fingerprint() {
        let native = native_fingerprint().unwrap();
        for requested in [cpu_profile::AUTO, cpu_profile::HOST] {
            let profile = cpu_profile::aarch64::select(requested, &native).unwrap();
            profile.validate().unwrap();
            println!(
                "ARM_CANONICAL requested={requested} id={} sha256={} native_f={} midr={:#x} backend_usable_gpa_bits={} architectural_pa_bits={} document={}",
                profile.id,
                profile
                    .sha256
                    .iter()
                    .map(|b| format!("{b:02x}"))
                    .collect::<String>(),
                native.counter_hz,
                native.midr,
                native.backend_usable_gpa_bits,
                profile.physical_address_bits,
                String::from_utf8(profile.encode().unwrap()).unwrap()
            );
        }
    }
}
